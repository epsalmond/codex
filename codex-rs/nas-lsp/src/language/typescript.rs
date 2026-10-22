//! Self-contained, saved-input TypeScript projects. See the language contract.

use crate::config::WorkspaceConfig;
use crate::freshness::Snapshot;
use crate::lsp::{ChildEnvironment, ReadinessPermit, RequestError, Session};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

const MAX_INPUT: usize = 64 * 1024 * 1024;
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;

pub(crate) struct Adapter {
    root: PathBuf,
    tsserver: PathBuf,
    files: BTreeSet<PathBuf>,
    sources: BTreeSet<PathBuf>,
    absent: Vec<PathBuf>,
}

impl Adapter {
    pub(super) async fn prepare(
        config: &WorkspaceConfig,
        snapshot: &Snapshot,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Self, RequestError> {
        check(deadline, &cancel)?;
        if !snapshot.files.contains_key(Path::new("package.json"))
            || !(snapshot.files.contains_key(Path::new("tsconfig.json"))
                || snapshot.files.contains_key(Path::new("jsconfig.json")))
        {
            return Err(RequestError::Failure("typescript_root_config_required"));
        }
        let tsserver = config
            .typescript_tsserver
            .clone()
            .ok_or(RequestError::Failure("tsserver_unavailable"))?;
        let compiler = tsserver
            .parent()
            .ok_or(RequestError::Failure("tsserver_unavailable"))?
            .join("typescript.js");
        if std::fs::canonicalize(&compiler).ok().as_ref() != Some(&compiler)
            || !compiler.is_file()
            || compiler.starts_with(&config.root)
        {
            return Err(RequestError::Failure("typescript_compiler_unavailable"));
        }
        // A root configuration and package boundary stop ancestor config/package
        // discovery. Module/type resolution still walks ancestor node_modules.
        let absent = config
            .root
            .ancestors()
            .skip(1)
            .map(|p| p.join("node_modules"))
            .collect();
        let mut adapter = Self {
            root: config.root.clone(),
            tsserver,
            files: snapshot.files.keys().cloned().collect(),
            sources: snapshot
                .files
                .keys()
                .filter(|p| source(p))
                .cloned()
                .collect(),
            absent,
        };
        adapter.validate_inputs(deadline, cancel.clone()).await?;
        // Declaration maps can redirect navigation through sourceRoot/sources
        // absent from projectInfo. This initial profile does not qualify maps.
        if snapshot.files.keys().any(|p| {
            p.to_str().is_some_and(|p| {
                p.ends_with(".d.ts.map") || p.ends_with(".d.mts.map") || p.ends_with(".d.cts.map")
            })
        }) {
            return Err(RequestError::Failure("unsupported_typescript_project"));
        }
        let mut files = serde_json::Map::new();
        for (path, contents) in &snapshot.files {
            check(deadline, &cancel)?;
            // Non-source binary assets cannot affect this profile's resolver.
            if source(path) || path.extension().is_some_and(|x| x == "json") {
                let text = std::str::from_utf8(contents)
                    .map_err(|_| RequestError::Failure("typescript_input_encoding"))?;
                files.insert(
                    path.to_str()
                        .ok_or(RequestError::Failure("typescript_input_path"))?
                        .into(),
                    json!(text),
                );
            }
        }
        let input = serde_json::to_vec(&json!({"root":config.root,"files":files}))
            .map_err(|_| RequestError::Failure("typescript_input"))?;
        if input.len() > MAX_INPUT {
            return Err(RequestError::Failure("typescript_helper_input_limit"));
        }
        let output = helper(config, &compiler, input, deadline, cancel.clone()).await?;
        let result: Value = serde_json::from_slice(&output)
            .map_err(|_| RequestError::Failure("typescript_helper_protocol"))?;
        if result["ok"] != true {
            // No source/config text or server-controlled string enters the wire.
            return Err(RequestError::Failure("unsupported_typescript_project"));
        }
        let qualified: BTreeSet<PathBuf> = result["sources"]
            .as_array()
            .ok_or(RequestError::Failure("typescript_helper_protocol"))?
            .iter()
            .map(|p| {
                p.as_str()
                    .map(PathBuf::from)
                    .ok_or(RequestError::Failure("typescript_helper_protocol"))
            })
            .collect::<Result<_, _>>()?;
        if qualified != adapter.sources || qualified.is_empty() {
            return Err(RequestError::Failure("unsupported_typescript_source"));
        }
        adapter.sources = qualified;
        adapter.validate_inputs(deadline, cancel).await?;
        Ok(adapter)
    }

    // Pinned TS 6.0.3 requestEnablePlugin rejects path/relative plugin names;
    // enablePluginsWithOptions adds workspace search paths only when the
    // allowLocalPluginLoads CLI flag is set. TSLS 6.0.2 never sets that flag;
    // empty global plugins also means no extra pluginProbeLocations. Still
    // reject every saved plugin config before spawn: empty init plugins alone
    // does not disable tsconfig-requested plugins in the pinned loader.
    pub(super) fn initialization_options(&self) -> Value {
        json!({
            "disableAutomaticTypingAcquisition":true, "plugins":[],
            "tsserver":{"path":self.tsserver,"fallbackPath":self.tsserver,
                "logVerbosity":"off","trace":"off","useSyntaxServer":"never",
                "useClientFileWatcher":false}
        })
    }

    pub(super) fn validate_query(&self, relative: &Path) -> Result<(), &'static str> {
        if self.sources.contains(relative) {
            Ok(())
        } else {
            Err("unsupported_typescript_source")
        }
    }

    pub(super) async fn validate_inputs(
        &self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<(), RequestError> {
        for candidate in &self.absent {
            check(deadline, &cancel)?;
            // symlink_metadata also rejects dangling symlinks and inaccessible
            // candidates; exists() would incorrectly treat those as absent.
            match std::fs::symlink_metadata(candidate) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(RequestError::Stale("typescript_external_lookup")),
            }
        }
        Ok(())
    }

    pub(super) async fn ready(
        &self,
        session: &mut Session,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<ReadinessPermit, RequestError> {
        // One request per admitted document also checks configured membership of
        // files outside include/exclude; opening an orphan is not qualification.
        for source in &self.sources {
            check(deadline, &cancel)?;
            let uri = url::Url::from_file_path(self.root.join(source))
                .map_err(|_| RequestError::Failure("typescript_input_path"))?;
            let result = session
                .request(
                    "workspace/executeCommand",
                    json!({
                        "command":"typescript.tsserverRequest",
                        "arguments":["projectInfo", {"file":uri.as_str(),"needFileNameList":true}]
                    }),
                    deadline,
                    cancel.clone(),
                )
                .await?;
            let body = &result["body"];
            if result["success"] != true
                || result["command"] != "projectInfo"
                || body["languageServiceDisabled"] != false
            {
                return Err(RequestError::Failure("typescript_project_unavailable"));
            }
            let configuration = body["configFileName"]
                .as_str()
                .ok_or(RequestError::Failure("typescript_project_unavailable"))?;
            if !self.saved_input(Path::new(configuration)) {
                return Err(RequestError::Failure("typescript_unconfigured_source"));
            }
            let members = body["fileNames"]
                .as_array()
                .ok_or(RequestError::Indexing("typescript_project_loading"))?;
            if members.len() > 4000 {
                return Err(RequestError::Failure("typescript_project_limit"));
            }
            let mut found = false;
            for member in members {
                let member = member
                    .as_str()
                    .ok_or(RequestError::Failure("typescript_project_unavailable"))?;
                let path = Path::new(member);
                if !self.saved_input(path) && !self.runtime_input(path) {
                    return Err(RequestError::Failure("typescript_external_input"));
                }
                found |= path == self.root.join(source);
            }
            if !found {
                return Err(RequestError::Indexing("typescript_project_loading"));
            }
        }
        session.navigation_permit(false)
    }

    fn saved_input(&self, path: &Path) -> bool {
        normalized_absolute(path)
            && path
                .strip_prefix(&self.root)
                .ok()
                .is_some_and(|relative| self.files.contains(relative))
    }

    fn runtime_input(&self, path: &Path) -> bool {
        // Only actual canonical pinned lib*.d.ts files are runtime inputs, not
        // arbitrary paths beneath the runtime or paths containing '..'.
        normalized_absolute(path)
            && self
                .tsserver
                .parent()
                .is_some_and(|lib| path.parent() == Some(lib))
            && path
                .file_name()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.starts_with("lib.") && x.ends_with(".d.ts"))
            && std::fs::canonicalize(path).ok().as_deref() == Some(path)
            && path.is_file()
    }
}

fn normalized_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|c| {
            matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
}
fn source(path: &Path) -> bool {
    path.extension().and_then(|x| x.to_str()).is_some_and(|x| {
        matches!(
            x,
            "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs"
        )
    })
}
fn check(deadline: Instant, cancel: &CancellationToken) -> Result<(), RequestError> {
    if cancel.is_cancelled() {
        Err(RequestError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(RequestError::Timeout)
    } else {
        Ok(())
    }
}

/// Retains both clean environment and group ownership across every exit. The
/// group is killed even after successful leader exit, then explicitly reaped.
async fn helper(
    config: &WorkspaceConfig,
    compiler: &Path,
    input: Vec<u8>,
    deadline: Instant,
    cancel: CancellationToken,
) -> Result<Vec<u8>, RequestError> {
    let environment = ChildEnvironment::new(config).map_err(RequestError::Failure)?;
    let mut command = Command::new(&config.executable);
    environment.apply(&mut command);
    command
        .args(["-e", HELPER])
        .arg(compiler)
        .current_dir(&config.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|_| RequestError::Failure("typescript_helper_unavailable"))?;
    struct Group(Option<u32>);
    impl Drop for Group {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
    let group = Group(child.id());
    let work = async {
        let mut stdin = child
            .stdin
            .take()
            .ok_or(RequestError::Failure("typescript_helper_io"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(RequestError::Failure("typescript_helper_io"))?;
        let write = async {
            stdin
                .write_all(&input)
                .await
                .map_err(|_| RequestError::Failure("typescript_helper_io"))?;
            stdin
                .shutdown()
                .await
                .map_err(|_| RequestError::Failure("typescript_helper_io"))?;
            drop(stdin);
            Ok::<_, RequestError>(())
        };
        let read = async {
            let mut output = Vec::new();
            stdout
                .take(MAX_OUTPUT + 1)
                .read_to_end(&mut output)
                .await
                .map_err(|_| RequestError::Failure("typescript_helper_io"))?;
            if output.len() as u64 > MAX_OUTPUT {
                return Err(RequestError::Failure("typescript_helper_output_limit"));
            }
            Ok(output)
        };
        let (_, output) = tokio::try_join!(write, read)?;
        let status = child
            .wait()
            .await
            .map_err(|_| RequestError::Failure("typescript_helper_io"))?;
        if !status.success() {
            return Err(RequestError::Failure("typescript_helper_failed"));
        }
        Ok(output)
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(RequestError::Cancelled),
        result = timeout_at(deadline, work) => result.unwrap_or(Err(RequestError::Timeout)),
    };
    drop(group);
    let _ = child.start_kill();
    let _ = child.wait().await;
    drop(environment);
    result
}

// This helper never uses ts.sys or a repository module. All compiler filesystem
// callbacks read the saved map. The only require target is the registered,
// canonical compiler sibling; JSONC is parsed as data by that compiler.
const HELPER: &str = r#"
'use strict';
const ts = require(process.argv[1]);
const path = require('node:path').posix;
let chunks=[], count=0;
process.stdin.on('data', b => { count+=b.length; if(count>67108864) process.exit(2); chunks.push(b); });
process.stdin.on('end', () => {
  try {
    const input=JSON.parse(Buffer.concat(chunks).toString('utf8'));
    const root=input.root, files=new Map(Object.entries(input.files).map(([p,t])=>[path.join(root,p),t]));
    const fail=reason=>{throw new Error(reason)};
    const inside=p=>(p===root||p.startsWith(root+'/')) && !path.relative(root,p).split('/').includes('.git');
    const local=(base,p)=>{
      if(typeof p!=='string' || !p || /[\\\0]/.test(p) || /^[a-z][a-z0-9+.-]*:/i.test(p)) fail('path');
      const result=path.resolve(base,p); if(!inside(result)) fail('escape'); return result;
    };
    const source=p=>/\.(?:[cm]?[tj]s|[tj]sx)$/.test(p);
    const dirs=new Set([root]);
    for(const p of files.keys()) { let d=path.dirname(p); while(inside(d)) {dirs.add(d);d=path.dirname(d);} }
    const host={useCaseSensitiveFileNames:true, fileExists:p=>files.has(p), readFile:p=>files.get(p),
      directoryExists:p=>dirs.has(p), getCurrentDirectory:()=>root, realpath:p=>p,
      readDirectory:(p,extensions,excludes,includes,depth)=>ts.matchFiles(p,extensions,excludes,includes,true,root,depth,
        d=>({files:[...files.keys()].filter(x=>path.dirname(x)===d).map(x=>path.basename(x)),
             directories:[...dirs].filter(x=>x!==d && path.dirname(x)===d).map(x=>path.basename(x))}),x=>x)};
    const configs=new Map();
    const safeOptions=new Set(('target module moduleResolution strict strictNullChecks strictFunctionTypes strictBindCallApply strictPropertyInitialization noImplicitAny noImplicitThis alwaysStrict useUnknownInCatchVariables noUnusedLocals noUnusedParameters noImplicitReturns noFallthroughCasesInSwitch noUncheckedIndexedAccess noImplicitOverride exactOptionalPropertyTypes allowUnreachableCode allowUnusedLabels skipLibCheck skipDefaultLibCheck noEmit declaration declarationMap sourceMap inlineSourceMap inlineSources removeComments preserveConstEnums isolatedModules verbatimModuleSyntax esModuleInterop allowSyntheticDefaultImports forceConsistentCasingInFileNames allowJs checkJs jsx jsxFactory jsxFragmentFactory experimentalDecorators emitDecoratorMetadata types lib noLib baseUrl paths rootDir rootDirs typeRoots resolveJsonModule resolvePackageJsonExports resolvePackageJsonImports allowImportingTsExtensions moduleDetection noResolve plugins ignoreDeprecations').split(' '));
    function config(p,stack=new Set()) {
      if(configs.has(p)) return configs.get(p);
      if(stack.has(p)||!files.has(p)) fail('config inheritance'); stack=new Set(stack).add(p);
      const parsed=ts.parseConfigFileTextToJson(p,files.get(p)); if(parsed.error || !parsed.config || typeof parsed.config!=='object') fail('config json');
      const c=parsed.config;
      for(const k of Object.keys(c)) if(!['compilerOptions','extends','files','include','exclude','references','typeAcquisition','compileOnSave','$schema','watchOptions'].includes(k)) fail('config field');
      if(c.references && (!Array.isArray(c.references)||c.references.length)) fail('project references');
      if(c.compileOnSave===true || (c.typeAcquisition && (c.typeAcquisition.enable!==false || Object.keys(c.typeAcquisition).some(k=>k!=='enable')))) fail('acquisition');
      // Watch options can name external paths and change discovery: no overrides.
      if(c.watchOptions && Object.keys(c.watchOptions).length) fail('watch options');
      const o=c.compilerOptions||{}; if(typeof o!=='object' || Array.isArray(o)) fail('options');
      for(const k of Object.keys(o)) if(!safeOptions.has(k)) fail('unsupported option '+k);
      if(o.plugins && (!Array.isArray(o.plugins)||o.plugins.length)) fail('plugins');
      if(o.jsxFactory && !/^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*$/.test(o.jsxFactory)) fail('jsx factory');
      for(const k of ['baseUrl','rootDir']) if(o[k]!==undefined) local(path.dirname(p),o[k]);
      for(const k of ['rootDirs','typeRoots']) if(o[k]!==undefined) {
        if(!Array.isArray(o[k])) fail(k); for(const v of o[k]) local(path.dirname(p),v);
      }
      if(o.paths) { if(typeof o.paths!=='object'||Array.isArray(o.paths)) fail('paths');
        for(const [k,values] of Object.entries(o.paths)) {
          if(k.includes('\\') || k.includes(':') || !Array.isArray(values)) fail('paths');
          for(const v of values) local(o.baseUrl ? local(path.dirname(p),o.baseUrl):path.dirname(p),v);
        }
      }
      for(const k of ['files','include','exclude']) if(c[k]!==undefined) {
        if(!Array.isArray(c[k])) fail(k); for(const v of c[k]) {
          const q=local(path.dirname(p),v); if(k==='files'&&!files.has(q)) fail('missing file');
        }
      }
      if(o.types!==undefined && (!Array.isArray(o.types)||o.types.some(t=>typeof t!=='string'||t.startsWith('.')||t.includes(':')||t.startsWith('/')))) fail('types');
      if(o.lib!==undefined && (!Array.isArray(o.lib)||o.lib.some(l=>!ts.libMap.has(String(l).toLowerCase())))) fail('lib');
      if(c.extends!==undefined) {
        const values=Array.isArray(c.extends)?c.extends:[c.extends];
        for(const v of values) {
          if(typeof v!=='string'||!v.startsWith('.')) fail('extends');
          let q=local(path.dirname(p),v); if(!files.has(q)&&files.has(q+'.json'))q+='.json'; config(q,stack);
        }
      }
      configs.set(p,c); return c;
    }
    // Reject plugin entries in every discoverable config, including nested
    // projects and recursively inherited JSON files which are not named tsconfig.
    for(const p of files.keys()) if(/\/(?:tsconfig|jsconfig)(?:\.[^/]*)?\.json$/.test(p))config(p);
    for(const [p,text] of files) if(path.basename(p)==='package.json') {
      const c=JSON.parse(text); if(!c||typeof c!=='object'||Array.isArray(c))fail('package');
      for(const k of ['main','types','typings','module']) if(c[k]!==undefined)local(path.dirname(p),c[k]);
      // Conditional/pattern metadata and package imports can select unmodelled
      // contexts. Simple exports are qualified; complex mappings fail explicitly.
      if(c.imports!==undefined||c.typesVersions!==undefined)fail('package mappings');
      if(c.exports!==undefined) {
        if(typeof c.exports!=='string'||!c.exports.startsWith('./'))fail('exports');
        local(path.dirname(p),c.exports);
      }
      if(c.browser!==undefined || c.workspaces!==undefined) fail('package resolution');
    }
    const parsed=new Map();
    for(const [p,c] of configs) {
      const result=ts.parseJsonConfigFileContent(c,host,path.dirname(p),undefined,p);
      // Compiler option errors are not safe to ignore; no-input config can be a
      // shared inheritance fragment. Source eligibility is checked separately.
      if(result.errors.some(e=>e.code!==18003))fail('compiler config');
      parsed.set(p,result);
    }
    const sources=[...files.keys()].filter(source);
    for(const p of sources) {
      let cfg;
      for(let d=path.dirname(p);inside(d);d=path.dirname(d)) {
        cfg=['tsconfig.json','jsconfig.json'].map(n=>path.join(d,n)).find(q=>files.has(q));
        if(cfg)break;
      }
      if(!cfg || !parsed.has(cfg))fail('source config');
      const model=parsed.get(cfg), options=model.options;
      // Excluded sources opened in inferred projects cannot establish complete
      // reverse references. Configured dependencies are resolved below.
      const text=files.get(p), pp=ts.preProcessFile(text,true,true);
      const imports=new Set(pp.importedFiles.map(x=>x.fileName));
      if(/\.[tj]sx$/.test(p) && (options.jsx===ts.JsxEmit.ReactJSX||options.jsx===ts.JsxEmit.ReactJSXDev))
        imports.add(options.jsx===ts.JsxEmit.ReactJSX?'react/jsx-runtime':'react/jsx-dev-runtime');
      const sf=ts.createSourceFile(p,text,ts.ScriptTarget.Latest,true);
      if(sf.parseDiagnostics.length)fail('source syntax');
      function visit(n) {
        if(ts.isCallExpression(n) && (n.expression.kind===ts.SyntaxKind.ImportKeyword ||
            (ts.isIdentifier(n.expression)&&n.expression.text==='require'))) {
          if(n.arguments.length!==1||!ts.isStringLiteralLike(n.arguments[0]))fail('dynamic module');
          imports.add(n.arguments[0].text);
        }
        if(ts.isImportTypeNode(n)) {
          if(!ts.isLiteralTypeNode(n.argument)||!ts.isStringLiteralLike(n.argument.literal))fail('import type');
          imports.add(n.argument.literal.text);
        }
        ts.forEachChild(n,visit);
      }
      visit(sf);
      // The pinned scanner identifies comment tokens, so strings containing
      // import spellings remain harmless. Conservatively reject JSDoc import
      // forms rather than letting preProcessFile silently omit their inputs.
      const scanner=ts.createScanner(ts.ScriptTarget.Latest,false,ts.LanguageVariant.Standard,text);
      for(let token=scanner.scan();token!==ts.SyntaxKind.EndOfFileToken;token=scanner.scan()) {
        if((token===ts.SyntaxKind.MultiLineCommentTrivia||token===ts.SyntaxKind.SingleLineCommentTrivia) &&
            /@import\b|\bimport\s*\(|[#@]\s*sourceMappingURL\s*=/.test(scanner.getTokenText()))fail('unsupported jsdoc import');
      }
      for(const spec of imports) {
        if(!spec||/[\\\0]/.test(spec)||/^[a-z][a-z0-9+.-]*:/i.test(spec)||spec.startsWith('#'))fail('module syntax');
        if(spec.startsWith('.')||spec.startsWith('/'))local(path.dirname(p),spec);
        const resolution=ts.resolveModuleName(spec,p,options,host).resolvedModule;
        if(!resolution || !files.has(resolution.resolvedFileName)||!inside(resolution.resolvedFileName))fail('unresolved module '+spec);
      }
      for(const ref of pp.referencedFiles) {const q=local(path.dirname(p),ref.fileName);if(!files.has(q))fail('reference');}
      for(const ref of pp.typeReferenceDirectives) {
        if(ref.fileName.startsWith('.')||ref.fileName.startsWith('/'))local(path.dirname(p),ref.fileName);
        const r=ts.resolveTypeReferenceDirective(ref.fileName,p,options,host).resolvedTypeReferenceDirective;
        if(!r||!files.has(r.resolvedFileName)||!inside(r.resolvedFileName))fail('type reference');
      }
      for(const ref of pp.libReferenceDirectives)if(!ts.libMap.has(ref.fileName.toLowerCase()))fail('lib reference');
      for(const t of options.types||[]) {
        const r=ts.resolveTypeReferenceDirective(t,p,options,host).resolvedTypeReferenceDirective;
        if(!r||!files.has(r.resolvedFileName)||!inside(r.resolvedFileName))fail('types');
      }
    }
    process.stdout.write(JSON.stringify({ok:true,sources:sources.map(p=>path.relative(root,p))}));
  } catch(e) { process.stdout.write(JSON.stringify({ok:false,reason:String(e.message).slice(0,160)})); }
});
"#;
