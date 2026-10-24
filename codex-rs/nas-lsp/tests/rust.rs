//! Direct-worker qualification. Real tests require the exact operator pins.
#![cfg(target_os = "linux")]

use codex_lsp_protocol::{Location, NavigationResponse, Operation, Position, Status};
use codex_nas_lsp::config::{Readiness, WorkspaceConfig};
use codex_nas_lsp::worker::{Worker, WorkerQuery};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}
fn config(root: &Path) -> Arc<WorkspaceConfig> {
    Arc::new(WorkspaceConfig {
        id: "rust-qualification".into(),
        root: root.to_owned(),
        language: "rust".into(),
        executable: std::env::var_os("NAS_LSP_RUST_ANALYZER")
            .map(PathBuf::from)
            .expect("set NAS_LSP_RUST_ANALYZER to pinned RA"),
        args: vec![],
        source_extensions: vec!["rs".into()],
        exclude_dirs: vec![],
        readiness: Readiness::RustAnalyzerProject,
        typescript_tsserver: None,
        rustc: Some(
            std::env::var_os("NAS_LSP_RUSTC")
                .map(PathBuf::from)
                .expect("set NAS_LSP_RUSTC to pinned real rustc"),
        ),
    })
}
async fn query(
    worker: &mut Worker,
    root: &Path,
    file: &str,
    operation: Operation,
    line: u32,
    column: u32,
) -> NavigationResponse {
    worker
        .query(
            WorkerQuery {
                operation,
                path: root.join(file),
                position: Position { line, column },
                max_results: 100,
            },
            deadline(),
            CancellationToken::new(),
        )
        .await
}
fn put(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), target).unwrap();
        }
    }
}
fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/nas-lsp-rust")
}
fn location(oracle: &Value, id: &str, offset: u32) -> Location {
    let at = &oracle["locations"][id];
    Location {
        path: at["path"].as_str().unwrap().into(),
        start: Position {
            line: at["line"].as_u64().unwrap() as u32 + offset,
            column: at["column"].as_u64().unwrap() as u32,
        },
        end: Position {
            line: at["line"].as_u64().unwrap() as u32 + offset,
            column: at["end_column"].as_u64().unwrap() as u32,
        },
    }
}
async fn audit(
    worker: &mut Worker,
    root: &Path,
    oracle: &Value,
    scenario: &str,
    offset: u32,
) -> usize {
    let mut count = 0;
    for probe in oracle["queries"].as_array().unwrap() {
        let at = location(oracle, probe["at"].as_str().unwrap(), offset);
        let operation = if probe["operation"] == "definition" {
            Operation::Definition
        } else {
            Operation::References
        };
        let mut expected = vec![];
        if let Some(symbol) = probe["symbol"].as_str() {
            if operation == Operation::Definition {
                expected.push(location(
                    oracle,
                    oracle["symbols"][symbol]["definition"].as_str().unwrap(),
                    offset,
                ));
            } else {
                expected.extend(
                    oracle["scenarios"][scenario]["references"][symbol]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|id| location(oracle, id.as_str().unwrap(), offset)),
                );
            }
        }
        expected.sort();
        let start = Instant::now();
        let response = query(
            worker,
            root,
            &at.path,
            operation,
            at.start.line,
            at.start.column,
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
            "rust exact {scenario}/{} locations={} elapsed={:?}",
            probe["id"],
            response.locations.len(),
            start.elapsed()
        );
        count += 1;
    }
    count
}

#[tokio::test]
#[ignore = "requires pinned NAS_LSP_RUST_ANALYZER and NAS_LSP_RUSTC"]
async fn real_cargo_corpus_cold_reverse_references_changes_and_separate_roots() {
    let fixture = corpus();
    let oracle: Value =
        serde_json::from_slice(&std::fs::read(fixture.join("oracle.json")).unwrap()).unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    copy(&fixture.join("workspace"), a.path());
    copy(&fixture.join("workspace"), b.path());
    for e in std::fs::read_dir(b.path().join("src")).unwrap() {
        let p = e.unwrap().path();
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(p, format!("// independent root B\n\n{text}")).unwrap();
    }
    let cold = Instant::now();
    let mut wa = Worker::start(config(a.path()), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let mut wb = Worker::start(config(b.path()), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let mut pid = wa.process_id();
    let other_pid = wb.process_id();
    let mut checks = 0;
    for stage in ["base", "changed", "deleted"] {
        let model = &oracle["scenarios"][stage];
        if let Some(replace) = model["replace"].as_object() {
            for (target, source) in replace {
                std::fs::copy(
                    fixture.join(source.as_str().unwrap()),
                    a.path().join(target),
                )
                .unwrap();
            }
        }
        if let Some(delete) = model["delete"].as_array() {
            for target in delete {
                std::fs::remove_file(a.path().join(target.as_str().unwrap())).unwrap();
            }
        }
        checks += audit(&mut wa, a.path(), &oracle, stage, 0).await;
        checks += audit(&mut wb, b.path(), &oracle, "base", 2).await;
        if stage != "base" {
            assert_ne!(wa.process_id(), pid);
            pid = wa.process_id();
        }
        assert_eq!(wb.process_id(), other_pid);
        for (worker, root) in [(&wa, a.path()), (&wb, b.path())] {
            let pid = worker.process_id().unwrap();
            let threads = std::fs::read_dir(format!("/proc/{pid}/task"))
                .unwrap()
                .filter_map(|entry| std::fs::read_to_string(entry.ok()?.path().join("comm")).ok())
                .collect::<Vec<_>>();
            assert_eq!(
                threads
                    .iter()
                    .filter(|name| name.starts_with("RayonWorker"))
                    .count(),
                2,
                "global Rayon pool must be bounded independently of numThreads: {threads:?}"
            );
            assert!(
                threads.len() <= 20,
                "unexpected analyzer task budget: {threads:?}"
            );
            assert!(std::fs::read_dir(root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("rustc-ice-")
            }));
        }
    }
    assert_eq!(checks, 84);
    println!("rust two-root corpus total={:?}", cold.elapsed());
    wa.shutdown().await;
    wb.shutdown().await;
}

fn graph() -> Value {
    json!({"crates":[
        {"root_module":"lib.rs","edition":"2021","deps":[],"display_name":"local_lib","is_workspace_member":true},
        {"root_module":"app.rs","edition":"2021","deps":[{"crate":0,"name":"local_lib"}],"display_name":"app","is_workspace_member":true}
    ]})
}
#[tokio::test]
#[ignore = "requires pinned NAS_LSP_RUST_ANALYZER and NAS_LSP_RUSTC"]
async fn real_explicit_cross_crate_graph_model_changes_and_orphan_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    put(root, "rust-project.json", &graph().to_string());
    put(root, "lib.rs", "pub fn symbol() {}\n");
    put(root, "app.rs", "pub fn caller() { local_lib::symbol(); }\n");
    put(root, "orphan.rs", "fn orphan() {}\n");
    put(root, "build.rs", "fn main() {}\n");
    let mut w = Worker::start(config(root), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let refs = query(&mut w, root, "lib.rs", Operation::References, 1, 8).await;
    assert_eq!(refs.status, Status::Ok, "{refs:?}");
    assert_eq!(
        refs.locations
            .iter()
            .map(|l| (l.path.as_str(), l.start.column))
            .collect::<Vec<_>>(),
        vec![("app.rs", 30), ("lib.rs", 8)]
    );
    let def = query(&mut w, root, "app.rs", Operation::Definition, 1, 30).await;
    assert_eq!(def.status, Status::Ok, "{def:?}");
    assert_eq!(def.locations.len(), 1);
    assert_eq!(def.locations[0].path, "lib.rs");
    for path in ["orphan.rs", "build.rs"] {
        let r = query(&mut w, root, path, Operation::References, 1, 4).await;
        assert_eq!(r.status, Status::Unavailable);
        assert_eq!(r.reason.as_deref(), Some("unsupported_rust_source"));
        assert!(r.locations.is_empty());
    }
    let mut changed = graph();
    changed["crates"].as_array_mut().unwrap().pop();
    let pid = w.process_id();
    put(root, "rust-project.json", &changed.to_string());
    let refs = query(&mut w, root, "lib.rs", Operation::References, 1, 8).await;
    assert_eq!(refs.status, Status::Ok, "{refs:?}");
    assert_eq!(refs.locations.len(), 1);
    assert_eq!(refs.locations[0].path, "lib.rs");
    assert_ne!(w.process_id(), pid);
    let orphan = query(&mut w, root, "app.rs", Operation::Definition, 1, 30).await;
    assert_eq!(orphan.status, Status::Unavailable);
    std::fs::remove_file(root.join("lib.rs")).unwrap();
    let deleted = query(&mut w, root, "app.rs", Operation::References, 1, 30).await;
    assert_eq!(deleted.status, Status::Unavailable);
    assert!(deleted.locations.is_empty());
    w.shutdown().await;

    // Raw-spelled cfg/test identifiers have the same compiler meaning. The
    // explicit graph enables test here; removing it must reject this module.
    let raw = tempfile::tempdir().unwrap();
    let mut model = graph();
    model["crates"].as_array_mut().unwrap().truncate(1);
    model["crates"][0]["cfg"] = json!(["test"]);
    put(raw.path(), "rust-project.json", &model.to_string());
    put(
        raw.path(),
        "lib.rs",
        "#[r#cfg(r#test)] mod tests;\npub fn symbol() {}\n",
    );
    put(
        raw.path(),
        "tests.rs",
        "pub fn test_user() { crate::symbol(); }\n",
    );
    let mut w = Worker::start(config(raw.path()), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let refs = query(&mut w, raw.path(), "lib.rs", Operation::References, 2, 8).await;
    assert_eq!(refs.status, Status::Ok, "{refs:?}");
    assert_eq!(
        refs.locations
            .iter()
            .map(|l| l.path.as_str())
            .collect::<Vec<_>>(),
        vec!["lib.rs", "tests.rs"]
    );
    model["crates"][0]["cfg"] = json!([]);
    put(raw.path(), "rust-project.json", &model.to_string());
    let disabled = query(&mut w, raw.path(), "tests.rs", Operation::References, 1, 8).await;
    assert_eq!(disabled.status, Status::Unavailable, "{disabled:?}");
    assert_eq!(disabled.reason.as_deref(), Some("unsupported_rust_source"));
    w.shutdown().await;
}

#[tokio::test]
#[ignore = "requires pinned NAS_LSP_RUST_ANALYZER and NAS_LSP_RUSTC; trace with strace -f -e execve"]
async fn real_hostile_cargo_toolchain_build_scripts_never_execute() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let sentinel = root.join("EXECUTED");
    put(
        root,
        "Cargo.toml",
        "[package]\nname='hostile'\nversion='0.1.0'\nedition='2021'\nbuild='build.rs'\n",
    );
    put(
        root,
        "src/lib.rs",
        "pub fn target() {}\npub fn caller() { target(); }\n",
    );
    put(
        root,
        "build.rs",
        "fn main() { std::fs::write(\"EXECUTED\", \"build script\").unwrap(); }\n",
    );
    put(root, "attack", "#!/bin/sh\ntouch EXECUTED\nexit 9\n");
    std::fs::set_permissions(root.join("attack"), std::fs::Permissions::from_mode(0o700)).unwrap();
    put(
        root,
        ".cargo/config.toml",
        &format!(
            "[build]\nrustc-wrapper='{}'\nrustc='{}'\n[target.x86_64-unknown-linux-gnu]\nrunner='{}'\n",
            root.join("attack").display(),
            root.join("attack").display(),
            root.join("attack").display()
        ),
    );
    put(
        root,
        "rust-toolchain.toml",
        "[toolchain]\nchannel='definitely-not-installed-navigation-sentinel'\n",
    );
    let mut w = Worker::start(config(root), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let r = query(&mut w, root, "src/lib.rs", Operation::References, 1, 8).await;
    assert_eq!(r.status, Status::Ok, "{r:?}");
    assert_eq!(r.locations.len(), 2);
    assert!(!sentinel.exists());
    w.shutdown().await;
    assert!(!sentinel.exists());
    for key in ["proc_macro_dylib_path", "runnables", "env"] {
        let mut model = graph();
        model["crates"][0][key] = json!(root.join("attack"));
        put(root, "rust-project.json", &model.to_string());
        let error = Worker::start(config(root), deadline(), CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error.status, Status::Unavailable);
        assert!(!sentinel.exists());
    }
}

#[tokio::test]
#[ignore = "requires pinned NAS_LSP_RUST_ANALYZER and NAS_LSP_RUSTC and existing WezTerm base91"]
async fn real_existing_wezterm_base91_own_symbol_navigation() {
    let from = Path::new("/home/eric/git/wezterm/base91");
    assert!(from.is_dir());
    let dir = tempfile::tempdir().unwrap();
    copy(from, dir.path());
    let source = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    let symbol = "Base91Encoder";
    let declaration = source
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("pub struct Base91Encoder"))
        .unwrap();
    let line = declaration.0 as u32 + 1;
    let column = declaration.1.find(symbol).unwrap() as u32 + 1;
    let mut w = Worker::start(config(dir.path()), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let refs = query(
        &mut w,
        dir.path(),
        "src/lib.rs",
        Operation::References,
        line,
        column,
    )
    .await;
    assert_eq!(refs.status, Status::Ok, "{refs:?}");
    assert!(refs.locations.len() >= 3, "{refs:?}");
    assert!(refs.locations.iter().all(|l| l.path == "src/lib.rs"));
    let usage = refs
        .locations
        .iter()
        .find(|l| l.start.line != line)
        .unwrap();
    let def = query(
        &mut w,
        dir.path(),
        "src/lib.rs",
        Operation::Definition,
        usage.start.line,
        usage.start.column,
    )
    .await;
    assert_eq!(def.status, Status::Ok, "{def:?}");
    assert_eq!(def.locations.len(), 1);
    assert_eq!(def.locations[0].start, Position { line, column });
    println!(
        "existing WezTerm base91 {} refs={}",
        symbol,
        refs.locations.len()
    );
    w.shutdown().await;
}

const MOCK: &str = r#"
import json,os,pathlib,socket,sys
mode=sys.argv[1];opened={};barriers=0;recover=mode=='recover'

def checkpoint(kind):
    with socket.socket(socket.AF_UNIX) as channel:
        channel.connect(sys.argv[2]);channel.sendall((kind+'\n').encode());reply=b''
        while not reply.endswith(b'\n'):
            chunk=channel.recv(64)
            if not chunk:break
            reply+=chunk
        return reply.decode().strip()

def send(v):
    b=json.dumps(dict(jsonrpc='2.0',**v)).encode();sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(b)).encode()+b);sys.stdout.buffer.flush()
def status(health='ok',quiet=True):
    send(dict(method='experimental/serverStatus',params=dict(health=health,quiescent=quiet)))
while True:
    headers={}
    while True:
        line=sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line==b'\r\n':break
        k,v=line.decode().strip().split(':',1);headers[k.lower()]=v.strip()
    msg=json.loads(sys.stdin.buffer.read(int(headers['content-length'])));method=msg.get('method')
    if method=='initialize':
        if recover:mode=checkpoint('initialize')
        opts=msg['params']['initializationOptions']
        assert opts['cargo']['sysroot'] is None
        assert opts['cargo']['autoreload'] is True
        assert opts['cfg']['setTest'] is False
        assert opts['linkedProjects'][0]['crates']
        assert opts['cargo']['buildScripts']['enable'] is False
        assert opts['procMacro']['enable'] is False
        assert os.environ['RAYON_NUM_THREADS']=='2'
        crash_dir=pathlib.Path(os.environ['RUSTC_ICE'])
        assert crash_dir.is_absolute() and crash_dir.is_dir()
        assert crash_dir == pathlib.Path(os.environ['TMPDIR'])
        assert not crash_dir.is_relative_to(pathlib.Path.cwd())
        # Exercise the compiler's directory-based report naming contract with
        # a controlled diagnostic file, without deliberately crashing rustc.
        (crash_dir/'rustc-ice-mock.txt').write_text('private diagnostic')
        assert msg['params']['capabilities']['experimental']['serverStatusNotification'] is True
        send(dict(id=msg['id'],result=dict(capabilities=dict(definitionProvider=True,referencesProvider=True,textDocumentSync=1))))
        if mode!='missing':status('warning' if mode=='warning' else 'ok',mode!='nonquiescent')
    elif method=='textDocument/didOpen':
        doc=msg['params']['textDocument'];assert doc['languageId']=='rust';opened[doc['uri']]=doc['text']
    elif method=='rust-analyzer/viewFileText':
        assert set(msg['params'])=={'uri'}
        barriers+=1
        if mode=='cancel':checkpoint('barrier')
        if mode=='barrier-once': assert barriers<=len(opened)
        send(dict(id=msg['id'],result='' if mode=='mismatch' else opened[msg['params']['uri']]))
    elif method in ('textDocument/definition','textDocument/references'):
        assert barriers==len(opened)
        if mode=='epoch': status(quiet=False);status()
        if mode=='change-config':
            with open('rust-analyzer.toml','w') as f:f.write('')
        locations=[dict(uri=msg['params']['textDocument']['uri'],range=dict(start=dict(line=0,character=7),end=dict(line=0,character=13)))] if recover else []
        send(dict(id=msg['id'],result=locations))
    elif method=='shutdown':send(dict(id=msg['id'],result=None))
    elif method=='exit':sys.exit(0)
"#;
fn mocked(root: &Path, mode: &str) -> Arc<WorkspaceConfig> {
    Arc::new(WorkspaceConfig {
        id: "mock-rust".into(),
        root: root.to_owned(),
        language: "rust".into(),
        executable: PathBuf::from("/usr/bin/python3"),
        args: vec!["-u".into(), "-c".into(), MOCK.into(), mode.into()],
        source_extensions: vec!["rs".into()],
        exclude_dirs: vec![],
        readiness: Readiness::RustAnalyzerProject,
        typescript_tsserver: None,
        rustc: Some(PathBuf::from("/usr/bin/true")),
    })
}
fn simple(root: &Path) {
    put(
        root,
        "Cargo.toml",
        "[package]\nname='simple'\nversion='0.1.0'\nedition='2021'\n",
    );
    put(root, "src/lib.rs", "pub fn symbol() {}\n");
}
#[tokio::test]
async fn readiness_requires_status_health_quiescence_saved_text_and_stable_epoch() {
    for (mode, expected) in [
        ("missing", Status::Indexing),
        ("nonquiescent", Status::Indexing),
        ("warning", Status::Unavailable),
        ("mismatch", Status::Indexing),
        ("epoch", Status::Indexing),
        ("change-config", Status::Stale),
        ("barrier-once", Status::Ok),
    ] {
        let dir = tempfile::tempdir().unwrap();
        simple(dir.path());
        let mut w = Worker::start(
            mocked(dir.path(), mode),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        for _ in 0..if mode == "barrier-once" { 2 } else { 1 } {
            let r = w
                .query(
                    WorkerQuery {
                        operation: Operation::References,
                        path: dir.path().join("src/lib.rs"),
                        position: Position { line: 1, column: 8 },
                        max_results: 100,
                    },
                    Instant::now() + Duration::from_millis(350),
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(r.status, expected, "{mode}: {r:?}");
            assert!(r.locations.is_empty());
        }
        w.shutdown().await;
    }
}
#[tokio::test]
async fn source_directive_edits_and_orphan_queries_never_become_empty_success() {
    let outside = tempfile::tempdir().unwrap();
    put(outside.path(), "outside.rs", "pub fn external() {}\n");
    let dir = tempfile::tempdir().unwrap();
    simple(dir.path());
    put(dir.path(), "orphan.rs", "fn orphan() {}\n");
    put(dir.path(), ".git/input.rs", "pub fn hidden() {}\n");
    let mut w = Worker::start(
        mocked(dir.path(), "barrier-once"),
        deadline(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let orphan = query(&mut w, dir.path(), "orphan.rs", Operation::References, 1, 4).await;
    assert_eq!(orphan.status, Status::Unavailable);
    assert_eq!(orphan.reason.as_deref(), Some("unsupported_rust_source"));
    for directive in [
        format!(
            "#[path=\"{}\"] mod outside;",
            outside.path().join("outside.rs").display()
        ),
        "#[path=\"../.git/input.rs\"] mod hidden;".into(),
        "wrapper!({r#include!(\"../.git/input.rs\")});".into(),
        "macro_rules! generate {()=>{mod hidden;}}".into(),
    ] {
        put(dir.path(), "src/lib.rs", &directive);
        let r = query(
            &mut w,
            dir.path(),
            "src/lib.rs",
            Operation::References,
            1,
            1,
        )
        .await;
        assert_eq!(r.status, Status::Unavailable, "{r:?}");
        assert!(r.locations.is_empty());
        put(outside.path(), "outside.rs", "pub fn changed() {}\n");
        put(dir.path(), ".git/input.rs", "pub fn hidden_changed() {}\n");
        std::fs::remove_file(dir.path().join(".git/input.rs")).unwrap();
        std::fs::remove_file(outside.path().join("outside.rs")).unwrap();
        // The outside input's existence/content is irrelevant: the directive
        // is rejected from saved syntax before any server can read that input.
        let r = query(
            &mut w,
            dir.path(),
            "src/lib.rs",
            Operation::References,
            1,
            1,
        )
        .await;
        assert_eq!(r.status, Status::Unavailable);
        put(outside.path(), "outside.rs", "pub fn recreated() {}\n");
        put(
            dir.path(),
            ".git/input.rs",
            "pub fn hidden_recreated() {}\n",
        );
    }
    w.shutdown().await;
}
#[tokio::test]
async fn ancestor_config_creation_is_guarded_on_warm_worker() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("project");
    std::fs::create_dir(&root).unwrap();
    simple(&root);
    let mut w = Worker::start(
        mocked(&root, "barrier-once"),
        deadline(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let first = query(&mut w, &root, "src/lib.rs", Operation::References, 1, 8).await;
    assert_eq!(first.status, Status::Ok);
    put(parent.path(), "rust-analyzer.toml", "");
    let changed = query(&mut w, &root, "src/lib.rs", Operation::References, 1, 8).await;
    assert_eq!(changed.status, Status::Stale);
    assert!(changed.locations.is_empty());
    std::fs::remove_file(parent.path().join("rust-analyzer.toml")).unwrap();
    let restored = query(&mut w, &root, "src/lib.rs", Operation::References, 1, 8).await;
    assert_eq!(restored.status, Status::Ok);
    w.shutdown().await;
}

#[tokio::test]
#[ignore = "requires pinned NAS_LSP_RUST_ANALYZER and NAS_LSP_RUSTC"]
async fn real_declared_cargo_lib_bin_dependency_and_nested_modules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    put(
        root,
        "Cargo.toml",
        "[package]\nname='package-name'\nversion='0.1.0'\nedition='2018'\nautobins=false\n[lib]\nname='renamed_lib'\npath='library.rs'\n[[bin]]\nname='tool'\npath='tool.rs'\n",
    );
    put(root, "library.rs", "pub mod outer { pub mod nested; }\n");
    put(root, "outer/nested.rs", "pub fn symbol() {}\n");
    put(
        root,
        "tool.rs",
        "fn main() { renamed_lib::outer::nested::symbol(); }\n",
    );
    let mut worker = Worker::start(config(root), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let refs = query(
        &mut worker,
        root,
        "outer/nested.rs",
        Operation::References,
        1,
        8,
    )
    .await;
    assert_eq!(refs.status, Status::Ok, "{refs:?}");
    assert_eq!(
        refs.locations
            .iter()
            .map(|l| l.path.as_str())
            .collect::<Vec<_>>(),
        vec!["outer/nested.rs", "tool.rs"]
    );
    let usage = &refs.locations[1];
    let def = query(
        &mut worker,
        root,
        "tool.rs",
        Operation::Definition,
        usage.start.line,
        usage.start.column,
    )
    .await;
    assert_eq!(def.status, Status::Ok, "{def:?}");
    assert_eq!(def.locations, vec![refs.locations[0].clone()]);
    worker.shutdown().await;
}

#[tokio::test]
async fn deeply_nested_source_returns_unavailable_before_server_launch() {
    let dir = tempfile::tempdir().unwrap();
    simple(dir.path());
    let nested = format!(
        "fn f() {{ let _ = {}0{}; }}",
        "(".repeat(1024),
        ")".repeat(1024)
    );
    put(dir.path(), "src/lib.rs", &nested);
    let result = Worker::start(
        mocked(dir.path(), "barrier-once"),
        deadline(),
        CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(result.status, Status::Unavailable);
    assert_eq!(result.reason.as_deref(), Some("unsupported_source_syntax"));
}

#[tokio::test]
async fn raw_cfg_disabled_source_is_unavailable_before_server_launch() {
    for source in [
        "#[r#cfg(any())] mod disabled;",
        "#![r#cfg(any())] mod disabled;",
    ] {
        let dir = tempfile::tempdir().unwrap();
        simple(dir.path());
        put(dir.path(), "src/lib.rs", source);
        put(dir.path(), "src/disabled.rs", "pub fn hidden() {}\n");
        let result = Worker::start(
            mocked(dir.path(), "barrier-once"),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(result.status, Status::Unavailable);
        assert_eq!(result.reason.as_deref(), Some("unsupported_rust_source"));
    }
}

#[tokio::test]
async fn first_text_barrier_error_retires_session_then_survivor_is_exact() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    async fn checkpoint(
        listener: &UnixListener,
        expected: &str,
        reply: Option<&str>,
    ) -> UnixStream {
        let control_deadline = deadline();
        let (stream, _) = tokio::time::timeout_at(control_deadline, listener.accept())
            .await
            .expect("mock initialize/barrier event within request budget")
            .unwrap();
        let mut stream = BufReader::new(stream);
        let mut message = String::new();
        tokio::time::timeout_at(control_deadline, stream.read_line(&mut message))
            .await
            .expect("mock control message within request budget")
            .unwrap();
        assert_eq!(message.trim_end(), expected);
        if let Some(reply) = reply {
            stream
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        stream.into_inner()
    }

    for first in ["cancel", "mismatch"] {
        let dir = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        let socket = control.path().join("barrier.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        simple(dir.path());
        let mut config = mocked(dir.path(), "recover");
        Arc::make_mut(&mut config)
            .args
            .push(socket.to_str().unwrap().into());
        let (started, _) = tokio::join!(
            Worker::start(config, deadline(), CancellationToken::new()),
            checkpoint(&listener, "initialize", Some(first)),
        );
        let mut worker = started.unwrap();
        let original_pid = worker.process_id();
        let first_response = if first == "cancel" {
            let cancel = CancellationToken::new();
            let requested = worker.query(
                WorkerQuery {
                    operation: Operation::References,
                    path: dir.path().join("src/lib.rs"),
                    position: Position { line: 1, column: 8 },
                    max_results: 100,
                },
                deadline(),
                cancel.clone(),
            );
            let (response, blocked_barrier) = tokio::join!(requested, async {
                let channel = checkpoint(&listener, "barrier", None).await;
                cancel.cancel();
                channel
            });
            // Hold the server's barrier channel until session retirement has
            // completed, so it cannot accidentally answer before cancellation.
            drop(blocked_barrier);
            response
        } else {
            query(
                &mut worker,
                dir.path(),
                "src/lib.rs",
                Operation::References,
                1,
                8,
            )
            .await
        };
        assert_eq!(
            first_response.status,
            if first == "cancel" {
                Status::Cancelled
            } else {
                Status::Indexing
            },
            "{first}: {first_response:?}"
        );
        assert!(first_response.locations.is_empty());
        assert_eq!(
            worker.process_id(),
            None,
            "{first}: failed admission must retire the session"
        );
        let (survivor, _) = tokio::join!(
            query(
                &mut worker,
                dir.path(),
                "src/lib.rs",
                Operation::References,
                1,
                8
            ),
            checkpoint(&listener, "initialize", Some("exact")),
        );
        assert_eq!(survivor.status, Status::Ok, "{first}: {survivor:?}");
        let expected = vec![Location {
            path: "src/lib.rs".into(),
            start: Position { line: 1, column: 8 },
            end: Position {
                line: 1,
                column: 14,
            },
        }];
        assert_eq!(
            survivor.locations, expected,
            "{first}: survivor must pass its own text barrier and return exact locations"
        );
        let survivor_pid = worker.process_id();
        assert_ne!(survivor_pid, original_pid);
        let warm = query(
            &mut worker,
            dir.path(),
            "src/lib.rs",
            Operation::References,
            1,
            8,
        )
        .await;
        assert_eq!(warm.status, Status::Ok, "{warm:?}");
        assert_eq!(warm.locations, expected);
        assert_eq!(worker.process_id(), survivor_pid);
        worker.shutdown().await;
    }
}
