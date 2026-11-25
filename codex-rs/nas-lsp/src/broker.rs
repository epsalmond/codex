//! Private persistent navigation service. See `.codex/design/nas-lsp-broker.md`.

use crate::config::BrokerConfig;
use crate::config::RootIdentity;
use crate::index;
use crate::linked_checkout;
use crate::linked_checkout::CheckoutAnchor;
use crate::linked_checkout::CheckoutError;
use crate::metrics::Metrics;
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
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Once;
use std::sync::OnceLock;
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
    metrics: Option<Arc<Metrics>>,
}
struct Inner {
    config: BrokerConfig,
    identities: Vec<RootIdentity>,
    index_identities: Vec<RootIdentity>,
    checkout_anchors: Vec<Option<IndexCheckoutAnchors>>,
    pool: Pool,
    queries: Arc<Semaphore>,
}
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BrokerError(pub String);
fn status(s: Status, reason: &str) -> NavigationResponse {
    NavigationResponse::status(s, reason)
}

type WorkspaceLabel<'a> = (&'a str, &'a str);

struct IndexCheckoutAnchors {
    primary: CheckoutAnchor,
    publisher: Option<CheckoutAnchor>,
    publisher_warning: Once,
}

impl IndexCheckoutAnchors {
    fn new(workspace: &index::IndexConfig) -> io::Result<Option<Self>> {
        let Some(prepared) = &workspace.prepared else {
            return Ok(None);
        };
        let primary = CheckoutAnchor::new(&workspace.checkout)?;
        let publisher = if workspace.database.is_some()
            && prepared.publisher_checkout != workspace.checkout
        {
            match CheckoutAnchor::new(&prepared.publisher_checkout) {
                Ok(anchor) => Some(anchor),
                Err(error) => {
                    eprintln!(
                        "semantic navigation publisher route for workspace {:?} was skipped ({:?}). Restart the broker once an owned canonical publisher checkout is available",
                        workspace.id,
                        error.kind(),
                    );
                    None
                }
            }
        } else {
            None
        };
        Ok(Some(Self {
            primary,
            publisher,
            publisher_warning: Once::new(),
        }))
    }

    fn resolve(&self, workspace: &str, checkout: &Path) -> Option<(&CheckoutAnchor, RootIdentity)> {
        if let Ok(identity) = self.primary.resolve(checkout) {
            return Some((&self.primary, identity));
        }
        let publisher = self.publisher.as_ref()?;
        match publisher.resolve(checkout) {
            Ok(identity) => Some((publisher, identity)),
            Err(CheckoutError::InvalidLink) => None,
            Err(CheckoutError::AnchorChanged) => {
                self.publisher_warning.call_once(|| {
                    eprintln!(
                        "semantic navigation publisher identity for workspace {workspace:?} is unavailable or changed. Verify the publisher checkout and restart the broker"
                    );
                });
                None
            }
        }
    }
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
        let index_identities = config
            .indexed_workspaces
            .iter()
            .map(|workspace| RootIdentity::read(&workspace.checkout))
            .collect::<io::Result<Vec<_>>>()
            .map_err(|error| BrokerError(error.to_string()))?;
        let checkout_anchors = config
            .indexed_workspaces
            .iter()
            .map(|workspace| {
                IndexCheckoutAnchors::new(workspace).map_err(|error| {
                    BrokerError(format!(
                        "prepared workspace {} needs canonical primary Git checkouts owned by the service: {error}",
                        workspace.id,
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            inner: Arc::new(Inner {
                queries: Arc::new(Semaphore::new(config.limits.max_queries)),
                pool: Pool::new(config.limits.clone()),
                config,
                identities,
                index_identities,
                checkout_anchors,
            }),
            metrics: None,
        })
    }

    pub fn with_metrics(
        mut self,
        file: PathBuf,
        synthetic_workspace_ids: Vec<String>,
    ) -> io::Result<Self> {
        if !file.is_absolute() || file.extension() != Some(std::ffi::OsStr::new("prom")) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "metrics file must be an absolute .prom path",
            ));
        }
        if synthetic_workspace_ids.iter().any(|id| {
            !self
                .inner
                .config
                .workspaces
                .iter()
                .any(|workspace| workspace.id == *id)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "synthetic workspace must be registered",
            ));
        }
        self.metrics = Some(Arc::new(Metrics::new(
            file,
            synthetic_workspace_ids.into_iter().collect(),
        )));
        Ok(self)
    }

    async fn publish_metrics(self, metrics: Arc<Metrics>, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reported_failure = false;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let workers = self.inner.pool.active_workers().await;
                    let valid_limits = ResourceLimits::validate(&self.inner.config.limits).is_ok();
                    let metrics = metrics.clone();
                    let published = tokio::task::spawn_blocking(move || {
                        metrics.publish(workers, valid_limits)
                    }).await.is_ok_and(|result| result.is_ok());
                    if !published && !reported_failure {
                        eprintln!("semantic navigation metrics snapshot could not be published");
                    }
                    reported_failure = !published;
                }
            }
        }
    }

    pub async fn query(
        &self,
        request: NavigationRequest,
        cancel: CancellationToken,
    ) -> NavigationResponse {
        self.query_with_workspace(request, cancel, None).await
    }

    async fn query_with_workspace<'a>(
        &'a self,
        request: NavigationRequest,
        cancel: CancellationToken,
        workspace_label: Option<&OnceLock<WorkspaceLabel<'a>>>,
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
            .query_inner(request, deadline, cancel.child_token(), workspace_label)
            .await;
        if result.locations.len() > max_results {
            result.locations.truncate(max_results);
            result.truncated = true;
        }
        // A tiny wire budget is represented by a scalar by the transport. The typed
        // API returns its status; the native tool independently enforces host budgets.
        fit_response(result.clone(), budget).unwrap_or(result)
    }

    fn indexed_workspace<'a>(&'a self, request: &'a NavigationRequest) -> Option<IndexRoute<'a>> {
        let nearest_checkout = self
            .inner
            .checkout_anchors
            .iter()
            .any(Option::is_some)
            .then(|| linked_checkout::nearest_checkout(&request.cwd))
            .flatten();
        let live_workspace = self
            .inner
            .config
            .workspaces
            .iter()
            .find(|workspace| request.cwd.starts_with(&workspace.root));
        self.inner
            .config
            .indexed_workspaces
            .iter()
            .enumerate()
            .filter_map(|(position, workspace)| {
                let (checkout, identity, anchor) = if let Some(anchors) =
                    &self.inner.checkout_anchors[position]
                {
                    let checkout = nearest_checkout?;
                    if checkout != workspace.checkout
                        && live_workspace.is_some_and(|live| {
                            live.root.starts_with(checkout) && live.language == workspace.language
                        })
                    {
                        return None;
                    }
                    let (anchor, identity) = anchors.resolve(&workspace.id, checkout)?;
                    (checkout, identity, Some(anchor))
                } else {
                    (
                        workspace.checkout.as_path(),
                        self.inner.index_identities[position].clone(),
                        None,
                    )
                };
                let cwd = request.cwd.strip_prefix(checkout).ok()?;
                let from_checkout = workspace.matches_path(&request.path);
                let from_cwd = (workspace.valid_source_path(&request.path)
                    && !cwd.as_os_str().is_empty())
                .then(|| cwd.join(&request.path));
                let from_cwd = from_cwd
                    .as_deref()
                    .and_then(Path::to_str)
                    .filter(|path| workspace.matches_path(path));
                let path = if from_checkout
                    && (from_cwd.is_none()
                        || index::IndexConfig::has_source(checkout, &request.path)
                        || from_cwd
                            .is_some_and(|path| !index::IndexConfig::has_source(checkout, path)))
                {
                    &request.path
                } else {
                    from_cwd?
                };
                Some(IndexRoute {
                    position,
                    workspace,
                    checkout,
                    identity,
                    anchor,
                    path: path.to_owned(),
                })
            })
            .max_by_key(|route| {
                (
                    route.checkout.components().count(),
                    Path::new(&route.path).starts_with(&route.workspace.project),
                    request
                        .cwd
                        .starts_with(route.checkout.join(&route.workspace.project)),
                    route.workspace.project.components().count(),
                    route.anchor.is_none(),
                    route.checkout == route.workspace.checkout,
                )
            })
    }

    async fn query_inner<'a>(
        &'a self,
        request: NavigationRequest,
        deadline: Instant,
        cancel: CancellationToken,
        workspace_label: Option<&OnceLock<WorkspaceLabel<'a>>>,
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
        if let Some(route) = self.indexed_workspace(&request) {
            if let Some(label) = workspace_label {
                let workspace = &self.inner.config.indexed_workspaces[route.position];
                let _ = label.set((&workspace.id, &workspace.language));
            }
            if !fs::canonicalize(&request.cwd).is_ok_and(|cwd| cwd == request.cwd && cwd.is_dir()) {
                return status(Status::InvalidRequest, "invalid_cwd");
            }
            if crosses_checkout_boundary(route.checkout, &request.cwd) {
                return status(Status::Unavailable, "unregistered_workspace");
            }
            if ResourceLimits::validate(&self.inner.config.limits).is_err() {
                return status(Status::Unavailable, "resource_limit_unavailable");
            }
            let workspace = route.index_config();
            let mut request = request.clone();
            request.path = route.path;
            let response = index::query(
                &workspace,
                &route.identity,
                &request,
                deadline,
                cancel,
                permit,
            )
            .await;
            if response.status == Status::Ok
                && route.anchor.is_some_and(|anchor| {
                    linked_checkout::nearest_checkout(&request.cwd) != Some(route.checkout)
                        || anchor.resolve(route.checkout).ok().as_ref() != Some(&route.identity)
                })
            {
                return status(Status::Stale, "index_checkout_changed");
            }
            return response;
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
        if let Some(label) = workspace_label {
            let _ = label.set((&workspace.id, &workspace.language));
        }
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
        let metrics_task = self.metrics.as_ref().map(|metrics| {
            tokio::spawn(
                self.clone()
                    .publish_metrics(metrics.clone(), shutdown.child_token()),
            )
        });
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
                    if let Some(metrics_task) = metrics_task {
                        let _ = metrics_task.await;
                    }
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
        let workspace_label = OnceLock::new();
        let measurement = self.metrics.as_ref().map(|metrics| {
            let operation = request.operation;
            let start = Instant::now();
            let live_workspace = self.inner.config.workspaces.iter().find_map(|workspace| {
                request
                    .cwd
                    .starts_with(&workspace.root)
                    .then_some((workspace.id.as_str(), workspace.language.as_str()))
            });
            let workspace_label = &workspace_label;
            move |status, reason: Option<&str>| {
                let workspace = workspace_label.get().copied().or(live_workspace);
                let key = metrics.lookup_key(workspace, operation);
                metrics.record(key, status, reason, start.elapsed());
            }
        });
        let (mut reader, mut writer) = stream.into_split();
        let request_cancel = cancel.child_token();
        let operation = self.query_with_workspace(
            request,
            request_cancel.clone(),
            measurement.as_ref().map(|_| &workspace_label),
        );
        tokio::pin!(operation);
        let mut extra = [0u8; 1];
        let response = tokio::select! {
            response = &mut operation=>response,
            _ = reader.read(&mut extra) => {
                request_cancel.cancel();
                if let Some(record) = measurement {
                    record(Status::Cancelled, Some("client_disconnected"));
                }
                return;
            }
        };
        if let Some(record) = measurement {
            record(response.status, response.reason.as_deref());
        }
        send_response(&mut writer, response, budget).await;
    }
}

struct IndexRoute<'a> {
    position: usize,
    workspace: &'a index::IndexConfig,
    checkout: &'a Path,
    identity: RootIdentity,
    anchor: Option<&'a CheckoutAnchor>,
    path: String,
}

impl IndexRoute<'_> {
    fn index_config(&self) -> index::IndexConfig {
        let mut workspace = self.workspace.clone();
        if self.checkout != workspace.checkout {
            workspace.checkout = self.checkout.to_owned();
            workspace.database = None;
        }
        workspace
    }
}

fn crosses_checkout_boundary(checkout: &Path, cwd: &Path) -> bool {
    cwd.ancestors()
        .take_while(|ancestor| *ancestor != checkout)
        .any(
            |ancestor| match fs::symlink_metadata(ancestor.join(".git")) {
                Ok(_) => true,
                Err(error) => error.kind() != io::ErrorKind::NotFound,
            },
        )
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
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
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
    use std::ffi::OsStr;
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
                typescript_tsserver: None,
                rustc: None,
            })],
            indexed_workspaces: Vec::new(),
            limits: PoolLimits::default(),
        };
        (root, config)
    }

    #[test]
    fn indexed_checkout_routing_stops_at_nested_git_worktree() {
        let checkout = tempfile::tempdir().expect("checkout");
        let linked = checkout.path().join(".worktrees/feature");
        let app = linked.join("apps/project");
        fs::create_dir_all(&app).expect("linked app");
        fs::write(
            linked.join(".git"),
            "gitdir: /operator/private/worktrees/feature\n",
        )
        .expect("worktree marker");
        assert!(crosses_checkout_boundary(checkout.path(), &app));
        assert!(!crosses_checkout_boundary(&linked, &app));
    }

    #[test]
    fn managed_index_accepts_paths_from_checkout_and_app() {
        let (root, mut config) = fixture();
        let index_dir = tempfile::tempdir().expect("private index directory");
        fs::set_permissions(index_dir.path(), fs::Permissions::from_mode(0o700))
            .expect("private directory permissions");
        let project = PathBuf::from("apps/project");
        let app = root.path().join(&project);
        fs::create_dir(root.path().join(".git")).expect("checkout marker");
        fs::create_dir_all(app.join("src")).expect("app source directory");
        fs::write(app.join("src/source.py"), "value = 1\n").expect("app source");
        config.indexed_workspaces.push(index::IndexConfig {
            id: "indexed_app".into(),
            checkout: root.path().to_owned(),
            project: project.clone(),
            language: "python".into(),
            database: Some(index_dir.path().join("app.sqlite3")),
            prepared: None,
            inputs: vec![project],
            generated_dirs: Vec::new(),
        });
        let mut request = request(&config);
        let broker = Broker::new(config).expect("broker");
        request.path = "apps/project/src/source.py".into();
        assert_eq!(
            broker.indexed_workspace(&request).map(|route| route.path),
            Some(request.path.clone())
        );
        request.cwd = app;
        request.path = "src/source.py".into();
        assert_eq!(
            broker.indexed_workspace(&request).map(|route| route.path),
            Some("apps/project/src/source.py".into())
        );
        request.path = "../project/src/source.py".into();
        assert!(broker.indexed_workspace(&request).is_none());
        request.path = "src/removed.py".into();
        assert_eq!(
            broker.indexed_workspace(&request).map(|route| route.path),
            Some("apps/project/src/removed.py".into())
        );
    }

    #[test]
    fn prepared_linked_checkout_preserves_explicit_registrations() {
        let (_ordinary, mut config) = fixture();
        let temp = tempfile::tempdir().expect("Git fixture");
        let (repository, linked) = linked_checkout::tests::make_linked_checkout(temp.path(), true);
        let catalog = temp.path().join("catalog");
        fs::create_dir(&catalog).expect("catalog");
        fs::set_permissions(&catalog, fs::Permissions::from_mode(0o700)).expect("catalog mode");
        let prepared = index::IndexConfig {
            id: "prepared".into(),
            checkout: repository.clone(),
            project: PathBuf::new(),
            language: "python".into(),
            database: None,
            prepared: Some(index::PreparedIndex {
                catalog,
                publisher_checkout: temp.path().join("publisher"),
            }),
            inputs: vec![PathBuf::new()],
            generated_dirs: Vec::new(),
        };
        config.indexed_workspaces.push(prepared.clone());
        let mut request = request(&config);
        let broker = Broker::new(config.clone()).expect("broker with prepared workspace");
        request.cwd = linked.clone();
        assert_eq!(
            broker
                .indexed_workspace(&request)
                .map(|route| route.checkout),
            Some(linked.as_path())
        );

        Arc::make_mut(&mut config.workspaces[0]).root = repository.clone();
        let broker = Broker::new(config.clone()).expect("broker with language server at main root");
        assert_eq!(
            broker
                .indexed_workspace(&request)
                .map(|route| route.checkout),
            Some(linked.as_path()),
        );

        Arc::make_mut(&mut config.workspaces[0]).root = linked.clone();
        let mut typescript = config.clone();
        typescript.indexed_workspaces[0].language = "typescript".into();
        request.path = "source.ts".into();
        let broker =
            Broker::new(typescript).expect("broker with live Python and indexed TypeScript");
        assert_eq!(
            broker
                .indexed_workspace(&request)
                .map(|route| route.checkout),
            Some(linked.as_path()),
        );

        request.path = "source.py".into();
        let broker = Broker::new(config.clone()).expect("broker with explicit Python server");
        assert!(broker.indexed_workspace(&request).is_none());
        request.cwd = repository;
        assert!(broker.indexed_workspace(&request).is_some());

        config.workspaces.clear();
        let explicit_index_dir = temp.path().join("explicit-index");
        fs::create_dir(&explicit_index_dir).expect("explicit index directory");
        fs::set_permissions(&explicit_index_dir, fs::Permissions::from_mode(0o700))
            .expect("explicit index directory mode");
        config.indexed_workspaces.insert(
            0,
            index::IndexConfig {
                id: "explicit".into(),
                checkout: linked.clone(),
                database: Some(explicit_index_dir.join("explicit.sqlite3")),
                prepared: None,
                ..prepared
            },
        );
        request.cwd = linked;
        let broker = Broker::new(config).expect("broker with explicit database");
        assert_eq!(
            broker
                .indexed_workspace(&request)
                .map(|route| route.workspace.id.as_str()),
            Some("explicit"),
        );
    }

    #[tokio::test]
    async fn dual_source_routes_only_the_two_enrolled_git_repositories() {
        let (_ordinary, mut config) = fixture();
        let original = tempfile::tempdir().expect("original");
        let publisher = tempfile::tempdir().expect("publisher");
        let unrelated = tempfile::tempdir().expect("unrelated");
        let (original_root, original_linked) =
            linked_checkout::tests::make_linked_checkout(original.path(), true);
        let (publisher_root, publisher_linked) =
            linked_checkout::tests::make_linked_checkout(publisher.path(), false);
        let (unrelated_root, unrelated_linked) =
            linked_checkout::tests::make_linked_checkout(unrelated.path(), false);
        let catalog = original.path().join("catalog");
        fs::create_dir(&catalog).expect("catalog");
        fs::set_permissions(&catalog, fs::Permissions::from_mode(0o700)).expect("catalog mode");
        let database = catalog.join("local.sqlite3");
        config.indexed_workspaces.push(index::IndexConfig {
            id: "dual".into(),
            checkout: original_root.clone(),
            project: PathBuf::new(),
            language: "python".into(),
            database: Some(database.clone()),
            prepared: Some(index::PreparedIndex {
                catalog,
                publisher_checkout: publisher_root.clone(),
            }),
            inputs: vec![PathBuf::new()],
            generated_dirs: Vec::new(),
        });
        let mut request = request(&config);
        request.path = "apps/project/importer.py".into();
        request.position.column = 20;
        for checkout in [
            &original_root,
            &original_linked,
            &publisher_root,
            &publisher_linked,
        ] {
            let project = checkout.join("apps/project");
            fs::create_dir_all(&project).expect("create project");
            fs::write(project.join("importer.py"), "from origin import value\n")
                .expect("write importer");
            fs::write(project.join("origin.py"), "value = 1\n").expect("write definition");
        }
        let workspace = &config.indexed_workspaces[0];
        let deadline = Instant::now() + Duration::from_secs(10);
        let manifest = workspace.manifest(deadline).expect("manifest");
        index::tests::build_database(&database, &workspace.id, &original_root, &manifest).await;
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600))
            .expect("private fallback");
        let different_manifest = {
            let source = original_root.join("apps/project/origin.py");
            fs::write(&source, "value = 7\n").expect("change source");
            let different = workspace.manifest(deadline).expect("different manifest");
            fs::write(&source, "value = 1\n").expect("restore source");
            different
        };
        let broker = Broker::new(config.clone()).expect("dual-source broker");
        // Advancing main in place retains the identities the broker enrolled.
        let git = |checkout: &Path, args: &[&str]| {
            let args = args.iter().map(OsStr::new).collect::<Vec<_>>();
            linked_checkout::tests::git(checkout, &args);
        };
        git(&publisher_linked, &["checkout", "-b", "advance"]);
        git(
            &publisher_linked,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "publish next revision",
            ],
        );
        git(&publisher_root, &["merge", "--ff-only", "advance"]);
        for (published, rebound_status) in [
            (None, Status::Indexing),
            (Some(&different_manifest), Status::Stale),
            (Some(&manifest), Status::Ok),
        ] {
            if let Some(published) = published {
                index::tests::publish_fixture(
                    workspace.prepared.as_ref().expect("prepared catalog"),
                    &workspace.id,
                    &"a".repeat(40),
                    published,
                )
                .await;
            }
            for checkout in [
                &original_root,
                &original_linked,
                &publisher_root,
                &publisher_linked,
            ] {
                request.cwd = checkout.to_path_buf();
                let route = broker
                    .indexed_workspace(&request)
                    .expect("registered route");
                assert_eq!(route.checkout, checkout);
                assert_eq!(route.workspace.id, "dual");
                let routed = route.index_config();
                assert_eq!(
                    routed.database.as_ref(),
                    (checkout == &original_root).then_some(&database)
                );
                let response = query_index_route(&broker, &route, &request, deadline).await;
                let expected = if checkout == &original_root {
                    Status::Ok
                } else {
                    rebound_status
                };
                assert_eq!(response.status, expected);
                if expected == Status::Ok {
                    assert_eq!(response.locations[0].path, "apps/project/origin.py");
                }
            }
        }
        for checkout in [unrelated_root, unrelated_linked] {
            request.cwd = checkout;
            assert!(broker.indexed_workspace(&request).is_none());
        }

        let mut with_live = config.clone();
        Arc::make_mut(&mut with_live.workspaces[0]).root = publisher_linked.clone();
        let live_broker =
            Broker::new(with_live).expect("broker with explicit publisher-linked live server");
        request.cwd = publisher_linked;
        assert!(live_broker.indexed_workspace(&request).is_none());

        config.indexed_workspaces[0].database = None;
        let existing = Broker::new(config).expect("prepared-only broker");
        request.cwd = publisher_root;
        assert!(existing.indexed_workspace(&request).is_none());
    }

    #[tokio::test]
    async fn unavailable_or_replaced_publisher_only_affects_its_optional_routes() {
        let (_ordinary, mut config) = fixture();
        Arc::make_mut(&mut config.workspaces[0]).executable =
            config.workspaces[0].root.join("missing-server");
        let original = tempfile::tempdir().expect("original");
        let publisher = tempfile::tempdir().expect("publisher");
        let (original_root, original_linked) =
            linked_checkout::tests::make_linked_checkout(original.path(), true);
        let publisher_root = publisher.path().join("repository");
        let catalog = original.path().join("catalog");
        fs::create_dir(&catalog).expect("catalog");
        fs::set_permissions(&catalog, fs::Permissions::from_mode(0o700)).expect("catalog mode");
        let database = catalog.join("local.sqlite3");
        config.indexed_workspaces.push(index::IndexConfig {
            id: "dual".into(),
            checkout: original_root.clone(),
            project: PathBuf::new(),
            language: "python".into(),
            database: Some(database.clone()),
            prepared: Some(index::PreparedIndex {
                catalog,
                publisher_checkout: publisher_root.clone(),
            }),
            inputs: vec![PathBuf::new()],
            generated_dirs: Vec::new(),
        });
        let source = original_root.join("apps/project");
        fs::create_dir_all(&source).expect("source directory");
        fs::write(source.join("importer.py"), "from origin import value\n").expect("importer");
        fs::write(source.join("origin.py"), "value = 1\n").expect("definition");
        let workspace = &config.indexed_workspaces[0];
        let deadline = Instant::now() + Duration::from_secs(10);
        let manifest = workspace.manifest(deadline).expect("manifest");
        index::tests::build_database(&database, &workspace.id, &original_root, &manifest).await;
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600))
            .expect("private fallback");

        let missing = Broker::new(config.clone()).expect("broker before publisher bootstrap");
        let (created, publisher_linked) =
            linked_checkout::tests::make_linked_checkout(publisher.path(), false);
        assert_eq!(created, publisher_root);
        let mut indexed = request(&config);
        indexed.path = "apps/project/importer.py".into();
        indexed.position.column = 20;
        let live = request(&config);
        let enrolled = Broker::new(config.clone()).expect("broker after publisher bootstrap");
        for checkout in [&publisher_root, &publisher_linked] {
            indexed.cwd = checkout.clone();
            assert!(missing.indexed_workspace(&indexed).is_none());
            assert!(enrolled.indexed_workspace(&indexed).is_some());
        }

        fs::set_permissions(
            publisher_root.join(".git"),
            fs::Permissions::from_mode(0o777),
        )
        .expect("set unsafe metadata permissions");
        let unsafe_git = Broker::new(config.clone()).expect("broker with unsafe publisher");
        fs::set_permissions(
            publisher_root.join(".git"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("restore safe metadata permissions");

        let alias = original.path().join("publisher-alias");
        std::os::unix::fs::symlink(publisher.path(), &alias).expect("publisher alias");
        let mut noncanonical = config.clone();
        noncanonical.indexed_workspaces[0]
            .prepared
            .as_mut()
            .expect("prepared")
            .publisher_checkout = alias.join("repository");
        let noncanonical = Broker::new(noncanonical).expect("broker with noncanonical publisher");
        for broker in [&unsafe_git, &noncanonical] {
            assert!(
                broker.inner.checkout_anchors[0]
                    .as_ref()
                    .expect("primary checkout anchor")
                    .publisher
                    .is_none()
            );
        }

        fs::rename(&publisher_root, publisher.path().join("old-repository"))
            .expect("move publisher");
        fs::rename(&publisher_linked, publisher.path().join("old-worktree"))
            .expect("move worktree");
        linked_checkout::tests::make_linked_checkout(publisher.path(), false);
        for broker in [&missing, &unsafe_git, &noncanonical, &enrolled] {
            for checkout in [&publisher_root, &publisher_linked] {
                indexed.cwd = checkout.clone();
                assert!(broker.indexed_workspace(&indexed).is_none());
            }
            indexed.cwd = original_root.clone();
            let original = broker
                .indexed_workspace(&indexed)
                .expect("primary checkout route");
            assert_eq!(
                query_index_route(broker, &original, &indexed, deadline)
                    .await
                    .status,
                Status::Ok
            );
            indexed.cwd = original_linked.clone();
            assert!(broker.indexed_workspace(&indexed).is_some());
            let response = broker.query(live.clone(), CancellationToken::new()).await;
            assert_eq!(response.reason.as_deref(), Some("server_unavailable"));
        }
        assert!(
            enrolled.inner.checkout_anchors[0]
                .as_ref()
                .expect("primary checkout anchor")
                .publisher_warning
                .is_completed()
        );
        let reloaded = Broker::new(config).expect("broker reloaded after publisher replacement");
        for checkout in [publisher_root, publisher_linked] {
            indexed.cwd = checkout;
            assert!(reloaded.indexed_workspace(&indexed).is_some());
        }
    }

    async fn query_index_route(
        broker: &Broker,
        route: &IndexRoute<'_>,
        request: &NavigationRequest,
        deadline: Instant,
    ) -> NavigationResponse {
        let mut request = request.clone();
        request.path.clone_from(&route.path);
        index::query(
            &route.index_config(),
            &route.identity,
            &request,
            deadline,
            CancellationToken::new(),
            broker
                .inner
                .queries
                .clone()
                .acquire_owned()
                .await
                .expect("permit"),
        )
        .await
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
    async fn decoded_requests_are_counted_by_operator_registered_traffic() {
        let (root, mut config) = fixture();
        let operator_root = tempfile::tempdir().expect("operator root");
        let mut operator = (*config.workspaces[0]).clone();
        operator.id = "operator-private-id".to_owned();
        operator.root = operator_root.path().to_owned();
        config.workspaces.push(Arc::new(operator));
        let metrics_path = root.path().join("navigation.prom");
        let broker = Broker::new(config.clone())
            .expect("broker")
            .with_metrics(metrics_path.clone(), vec!["fixture".to_owned()])
            .expect("metrics");

        for (cwd, operation) in [
            (root.path(), Operation::Definition),
            (operator_root.path(), Operation::References),
            (std::path::Path::new("/"), Operation::References),
        ] {
            let (mut client, server) = UnixStream::pair().expect("pair");
            let service = broker.clone();
            let task = tokio::spawn(async move {
                service.connection(server, CancellationToken::new()).await;
            });
            let mut request = request(&config);
            request.cwd = cwd.to_owned();
            request.operation = operation;
            request.path = "private-absent-file.py".to_owned();
            write_frame(
                &mut client,
                &serde_json::to_vec(&request).expect("request JSON"),
                MAX_REQUEST_BYTES,
            )
            .await
            .expect("write");
            read_frame(&mut client, MAX_RESPONSE_BYTES)
                .await
                .expect("response");
            task.await.expect("handler");
        }
        broker
            .metrics
            .as_ref()
            .expect("metrics")
            .publish(0, false)
            .expect("snapshot");
        let snapshot = fs::read_to_string(metrics_path).expect("metrics snapshot");
        for labels in [
            r#"traffic="synthetic",language="python",operation="definition",status="invalid_request",reason="invalid_source_path""#,
            r#"traffic="operator",language="python",operation="references",status="invalid_request",reason="invalid_source_path""#,
            r#"traffic="unknown",language="unknown",operation="references",status="unavailable",reason="unregistered_workspace""#,
        ] {
            assert!(
                snapshot.contains(&format!(
                    "codex_semantic_navigation_lookups_total{{{labels}}} 1"
                )),
                "{snapshot}"
            );
        }
        assert!(!snapshot.contains("operator-private-id"));
        assert!(!snapshot.contains("private-absent-file.py"));
        assert!(!snapshot.contains("lookup_duration_seconds_count"));
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
