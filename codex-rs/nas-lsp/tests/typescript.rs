//! Direct worker qualification. Pinned real tests are opt-in:
//! NAS_LSP_TS_NODE=<canonical node24> NAS_LSP_TS_SERVER=<canonical cli.mjs>
//! NAS_LSP_TS_TSSERVER=<canonical typescript6/lib/tsserver.js>
//! cargo test -p codex-nas-lsp --test typescript -- --ignored --nocapture
//! These tests do not qualify the broker/cgroup/native/model transport.
#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "Integration fixture helpers fail the test on invalid setup or oracle data"
)]

use codex_lsp_protocol::Location;
use codex_lsp_protocol::NavigationResponse;
use codex_lsp_protocol::Operation;
use codex_lsp_protocol::Position;
use codex_lsp_protocol::Status;
use codex_nas_lsp::config::Readiness;
use codex_nas_lsp::config::WorkspaceConfig;
use codex_nas_lsp::worker::Worker;
use codex_nas_lsp::worker::WorkerQuery;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}
fn config(root: &Path) -> Arc<WorkspaceConfig> {
    Arc::new(WorkspaceConfig {
        id: "typescript-test".into(),
        root: root.to_path_buf(),
        language: "typescript".into(),
        executable: PathBuf::from(
            std::env::var("NAS_LSP_TS_NODE").expect("explicit pinned node required"),
        ),
        args: vec![
            std::env::var("NAS_LSP_TS_SERVER").expect("explicit pinned TSLS required"),
            "--stdio".into(),
        ],
        source_extensions: vec![
            "ts".into(),
            "tsx".into(),
            "js".into(),
            "jsx".into(),
            "mts".into(),
            "cts".into(),
            "mjs".into(),
            "cjs".into(),
        ],
        exclude_dirs: vec![],
        readiness: Readiness::TypeScriptProject,
        typescript_tsserver: Some(PathBuf::from(
            std::env::var("NAS_LSP_TS_TSSERVER").expect("explicit pinned tsserver required"),
        )),
        rustc: None,
    })
}
async fn start(root: &Path) -> Worker {
    Worker::start(config(root), deadline(), CancellationToken::new())
        .await
        .unwrap()
}
async fn query(
    worker: &mut Worker,
    root: &Path,
    operation: Operation,
    path: &str,
    line: u32,
    column: u32,
) -> NavigationResponse {
    worker
        .query(
            WorkerQuery {
                operation,
                path: root.join(path),
                position: Position { line, column },
                max_results: 100,
            },
            deadline(),
            CancellationToken::new(),
        )
        .await
}
fn put(root: &Path, name: &str, text: &str) {
    let target = root.join(name);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, text).unwrap();
}
fn simple(root: &Path) {
    put(root, "package.json", r#"{"private":true}"#);
    put(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"target":"ES2022","module":"ESNext","moduleResolution":"Bundler","types":[],"lib":["ES2022"]},"include":["**/*.ts"]}"#,
    );
    put(root, "a.ts", "export const value = 1;\n");
    put(root, "main.ts", "import {value} from './a';\nvalue;\n");
}
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let e = entry.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &target)
        } else {
            std::fs::copy(e.path(), target).unwrap();
        }
    }
}
fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/nas-lsp-typescript")
}
fn location(oracle: &Value, id: &str, shift: u32) -> Location {
    let at = &oracle["locations"][id];
    Location {
        path: at["path"].as_str().unwrap().into(),
        start: Position {
            line: at["line"].as_u64().unwrap() as u32 + shift,
            column: at["column"].as_u64().unwrap() as u32,
        },
        end: Position {
            line: at["line"].as_u64().unwrap() as u32 + shift,
            column: at["end_column"].as_u64().unwrap() as u32,
        },
    }
}

#[tokio::test]
#[ignore = "requires explicit pinned Node, TSLS and TypeScript environment"]
async fn real_corpus_cold_declaration_unopened_reverse_users_unicode_changes_deletes_and_roots() {
    let corpus = corpus();
    let oracle: Value =
        serde_json::from_slice(&std::fs::read(corpus.join("oracle.json")).unwrap()).unwrap();
    let container = tempfile::tempdir().unwrap();
    for shift in [0, 2] {
        let root = container.path().join(format!("root-{shift}"));
        copy_tree(&corpus.join("workspace"), &root);
        if shift != 0 {
            for name in ["consumer", "helper", "primary", "secondary", "transient"] {
                let p = root.join(format!("src/{name}.ts"));
                let t = std::fs::read_to_string(&p).unwrap();
                std::fs::write(p, format!("// isolated root\n\n{t}")).unwrap();
            }
        }
        let cold = Instant::now();
        let mut worker = start(&root).await;
        let mut pid = worker.process_id();
        let mut count = 0;
        for scenario in ["base", "changed", "deleted"] {
            let stage = &oracle["scenarios"][scenario];
            if let Some(replace) = stage["replace"].as_object() {
                for (target, source) in replace {
                    let text =
                        std::fs::read_to_string(corpus.join(source.as_str().unwrap())).unwrap();
                    put(
                        &root,
                        target,
                        &format!(
                            "{}{text}",
                            if shift == 0 {
                                ""
                            } else {
                                "// isolated root\n\n"
                            }
                        ),
                    );
                }
            }
            if let Some(delete) = stage["delete"].as_array() {
                for name in delete {
                    std::fs::remove_file(root.join(name.as_str().unwrap())).unwrap();
                }
            }
            for (index, probe) in oracle["queries"].as_array().unwrap().iter().enumerate() {
                let at = location(&oracle, probe["at"].as_str().unwrap(), shift);
                let op = if probe["operation"] == "references" {
                    Operation::References
                } else {
                    Operation::Definition
                };
                let mut expected = vec![];
                if let Some(symbol) = probe["symbol"].as_str() {
                    if op == Operation::Definition {
                        expected.push(location(
                            &oracle,
                            oracle["symbols"][symbol]["definition"].as_str().unwrap(),
                            shift,
                        ));
                    } else {
                        expected.extend(
                            stage["references"][symbol]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|id| location(&oracle, id.as_str().unwrap(), shift)),
                        );
                    }
                }
                expected.sort();
                let before = Instant::now();
                let result = query(
                    &mut worker,
                    &root,
                    op,
                    &at.path,
                    at.start.line,
                    at.start.column,
                )
                .await;
                assert_eq!(
                    result.status,
                    Status::Ok,
                    "root {shift} {scenario} {probe}: {result:?}"
                );
                assert_eq!(
                    result.locations, expected,
                    "root {shift} {scenario} {probe}"
                );
                assert!(!result.truncated);
                if index == 0 && scenario != "base" {
                    assert_ne!(worker.process_id(), pid);
                    pid = worker.process_id();
                } else {
                    assert_eq!(worker.process_id(), pid);
                }
                println!(
                    "TS exact root={shift} {scenario}/{} n={} query={:?} cold_elapsed={:?}",
                    probe["id"],
                    result.locations.len(),
                    before.elapsed(),
                    cold.elapsed()
                );
                count += 1;
            }
        }
        assert_eq!(count, 42);
        worker.shutdown().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit pinned Node, TSLS and TypeScript environment"]
async fn real_safety_rejects_before_server_spawn_and_never_runs_plugin() {
    let cases = [
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"plugins":[{"name":"hostile-plugin"}]}}"#,
        ),
        (
            "nested/tsconfig.json",
            r#"{"compilerOptions":{"plugins":[{"name":"hostile-plugin"}]}}"#,
        ),
        (
            "base.json",
            r#"{"compilerOptions":{"plugins":[{"name":"hostile-plugin"}]}}"#,
        ),
        ("tsconfig.json", r#"{"extends":"../outside.json"}"#),
        ("tsconfig.json", r#"{"references":[{"path":"./sub"}]}"#),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"typeRoots":["../outside"]}}"#,
        ),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"jsxImportSource":"evil"}}"#,
        ),
        ("tsconfig.json", r#"{"files":["../outside.ts"]}"#),
        ("tsconfig.json", r#"{"include":["../**/*.ts"]}"#),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"paths":{"x":["../outside"]}}}"#,
        ),
        (
            "main.ts",
            "/** @import {T} from '../outside' */\nexport const x=1;",
        ),
        (
            "main.ts",
            "/** @type {import('../outside').T} */\nexport const x=1;",
        ),
        (
            "main.tsx",
            "/** @jsxImportSource ../outside */\nexport const x = <div />;",
        ),
        (
            "main.tsx",
            "/** @jsxRuntime automatic */\nexport const x = <div />;",
        ),
        ("main.ts", "import '../outside';"),
        ("main.ts", "export {x} from '../outside';"),
        ("main.ts", "export * as ns from '../outside';"),
        ("main.ts", "export type * as ns from '../outside';"),
        ("main.ts", "import x = require('../outside');"),
        ("main.ts", "const x = require('../outside');"),
        ("main.ts", "import('../outside');"),
        ("main.ts", "const name = './a'; import(name);"),
        ("main.ts", "import 'https://example.test/module';"),
        ("main.ts", "import {broken from '../outside';"),
        (
            "main.ts",
            "export const x=1;\n//# sourceMappingURL=../outside.map",
        ),
        ("main.ts", "import 'unavailable-external-package';"),
        (
            "main.ts",
            "/// <reference path='../outside.d.ts' />\nexport const x=1;",
        ),
        (
            "main.ts",
            "/// <reference types='unavailable-package' />\nexport const x=1;",
        ),
        ("package.json", r#"{"main":"../outside.js"}"#),
        (
            "package.json",
            r#"{"exports":{".":{"types":"../outside.ts"}}}"#,
        ),
        (
            "package.json",
            r##"{"imports":{"#outside":"../outside.ts"}}"##,
        ),
        (
            "package.json",
            r#"{"typesVersions":{"*":{"*": ["../outside.ts"]}}}"#,
        ),
    ];
    for (index, (target, text)) in cases.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        simple(root);
        put(root, target, text);
        if *target == "main.tsx" {
            put(
                root,
                "tsconfig.json",
                r#"{"compilerOptions":{"jsx":"preserve","module":"ESNext","moduleResolution":"Bundler","types":[]},"include":["*.ts","*.tsx"]}"#,
            );
        }
        if *target == "base.json" {
            put(root, "tsconfig.json", r#"{"extends":"./base.json"}"#);
        }
        let sentinel = dir.path().join("plugin-executed");
        put(
            root,
            "node_modules/hostile-plugin/package.json",
            r#"{"main":"index.js"}"#,
        );
        put(
            root,
            "node_modules/hostile-plugin/index.js",
            &format!(
                "process.getBuiltinModule('fs').writeFileSync({},'executed'); module.exports=()=>({{create:x=>x.languageService}});",
                json!(sentinel)
            ),
        );
        // If the adapter ever starts the server this invalid CLI fails with a
        // different reason. This proves plugin/config rejection precedes spawn.
        let mut cfg = (*config(root)).clone();
        cfg.args = vec!["/this-must-never-start.mjs".into(), "--stdio".into()];
        let response = Worker::start(Arc::new(cfg), deadline(), CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(
            response.status,
            Status::Unavailable,
            "{index}: {response:?}"
        );
        assert_eq!(
            response.reason.as_deref(),
            Some("unsupported_typescript_project"),
            "{index}: {response:?}"
        );
        assert!(!sentinel.exists(), "plugin executed in case {index}");
    }
}

#[tokio::test]
#[ignore = "requires explicit pinned Node, TSLS and TypeScript environment"]
async fn real_alias_and_package_metadata_changes_deletion_and_negative_ancestor_guard() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    simple(&root);
    put(&root, "b.ts", "export const value = 2;\n");
    put(&root, "main.ts", "import {value} from '@local';\nvalue;\n");
    let cfg = |to: &str| {
        json!({"compilerOptions":{"target":"ES2022","module":"ESNext","moduleResolution":"Bundler","types":[],"lib":["ES2022"],"paths":{"@local":[to]}}}).to_string()
    };
    put(&root, "tsconfig.json", &cfg("./a.ts"));
    let mut worker = start(&root).await;
    for name in ["a", "b"] {
        put(&root, "tsconfig.json", &cfg(&format!("./{name}.ts")));
        let result = query(&mut worker, &root, Operation::Definition, "main.ts", 2, 1).await;
        assert_eq!(result.status, Status::Ok, "{result:?}");
        assert_eq!(result.locations[0].path, format!("{name}.ts"));
    }
    std::fs::remove_file(root.join("b.ts")).unwrap();
    let deleted = query(&mut worker, &root, Operation::Definition, "main.ts", 2, 1).await;
    assert_eq!(
        deleted.status,
        Status::Unavailable,
        "missing closure must not return empty OK: {deleted:?}"
    );
    worker.shutdown().await;
    simple(&root);
    put(
        &root,
        "main.ts",
        "import {value} from 'local-package';\nvalue;\n",
    );
    put(
        &root,
        "node_modules/local-package/package.json",
        r#"{"types":"a.d.ts"}"#,
    );
    put(
        &root,
        "node_modules/local-package/a.d.ts",
        "export declare const value: 1;\n",
    );
    put(
        &root,
        "node_modules/local-package/b.d.ts",
        "export declare const value: 2;\n",
    );
    put(&root, "tsconfig.json", &json!({"compilerOptions":{"target":"ES2022","module":"ESNext","moduleResolution":"Bundler","types":[],"lib":["ES2022"]},"files":["node_modules/local-package/a.d.ts","node_modules/local-package/b.d.ts"],"include":["*.ts"]}).to_string());
    let mut worker = start(&root).await;
    let pid = worker.process_id();
    let a = query(&mut worker, &root, Operation::Definition, "main.ts", 2, 1).await;
    assert_eq!(a.status, Status::Ok, "{a:?}");
    assert_eq!(a.locations[0].path, "node_modules/local-package/a.d.ts");
    put(
        &root,
        "node_modules/local-package/package.json",
        r#"{"types":"b.d.ts"}"#,
    );
    let b = query(&mut worker, &root, Operation::Definition, "main.ts", 2, 1).await;
    assert_eq!(b.status, Status::Ok, "{b:?}");
    assert_eq!(b.locations[0].path, "node_modules/local-package/b.d.ts");
    assert_ne!(worker.process_id(), pid);
    let saved_config = std::fs::read_to_string(root.join("tsconfig.json")).unwrap();
    std::fs::remove_file(root.join("tsconfig.json")).unwrap();
    let removed_config = query(&mut worker, &root, Operation::Definition, "main.ts", 2, 1).await;
    assert_eq!(
        removed_config.status,
        Status::Unavailable,
        "{removed_config:?}"
    );
    put(&root, "tsconfig.json", &saved_config);
    let recovered = query(&mut worker, &root, Operation::Definition, "main.ts", 2, 1).await;
    assert_eq!(recovered.status, Status::Ok, "{recovered:?}");
    let before_external = query(&mut worker, &root, Operation::References, "main.ts", 2, 1).await;
    assert_eq!(before_external.status, Status::Ok, "{before_external:?}");
    assert!(!before_external.locations.is_empty());
    let before_external_pid = worker.process_id();
    std::fs::create_dir(parent.path().join("node_modules")).unwrap();
    let external = query(&mut worker, &root, Operation::Definition, "main.ts", 2, 1).await;
    assert_eq!(external.status, Status::Unavailable, "{external:?}");
    assert!(external.locations.is_empty());
    assert!(worker.process_id().is_none());
    let still_external = query(&mut worker, &root, Operation::Definition, "main.ts", 2, 1).await;
    assert_eq!(
        still_external.status,
        Status::Unavailable,
        "{still_external:?}"
    );
    assert!(worker.process_id().is_none());
    std::fs::remove_dir(parent.path().join("node_modules")).unwrap();
    let recovered = query(&mut worker, &root, Operation::Definition, "main.ts", 2, 1).await;
    assert_eq!(recovered.status, Status::Ok, "{recovered:?}");
    assert_eq!(
        recovered.locations[0].path,
        "node_modules/local-package/b.d.ts"
    );
    assert_ne!(worker.process_id(), before_external_pid);
    let after_external = query(&mut worker, &root, Operation::References, "main.ts", 2, 1).await;
    assert_eq!(after_external.status, Status::Ok, "{after_external:?}");
    assert_eq!(after_external.locations, before_external.locations);
    worker.shutdown().await;
}

#[tokio::test]
#[ignore = "requires explicit pinned Node, TSLS and TypeScript environment"]
async fn real_js_tsx_declarations_and_local_config_inheritance() {
    let root = tempfile::tempdir().unwrap();
    simple(root.path());
    put(
        root.path(),
        "base.json",
        r#"{"compilerOptions":{"target":"ES2022","module":"ESNext","moduleResolution":"Bundler","allowJs":true,"checkJs":true,"jsx":"preserve","types":[],"lib":["ES2022"]}}"#,
    );
    put(
        root.path(),
        "tsconfig.json",
        r#"{"extends":"./base.json","include":["**/*"]}"#,
    );
    put(
        root.path(),
        "types.d.ts",
        "export declare const described: number;\n",
    );
    put(
        root.path(),
        "widget.tsx",
        "import {described} from './types';\nexport const widget = described;\n",
    );
    put(
        root.path(),
        "use.js",
        "import {widget} from './widget';\nwidget;\n",
    );
    put(
        root.path(),
        "barrel.ts",
        "export * as widgets from './widget';\nexport type * as declared from './types';\n",
    );
    put(
        root.path(),
        "namespace.ts",
        "import {widgets} from './barrel';\nwidgets.widget;\n",
    );
    let mut worker = start(root.path()).await;
    let js = query(
        &mut worker,
        root.path(),
        Operation::Definition,
        "use.js",
        2,
        1,
    )
    .await;
    assert_eq!(js.status, Status::Ok, "{js:?}");
    assert_eq!(js.locations[0].path, "widget.tsx");
    let namespace = query(
        &mut worker,
        root.path(),
        Operation::Definition,
        "namespace.ts",
        2,
        9,
    )
    .await;
    assert_eq!(namespace.status, Status::Ok, "{namespace:?}");
    assert_eq!(namespace.locations[0].path, "widget.tsx");
    let dts = query(
        &mut worker,
        root.path(),
        Operation::Definition,
        "widget.tsx",
        2,
        23,
    )
    .await;
    assert_eq!(dts.status, Status::Ok, "{dts:?}");
    assert_eq!(dts.locations[0].path, "types.d.ts");
    worker.shutdown().await;
}

// Process-real protocol mocks isolate adapter readiness from compiler/server
// semantics. The wrapper returns a qualified helper result, then speaks LSP.
const MOCK: &str = r#"
import json,sys,os,time
from pathlib import Path
if sys.argv[1:2]==['-e']:
    data=json.load(sys.stdin)
    if mode in ['helper-timeout','helper-cancel']:
        Path(pidfile).write_text(str(os.getpid()))
        if mode=='helper-cancel':
            import socket
            sock=socket.socket(socket.AF_UNIX);sock.connect(pidfile+'.sock');sock.close()
        time.sleep(30)
    if mode=='helper-overflow':
        sys.stdout.write('x'* (4*1024*1024+1));sys.exit(0)
    sources=[p for p in data['files'] if Path(p).suffix in ['.ts','.tsx','.js','.jsx','.mts','.cts','.mjs','.cjs']]
    print(json.dumps(dict(ok=True,sources=sources)));sys.exit(0)
def send(v):
    b=json.dumps(dict(jsonrpc='2.0',**v)).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(b)).encode()+b);sys.stdout.buffer.flush()
root=os.getcwd(); opened=[]
while True:
    headers={}
    while True:
        line=sys.stdin.buffer.readline()
        if not line:sys.exit(0)
        if line==b'\r\n':break
        k,v=line.decode().split(':',1);headers[k.lower()]=v.strip()
    m=json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    method=m.get('method')
    if method=='textDocument/didOpen':
        opened.append(m['params']['textDocument']);continue
    if method=='exit':sys.exit(0)
    if 'id' not in m:continue
    if method=='initialize':
        o=m['params']['initializationOptions'];t=o['tsserver']
        assert o['plugins']==[] and o['disableAutomaticTypingAcquisition']
        assert t['path']==t['fallbackPath'] and t['useSyntaxServer']=='never'
        assert not t['useClientFileWatcher'] and t['trace']=='off' and t['logVerbosity']=='off'
        send(dict(id=m['id'],result=dict(capabilities=dict(definitionProvider=True,referencesProvider=True,textDocumentSync=dict(openClose=True,change=1)))));continue
    if method=='workspace/executeCommand':
        p=m['params'];assert p['command']=='typescript.tsserverRequest'
        assert p['arguments'][0]=='projectInfo' and p['arguments'][1]['needFileNameList'] is True
        assert len(opened)==2
        files=[root+'/a.ts',root+'/main.ts',root+'/tsconfig.json']
        body=dict(configFileName=root+'/tsconfig.json',fileNames=files,languageServiceDisabled=False)
        result=dict(success=True,command='projectInfo',body=body)
        if mode=='missing-file':body['fileNames']=[root+'/tsconfig.json']
        if mode=='external-file':body['fileNames'].append('/outside/escape.ts')
        if mode=='bad-runtime':body['fileNames'].append(str(Path(runtime)/'not-a-lib.d.ts'))
        if mode=='inferred':body['configFileName']='/dev/null/inferredProject1*'
        if mode=='disabled':body['languageServiceDisabled']=True
        if mode=='missing-semantic':body.pop('languageServiceDisabled')
        if mode=='malformed':result={}
        if mode=='progress':send(dict(method='$/progress',params=dict(token='project',value=dict(kind='begin'))))
        if mode=='reject':send(dict(id=m['id'],error=dict(code=-32601,message='unsupported')));continue
        send(dict(id=m['id'],result=result));continue
    if method=='shutdown':send(dict(id=m['id'],result=None));continue
    if method=='textDocument/references':assert m['params']['context']['includeDeclaration']
    send(dict(id=m['id'],result=[]))
"#;

fn mock(root: &Path, runtime: &Path, mode: &str) -> Arc<WorkspaceConfig> {
    use std::os::unix::fs::PermissionsExt;
    let executable = runtime.join("mock-node");
    let program = format!(
        "#!/usr/bin/python3\nmode={}\npidfile={}\nruntime={}\n{}",
        json!(mode),
        json!(runtime.join("helper-pid")),
        json!(runtime),
        MOCK
    );
    std::fs::write(&executable, program).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(runtime.join("tsserver.js"), "").unwrap();
    std::fs::write(runtime.join("typescript.js"), "").unwrap();
    Arc::new(WorkspaceConfig {
        id: "mock-typescript".into(),
        root: root.to_path_buf(),
        language: "typescript".into(),
        executable,
        args: vec!["--stdio".into()],
        source_extensions: vec!["ts".into()],
        exclude_dirs: vec![],
        readiness: Readiness::TypeScriptProject,
        typescript_tsserver: Some(runtime.join("tsserver.js")),
        rustc: None,
    })
}

#[tokio::test]
async fn mocked_semantic_barrier_checks_every_document_and_rejects_incomplete_projects() {
    for (mode, status) in [
        ("ok", Status::Ok),
        ("missing-file", Status::Indexing),
        ("external-file", Status::Unavailable),
        ("bad-runtime", Status::Unavailable),
        ("inferred", Status::Unavailable),
        ("disabled", Status::Unavailable),
        ("missing-semantic", Status::Unavailable),
        ("malformed", Status::Unavailable),
        ("progress", Status::Indexing),
        ("reject", Status::Unavailable),
    ] {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        simple(root.path());
        let mut worker = Worker::start(
            mock(root.path(), runtime.path(), mode),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let result = query(
            &mut worker,
            root.path(),
            Operation::References,
            "a.ts",
            1,
            14,
        )
        .await;
        assert_eq!(result.status, status, "{mode}: {result:?}");
        assert!(result.locations.is_empty());
        worker.shutdown().await;
    }
}

#[tokio::test]
async fn mocked_helper_bounds_timeout_cancellation_missing_runtime_and_root_boundary() {
    for mode in ["helper-timeout", "helper-overflow"] {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        simple(root.path());
        let cfg = mock(root.path(), runtime.path(), mode);
        let response = Worker::start(
            cfg,
            Instant::now() + Duration::from_secs(2),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            response.status,
            if mode == "helper-timeout" {
                Status::Timeout
            } else {
                Status::Unavailable
            },
            "{response:?}"
        );
        if mode == "helper-timeout" {
            let pid = std::fs::read_to_string(runtime.path().join("helper-pid"))
                .unwrap()
                .parse::<i32>()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
    let root = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    simple(root.path());
    let cfg = mock(root.path(), runtime.path(), "ok");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let response = Worker::start(cfg.clone(), deadline(), cancel)
        .await
        .err()
        .unwrap();
    assert_eq!(response.status, Status::Cancelled);
    std::fs::remove_file(runtime.path().join("typescript.js")).unwrap();
    let response = Worker::start(cfg, deadline(), CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(response.status, Status::Unavailable);
    for missing in ["package.json", "tsconfig.json"] {
        let root = tempfile::tempdir().unwrap();
        simple(root.path());
        std::fs::remove_file(root.path().join(missing)).unwrap();
        let response = Worker::start(
            mock(root.path(), runtime.path(), "ok"),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            response.reason.as_deref(),
            Some("typescript_root_config_required")
        );
    }
}

#[tokio::test]
async fn mocked_active_compiler_helper_cancellation_kills_and_reaps() {
    let root = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    simple(root.path());
    let listener = tokio::net::UnixListener::bind(runtime.path().join("helper-pid.sock")).unwrap();
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let future = Worker::start(
        mock(root.path(), runtime.path(), "helper-cancel"),
        deadline(),
        cancel,
    );
    let (result, ()) = tokio::join!(future, async {
        tokio::time::timeout_at(deadline(), listener.accept())
            .await
            .unwrap()
            .unwrap();
        signal.cancel();
    });
    let response = result.err().unwrap();
    assert_eq!(response.status, Status::Cancelled);
    let pid = std::fs::read_to_string(runtime.path().join("helper-pid"))
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
