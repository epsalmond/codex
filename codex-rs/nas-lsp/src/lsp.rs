//! Bounded stdio LSP session. See `.codex/design/nas-lsp-broker.md`.

use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;
use std::io;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Notify;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::timeout_at;
use tokio_util::sync::CancellationToken;

const MAX_HEADER: usize = 8192;
const MAX_BODY: usize = 2 * 1024 * 1024;
const STDERR_TAIL: usize = 4096;

/// Inspect before copying, so a missing delimiter never allocates a huge line.
async fn frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut header = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let count = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        if header.len() + count > MAX_HEADER {
            return Err(io::ErrorKind::InvalidData.into());
        }
        header.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&header).map_err(|_| io::ErrorKind::InvalidData)?;
    let mut length = None;
    for line in header.split("\r\n").filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(io::ErrorKind::InvalidData)?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let size = value
                .trim()
                .parse::<usize>()
                .map_err(|_| io::ErrorKind::InvalidData)?;
            if size == 0 || size > MAX_BODY {
                return Err(io::ErrorKind::InvalidData.into());
            }
            length = Some(size);
        }
    }
    let mut body = vec![0; length.ok_or(io::ErrorKind::InvalidData)?];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

async fn send(
    writer: &Arc<AsyncMutex<ChildStdin>>,
    message: Value,
    deadline: Instant,
) -> Result<(), &'static str> {
    let bytes = serde_json::to_vec(&message).map_err(|_| "lsp_protocol")?;
    if bytes.len() > MAX_BODY {
        return Err("lsp_message_too_large");
    }
    timeout_at(deadline, async {
        let mut writer = writer.lock().await;
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
            .await?;
        writer.write_all(&bytes).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| "lsp_timeout")?
    .map_err(|_| "lsp_io")
}

#[derive(Debug)]
pub(crate) enum RequestError {
    Failure(&'static str),
    Timeout,
    Cancelled,
    Server(i64),
    Indexing(&'static str),
    Stale(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadinessPermit {
    pub(crate) epoch: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ServerStatus {
    pub(crate) healthy: bool,
    pub(crate) quiescent: bool,
}

#[derive(Default)]
struct ReadinessState {
    progress: HashSet<String>,
    epoch: u64,
    server_status: Option<ServerStatus>,
}

/// Clean child-only environment for pinned TypeScript/Rust runtimes. The scratch
/// directory lives as long as its child; no host/user configuration is imported.
pub(crate) struct ChildEnvironment {
    scratch: tempfile::TempDir,
    path: std::ffi::OsString,
    rustc: Option<std::path::PathBuf>,
}
impl ChildEnvironment {
    pub(crate) fn new(config: &crate::config::WorkspaceConfig) -> Result<Self, &'static str> {
        let scratch = tempfile::Builder::new()
            .prefix("codex-lsp-child-")
            .tempdir()
            .map_err(|_| "child_environment")?;
        for name in ["cargo", "config", "tmp"] {
            std::fs::create_dir(scratch.path().join(name)).map_err(|_| "child_environment")?;
        }
        let runtime = if config.language == "rust" {
            config.rustc.as_deref().ok_or("rustc_unavailable")?
        } else {
            config.executable.as_path()
        };
        let parent = runtime.parent().ok_or("runtime_unavailable")?;
        let path = std::env::join_paths([
            parent,
            std::path::Path::new("/usr/bin"),
            std::path::Path::new("/bin"),
        ])
        .map_err(|_| "child_environment")?;
        Ok(Self {
            scratch,
            path,
            rustc: config.rustc.clone(),
        })
    }
    pub(crate) fn apply(&self, command: &mut Command) {
        command
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.scratch.path())
            .env("CARGO_HOME", self.scratch.path().join("cargo"))
            .env("XDG_CONFIG_HOME", self.scratch.path().join("config"))
            .env("TMPDIR", self.scratch.path().join("tmp"))
            .env("LANG", "C.UTF-8")
            .env("CARGO_NET_OFFLINE", "true");
        if let Some(rustc) = &self.rustc {
            // RA's global Rayon pool is independent of its numThreads option.
            // Bound both pools so two workers leave room for rustc helpers
            // inside the broker's unchanged 64-task cgroup ceiling.
            command
                .env("RUSTC", rustc)
                .env("RAYON_NUM_THREADS", "2")
                // The pinned compiler treats this value as a report directory.
                // Diagnostic crashes must not create new workspace inputs.
                .env("RUSTC_ICE", self.scratch.path().join("tmp"));
        }
    }
}

pub(crate) struct Session {
    child: Child,
    group: Option<u32>,
    writer: Arc<AsyncMutex<ChildStdin>>,
    responses: mpsc::Receiver<Result<Value, &'static str>>,
    reader: JoinHandle<()>,
    stderr: JoinHandle<()>,
    healthy: Arc<AtomicBool>,
    readiness: Arc<Mutex<ReadinessState>>,
    changed: Arc<Notify>,
    _environment: Option<ChildEnvironment>,
    next_id: u64,
}

impl Session {
    pub fn spawn(
        config: &crate::config::WorkspaceConfig,
        options: Value,
    ) -> Result<Self, &'static str> {
        let mut command = Command::new(&config.executable);
        let environment = if config.language == "python" {
            None
        } else {
            let environment = ChildEnvironment::new(config)?;
            environment.apply(&mut command);
            Some(environment)
        };
        command
            .args(&config.args)
            .current_dir(&config.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn().map_err(|_| "server_unavailable")?;
        let group = child.id();
        let writer = Arc::new(AsyncMutex::new(
            child.stdin.take().ok_or("server_unavailable")?,
        ));
        let stdout = child.stdout.take().ok_or("server_unavailable")?;
        let mut stderr_pipe = child.stderr.take().ok_or("server_unavailable")?;
        let healthy = Arc::new(AtomicBool::new(true));
        let readiness = Arc::new(Mutex::new(ReadinessState::default()));
        let changed = Arc::new(Notify::new());
        let (tx, responses) = mpsc::channel(8);
        let reader_writer = writer.clone();
        let reader_health = healthy.clone();
        let reader_readiness = readiness.clone();
        let reader_changed = changed.clone();
        let language = config.language.clone();
        let reader = tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            loop {
                let message = match frame(&mut stdout).await.and_then(|body| {
                    serde_json::from_slice::<Value>(&body)
                        .map_err(|_| io::ErrorKind::InvalidData.into())
                }) {
                    Ok(message)
                        if message.get("jsonrpc").and_then(Value::as_str) == Some("2.0") =>
                    {
                        message
                    }
                    _ => {
                        reader_health.store(false, Ordering::Release);
                        let _ = tx.send(Err("lsp_protocol")).await;
                        break;
                    }
                };
                if let Some(method) = message.get("method").and_then(Value::as_str) {
                    if let Some(id) = message.get("id") {
                        let reply = if method == "workspace/applyEdit" {
                            json!({"jsonrpc":"2.0", "id":id, "result":{"applied":false}})
                        } else if method == "window/workDoneProgress/create"
                            && message["params"]["token"].to_string().len() <= 256
                            && matches!(
                                message["params"]["token"],
                                Value::String(_) | Value::Number(_)
                            )
                        {
                            json!({"jsonrpc":"2.0", "id":id, "result":null})
                        } else if method == "workspace/configuration"
                            && message["params"]["items"]
                                .as_array()
                                .is_some_and(|items| items.len() <= 64)
                        {
                            let values: Vec<Value> = message["params"]["items"]
                                .as_array()
                                .expect("checked items")
                                .iter()
                                .map(|item| {
                                    let section = item["section"].as_str().unwrap_or("");
                                    if language == "rust" && section == "rust-analyzer" {
                                        options.clone()
                                    } else if language == "rust"
                                        && section.starts_with("rust-analyzer.")
                                    {
                                        section
                                            .trim_start_matches("rust-analyzer.")
                                            .split('.')
                                            .fold(&options, |value, key| &value[key])
                                            .clone()
                                    } else {
                                        Value::Null
                                    }
                                })
                                .collect();
                            json!({"jsonrpc":"2.0", "id":id, "result":values})
                        } else {
                            json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601,"message":"Method not allowed"}})
                        };
                        if send(
                            &reader_writer,
                            reply,
                            Instant::now() + Duration::from_millis(250),
                        )
                        .await
                        .is_err()
                        {
                            reader_health.store(false, Ordering::Release);
                            break;
                        }
                    } else if method == "$/progress" {
                        let params = &message["params"];
                        let token = params
                            .get("token")
                            .map(Value::to_string)
                            .unwrap_or_default();
                        let mut state = reader_readiness.lock().unwrap_or_else(|e| e.into_inner());
                        match params["value"]["kind"].as_str() {
                            Some("begin") if token.len() <= 256 && state.progress.len() < 64 => {
                                state.progress.insert(token);
                                state.epoch = state.epoch.wrapping_add(1);
                            }
                            Some("end") => {
                                state.progress.remove(&token);
                                state.epoch = state.epoch.wrapping_add(1);
                            }
                            Some("begin") => {
                                reader_health.store(false, Ordering::Release);
                                break;
                            }
                            _ => {}
                        }
                        drop(state);
                        reader_changed.notify_waiters();
                    } else if method == "experimental/serverStatus" && language == "rust" {
                        let params = &message["params"];
                        let next = match (params["health"].as_str(), params["quiescent"].as_bool())
                        {
                            (Some(health @ ("ok" | "warning" | "error")), Some(quiescent)) => {
                                ServerStatus {
                                    healthy: health == "ok",
                                    quiescent,
                                }
                            }
                            _ => {
                                reader_health.store(false, Ordering::Release);
                                break;
                            }
                        };
                        let mut state = reader_readiness.lock().unwrap_or_else(|e| e.into_inner());
                        if state.server_status != Some(next) {
                            state.epoch = state.epoch.wrapping_add(1);
                        }
                        state.server_status = Some(next);
                        drop(state);
                        reader_changed.notify_waiters();
                    }
                    // Diagnostics, log messages and every other notification are discarded.
                } else if message.get("id").is_some() {
                    if tx.try_send(Ok(message)).is_err() {
                        reader_health.store(false, Ordering::Release);
                        break;
                    }
                } else {
                    reader_health.store(false, Ordering::Release);
                    break;
                }
            }
            reader_changed.notify_waiters();
        });
        let stderr = tokio::spawn(async move {
            let mut tail = Vec::with_capacity(STDERR_TAIL);
            let mut buffer = [0; 1024];
            while let Ok(count) = stderr_pipe.read(&mut buffer).await {
                if count == 0 {
                    break;
                }
                let remove = (tail.len() + count).saturating_sub(STDERR_TAIL);
                tail.drain(..remove);
                tail.extend_from_slice(&buffer[..count]);
            }
            // Kept bounded for host-side debugging only; never returned to the model.
        });
        Ok(Self {
            child,
            group,
            writer,
            responses,
            reader,
            stderr,
            healthy,
            readiness,
            changed,
            _environment: environment,
            next_id: 1,
        })
    }

    pub fn process_id(&self) -> Option<u32> {
        self.group
    }
    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }
    pub fn invalidate(&self) {
        self.healthy.store(false, Ordering::Release);
    }
    pub fn indexing(&self) -> bool {
        !self
            .readiness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .progress
            .is_empty()
    }
    pub(crate) fn readiness_epoch(&self) -> u64 {
        self.readiness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .epoch
    }
    pub(crate) fn server_status(&self) -> Option<ServerStatus> {
        self.readiness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .server_status
    }
    pub(crate) fn permit_valid(&self, permit: ReadinessPermit, require_status: bool) -> bool {
        let state = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
        self.healthy()
            && state.progress.is_empty()
            && permit.epoch.is_none_or(|epoch| epoch == state.epoch)
            && (!require_status
                || state
                    .server_status
                    .is_some_and(|status| status.healthy && status.quiescent))
    }
    pub(crate) fn navigation_permit(
        &self,
        require_status: bool,
    ) -> Result<ReadinessPermit, RequestError> {
        let permit = ReadinessPermit {
            epoch: Some(self.readiness_epoch()),
        };
        if self.permit_valid(permit, require_status) {
            Ok(permit)
        } else if !self.healthy() {
            Err(RequestError::Failure("lsp_io"))
        } else if require_status && self.server_status().is_some_and(|status| !status.healthy) {
            Err(RequestError::Failure("server_health"))
        } else {
            Err(RequestError::Indexing("indexing"))
        }
    }
    pub(crate) async fn wait_rust_ready(
        &self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<ReadinessPermit, RequestError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.navigation_permit(true) {
                Ok(permit) => return Ok(permit),
                Err(RequestError::Indexing(_)) => {}
                Err(error) => return Err(error),
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(RequestError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => return Err(RequestError::Indexing("server_not_ready")),
                _ = changed => {}
            }
        }
    }

    pub async fn notify(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<(), &'static str> {
        send(
            &self.writer,
            json!({"jsonrpc":"2.0","method":method,"params":params}),
            deadline,
        )
        .await
    }

    /// Cancellation signals only this ID. Keep the reader and lease alive until
    /// its response drains or the absolute operation deadline expires.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Value, RequestError> {
        let id = self.next_id;
        self.next_id += 1;
        if let Err(error) = send(
            &self.writer,
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            deadline,
        )
        .await
        {
            self.invalidate();
            return Err(RequestError::Failure(error));
        }
        let mut cancelled = false;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    let _ = self.notify("$/cancelRequest", json!({"id":id}), deadline.min(Instant::now() + Duration::from_millis(50))).await;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if !cancelled { let _ = self.notify("$/cancelRequest", json!({"id":id}), Instant::now() + Duration::from_millis(10)).await; }
                    self.healthy.store(false, Ordering::Release);
                    return Err(if cancelled { RequestError::Cancelled } else { RequestError::Timeout });
                }
                message = self.responses.recv() => {
                    let message = message.ok_or(RequestError::Failure("lsp_io"))?.map_err(RequestError::Failure)?;
                    if message["id"].as_u64() != Some(id) { continue; }
                    if cancelled { return Err(RequestError::Cancelled); }
                    if let Some(error) = message.get("error") { return Err(RequestError::Server(error["code"].as_i64().unwrap_or(-32603))); }
                    return message.get("result").cloned().ok_or(RequestError::Failure("lsp_protocol"));
                }
            }
        }
    }

    pub async fn shutdown(&mut self) {
        if self.group.is_none() {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let polite = Instant::now() + Duration::from_millis(100);
        if self.healthy() {
            let _ = self
                .request("shutdown", Value::Null, polite, CancellationToken::new())
                .await;
            let _ = self.notify("exit", Value::Null, polite).await;
        }
        if let Some(group) = self.group {
            unsafe {
                libc::kill(-(group as i32), libc::SIGTERM);
            }
        }
        let _ = timeout_at(
            Instant::now() + Duration::from_millis(50),
            self.child.wait(),
        )
        .await;
        // Always kill the group too: a successfully reaped leader may have descendants.
        if let Some(group) = self.group.take() {
            unsafe {
                libc::kill(-(group as i32), libc::SIGKILL);
            }
        }
        let _ = timeout_at(deadline, self.child.wait()).await;
        self.reader.abort();
        self.stderr.abort();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(group) = self.group.take() {
            unsafe {
                libc::kill(-(group as i32), libc::SIGKILL);
            }
        }
        self.reader.abort();
        self.stderr.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_indexing_cycle_invalidates_permit() {
        let root = tempfile::tempdir().expect("root");
        let config = crate::config::WorkspaceConfig {
            id: "epoch".into(), root: root.path().to_owned(), language: "python".into(),
            executable: "/usr/bin/python3".into(),
            args: vec!["-u".into(), "-c".into(), r#"
import json,sys
def send(value):
 data=json.dumps(dict(jsonrpc='2.0',**value)).encode()
 sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(data)).encode()+data);sys.stdout.buffer.flush()
def read():
 headers={}
 while True:
  line=sys.stdin.buffer.readline()
  if not line: sys.exit(0)
  if line==b'\r\n':break
  key,value=line.decode().split(':',1);headers[key.lower()]=value.strip()
 return json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
while True:
 msg=read()
 if msg.get('method')=='exit':sys.exit(0)
 if 'id' not in msg:continue
 if msg.get('method')=='cycle':
  send(dict(id=900,method='window/workDoneProgress/create',params=dict(token='index')))
  reply=read()
  assert reply['id']==900 and 'result' in reply and reply['result'] is None
  send(dict(method='$/progress',params=dict(token='index',value=dict(kind='begin'))))
  send(dict(method='$/progress',params=dict(token='index',value=dict(kind='end'))))
 send(dict(id=msg['id'],result=None))
"#.into()],
            source_extensions: vec!["py".into()], exclude_dirs: vec![],
            readiness: crate::config::Readiness::PyreflyLazyBlocking,
            typescript_tsserver: None, rustc: None,
        };
        let mut session = Session::spawn(&config, Value::Null).expect("spawn");
        let permit = session.navigation_permit(false).expect("initial idle");
        session
            .request(
                "cycle",
                Value::Null,
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
            .expect("cycle");
        assert!(!session.indexing(), "progress ended before response");
        assert!(
            !session.permit_valid(permit, false),
            "the completed cycle must still invalidate the response"
        );
        assert!(
            session.navigation_permit(false).is_ok(),
            "later requests can use the surviving idle session"
        );
        session.shutdown().await;
    }

    #[tokio::test]
    async fn framing_is_bounded_before_allocation() {
        for bad in [
            b"Content-Length: 2097153\r\n\r\n".to_vec(),
            vec![b'x'; MAX_HEADER + 1],
            b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        ] {
            assert!(frame(&mut BufReader::new(bad.as_slice())).await.is_err());
        }
        assert_eq!(
            frame(&mut BufReader::new(
                b"Content-Length: 2\r\n\r\n{}".as_slice()
            ))
            .await
            .unwrap(),
            b"{}"
        );
    }
}
