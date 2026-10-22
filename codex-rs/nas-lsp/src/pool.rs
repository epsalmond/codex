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
    retiring: bool,
}
struct State {
    leases: AtomicUsize,
    capacity: Arc<Semaphore>,
    last_used: StdMutex<Instant>,
    pid: AtomicU32,
    stopped: CancellationToken,
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

    /// A full leased pool returns Busy. Idle replacement waits for broker-owned
    /// retirement within this caller's deadline and cancellation token.
    pub(crate) async fn submit(
        &self,
        config: Arc<WorkspaceConfig>,
        key: String,
        query: WorkerQuery,
        deadline: Instant,
        cancel: CancellationToken,
        global_permit: OwnedSemaphorePermit,
    ) -> Result<oneshot::Receiver<NavigationResponse>, NavigationResponse> {
        let mut slots = loop {
            let mut slots = self.slots.lock().await;
            if cancel.is_cancelled() {
                return Err(status(Status::Cancelled, "cancelled"));
            }
            if Instant::now() >= deadline {
                return Err(status(Status::Timeout, "deadline_exceeded"));
            }
            // A slot owns its actor even after retirement starts. Only finished
            // actors (whose workers have been reaped) release process capacity.
            slots.retain(|_, slot| !slot.task.is_finished());
            if slots.get(&key).is_some_and(|slot| slot.sender.is_closed()) {
                if let Some(slot) = slots.get_mut(&key) {
                    slot.retire();
                }
            }
            let stopped = if let Some(slot) = slots.get(&key).filter(|slot| slot.retiring) {
                Some(slot.state.stopped.clone())
            } else if !slots.contains_key(&key) && slots.len() >= self.limits.max_workers {
                // Reuse an existing retirement before evicting another worker.
                let victim = slots
                    .iter()
                    .filter(|(_, slot)| {
                        slot.retiring || slot.state.leases.load(Ordering::SeqCst) == 0
                    })
                    .min_by_key(|(_, slot)| {
                        (
                            !slot.retiring,
                            *slot.state.last_used.lock().expect("last-used lock"),
                        )
                    })
                    .map(|(key, _)| key.clone());
                let Some(victim) = victim else {
                    return Err(status(Status::Busy, "pool_full"));
                };
                let slot = slots.get_mut(&victim).expect("selected victim");
                slot.retire();
                Some(slot.state.stopped.clone())
            } else {
                None
            };
            if let Some(stopped) = stopped {
                // Caller cancellation drops only a notification waiter. The
                // actor and its counted slot remain owned by this pool.
                drop(slots);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(status(Status::Cancelled, "cancelled")),
                    _ = tokio::time::sleep_until(deadline) => return Err(status(Status::Timeout, "deadline_exceeded")),
                    _ = stopped.cancelled() => {},
                }
                continue;
            }
            break slots;
        };
        if !slots.contains_key(&key) {
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
        if let Some(slot) = self.slots.lock().await.get_mut(key) {
            slot.retire();
        }
    }

    pub(crate) async fn maintain(&self, resources: Option<&ResourceLimits>) {
        let mut slots = self.slots.lock().await;
        slots.retain(|_, slot| !slot.task.is_finished());
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
        for key in keys {
            if let Some(slot) = slots.get_mut(&key) {
                slot.retire();
            }
        }
        let stopped: Vec<_> = slots
            .values()
            .filter(|slot| slot.retiring)
            .map(|slot| slot.state.stopped.clone())
            .collect();
        for slot in slots.values() {
            let pid = slot.state.pid.load(Ordering::Relaxed);
            if pid != 0 {
                if let Ok(rss) = crate::resource::process_tree_rss(pid) {
                    tracing::debug!(pid, rss_bytes = rss, "navigation worker process tree");
                }
            }
        }
        // No global map lock during worker shutdown. Dropping maintenance only
        // drops these notifications; the actors and capacity stay in the pool.
        drop(slots);
        for stopped in stopped {
            stopped.cancelled().await;
        }
        self.slots
            .lock()
            .await
            .retain(|_, slot| !slot.task.is_finished());
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel.cancel();
        let mut slots = self.slots.lock().await;
        // Cancel together before awaiting any task, preserving a global bound.
        for slot in slots.values_mut() {
            slot.retire();
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        for slot in slots.values_mut() {
            if tokio::time::timeout_at(deadline, &mut slot.task)
                .await
                .is_err()
            {
                slot.task.abort();
                let _ = (&mut slot.task).await;
            }
        }
        slots.clear();
    }
}

impl Slot {
    fn retire(&mut self) {
        self.retiring = true;
        self.cancel.cancel();
    }

    fn new(config: Arc<WorkspaceConfig>, queue: usize, cancel: CancellationToken) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Job>(queue + 1);
        let state = Arc::new(State {
            leases: AtomicUsize::new(0),
            capacity: Arc::new(Semaphore::new(queue + 1)),
            last_used: StdMutex::new(Instant::now()),
            pid: AtomicU32::new(0),
            stopped: CancellationToken::new(),
        });
        let actor_state = state.clone();
        let actor_cancel = cancel.clone();
        let completion = state.stopped.clone().drop_guard();
        let task = tokio::spawn(async move {
            // Signal maintenance on normal exit, panic, or explicit abort.
            let _completion = completion;
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
            retiring: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Readiness;
    use codex_lsp_protocol::Operation;
    use codex_lsp_protocol::Position;
    use std::cell::Cell;
    use std::future::Future;
    use std::future::poll_fn;
    use std::path::Path;
    use std::path::PathBuf;
    use std::task::Poll;
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
            typescript_tsserver: None,
            rustc: None,
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

    const RETIRING_SERVER: &str = r#"
import json,os,socket,struct,sys
control=socket.socket(socket.AF_UNIX);control.connect(sys.argv[1])
control.sendall(struct.pack('!I',os.getpid()))
if len(sys.argv)>3:
 control.sendall(b'L' if os.path.exists('/proc/'+sys.argv[3]) else b'R')
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
  send(dict(id=msg['id'],result=dict(capabilities=dict(positionEncoding='utf-16',definitionProvider=True,referencesProvider=True,textDocumentSync=1))))
 elif method=='textDocument/references': send(dict(id=msg['id'],result=[]))
 elif method=='shutdown':
  if sys.argv[2]=='stall': control.sendall(b'S');control.recv(1)
  send(dict(id=msg['id'],result=None))
 elif method=='exit': sys.exit(0)
"#;

    /// External processes use wall time. Keep the paused Tokio clock from
    /// auto-advancing while they perform I/O, with a real-time failure bound.
    async fn real_io<T>(stage: &Cell<&str>, future: impl Future<Output = T>) -> T {
        let started = std::time::Instant::now();
        tokio::pin!(future);
        poll_fn(|cx| {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "I/O stalled: {}",
                stage.get()
            );
            match future.as_mut().poll(cx) {
                Poll::Ready(value) => Poll::Ready(value),
                Poll::Pending => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        })
        .await
    }

    /// Real stdio worker shutdown is socket-gated with virtual time frozen, so
    /// retirement cannot finish between cancellation and the admission probes.
    #[tokio::test]
    async fn cancelled_replacement_and_maintenance_keep_retiring_process_counted() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("main.py"), "x=1\n").expect("source");
        let signals = tempfile::tempdir().expect("signals");
        let path = signals.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("listener");
        let mut stalled = (*config(root.path(), &path)).clone();
        stalled.args[2] = RETIRING_SERVER.into();
        stalled.args.push("stall".into());
        let pool = Arc::new(Pool::new(PoolLimits {
            max_workers: 1,
            ..PoolLimits::default()
        }));
        let global = Arc::new(Semaphore::new(20));
        let deadline = Instant::now() + Duration::from_secs(10);
        let first = pool
            .submit(
                Arc::new(stalled.clone()),
                "victim".into(),
                query(root.path(), Operation::References),
                deadline,
                CancellationToken::new(),
                global.clone().try_acquire_owned().expect("permit"),
            )
            .await
            .expect("victim admitted");
        assert_eq!(first.await.expect("victim response").status, Status::Ok);
        let (mut control, _) = listener.accept().await.expect("victim control");
        let pid = control.read_u32().await.expect("victim PID");
        let mut replacement = stalled;
        replacement.args[4] = "observe".into();
        replacement.args.push(pid.to_string());
        let replacement = Arc::new(replacement);

        tokio::time::pause();
        let stage = Cell::new("replacement retirement");
        real_io(&stage, async {
            let mut submission = Box::pin(pool.submit(
                replacement.clone(),
                "replacement".into(),
                query(root.path(), Operation::References),
                deadline,
                CancellationToken::new(),
                global.clone().try_acquire_owned().expect("permit"),
            ));
            // Poll exactly once, then drop as a cancelled Broker::query select
            // would. The broken implementation is pending inside retire here.
            let submitted = poll_fn(|cx| Poll::Ready(submission.as_mut().poll(cx))).await;
            drop(submission);
            let mut byte = [0];
            stage.set("shutdown marker");
            control
                .read_exact(&mut byte)
                .await
                .expect("shutdown marker");
            assert_eq!(byte, *b"S");
            assert!(PathBuf::from(format!("/proc/{pid}")).exists());
            assert!(submitted.is_pending(), "replacement waits for retirement");

            let stopped = {
                let slots = pool.slots.lock().await;
                assert_eq!(slots.len(), 1, "active + retiring respects max_workers");
                let victim = slots.get("victim").expect("retiring slot remains owned");
                assert!(victim.retiring);
                assert_eq!(victim.state.pid.load(Ordering::Relaxed), pid);
                victim.state.stopped.clone()
            };
            // Cancellation of periodic maintenance must not detach cleanup or
            // retain the map lock while it waits for this same stalled worker.
            let mut maintenance = Box::pin(pool.maintain(None));
            assert!(
                poll_fn(|cx| Poll::Ready(maintenance.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            drop(maintenance);
            let mut attempts = tokio::task::JoinSet::new();
            let (started, mut starts) = mpsc::channel(16);
            for index in 0..16 {
                let pool = pool.clone();
                let config = replacement.clone();
                let global = global.clone();
                let started = started.clone();
                attempts.spawn(async move {
                    let cancel = CancellationToken::new();
                    let mut submission = Box::pin(pool.submit(
                        config.clone(),
                        format!("replacement-{index}"),
                        query(&config.root, Operation::References),
                        Instant::now() + Duration::from_millis(20),
                        cancel.clone(),
                        global.try_acquire_owned().expect("permit"),
                    ));
                    assert!(
                        poll_fn(|cx| Poll::Ready(submission.as_mut().poll(cx)))
                            .await
                            .is_pending(),
                        "retiring process still occupies capacity"
                    );
                    started.send(()).await.expect("started marker");
                    if index % 2 == 0 {
                        cancel.cancel();
                    }
                    (
                        index,
                        submission.await.expect_err("replacement was not admitted"),
                    )
                });
            }
            stage.set("concurrent starts");
            for _ in 0..16 {
                starts.recv().await.expect("concurrent submit started");
            }
            // Include timer-wheel rounding, while staying below the worker's
            // 100ms graceful-shutdown deadline so the process remains stalled.
            tokio::time::advance(Duration::from_millis(25)).await;
            stage.set("concurrent cancellation and timeout");
            while let Some(attempt) = attempts.join_next().await {
                let (index, response) = attempt.expect("attempt task");
                assert_eq!(
                    response.status,
                    if index % 2 == 0 {
                        Status::Cancelled
                    } else {
                        Status::Timeout
                    }
                );
            }
            assert_eq!(pool.slots.lock().await.len(), 1);
            assert!(
                poll_fn(|cx| Poll::Ready(listener.poll_accept(cx)))
                    .await
                    .is_pending(),
                "no replacement process connected before victim exit"
            );
            assert!(PathBuf::from(format!("/proc/{pid}")).exists());
            assert_eq!(global.available_permits(), 20);

            let mut successor = Box::pin(pool.submit(
                replacement,
                "replacement".into(),
                query(root.path(), Operation::References),
                deadline,
                CancellationToken::new(),
                global.clone().try_acquire_owned().expect("permit"),
            ));
            assert!(
                poll_fn(|cx| Poll::Ready(successor.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            control.write_all(b"R").await.expect("release shutdown");
            stage.set("successor admission after retirement");
            let next = successor
                .await
                .expect("waiting replacement admitted after reaping");
            assert!(stopped.is_cancelled());
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "victim reaped"
            );
            let (mut observer, _) = listener.accept().await.expect("replacement control");
            stage.set("replacement PID observation");
            let next_pid = observer.read_u32().await.expect("replacement PID");
            assert_ne!(pid, next_pid);
            observer
                .read_exact(&mut byte)
                .await
                .expect("reaping observation");
            assert_eq!(byte, *b"R", "new process observed victim already reaped");
            stage.set("successor query");
            assert_eq!(next.await.expect("replacement response").status, Status::Ok);
            stage.set("pool shutdown");
            pool.shutdown().await;
            assert!(!PathBuf::from(format!("/proc/{next_pid}")).exists());
        })
        .await;
        tokio::time::resume();
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
