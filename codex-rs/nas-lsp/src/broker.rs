//! Private persistent navigation service. See `.codex/design/nas-lsp-broker.md`.

use crate::config::BrokerConfig;
use crate::config::RootIdentity;
use crate::pool::Pool;
use crate::resource::ResourceLimits;
use crate::worker::WorkerQuery;
use codex_lsp_protocol::DEFAULT_TIMEOUT_MS;
use codex_lsp_protocol::MAX_REQUEST_BYTES;
use codex_lsp_protocol::MAX_RESPONSE_BYTES;
use codex_lsp_protocol::MAX_RESULTS;
use codex_lsp_protocol::MAX_TIMEOUT_MS;
use codex_lsp_protocol::NavigationRequest;
use codex_lsp_protocol::NavigationResponse;
use codex_lsp_protocol::PROTOCOL_VERSION;
use codex_lsp_protocol::Status;
use codex_lsp_protocol::fit_response;
use codex_lsp_protocol::read_frame;
use codex_lsp_protocol::write_frame;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Broker {
    inner: Arc<Inner>,
}
struct Inner {
    config: BrokerConfig,
    identities: Vec<RootIdentity>,
    pool: Pool,
    queries: Arc<Semaphore>,
}
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BrokerError(pub String);
fn status(s: Status, reason: &str) -> NavigationResponse {
    NavigationResponse::status(s, reason)
}

impl Broker {
    pub fn new(config: BrokerConfig) -> Result<Self, BrokerError> {
        config.validate().map_err(|e| BrokerError(e.to_string()))?;
        let identities = config
            .workspaces
            .iter()
            .map(|w| RootIdentity::read(&w.root))
            .collect::<io::Result<Vec<_>>>()
            .map_err(|e| BrokerError(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(Inner {
                queries: Arc::new(Semaphore::new(config.limits.max_queries)),
                pool: Pool::new(config.limits.clone()),
                config,
                identities,
            }),
        })
    }

    pub async fn query(
        &self,
        request: NavigationRequest,
        cancel: CancellationToken,
    ) -> NavigationResponse {
        let timeout_ms = if request.timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            request.timeout_ms.min(MAX_TIMEOUT_MS)
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let budget = request.max_response_bytes.min(MAX_RESPONSE_BYTES);
        let max_results = request.max_results.min(MAX_RESULTS);
        let mut result = self
            .query_inner(request, deadline, cancel.child_token())
            .await;
        if result.locations.len() > max_results {
            result.locations.truncate(max_results);
            result.truncated = true;
        }
        // A tiny wire budget is represented by a scalar by the transport. The typed
        // API returns its status; the native tool independently enforces host budgets.
        fit_response(result.clone(), budget).unwrap_or(result)
    }

    async fn query_inner(
        &self,
        request: NavigationRequest,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> NavigationResponse {
        if cancel.is_cancelled() {
            return status(Status::Cancelled, "cancelled");
        }
        if request.version != PROTOCOL_VERSION
            || request.position.line == 0
            || request.position.column == 0
        {
            return status(Status::InvalidRequest, "invalid_request");
        }
        let permit = match self.inner.queries.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return status(Status::Busy, "query_limit"),
        };
        if !request.cwd.is_absolute() {
            return status(Status::InvalidRequest, "invalid_cwd");
        }
        let Some((index, workspace)) = self
            .inner
            .config
            .workspaces
            .iter()
            .enumerate()
            .find(|(_, w)| request.cwd.starts_with(&w.root))
        else {
            return status(Status::Unavailable, "unregistered_workspace");
        };
        if RootIdentity::read(&workspace.root).ok().as_ref() != Some(&self.inner.identities[index])
        {
            self.inner
                .pool
                .invalidate(&workspace.key(&self.inner.identities[index]))
                .await;
            return status(Status::Unavailable, "workspace_identity_changed");
        }
        match fs::canonicalize(&request.cwd) {
            Ok(cwd) if cwd == request.cwd && cwd.is_dir() => {}
            _ => return status(Status::InvalidRequest, "invalid_cwd"),
        }
        let path = match workspace.resolve_source(&request.path) {
            Ok(path) => path,
            Err(_) => return status(Status::InvalidRequest, "invalid_source_path"),
        };
        if !fs::canonicalize(&workspace.executable).is_ok_and(|path| path == workspace.executable)
            || !fs::metadata(&workspace.executable)
                .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        {
            return status(Status::Unavailable, "server_unavailable");
        }
        if ResourceLimits::validate(&self.inner.config.limits).is_err() {
            return status(Status::Unavailable, "resource_limit_unavailable");
        }
        let query = WorkerQuery {
            operation: request.operation,
            path,
            position: request.position,
            max_results: request.max_results.min(MAX_RESULTS),
        };
        let submitted = self.inner.pool.submit(
            workspace.clone(),
            workspace.key(&self.inner.identities[index]),
            query,
            deadline,
            cancel.clone(),
            permit,
        );
        tokio::pin!(submitted);
        let receiver = tokio::select! {
            biased;
            _ = cancel.cancelled()=>return status(Status::Cancelled,"cancelled"),
            _ = tokio::time::sleep_until(deadline)=>return status(Status::Timeout,"deadline_exceeded"),
            result = &mut submitted=>match result {Ok(receiver)=>receiver,Err(response)=>return response},
        };
        let response = await_response(receiver, deadline, cancel).await;
        if RootIdentity::read(&workspace.root).ok().as_ref() != Some(&self.inner.identities[index])
        {
            self.inner
                .pool
                .invalidate(&workspace.key(&self.inner.identities[index]))
                .await;
            return status(Status::Unavailable, "workspace_identity_changed");
        }
        response
    }

    pub async fn serve(self, shutdown: CancellationToken) -> io::Result<()> {
        let (listener, _guard) = bind(&self.inner.config.socket_path).await?;
        let connection_slots = Arc::new(Semaphore::new(self.inner.config.limits.max_connections));
        let mut handlers = JoinSet::new();
        let maintenance_broker = self.clone();
        let maintenance_cancel = shutdown.child_token();
        let maintenance_stop = maintenance_cancel.clone();
        let maintenance = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = maintenance_cancel.cancelled()=>break,
                    _ = interval.tick()=> {
                        let resources = ResourceLimits::validate(&maintenance_broker.inner.config.limits).ok();
                        maintenance_broker.inner.pool.maintain(resources.as_ref()).await;
                    }
                }
            }
        });
        let outcome = loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled()=>break Ok(()),
                Some(_) = handlers.join_next(), if !handlers.is_empty()=>{},
                accepted = listener.accept()=> {
                    let (mut stream,_) = match accepted {Ok(accepted)=>accepted,Err(error)=>break Err(error)};
                    if !stream.peer_cred().is_ok_and(|credentials| credentials.uid() == unsafe {libc::geteuid()}) {continue;}
                    let permit = match connection_slots.clone().try_acquire_owned() {
                        Ok(permit)=>permit,
                        Err(_)=> {
                            send_response(&mut stream,status(Status::Busy,"connection_limit"),MAX_RESPONSE_BYTES).await;
                            continue;
                        }
                    };
                    let broker = self.clone();
                    let cancel = shutdown.child_token();
                    handlers.spawn(async move {let _permit=permit; broker.connection(stream,cancel).await;});
                }
            }
        };
        shutdown.cancel();
        maintenance_stop.cancel();
        let finish = async {
            tokio::join!(
                async {
                    let _ = maintenance.await;
                },
                self.inner.pool.shutdown(),
                async { while handlers.join_next().await.is_some() {} }
            );
        };
        if tokio::time::timeout(Duration::from_secs(5), finish)
            .await
            .is_err()
        {
            handlers.abort_all();
        }
        outcome
    }

    async fn connection(&self, mut stream: UnixStream, cancel: CancellationToken) {
        let bytes = tokio::select! {
            _ = cancel.cancelled()=>return,
            frame = tokio::time::timeout(Duration::from_secs(1),read_frame(&mut stream,MAX_REQUEST_BYTES))=> {
                match frame {Ok(Ok(bytes))=>bytes,_=>return}
            }
        };
        let request: NavigationRequest = match serde_json::from_slice(&bytes) {
            Ok(request) => request,
            Err(_) => {
                send_response(
                    &mut stream,
                    status(Status::InvalidRequest, "invalid_json"),
                    MAX_RESPONSE_BYTES,
                )
                .await;
                return;
            }
        };
        let budget = request.max_response_bytes.min(MAX_RESPONSE_BYTES);
        let (mut reader, mut writer) = stream.into_split();
        let request_cancel = cancel.child_token();
        let operation = self.query(request, request_cancel.clone());
        tokio::pin!(operation);
        let mut extra = [0u8; 1];
        let response = tokio::select! {
            response = &mut operation=>response,
            _ = reader.read(&mut extra)=> {request_cancel.cancel();return;}
        };
        send_response(&mut writer, response, budget).await;
    }
}

pub(crate) async fn await_response(
    receiver: tokio::sync::oneshot::Receiver<NavigationResponse>,
    deadline: Instant,
    cancel: CancellationToken,
) -> NavigationResponse {
    tokio::select! {
        biased;
        _ = cancel.cancelled()=>status(Status::Cancelled,"cancelled"),
        _ = tokio::time::sleep_until(deadline)=> {cancel.cancel();status(Status::Timeout,"deadline_exceeded")},
        response = receiver=>response.unwrap_or_else(|_|status(Status::Unavailable,"worker_lost")),
    }
}

async fn send_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response: NavigationResponse,
    budget: usize,
) {
    let bytes = match fit_response(response, budget) {
        Ok(response) => serde_json::to_vec(&response).unwrap_or_default(),
        Err(_) if budget >= 4 => b"null".to_vec(),
        Err(_) if budget >= 1 => b"0".to_vec(),
        Err(_) => return,
    };
    let _ = tokio::time::timeout(
        Duration::from_millis(250),
        write_frame(writer, &bytes, MAX_RESPONSE_BYTES),
    )
    .await;
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
    _lock: File,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            if metadata.dev() == self.device && metadata.ino() == self.inode {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}
async fn bind(path: &std::path::Path) -> io::Result<(UnixListener, SocketGuard)> {
    crate::config::validate_reserved_path(path)?;
    codex_uds::prepare_shared_daemon_socket_directory()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.with_extension("lock"))?;
    let metadata = lock.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe broker lock",
        ));
    }
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe broker socket",
                ));
            }
            match tokio::time::timeout(Duration::from_millis(250), UnixStream::connect(path)).await
            {
                Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(path)?
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "broker socket is live",
                    ));
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let metadata = fs::symlink_metadata(path)?;
    Ok((
        listener,
        SocketGuard {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            _lock: lock,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PoolLimits;
    use crate::config::Readiness;
    use crate::config::WorkspaceConfig;
    use codex_lsp_protocol::Operation;
    use codex_lsp_protocol::Position;
    use tokio::io::AsyncWriteExt;

    fn fixture() -> (tempfile::TempDir, BrokerConfig) {
        let root = tempfile::tempdir().expect("temp root");
        fs::write(root.path().join("source.py"), "value = 1\n").expect("source");
        let directory = codex_uds::shared_daemon_socket_directory().expect("directory");
        let config = BrokerConfig {
            socket_path: directory.join(format!("navigation-test-{}.sock", std::process::id())),
            workspaces: vec![Arc::new(WorkspaceConfig {
                id: "fixture".into(),
                root: root.path().to_owned(),
                language: "python".into(),
                executable: fs::canonicalize("/bin/true").expect("binary"),
                args: ["lsp", "--indexing-mode", "lazy-blocking", "--threads", "1"]
                    .map(String::from)
                    .to_vec(),
                source_extensions: vec!["py".into()],
                exclude_dirs: vec![".git".into()],
                readiness: Readiness::PyreflyLazyBlocking,
            })],
            limits: PoolLimits::default(),
        };
        (root, config)
    }
    fn request(config: &BrokerConfig) -> NavigationRequest {
        NavigationRequest {
            version: 1,
            cwd: config.workspaces[0].root.clone(),
            path: "source.py".into(),
            operation: Operation::Definition,
            position: Position { line: 1, column: 1 },
            max_results: 100,
            max_response_bytes: 16384,
            timeout_ms: 1000,
        }
    }
    #[tokio::test]
    async fn rejects_unknown_root_traversal_and_replaced_root() {
        let (root, config) = fixture();
        let broker = Broker::new(config.clone()).expect("broker");
        let mut r = request(&config);
        r.path = "../source.py".into();
        assert_eq!(
            broker.query(r, CancellationToken::new()).await.status,
            Status::InvalidRequest
        );
        let other = tempfile::tempdir().expect("other");
        let mut r = request(&config);
        r.cwd = other.path().to_owned();
        assert_eq!(
            broker
                .query(r, CancellationToken::new())
                .await
                .reason
                .as_deref(),
            Some("unregistered_workspace")
        );
        let moved = root.path().with_extension("moved");
        fs::rename(root.path(), &moved).expect("move root");
        assert_eq!(
            broker
                .query(request(&config), CancellationToken::new())
                .await
                .reason
                .as_deref(),
            Some("workspace_identity_changed"),
            "a missing registered root invalidates its slot before cwd resolution"
        );
        fs::create_dir(root.path()).expect("replace root");
        assert_eq!(
            broker
                .query(request(&config), CancellationToken::new())
                .await
                .reason
                .as_deref(),
            Some("workspace_identity_changed")
        );
        fs::remove_dir_all(&moved).expect("cleanup moved fixture");
    }
    #[tokio::test]
    async fn absent_or_replaced_registered_executable_fails_closed() {
        let (_root, mut config) = fixture();
        let tools = tempfile::tempdir().expect("tools");
        let mut workspace = (*config.workspaces[0]).clone();
        workspace.executable = tools.path().join("missing-server");
        config.workspaces = vec![Arc::new(workspace)];
        let broker = Broker::new(config.clone()).expect("broker accepts absent pinned server");
        let response = broker
            .query(request(&config), CancellationToken::new())
            .await;
        assert_eq!(response.reason.as_deref(), Some("server_unavailable"));
        std::os::unix::fs::symlink("/bin/true", &config.workspaces[0].executable)
            .expect("replace executable with symlink");
        let response = broker
            .query(request(&config), CancellationToken::new())
            .await;
        assert_eq!(response.reason.as_deref(), Some("server_unavailable"));
    }

    #[tokio::test]
    async fn malformed_json_returns_bounded_status_and_partial_frame_times_out() {
        let (_root, config) = fixture();
        let broker = Broker::new(config).expect("broker");
        let (mut client, server) = UnixStream::pair().expect("pair");
        let service = broker.clone();
        let task = tokio::spawn(async move {
            service.connection(server, CancellationToken::new()).await;
        });
        write_frame(&mut client, b"{", 8192).await.expect("write");
        let response = read_frame(&mut client, 16384).await.expect("response");
        assert_eq!(
            serde_json::from_slice::<NavigationResponse>(&response)
                .expect("json")
                .status,
            Status::InvalidRequest
        );
        task.await.expect("handler");
        let (mut client, server) = UnixStream::pair().expect("pair");
        let task = tokio::spawn(async move {
            broker.connection(server, CancellationToken::new()).await;
        });
        client.write_all(&[0, 0]).await.expect("partial header");
        tokio::time::timeout(Duration::from_millis(1300), task)
            .await
            .expect("fixed read deadline")
            .expect("handler");
    }
    #[tokio::test]
    async fn response_write_deadline_and_tiny_wire_budgets() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let started = Instant::now();
        send_response(&mut writer, status(Status::Busy, "queue_full"), 16384).await;
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_millis(600));
        for (budget, expected) in [
            (4, b"null".as_slice()),
            (1, b"0".as_slice()),
            (0, b"".as_slice()),
        ] {
            let mut output = Vec::new();
            send_response(
                &mut output,
                status(Status::Unavailable, "unavailable"),
                budget,
            )
            .await;
            if budget == 0 {
                assert!(output.is_empty());
            } else {
                assert_eq!(
                    read_frame(&mut output.as_slice(), 16384)
                        .await
                        .expect("scalar"),
                    expected
                );
            }
        }
    }
    #[tokio::test]
    async fn socket_lock_preserves_live_listener_and_cleans_only_owned_inode() {
        let (_root, config) = fixture();
        let (listener, guard) = bind(&config.socket_path).await.expect("bind");
        assert!(bind(&config.socket_path).await.is_err());
        assert_eq!(
            fs::metadata(&config.socket_path).expect("socket").mode() & 0o777,
            0o600
        );
        drop(listener);
        drop(guard);
        assert!(!config.socket_path.exists());
    }
}
