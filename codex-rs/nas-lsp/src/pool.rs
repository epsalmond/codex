//! Slot-owned workers and leases. See `.codex/design/nas-lsp-broker.md`.

use crate::config::PoolLimits;
use crate::config::WorkspaceConfig;
use crate::resource::ResourceLimits;
use crate::worker::Worker;
use crate::worker::WorkerQuery;
use codex_lsp_protocol::NavigationResponse;
use codex_lsp_protocol::Status;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn status(s: Status, reason: &str) -> NavigationResponse {
    NavigationResponse::status(s, reason)
}

pub(crate) struct Pool {
    limits: PoolLimits,
    slots: Mutex<HashMap<String, Slot>>,
    cancel: CancellationToken,
}
struct Slot {
    sender: mpsc::Sender<Job>,
    state: Arc<State>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}
struct State {
    leases: AtomicUsize,
    capacity: Arc<Semaphore>,
    last_used: StdMutex<Instant>,
    pid: AtomicU32,
}
struct Lease {
    state: Arc<State>,
    _worker_permit: OwnedSemaphorePermit,
    _global_permit: OwnedSemaphorePermit,
}
impl Drop for Lease {
    fn drop(&mut self) {
        *self.state.last_used.lock().expect("last-used lock") = Instant::now();
        self.state.leases.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Job {
    query: WorkerQuery,
    deadline: Instant,
    cancel: CancellationToken,
    response: oneshot::Sender<NavigationResponse>,
    _lease: Lease,
}

impl Pool {
    pub(crate) fn new(limits: PoolLimits) -> Self {
        Self {
            limits,
            slots: Mutex::new(HashMap::new()),
            cancel: CancellationToken::new(),
        }
    }

    pub(crate) async fn submit(
        &self,
        config: Arc<WorkspaceConfig>,
        key: String,
        query: WorkerQuery,
        deadline: Instant,
        cancel: CancellationToken,
        global_permit: OwnedSemaphorePermit,
    ) -> Result<oneshot::Receiver<NavigationResponse>, NavigationResponse> {
        let mut slots = self.slots.lock().await;
        if slots.get(&key).is_some_and(|slot| slot.sender.is_closed()) {
            if let Some(slot) = slots.remove(&key) {
                retire(slot).await;
            }
        }
        if !slots.contains_key(&key) {
            if slots.len() >= self.limits.max_workers {
                let oldest = slots
                    .iter()
                    .filter(|(_, slot)| slot.state.leases.load(Ordering::SeqCst) == 0)
                    .min_by_key(|(_, slot)| *slot.state.last_used.lock().expect("last-used lock"))
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    if let Some(slot) = slots.remove(&oldest) {
                        retire(slot).await;
                    }
                } else {
                    return Err(status(Status::Busy, "pool_full"));
                }
            }
            slots.insert(
                key.clone(),
                Slot::new(
                    config,
                    self.limits.max_queue_per_worker,
                    self.cancel.child_token(),
                ),
            );
        }
        let slot = slots.get(&key).expect("inserted slot");
        let worker_permit = slot
            .state
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| status(Status::Busy, "worker_queue_full"))?;
        slot.state.leases.fetch_add(1, Ordering::SeqCst);
        let lease = Lease {
            state: slot.state.clone(),
            _worker_permit: worker_permit,
            _global_permit: global_permit,
        };
        let (response, receiver) = oneshot::channel();
        slot.sender
            .try_send(Job {
                query,
                deadline,
                cancel,
                response,
                _lease: lease,
            })
            .map_err(|_| status(Status::Busy, "worker_queue_full"))?;
        Ok(receiver)
    }

    pub(crate) async fn invalidate(&self, key: &str) {
        if let Some(slot) = self.slots.lock().await.get(key) {
            slot.cancel.cancel();
        }
    }

    pub(crate) async fn maintain(&self, resources: Option<&ResourceLimits>) {
        let mut slots = self.slots.lock().await;
        let pressure = resources.is_some_and(ResourceLimits::under_pressure);
        let keys: Vec<_> = slots
            .iter()
            .filter(|(_, slot)| {
                slot.state.leases.load(Ordering::SeqCst) == 0
                    && (pressure
                        || slot
                            .state
                            .last_used
                            .lock()
                            .expect("last-used lock")
                            .elapsed()
                            >= self.limits.idle_ttl)
            })
            .map(|(key, _)| key.clone())
            .collect();
        let mut retired = JoinSet::new();
        for key in keys {
            if let Some(slot) = slots.remove(&key) {
                retired.spawn(retire(slot));
            }
        }
        while retired.join_next().await.is_some() {}
        for slot in slots.values() {
            let pid = slot.state.pid.load(Ordering::Relaxed);
            if pid != 0 {
                if let Ok(rss) = crate::resource::process_tree_rss(pid) {
                    tracing::debug!(pid, rss_bytes = rss, "navigation worker process tree");
                }
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel.cancel();
        let mut slots = self.slots.lock().await;
        let all: Vec<_> = slots.drain().map(|(_, slot)| slot).collect();
        // Cancel together before awaiting any task, preserving a global bound.
        for slot in &all {
            slot.cancel.cancel();
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        for mut slot in all {
            if tokio::time::timeout_at(deadline, &mut slot.task)
                .await
                .is_err()
            {
                slot.task.abort();
                let _ = slot.task.await;
            }
        }
    }
}

impl Slot {
    fn new(config: Arc<WorkspaceConfig>, queue: usize, cancel: CancellationToken) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Job>(queue + 1);
        let state = Arc::new(State {
            leases: AtomicUsize::new(0),
            capacity: Arc::new(Semaphore::new(queue + 1)),
            last_used: StdMutex::new(Instant::now()),
            pid: AtomicU32::new(0),
        });
        let actor_state = state.clone();
        let actor_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let mut worker: Option<Worker> = None;
            loop {
                let job = tokio::select! {
                    biased;
                    _ = actor_cancel.cancelled() => break,
                    job = receiver.recv() => match job {Some(job)=>job,None=>break},
                };
                if job.cancel.is_cancelled() {
                    let _ = job.response.send(status(Status::Cancelled, "cancelled"));
                    continue;
                }
                if Instant::now() >= job.deadline {
                    let _ = job
                        .response
                        .send(status(Status::Timeout, "deadline_exceeded"));
                    continue;
                }
                if worker.is_none() {
                    // Startup belongs to the slot, not the first client's lifetime.
                    match Worker::start(config.clone(), job.deadline, actor_cancel.clone()).await {
                        Ok(started) => {
                            actor_state
                                .pid
                                .store(started.process_id().unwrap_or(0), Ordering::Relaxed);
                            worker = Some(started);
                        }
                        Err(response) => {
                            let _ = job.response.send(response);
                            continue;
                        }
                    }
                }
                if let Some(worker) = worker.as_mut() {
                    let operation_cancel = job.cancel.child_token();
                    let response = {
                        let operation =
                            worker.query(job.query, job.deadline, operation_cancel.clone());
                        tokio::pin!(operation);
                        tokio::select! {
                            response = &mut operation => response,
                            _ = actor_cancel.cancelled() => {
                                operation_cancel.cancel();
                                // During service shutdown the worker gets a bounded drain.
                                match tokio::time::timeout(Duration::from_secs(1),&mut operation).await {
                                    Ok(response)=>response,
                                    Err(_)=>status(Status::Cancelled,"broker_shutdown"),
                                }
                            }
                        }
                    };
                    actor_state
                        .pid
                        .store(worker.process_id().unwrap_or(0), Ordering::Relaxed);
                    // A dropped receiver does not drop the operation or lease.
                    let _ = job.response.send(response);
                }
            }
            receiver.close();
            if let Some(worker) = worker.as_mut() {
                worker.shutdown().await;
            }
            actor_state.pid.store(0, Ordering::Relaxed);
        });
        Self {
            sender,
            state,
            cancel,
            task,
        }
    }
}
async fn retire(mut slot: Slot) {
    slot.cancel.cancel();
    if tokio::time::timeout(Duration::from_secs(3), &mut slot.task)
        .await
        .is_err()
    {
        slot.task.abort();
        let _ = slot.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Readiness;
    use codex_lsp_protocol::Operation;
    use codex_lsp_protocol::Position;
    use std::path::Path;
    use std::path::PathBuf;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    const SERVER: &str = r#"
import json,sys,socket
signal=None
waiting=None
def send(value):
 data=json.dumps(dict(jsonrpc='2.0',**value)).encode()
 sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(data)).encode()+data);sys.stdout.buffer.flush()
while True:
 headers={}
 while True:
  line=sys.stdin.buffer.readline()
  if not line: sys.exit(0)
  if line==b'\r\n': break
  k,v=line.decode().strip().split(':',1);headers[k.lower()]=v.strip()
 msg=json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
 method=msg.get('method')
 if method=='initialize':
  if len(sys.argv)>2 and sys.argv[2]=='startup':
   signal=socket.socket(socket.AF_UNIX);signal.connect(sys.argv[1]);signal.send(b'I');signal.recv(1)
  send(dict(id=msg['id'],result=dict(capabilities=dict(positionEncoding='utf-16',definitionProvider=True,referencesProvider=True,textDocumentSync=1))))
 elif method=='textDocument/definition':
  waiting=msg['id'];signal=socket.socket(socket.AF_UNIX);signal.connect(sys.argv[1]);signal.send(b'R')
 elif method=='$/cancelRequest':
  signal.send(b'C');signal.recv(1)
  send(dict(id=waiting,error=dict(code=-32800,message='cancelled')))
 elif method=='textDocument/references':
  send(dict(id=msg['id'],result=[dict(uri=msg['params']['textDocument']['uri'],range=dict(start=dict(line=0,character=0),end=dict(line=0,character=1)))]))
 elif method=='shutdown': send(dict(id=msg['id'],result=None))
 elif method=='exit': sys.exit(0)
"#;
    fn config(root: &Path, signal: &Path) -> Arc<WorkspaceConfig> {
        Arc::new(WorkspaceConfig {
            id: "fake".into(),
            root: root.to_owned(),
            language: "python".into(),
            executable: PathBuf::from("/usr/bin/python3"),
            args: vec![
                "-u".into(),
                "-c".into(),
                SERVER.into(),
                signal.to_string_lossy().into_owned(),
            ],
            source_extensions: vec!["py".into()],
            exclude_dirs: vec![],
            readiness: Readiness::PyreflyLazyBlocking,
        })
    }
    fn query(root: &Path, operation: Operation) -> WorkerQuery {
        WorkerQuery {
            operation,
            path: root.join("main.py"),
            position: Position { line: 1, column: 1 },
            max_results: 100,
        }
    }

    /// This bypasses only broker transport/resource admission, exercising the real
    /// slot actor, Worker, stdio server, request waiter, and surviving process.
    #[tokio::test]
    async fn cancelled_client_returns_while_lease_drains_and_queued_query_reuses_pid() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("main.py"), "x=1\n").expect("source");
        let signals = tempfile::tempdir().expect("signals");
        let path = signals.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("listener");
        let config = config(root.path(), &path);
        let limits = PoolLimits {
            max_workers: 1,
            max_queue_per_worker: 1,
            idle_ttl: Duration::from_millis(10),
            ..PoolLimits::default()
        };
        let pool = Pool::new(limits);
        let global = Arc::new(Semaphore::new(3));
        let deadline = Instant::now() + Duration::from_secs(5);
        let cancel = CancellationToken::new();
        let receiver = pool
            .submit(
                config.clone(),
                "one".into(),
                query(root.path(), Operation::Definition),
                deadline,
                cancel.clone(),
                global.clone().try_acquire_owned().expect("permit"),
            )
            .await
            .expect("first admitted");
        let caller = tokio::spawn(crate::broker::await_response(
            receiver,
            deadline,
            cancel.clone(),
        ));
        let (mut control, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .expect("in flight")
            .expect("control");
        let mut byte = [0u8; 1];
        control
            .read_exact(&mut byte)
            .await
            .expect("inflight marker");
        assert_eq!(byte, b"R"[..]);
        let pid = pool
            .slots
            .lock()
            .await
            .get("one")
            .expect("slot")
            .state
            .pid
            .load(Ordering::Relaxed);
        assert!(pid > 0);
        let second = pool
            .submit(
                config.clone(),
                "one".into(),
                query(root.path(), Operation::References),
                deadline,
                CancellationToken::new(),
                global.clone().try_acquire_owned().expect("permit"),
            )
            .await
            .expect("second queued");
        let third = pool
            .submit(
                config.clone(),
                "one".into(),
                query(root.path(), Operation::References),
                deadline,
                CancellationToken::new(),
                global.clone().try_acquire_owned().expect("permit"),
            )
            .await;
        assert_eq!(
            third.expect_err("queue bounded").reason.as_deref(),
            Some("worker_queue_full")
        );
        let other = pool
            .submit(
                config,
                "two".into(),
                query(root.path(), Operation::References),
                deadline,
                CancellationToken::new(),
                global.clone().try_acquire_owned().expect("permit"),
            )
            .await;
        assert_eq!(
            other.expect_err("active pool bounded").reason.as_deref(),
            Some("pool_full")
        );
        let started = Instant::now();
        cancel.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(500), caller)
                .await
                .expect("prompt client return")
                .expect("caller")
                .status,
            Status::Cancelled
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        control
            .read_exact(&mut byte)
            .await
            .expect("server received cancellation");
        assert_eq!(byte, b"C"[..]);
        assert_eq!(
            global.available_permits(),
            1,
            "cancelled request retains lease during drain"
        );
        assert_eq!(
            pool.slots
                .lock()
                .await
                .get("one")
                .expect("slot")
                .state
                .leases
                .load(Ordering::SeqCst),
            2
        );
        control.write_all(b"D").await.expect("release response");
        assert_eq!(
            tokio::time::timeout_at(deadline, second)
                .await
                .expect("queued query completes")
                .expect("response")
                .status,
            Status::Ok
        );
        assert_eq!(
            pool.slots
                .lock()
                .await
                .get("one")
                .expect("slot")
                .state
                .pid
                .load(Ordering::Relaxed),
            pid
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        pool.maintain(None).await;
        assert!(pool.slots.lock().await.is_empty(), "idle slot evicted");
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "worker reaped"
        );
        assert_eq!(global.available_permits(), 3);
        pool.shutdown().await;
    }
    #[tokio::test]
    async fn slow_startup_isolated_by_workspace_and_shutdown_is_bounded() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        for root in [first.path(), second.path()] {
            std::fs::write(root.join("main.py"), "x=1\n").expect("source");
        }
        let signals = tempfile::tempdir().expect("signals");
        let path = signals.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("listener");
        let mut slow = (*config(first.path(), &path)).clone();
        slow.args.push("startup".into());
        let pool = Pool::new(PoolLimits::default());
        let global = Arc::new(Semaphore::new(3));
        let deadline = Instant::now() + Duration::from_secs(10);
        let slow_response = pool
            .submit(
                Arc::new(slow),
                "slow".into(),
                query(first.path(), Operation::References),
                deadline,
                CancellationToken::new(),
                global.clone().try_acquire_owned().expect("permit"),
            )
            .await
            .expect("slow submitted");
        let (mut control, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .expect("slow initializing")
            .expect("control");
        let mut byte = [0u8; 1];
        control
            .read_exact(&mut byte)
            .await
            .expect("initialization signal");
        assert_eq!(byte, b"I"[..]);
        let quick = pool
            .submit(
                config(second.path(), &path),
                "quick".into(),
                query(second.path(), Operation::References),
                deadline,
                CancellationToken::new(),
                global.clone().try_acquire_owned().expect("permit"),
            )
            .await
            .expect("separate slot");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), quick)
                .await
                .expect("other workspace independent of startup")
                .expect("response")
                .status,
            Status::Ok
        );
        control.write_all(b"R").await.expect("release startup");
        assert_eq!(
            tokio::time::timeout_at(deadline, slow_response)
                .await
                .expect("first startup completes")
                .expect("response")
                .status,
            Status::Ok
        );
        // Hold a live LSP operation even after cancellation, forcing shutdown's
        // bounded drain and process-group termination path.
        let blocked = pool
            .submit(
                config(second.path(), &path),
                "quick".into(),
                query(second.path(), Operation::Definition),
                deadline,
                CancellationToken::new(),
                global.clone().try_acquire_owned().expect("permit"),
            )
            .await
            .expect("blocked operation");
        let (mut control, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .expect("blocked query starts")
            .expect("control");
        control.read_exact(&mut byte).await.expect("query signal");
        let pids: Vec<_> = pool
            .slots
            .lock()
            .await
            .values()
            .map(|slot| slot.state.pid.load(Ordering::Relaxed))
            .collect();
        let started = Instant::now();
        pool.shutdown().await;
        assert!(started.elapsed() < Duration::from_millis(4500));
        for pid in pids {
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "reaped {pid}"
            );
        }
        drop(blocked);
    }
}
