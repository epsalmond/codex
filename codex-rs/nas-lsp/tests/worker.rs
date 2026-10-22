//! Worker-only real and adversarial tests; see `.codex/design/nas-lsp-broker.md`.

#![cfg(target_os = "linux")]

use codex_lsp_protocol::{NavigationResponse, Operation, Position, Status};
use codex_nas_lsp::config::{Readiness, WorkspaceConfig};
use codex_nas_lsp::worker::{Worker, WorkerQuery};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn config(root: &Path, executable: PathBuf, args: Vec<String>) -> Arc<WorkspaceConfig> {
    Arc::new(WorkspaceConfig {
        id: "worker-test".into(),
        root: root.to_path_buf(),
        language: "python".into(),
        executable,
        args,
        source_extensions: vec!["py".into()],
        exclude_dirs: vec![],
        readiness: Readiness::PyreflyLazyBlocking,
    })
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

async fn query(
    worker: &mut Worker,
    root: &Path,
    operation: Operation,
    line: u32,
    column: u32,
) -> NavigationResponse {
    worker
        .query(
            WorkerQuery {
                operation,
                path: root.join("main.py"),
                position: Position { line, column },
                max_results: 100,
            },
            deadline(),
            CancellationToken::new(),
        )
        .await
}

const FAKE_SERVER: &str = r#"
import json,sys,time,socket
mode=sys.argv[1]
def send(value):
    data=json.dumps(dict(jsonrpc='2.0',**value)).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(data)).encode()+data);sys.stdout.buffer.flush()
waiting=None
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
        send(dict(id=msg['id'],result=dict(capabilities=dict(positionEncoding='utf-8' if mode=='encoding' else 'utf-16',definitionProvider=True,referencesProvider=True,textDocumentSync=1))))
    elif method in ('textDocument/definition','textDocument/references'):
        if mode=='cancel' and waiting is None:
            waiting=msg['id']
            if len(sys.argv)>2:
                signal=socket.socket(socket.AF_UNIX);signal.connect(sys.argv[2]);signal.close()
            continue
        if mode=='timeout': continue
        if mode=='oversized':
            sys.stdout.buffer.write(b'Content-Length: 2097153\r\n\r\n');sys.stdout.buffer.flush();continue
        if mode=='requests':
            send(dict(id=10000,method='workspace/applyEdit',params=dict(edit={})))
            send(dict(id=10001,method='workspace/executeCommand',params=dict(command='bad')))
        if mode=='stale':
            with open('main.py','a') as file: file.write('# saved during request\n')
        uri='file:///etc/passwd' if mode=='external' else msg['params']['textDocument']['uri']
        location=dict(uri=uri,range=dict(start=dict(line=0,character=0),end=dict(line=0,character=1)))
        send(dict(id=msg['id'],result=[location]*101 if mode=='dedup' else [location]))
    elif method=='$/cancelRequest':
        if waiting is not None: send(dict(id=waiting,error=dict(code=-32800,message='cancelled')))
    elif method=='shutdown': send(dict(id=msg['id'],result=None))
    elif method=='exit': sys.exit(0)
    elif 'result' in msg or 'error' in msg:
        if msg['id']==10000: assert msg['result']==dict(applied=False)
        if msg['id']==10001: assert msg['error']['code']==-32601
"#;

fn fake(root: &Path, mode: &str) -> Arc<WorkspaceConfig> {
    // The server script is outside the inventoried workspace, just like the
    // operator-managed immutable real executable.
    config(
        root,
        PathBuf::from("/usr/bin/python3"),
        vec!["-u".into(), "-c".into(), FAKE_SERVER.into(), mode.into()],
    )
}

#[tokio::test]
async fn cancelled_request_drains_and_preserves_pid() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.py"), "x = 1\n").unwrap();
    let signals = tempfile::tempdir().unwrap();
    let signal_path = signals.path().join("inflight.sock");
    let listener = tokio::net::UnixListener::bind(&signal_path).unwrap();
    let mut config = (*fake(dir.path(), "cancel")).clone();
    config.args.push(signal_path.to_string_lossy().into_owned());
    let mut worker = Worker::start(Arc::new(config), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let pid = worker.process_id();
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let future = worker.query(
        WorkerQuery {
            operation: Operation::Definition,
            path: dir.path().join("main.py"),
            position: Position { line: 1, column: 1 },
            max_results: 100,
        },
        deadline(),
        cancel,
    );
    let (response, ()) = tokio::join!(future, async {
        tokio::time::timeout_at(deadline(), listener.accept())
            .await
            .unwrap()
            .unwrap();
        signal.cancel();
    });
    assert_eq!(response.status, Status::Cancelled);
    assert_eq!(worker.process_id(), pid);
    assert_eq!(
        query(&mut worker, dir.path(), Operation::References, 1, 1)
            .await
            .status,
        Status::Ok
    );
    assert_eq!(worker.process_id(), pid);
    worker.shutdown().await;
    assert_eq!(worker.process_id(), None);
}

#[tokio::test]
async fn protocol_errors_external_results_stale_and_request_rejection() {
    for (mode, expected) in [
        ("external", Status::Unavailable),
        ("oversized", Status::Unavailable),
        ("stale", Status::Stale),
        ("requests", Status::Ok),
        ("dedup", Status::Ok),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.py"), "x = 1\n").unwrap();
        let mut worker =
            Worker::start(fake(dir.path(), mode), deadline(), CancellationToken::new())
                .await
                .unwrap();
        let response = query(&mut worker, dir.path(), Operation::Definition, 1, 1).await;
        assert_eq!(response.status, expected, "{mode}: {response:?}");
        if mode == "dedup" {
            assert_eq!(response.locations.len(), 1);
        }
        worker.shutdown().await;
    }
}

#[tokio::test]
async fn timeout_and_failed_refresh_cannot_reuse_stale_state() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.py"), "x = 1\n").unwrap();
    let mut worker = Worker::start(
        fake(dir.path(), "timeout"),
        deadline(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let begin = Instant::now();
    let response = worker
        .query(
            WorkerQuery {
                operation: Operation::Definition,
                path: dir.path().join("main.py"),
                position: Position { line: 1, column: 1 },
                max_results: 100,
            },
            begin + Duration::from_millis(100),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(response.status, Status::Timeout);
    assert!(begin.elapsed() < Duration::from_millis(350));
    std::os::unix::fs::symlink("/tmp", dir.path().join("bad")).unwrap();
    assert_eq!(
        query(&mut worker, dir.path(), Operation::Definition, 1, 1)
            .await
            .status,
        Status::Unavailable
    );
    assert_eq!(worker.process_id(), None);
    worker.shutdown().await;
    std::fs::remove_file(dir.path().join("bad")).unwrap();
    let error = Worker::start(
        fake(dir.path(), "encoding"),
        deadline(),
        CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        error.reason.as_deref(),
        Some("unsupported_position_encoding")
    );
}

#[tokio::test]
#[ignore = "requires pinned operator Pyrefly 1.2.0"]
async fn real_pyrefly_unicode_empty_warm_saved_change_and_delete() {
    let executable =
        PathBuf::from("/home/eric/arcade/scripts/codeintel/toolchain/.venv/bin/pyrefly");
    let version = std::process::Command::new(&executable)
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "pyrefly 1.2.0"
    );
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.py"), "class Widget:\n    pass\n").unwrap();
    std::fs::write(
        root.join("main.py"),
        "from a import Widget\nmarker = \"😀é\"; value = Widget()\n",
    )
    .unwrap();
    let args = vec![
        "lsp".into(),
        "--indexing-mode".into(),
        "lazy-blocking".into(),
        "--threads".into(),
        "1".into(),
    ];
    let cold = Instant::now();
    let mut worker = Worker::start(
        config(root, executable, args),
        deadline(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let empty = query(&mut worker, root, Operation::Definition, 3, 1).await;
    println!("real cold empty {:?} elapsed {:?}", empty, cold.elapsed());
    assert_eq!(empty.status, Status::Ok);
    assert!(empty.locations.is_empty());
    let pid = worker.process_id();
    let column = "marker = \"😀é\"; value = ".chars().count() as u32 + 1;
    let definition = query(&mut worker, root, Operation::Definition, 2, column).await;
    println!("real definition {definition:?}");
    assert_eq!(definition.status, Status::Ok);
    assert_eq!(definition.locations.len(), 1);
    assert_eq!(definition.locations[0].path, "a.py");
    assert_eq!(
        definition.locations[0].start,
        Position { line: 1, column: 7 }
    );
    let references = query(&mut worker, root, Operation::References, 2, column).await;
    println!("real references {references:?}");
    assert_eq!(references.status, Status::Ok);
    assert_eq!(references.locations.len(), 3);
    assert_eq!(
        references
            .locations
            .iter()
            .map(|location| (location.path.as_str(), location.start))
            .collect::<Vec<_>>(),
        vec![
            ("a.py", Position { line: 1, column: 7 }),
            (
                "main.py",
                Position {
                    line: 1,
                    column: 15
                }
            ),
            ("main.py", Position { line: 2, column })
        ]
    );
    assert_eq!(worker.process_id(), pid);
    std::fs::write(
        root.join("a.py"),
        "# updated saved generation\nclass Widget:\n    pass\n",
    )
    .unwrap();
    let changed = query(&mut worker, root, Operation::Definition, 2, column).await;
    println!("real changed {changed:?}");
    assert_eq!(changed.status, Status::Ok);
    assert_eq!(changed.locations[0].start, Position { line: 2, column: 7 });
    assert_ne!(worker.process_id(), pid);
    std::fs::remove_file(root.join("a.py")).unwrap();
    let deleted = query(&mut worker, root, Operation::Definition, 2, column).await;
    println!("real deleted {deleted:?}");
    assert_eq!(deleted.status, Status::Ok);
    assert_eq!(deleted.locations.len(), 1);
    assert_eq!(deleted.locations[0].path, "main.py");
    assert_eq!(
        deleted.locations[0].start,
        Position {
            line: 1,
            column: 15
        }
    );
    worker.shutdown().await;
}
