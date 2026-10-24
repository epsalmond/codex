//! Worker-only real and adversarial tests; see `.codex/design/nas-lsp-broker.md`.

#![cfg(target_os = "linux")]

use codex_lsp_protocol::{Location, NavigationResponse, Operation, Position, Status};
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
        typescript_tsserver: None,
        rustc: None,
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
opened={}
queries=0
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
        assert msg['params']['capabilities']['workspace']['workspaceFolders'] is True
        sync={'openClose':True,'change':99} if mode=='unknown-sync' else (0 if mode=='no-sync' else 1)
        send(dict(id=msg['id'],result=dict(capabilities=dict(positionEncoding='utf-8' if mode=='encoding' else 'utf-16',definitionProvider=True,referencesProvider=True,textDocumentSync=sync))))
    elif method=='textDocument/didOpen':
        document=msg['params']['textDocument']
        assert document['uri'] not in opened
        opened[document['uri']]=document['text']
    elif method in ('textDocument/definition','textDocument/references'):
        queries+=1
        if mode=='admission':
            assert sorted(uri.rsplit('/',1)[-1] for uri in opened)==['dependent.py','main.py','types.pyi']
            assert any(text=='stub: int\n' for text in opened.values())
        if mode=='not-ready' and queries==1:
            send(dict(id=msg['id'],error=dict(code=-32002,message='not ready')));continue
        if mode=='progress':
            send(dict(method='$/progress',params=dict(token='index',value=dict(kind='begin' if queries==1 else 'end'))))
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
async fn all_sources_and_stubs_are_admitted_once_with_py_only_queries() {
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in [
        ("main.py", "x = 1\n"),
        ("dependent.py", "from main import x\n"),
        ("types.pyi", "stub: int\n"),
        ("settings.toml", "[settings]\n"),
    ] {
        std::fs::write(dir.path().join(path), text).unwrap();
    }
    let mut worker = Worker::start(
        fake(dir.path(), "admission"),
        deadline(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let response = query(&mut worker, dir.path(), Operation::References, 1, 1).await;
        assert_eq!(response.status, Status::Ok, "{response:?}");
    }
    worker.shutdown().await;
}

#[tokio::test]
async fn active_progress_and_server_not_ready_discard_results_until_ready() {
    for mode in ["progress", "not-ready"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.py"), "x = 1\n").unwrap();
        let mut worker =
            Worker::start(fake(dir.path(), mode), deadline(), CancellationToken::new())
                .await
                .unwrap();
        let pid = worker.process_id();
        let response = query(&mut worker, dir.path(), Operation::References, 1, 1).await;
        assert_eq!(response.status, Status::Indexing, "{mode}: {response:?}");
        assert!(response.locations.is_empty());
        let ready = query(&mut worker, dir.path(), Operation::References, 1, 1).await;
        assert_eq!(ready.status, Status::Ok, "{mode}: {ready:?}");
        assert_eq!(
            ready.locations,
            vec![Location {
                path: "main.py".into(),
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            }]
        );
        assert_eq!(worker.process_id(), pid);
        worker.shutdown().await;
    }
}

#[tokio::test]
async fn unsupported_sync_or_unreadable_python_inventory_cannot_claim_ready() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.py"), "x = 1\n").unwrap();
    for mode in ["no-sync", "unknown-sync"] {
        let response = Worker::start(fake(dir.path(), mode), deadline(), CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(response.status, Status::Unavailable);
        assert_eq!(
            response.reason.as_deref(),
            Some("unsupported_document_sync")
        );
    }
    std::fs::write(dir.path().join("invalid.pyi"), [0xff]).unwrap();
    let response = Worker::start(
        fake(dir.path(), "admission"),
        deadline(),
        CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(response.status, Status::Unavailable);
    assert_eq!(
        response.reason.as_deref(),
        Some("unsupported_source_encoding")
    );
}

#[tokio::test]
async fn executable_exiting_before_initialize_is_compact_unavailable_and_reaped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.py"), "x = 1\n").unwrap();
    let processes = tempfile::tempdir().unwrap();
    let pid_file = processes.path().join("pid");
    let cfg = config(
        dir.path(),
        PathBuf::from("/usr/bin/python3"),
        vec![
            "-c".into(),
            "import os,sys; open(sys.argv[1], 'w').write(str(os.getpid())); sys.exit(23)".into(),
            pid_file.to_string_lossy().into_owned(),
        ],
    );
    let response = Worker::start(cfg, deadline(), CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(response.status, Status::Unavailable);
    assert!(response.locations.is_empty());
    assert!(serde_json::to_vec(&response).unwrap().len() < 160);
    let pid = std::fs::read_to_string(pid_file)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
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

#[test]
fn cancellation_during_warm_inventory_preserves_healthy_generation() {
    // Hold the only blocking thread so cancellation occurs after query's entry
    // check but before its saved inventory completes and input validation runs.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.py"), "x = 1\n").unwrap();
        let mut worker = Worker::start(
            fake(dir.path(), "exact"),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let before = query(&mut worker, dir.path(), Operation::References, 1, 1).await;
        assert_eq!(before.status, Status::Ok, "{before:?}");
        let pid = worker.process_id();
        let (occupied_tx, occupied_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let occupied = tokio::task::spawn_blocking(move || {
            occupied_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        occupied_rx.await.unwrap();
        let cancel = CancellationToken::new();
        let mut requested = Box::pin(worker.query(
            WorkerQuery {
                operation: Operation::References,
                path: dir.path().join("main.py"),
                position: Position { line: 1, column: 1 },
                max_results: 100,
            },
            deadline(),
            cancel.clone(),
        ));
        let first_poll = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(requested.as_mut(), cx))
        })
        .await;
        assert!(first_poll.is_pending());
        cancel.cancel();
        release_tx.send(()).unwrap();
        let cancelled = requested.await;
        occupied.await.unwrap();
        assert_eq!(cancelled.status, Status::Cancelled, "{cancelled:?}");
        assert!(cancelled.locations.is_empty());
        assert_eq!(worker.process_id(), pid);
        let survivor = query(&mut worker, dir.path(), Operation::References, 1, 1).await;
        assert_eq!(survivor.status, Status::Ok, "{survivor:?}");
        assert_eq!(survivor.locations, before.locations);
        assert_eq!(worker.process_id(), pid);
        worker.shutdown().await;
    });
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

fn real_config(root: &Path) -> Arc<WorkspaceConfig> {
    config(
        root,
        PathBuf::from("/home/eric/arcade/scripts/codeintel/toolchain/.venv/bin/pyrefly"),
        vec![
            "lsp".into(),
            "--indexing-mode".into(),
            "lazy-blocking".into(),
            "--threads".into(),
            "1".into(),
        ],
    )
}

fn copy_corpus(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_corpus(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test]
#[ignore = "requires pinned operator Pyrefly 1.2.0"]
async fn real_pyrefly_full_corpus_exact_references_before_any_consumer_query() {
    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/nas-lsp");
    let oracle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(corpus.join("oracle.json")).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    copy_corpus(&corpus.join("workspace"), root);
    let location = |id: &str| {
        let at = &oracle["locations"][id];
        Location {
            path: at["path"].as_str().unwrap().into(),
            start: Position {
                line: at["line"].as_u64().unwrap() as u32,
                column: at["column"].as_u64().unwrap() as u32,
            },
            end: Position {
                line: at["line"].as_u64().unwrap() as u32,
                column: at["end_column"].as_u64().unwrap() as u32,
            },
        }
    };
    let mut worker = Worker::start(real_config(root), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let mut pid = worker.process_id();
    let mut checks = 0;
    for scenario in ["base", "changed", "deleted"] {
        let stage = &oracle["scenarios"][scenario];
        if let Some(replace) = stage["replace"].as_object() {
            for (target, source) in replace {
                std::fs::copy(corpus.join(source.as_str().unwrap()), root.join(target)).unwrap();
            }
        }
        if let Some(delete) = stage["delete"].as_array() {
            for target in delete {
                std::fs::remove_file(root.join(target.as_str().unwrap())).unwrap();
            }
        }
        // Reference queries run first in each generation, at both declaration
        // and consumer. No prior query opens helper.py or transient.py.
        let mut probes = vec![
            serde_json::json!({"id":"cold_declaration_refs", "operation":"references", "at":"primary_definition", "symbol":"pkg.primary.render"}),
            serde_json::json!({"id":"unopened_dependent_refs", "operation":"references", "at":"repeated_right", "symbol":"pkg.primary.render"}),
        ];
        probes.extend(oracle["queries"].as_array().unwrap().iter().cloned());
        for (index, probe) in probes.into_iter().enumerate() {
            let at = location(probe["at"].as_str().unwrap());
            let operation = if probe["operation"] == "definition" {
                Operation::Definition
            } else {
                Operation::References
            };
            let mut expected = vec![];
            if let Some(symbol) = probe["symbol"].as_str() {
                if operation == Operation::Definition {
                    expected.push(location(
                        oracle["symbols"][symbol]["definition"].as_str().unwrap(),
                    ));
                } else {
                    expected.extend(
                        stage["references"][symbol]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|id| location(id.as_str().unwrap())),
                    );
                }
            }
            expected.sort();
            let started = Instant::now();
            let response = worker
                .query(
                    WorkerQuery {
                        operation,
                        path: root.join(at.path),
                        position: at.start,
                        max_results: 100,
                    },
                    deadline(),
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(
                response.status,
                Status::Ok,
                "{scenario}/{probe}: {response:?}"
            );
            assert!(!response.truncated);
            assert_eq!(response.locations, expected, "{scenario}/{probe}");
            println!(
                "real corpus {scenario}/{} exact={} elapsed={:?}",
                probe["id"],
                response.locations.len(),
                started.elapsed()
            );
            if index == 0 && scenario != "base" {
                assert_ne!(worker.process_id(), pid, "changed generation must restart");
                pid = worker.process_id();
            } else {
                assert_eq!(
                    worker.process_id(),
                    pid,
                    "unchanged generation must stay warm"
                );
            }
            checks += 1;
        }
    }
    assert_eq!(checks, 42);
    worker.shutdown().await;
}

#[tokio::test]
#[ignore = "requires pinned operator Pyrefly 1.2.0"]
async fn real_pyrefly_excluded_bridge_saved_change_cannot_reuse_stale_definition() {
    for hidden in ["hidden", "target", "node_modules"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(hidden)).unwrap();
        std::fs::write(root.join(hidden).join("__init__.py"), "").unwrap();
        std::fs::write(root.join("a.py"), "class A:\n    value: int = 1\n").unwrap();
        std::fs::write(root.join("b.py"), "class B:\n    value: str = 'b'\n").unwrap();
        std::fs::write(
            root.join(hidden).join("bridge.py"),
            "from a import A\ndef make() -> A:\n    return A()\n",
        )
        .unwrap();
        std::fs::write(
            root.join("main.py"),
            format!("from {hidden}.bridge import make\nmake().value\n"),
        )
        .unwrap();
        let mut cfg = (*real_config(root)).clone();
        cfg.exclude_dirs = vec![hidden.into()];
        let mut worker = Worker::start(Arc::new(cfg), deadline(), CancellationToken::new())
            .await
            .unwrap();
        let first = query(&mut worker, root, Operation::Definition, 2, 8).await;
        assert_eq!(first.status, Status::Ok, "{hidden}: {first:?}");
        assert_eq!(
            first.locations,
            vec![Location {
                path: "a.py".into(),
                start: Position { line: 2, column: 5 },
                end: Position {
                    line: 2,
                    column: 10
                }
            }]
        );
        let pid = worker.process_id();
        std::fs::write(
            root.join(hidden).join("bridge.py"),
            "from b import B\ndef make() -> B:\n    return B()\n",
        )
        .unwrap();
        let changed = query(&mut worker, root, Operation::Definition, 2, 8).await;
        assert_eq!(changed.status, Status::Ok, "{hidden}: {changed:?}");
        assert_eq!(
            changed.locations,
            vec![Location {
                path: "b.py".into(),
                start: Position { line: 2, column: 5 },
                end: Position {
                    line: 2,
                    column: 10
                }
            }]
        );
        assert_ne!(worker.process_id(), pid);
        worker.shutdown().await;
    }
}
