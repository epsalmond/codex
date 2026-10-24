//! A deliberately bounded, non-executing Rust project model.
//! Custom macro definitions and deeply nested syntax are unsupported; ordinary
//! builtin macros remain usable after an iterative token audit.

use crate::config::WorkspaceConfig;
use crate::freshness::Snapshot;
use crate::lsp::{ReadinessPermit, RequestError, Session};
use proc_macro2::{TokenStream, TokenTree};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use syn::visit::Visit;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

pub(crate) struct Adapter {
    options: Value,
    sources: Vec<(String, String)>,
    reachable: BTreeSet<PathBuf>,
    // RA searches ancestor directories for these configurations, even for an
    // inline project. Their absence is a semantic input, including on warm use.
    absent_configs: Vec<PathBuf>,
    text_barrier_done: AtomicBool,
    text_barrier_started: AtomicBool,
}

type Result<T> = std::result::Result<T, &'static str>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Project {
    crates: Vec<Crate>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Crate {
    root_module: String,
    edition: String,
    deps: Vec<Dependency>,
    display_name: Option<String>,
    #[serde(default)]
    cfg: Vec<String>,
    is_workspace_member: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Dependency {
    #[serde(rename = "crate")]
    index: usize,
    name: String,
}

impl Adapter {
    pub(super) async fn prepare(
        config: &WorkspaceConfig,
        snapshot: &Snapshot,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> std::result::Result<Self, RequestError> {
        let root = config.root.clone();
        let files = snapshot.files.clone();
        let mut task = tokio::task::spawn_blocking(move || Self::parse(root, files, deadline));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(RequestError::Cancelled),
            result = timeout_at(deadline, &mut task) => {
                result.map_err(|_| RequestError::Timeout)?
                    .map_err(|_| RequestError::Failure("unsupported_rust_project"))?
                    .map_err(RequestError::Failure)
            }
        }
    }

    fn parse(
        root: PathBuf,
        files: BTreeMap<PathBuf, Arc<Vec<u8>>>,
        deadline: Instant,
    ) -> Result<Self> {
        let mut absent_configs = Vec::new();
        for directory in root.ancestors() {
            for name in ["rust-analyzer.toml", ".rust-analyzer.toml"] {
                let path = directory.join(name);
                if path.try_exists().map_err(|_| "unsupported_rust_config")?
                    || std::fs::symlink_metadata(&path).is_ok()
                {
                    return Err("unsupported_rust_config");
                }
                absent_configs.push(path);
            }
        }
        if files.keys().any(|p| {
            matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some("rust-analyzer.toml" | ".rust-analyzer.toml")
            )
        }) {
            return Err("unsupported_rust_config");
        }
        let crates = if let Some(bytes) = files.get(Path::new("rust-project.json")) {
            let project: Project =
                serde_json::from_slice(bytes).map_err(|_| "unsupported_rust_project")?;
            project.crates
        } else {
            cargo_project(&files)?
        };
        if crates.is_empty() || crates.len() > 128 {
            return Err("unsupported_rust_project");
        }
        let mut parsed = BTreeMap::new();
        let mut sources = Vec::new();
        for (path, bytes) in &files {
            if Instant::now() >= deadline {
                return Err("prepare_timeout");
            }
            if path.extension().and_then(|p| p.to_str()) != Some("rs") {
                continue;
            }
            let text = std::str::from_utf8(bytes).map_err(|_| "unsupported_source_encoding")?;
            let tokens: TokenStream = text.parse().map_err(|_| "unsupported_rust_source")?;
            let token_modules = guard_tokens(tokens.clone())?;
            let file: syn::File = syn::parse2(tokens).map_err(|_| "unsupported_rust_source")?;
            // File-level cfg can hide the entire module closure. This profile
            // only models cfg on ordinary items and exact cfg(test) modules.
            if file.attrs.iter().any(|a| path_is(a.path(), "cfg")) {
                return Err("unsupported_rust_source");
            }
            let mut audit = Audit {
                invalid: false,
                modules: 0,
            };
            audit.visit_file(&file);
            if audit.invalid {
                return Err("unsupported_rust_source");
            }
            let modules = modules(&file.items)?;
            if count_modules(&modules) != audit.modules || token_modules != audit.modules {
                return Err("unsupported_rust_source");
            }
            parsed.insert(path.clone(), modules);
            let uri =
                url::Url::from_file_path(root.join(path)).map_err(|_| "unsupported_rust_source")?;
            sources.push((uri.into(), text.to_owned()));
        }
        let mut reachable = BTreeSet::new();
        let mut linked = Vec::new();
        let mut roots = BTreeSet::new();
        for (index, krate) in crates.iter().enumerate() {
            let path = local_path(&krate.root_module)?;
            if !roots.insert(path.clone())
                || !parsed.contains_key(&path)
                || !matches!(krate.edition.as_str(), "2015" | "2018" | "2021" | "2024")
                || !krate.is_workspace_member
                || krate.cfg.len() > 128
                || krate.deps.len() > 128
                || path.file_name().is_some_and(|n| n == "build.rs")
                || krate.cfg.iter().any(|c| !valid_cfg(c))
                || krate.display_name.as_ref().is_some_and(|n| !identifier(n))
            {
                return Err("unsupported_rust_project");
            }
            let mut names = BTreeSet::new();
            for dep in &krate.deps {
                if dep.index >= crates.len()
                    || dep.index == index
                    || !identifier(&dep.name)
                    || !names.insert(&dep.name)
                {
                    return Err("unsupported_rust_project");
                }
            }
            // Reject dependency cycles; they are not valid Rust crate graphs.
            check_cycle(index, &crates, &mut BTreeSet::new(), &mut BTreeSet::new())?;
            let test = krate.cfg.iter().any(|c| c == "test");
            let directory = path.parent().ok_or("unsupported_rust_source")?;
            let mut seen = BTreeSet::new();
            walk_file(&path, directory, test, &parsed, &mut seen, &mut reachable)?;
            linked.push(json!({
                "root_module": root.join(path), "edition": krate.edition,
                "deps": krate.deps.iter().map(|d| json!({"crate":d.index,"name":d.name})).collect::<Vec<_>>(),
                "display_name": krate.display_name, "cfg": krate.cfg,
                "is_workspace_member": true
            }));
        }
        // Autoreload only re-admits this nonempty, validated inline graph.
        // On the pinned server, disabling it leaves the startup fetch queue
        // pending forever and reports unhealthy status; no Cargo manifest or
        // discovery-command project is supplied to the reload path.
        Ok(Self {
            options: json!({
                "linkedProjects": [{"crates":linked}],
                "cargo": {"sysroot":null,"buildScripts":{"enable":false,"rebuildOnSave":false},"autoreload":true},
                "cfg":{"setTest":false},
                "procMacro":{"enable":false}, "checkOnSave":false,"check":{"enable":false},
                "cachePriming":{"enable":false}, "numThreads":2,
                "diagnostics":{"enable":false}, "files":{"watcher":"client"}
            }),
            sources,
            reachable,
            absent_configs,
            text_barrier_done: AtomicBool::new(false),
            text_barrier_started: AtomicBool::new(false),
        })
    }
    pub(super) fn initialization_options(&self) -> Value {
        self.options.clone()
    }
    pub(super) fn validate_query(&self, relative: &Path) -> Result<()> {
        if self.reachable.contains(relative)
            && relative.file_name().is_some_and(|n| n != "build.rs")
        {
            Ok(())
        } else {
            Err("unsupported_rust_source")
        }
    }
    pub(super) async fn validate_inputs(
        &self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> std::result::Result<(), RequestError> {
        let paths = self.absent_configs.clone();
        let mut task = tokio::task::spawn_blocking(move || {
            for path in paths {
                match std::fs::symlink_metadata(path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err(RequestError::Stale("rust_config_changed")),
                }
            }
            Ok(())
        });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(RequestError::Cancelled),
            result = timeout_at(deadline, &mut task) => result.map_err(|_| RequestError::Timeout)?.map_err(|_| RequestError::Failure("rust_config_unreadable"))?,
        }
    }
    pub(super) async fn ready(
        &self,
        session: &mut Session,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> std::result::Result<ReadinessPermit, RequestError> {
        session.wait_rust_ready(deadline, cancel.clone()).await?;
        if !self.text_barrier_done.load(Ordering::Acquire) {
            if self.text_barrier_started.swap(true, Ordering::AcqRel) {
                return Err(RequestError::Indexing("rust_text_not_ready"));
            }
            for (uri, text) in &self.sources {
                let admitted = session
                    .request(
                        "rust-analyzer/viewFileText",
                        json!({"uri":uri}),
                        deadline,
                        cancel.clone(),
                    )
                    .await?;
                if admitted.as_str() != Some(text) {
                    return Err(RequestError::Indexing("rust_text_not_ready"));
                }
            }
            self.text_barrier_done.store(true, Ordering::Release);
        }
        session.wait_rust_ready(deadline, cancel).await
    }
}

fn identifier(name: &str) -> bool {
    !name.is_empty()
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn valid_cfg(cfg: &str) -> bool {
    // The selected graph may supply ordinary flags or key="value" pairs only.
    if identifier(cfg) {
        return true;
    }
    let Some((key, value)) = cfg.split_once('=') else {
        return false;
    };
    identifier(key)
        && value.len() >= 2
        && value.starts_with('"')
        && value.ends_with('"')
        && value[1..value.len() - 1]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
fn local_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|p| !matches!(p, Component::Normal(n) if n != ".git"))
        || path.extension().and_then(|e| e.to_str()) != Some("rs")
    {
        return Err("unsupported_rust_project");
    }
    Ok(path.to_owned())
}
fn check_cycle(
    index: usize,
    crates: &[Crate],
    active: &mut BTreeSet<usize>,
    done: &mut BTreeSet<usize>,
) -> Result<()> {
    if done.contains(&index) {
        return Ok(());
    }
    if !active.insert(index) {
        return Err("unsupported_rust_project");
    }
    for dep in &crates[index].deps {
        if dep.index >= crates.len() {
            return Err("unsupported_rust_project");
        }
        check_cycle(dep.index, crates, active, done)?;
    }
    active.remove(&index);
    done.insert(index);
    Ok(())
}

fn cargo_project(files: &BTreeMap<PathBuf, Arc<Vec<u8>>>) -> Result<Vec<Crate>> {
    let bytes = files
        .get(Path::new("Cargo.toml"))
        .ok_or("unsupported_cargo_project")?;
    let manifest: toml::Value =
        toml::from_str(std::str::from_utf8(bytes).map_err(|_| "unsupported_cargo_project")?)
            .map_err(|_| "unsupported_cargo_project")?;
    let table = manifest.as_table().ok_or("unsupported_cargo_project")?;
    for (key, value) in table {
        match key.as_str() {
            "package" | "lib" | "bin" => {}
            "dependencies" | "dev-dependencies" | "build-dependencies"
                if value.as_table().is_some_and(|v| v.is_empty()) => {}
            _ => return Err("unsupported_cargo_project"),
        }
    }
    let package = table
        .get("package")
        .and_then(|v| v.as_table())
        .ok_or("unsupported_cargo_project")?;
    let name = package
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("unsupported_cargo_project")?
        .replace('-', "_");
    let edition = package
        .get("edition")
        .map(|v| v.as_str().ok_or("unsupported_cargo_project"))
        .transpose()?
        .unwrap_or("2015");
    if !identifier(&name) || !matches!(edition, "2015" | "2018" | "2021" | "2024") {
        return Err("unsupported_cargo_project");
    }
    for (key, value) in package {
        if value.is_table() || matches!(key.as_str(), "workspace" | "links" | "default-run") {
            return Err("unsupported_cargo_project");
        }
        if matches!(
            key.as_str(),
            "autobins" | "autotests" | "autoexamples" | "autobenches" | "autolib"
        ) && !value.is_bool()
        {
            return Err("unsupported_cargo_project");
        }
    }
    let make = |path: String, name: String, deps: Vec<Dependency>| Crate {
        root_module: path,
        edition: edition.into(),
        deps,
        display_name: Some(name),
        cfg: vec!["test".into()],
        is_workspace_member: true,
    };
    let mut crates = Vec::new();
    let mut lib_name = name.clone();
    if let Some(lib) = table.get("lib") {
        let (path, target_name) = cargo_target(lib, "src/lib.rs", &name, true)?;
        lib_name = target_name.clone();
        crates.push(make(path, target_name, vec![]));
    } else if files.contains_key(Path::new("src/lib.rs"))
        && package.get("autolib").and_then(|v| v.as_bool()) != Some(false)
    {
        crates.push(make("src/lib.rs".into(), name.clone(), vec![]));
    }
    let has_lib = !crates.is_empty();
    let mut bins = Vec::new();
    if let Some(targets) = table.get("bin") {
        for target in targets.as_array().ok_or("unsupported_cargo_project")? {
            bins.push(cargo_target(target, "src/main.rs", &name, false)?);
        }
    }
    if files.contains_key(Path::new("src/main.rs"))
        && package.get("autobins").and_then(|v| v.as_bool()) != Some(false)
        && !bins.iter().any(|(p, _)| p == "src/main.rs")
    {
        bins.push(("src/main.rs".into(), name.clone()));
    }
    // Auto integration tests/examples/benches and src/bin need separate cfg and
    // dependency contexts. This profile rejects them instead of losing users.
    if files.keys().any(|p| {
        p.extension().and_then(|e| e.to_str()) == Some("rs")
            && (p.starts_with("tests")
                || p.starts_with("examples")
                || p.starts_with("benches")
                || (p.starts_with("src/bin") && !bins.iter().any(|(b, _)| Path::new(b) == p)))
    }) {
        return Err("unsupported_cargo_project");
    }
    for (path, name) in bins {
        let deps = if has_lib {
            vec![Dependency {
                index: 0,
                name: lib_name.clone(),
            }]
        } else {
            vec![]
        };
        crates.push(make(path, name, deps));
    }
    if crates.is_empty()
        || crates
            .iter()
            .any(|c| !files.contains_key(Path::new(&c.root_module)))
    {
        return Err("unsupported_cargo_project");
    }
    Ok(crates)
}
fn cargo_target(
    value: &toml::Value,
    default: &str,
    name: &str,
    library: bool,
) -> Result<(String, String)> {
    let table = value.as_table().ok_or("unsupported_cargo_project")?;
    for (key, value) in table {
        match key.as_str() {
            "name" | "path" if value.is_str() => {}
            "proc-macro" if library && value.as_bool() == Some(false) => {}
            "crate-type"
                if library
                    && value
                        .as_array()
                        .is_some_and(|a| a.len() == 1 && a[0].as_str() == Some("lib")) => {}
            // These select target contexts that the inline graph cannot model.
            _ => return Err("unsupported_cargo_project"),
        }
    }
    // Named auto binaries use several Cargo filesystem conventions. Only
    // the package main has an unambiguous default in this bounded profile.
    if !library
        && !table.contains_key("path")
        && table
            .get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|n| n.replace('-', "_") != name)
    {
        return Err("unsupported_cargo_project");
    }
    let path = table
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(default);
    local_path(path).map_err(|_| "unsupported_cargo_project")?;
    let name = table
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(name)
        .replace('-', "_");
    if !identifier(&name) {
        return Err("unsupported_cargo_project");
    }
    Ok((path.into(), name))
}

/// Bound syn's recursive parser before constructing any AST. Tokenization and
/// this traversal are iterative; strings/comments remain opaque tokens.
fn guard_tokens(tokens: TokenStream) -> Result<usize> {
    let mut stack = vec![(tokens, 0, false, false)];
    let mut total = 0;
    let mut modules = 0;
    while let Some((tokens, depth, in_macro, in_attribute)) = stack.pop() {
        if depth > 32 {
            return Err("unsupported_source_syntax");
        }
        let mut tokens = tokens.into_iter().peekable();
        let mut previous_bang = false;
        let mut previous_hash = false;
        let mut run = 0;
        while let Some(token) = tokens.next() {
            total += 1;
            run += 1;
            if total > 65_536 || run > 64 {
                return Err("unsupported_source_syntax");
            }
            match &token {
                TokenTree::Ident(ident) => {
                    let name = ident.to_string();
                    let name = name.trim_start_matches("r#");
                    if name == "mod" {
                        modules += 1;
                    }
                    if (in_macro && forbidden(name))
                        || (in_attribute && matches!(name, "path" | "cfg_attr"))
                        || (matches!(
                            name,
                            "include"
                                | "include_str"
                                | "include_bytes"
                                | "env"
                                | "option_env"
                                | "macro_rules"
                        ) && matches!(tokens.peek(), Some(TokenTree::Punct(p)) if p.as_char() == '!'))
                    {
                        return Err("unsupported_rust_source");
                    }
                }
                TokenTree::Group(group) => {
                    stack.push((
                        group.stream(),
                        depth + 1,
                        in_macro || previous_bang,
                        in_attribute || previous_hash,
                    ));
                    if group.delimiter() == proc_macro2::Delimiter::Brace {
                        run = 0;
                    }
                }
                TokenTree::Punct(p) if matches!(p.as_char(), ';' | ',') => run = 0,
                _ => {}
            }
            previous_bang = matches!(&token, TokenTree::Punct(p) if p.as_char() == '!');
            previous_hash = matches!(&token, TokenTree::Punct(p) if p.as_char() == '#');
        }
    }
    Ok(modules)
}

struct Audit {
    invalid: bool,
    modules: usize,
}
impl<'ast> Visit<'ast> for Audit {
    // Directive/macro scanning is iterative over tokens. Expressions do not
    // contribute modules to the supported closure, so avoid recursively
    // visiting arbitrarily long expression trees. The token module count
    // rejects declarations hidden in expression blocks.
    fn visit_expr(&mut self, _: &'ast syn::Expr) {}
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attr.path().segments.iter().any(|p| {
            matches!(
                p.ident.to_string().trim_start_matches("r#"),
                "path" | "cfg_attr"
            )
        }) {
            self.invalid = true;
        }
        if let syn::Meta::List(list) = &attr.meta {
            self.invalid |= unsafe_tokens(list.tokens.clone());
        }
        syn::visit::visit_attribute(self, attr);
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.invalid |= mac
            .path
            .segments
            .iter()
            .any(|p| forbidden(&p.ident.to_string()));
        self.invalid |= unsafe_tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.modules += 1;
        syn::visit::visit_item_mod(self, item);
    }
}
fn forbidden(ident: &str) -> bool {
    matches!(
        ident.trim_start_matches("r#"),
        "include"
            | "include_str"
            | "include_bytes"
            | "env"
            | "option_env"
            | "path"
            | "cfg_attr"
            | "mod"
    )
}
fn unsafe_tokens(tokens: TokenStream) -> bool {
    tokens.into_iter().any(|t| match t {
        TokenTree::Ident(i) => forbidden(&i.to_string()),
        TokenTree::Group(g) => unsafe_tokens(g.stream()),
        _ => false,
    })
}
// syn's is_ident compares the raw spelling. Rust treats r#cfg/r#test as
// their ordinary identifiers, so closure selection must normalize them too.
fn path_is(path: &syn::Path, expected: &str) -> bool {
    path.leading_colon.is_none()
        && path.segments.len() == 1
        && path.segments.first().is_some_and(|segment| {
            matches!(&segment.arguments, syn::PathArguments::None)
                && segment.ident.to_string().trim_start_matches("r#") == expected
        })
}

struct Module {
    name: String,
    inline: Option<Vec<Module>>,
    test: bool,
}
fn modules(items: &[syn::Item]) -> Result<Vec<Module>> {
    items
        .iter()
        .filter_map(|i| {
            if let syn::Item::Mod(m) = i {
                Some(m)
            } else {
                None
            }
        })
        .map(|m| {
            let mut test = false;
            {
                for attr in &m.attrs {
                    if path_is(attr.path(), "cfg") {
                        if test
                            || !attr
                                .parse_args::<syn::Path>()
                                .is_ok_and(|p| path_is(&p, "test"))
                        {
                            return Err("unsupported_rust_source");
                        }
                        test = true;
                    }
                }
            }
            Ok(Module {
                name: m.ident.to_string().trim_start_matches("r#").into(),
                inline: m
                    .content
                    .as_ref()
                    .map(|(_, items)| modules(items))
                    .transpose()?,
                test,
            })
        })
        .collect()
}
fn count_modules(modules: &[Module]) -> usize {
    modules
        .iter()
        .map(|m| 1 + m.inline.as_ref().map_or(0, |m| count_modules(m)))
        .sum()
}
fn walk_file(
    path: &Path,
    directory: &Path,
    test: bool,
    parsed: &BTreeMap<PathBuf, Vec<Module>>,
    seen: &mut BTreeSet<PathBuf>,
    reachable: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if !seen.insert(path.to_owned()) {
        return Err("unsupported_rust_source");
    }
    reachable.insert(path.to_owned());
    walk_modules(
        parsed.get(path).ok_or("unsupported_rust_source")?,
        directory,
        test,
        parsed,
        seen,
        reachable,
    )
}
fn walk_modules(
    modules: &[Module],
    directory: &Path,
    test: bool,
    parsed: &BTreeMap<PathBuf, Vec<Module>>,
    seen: &mut BTreeSet<PathBuf>,
    reachable: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if directory.components().count() > 128 {
        return Err("unsupported_rust_source");
    }
    for module in modules {
        let child = directory.join(&module.name);
        if module.test && !test {
            return Err("unsupported_rust_source");
        }
        if let Some(inline) = &module.inline {
            walk_modules(inline, &child, test, parsed, seen, reachable)?;
        } else {
            if module.test && !test {
                return Err("unsupported_rust_source");
            }
            let a = directory.join(format!("{}.rs", module.name));
            let b = child.join("mod.rs");
            let path = match (parsed.contains_key(&a), parsed.contains_key(&b)) {
                (true, false) => a,
                (false, true) => b,
                _ => return Err("unsupported_rust_source"),
            };
            walk_file(&path, &child, test, parsed, seen, reachable)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(inputs: &[(&str, &str)]) -> Result<Adapter> {
        let root = tempfile::tempdir().unwrap();
        let mut files: BTreeMap<_, _> = inputs
            .iter()
            .map(|(p, t)| (PathBuf::from(p), Arc::new(t.as_bytes().to_vec())))
            .collect();
        files.entry("Cargo.toml".into()).or_insert_with(|| {
            Arc::new(b"[package]\nname='sample'\nedition='2021'\nversion='0.1.0'\n".to_vec())
        });
        Adapter::parse(
            root.path().into(),
            files,
            Instant::now() + std::time::Duration::from_secs(10),
        )
    }
    #[test]
    fn cargo_lib_bin_nested_modules_and_orphans() {
        let adapter = parse(&[
            ("src/lib.rs", "mod outer { pub mod nested; }"),
            ("src/outer/nested.rs", "pub fn f() {}"),
            ("src/main.rs", "fn main() { sample::outer::nested::f(); }"),
            ("orphan.rs", "fn orphan() {}"),
            ("build.rs", "fn main() {}"),
        ])
        .unwrap();
        assert!(
            adapter
                .validate_query(Path::new("src/outer/nested.rs"))
                .is_ok()
        );
        for p in ["orphan.rs", "build.rs"] {
            assert_eq!(
                adapter.validate_query(Path::new(p)),
                Err("unsupported_rust_source")
            );
        }
        let graph = adapter.initialization_options();
        assert_eq!(
            graph["linkedProjects"][0]["crates"][1]["deps"],
            json!([{"crate":0,"name":"sample"}])
        );
        assert!(graph["cargo"]["sysroot"].is_null());
        assert_eq!(graph["cargo"]["autoreload"], true);
        assert_eq!(graph["cfg"]["setTest"], false);
    }
    #[test]
    fn deeply_nested_and_flat_adversarial_syntax_is_bounded() {
        let deep = format!(
            "fn f() {{ let _ = {}0{}; }}",
            "(".repeat(1024),
            ")".repeat(1024)
        );
        assert!(matches!(
            parse(&[("src/lib.rs", &deep)]),
            Err("unsupported_source_syntax")
        ));
        let unary = format!("fn f() {{ let _ = {}true; }}", "!".repeat(4096));
        assert!(matches!(
            parse(&[("src/lib.rs", &unary)]),
            Err("unsupported_source_syntax")
        ));
    }
    #[test]
    fn rust_directives_raw_and_nested_macro_tokens_fail_closed() {
        for text in [
            "#[path=\"../../outside.rs\"] mod x;",
            "#[r#path=\".git/x.rs\"] mod x;",
            "#[cfg_attr(test,path=\"external.rs\")] mod x;",
            "const X:&str=include_str!(\"outside\");",
            "const X:&str=env!(\"HOME\");",
            "m!({other!(r#include!(\"outside\"))});",
            "macro_rules! bad { () => { mod x; } }",
            "m!(#[r#path=\"outside\"] fn f(){});",
            "m!({r#cfg_attr(test,path=\"outside\")});",
            "mod missing;",
            "#[cfg(any())] mod disabled;",
            "fn f() { mod nested; }",
            "macro_rules! harmless { () => { 1 } }",
            "#[cfg(any())] mod inactive { mod nested; }",
        ] {
            assert!(parse(&[("src/lib.rs", text)]).is_err(), "{text}");
        }
        assert!(
            parse(&[(
                "src/lib.rs",
                "// include!(\"outside\")\n const TEXT:&str=\"env! mod path cfg_attr\";"
            )])
            .is_ok()
        );
        assert!(
            parse(&[
                ("src/lib.rs", "mod x;"),
                ("src/x.rs", ""),
                ("src/x/mod.rs", "")
            ])
            .is_err()
        );
    }
    #[test]
    fn cargo_unsupported_semantics_and_auto_targets_are_explicit() {
        for addition in [
            "[workspace]",
            "[features]\ndefault=[]",
            "[dependencies]\nserde='1'",
            "[target.'cfg(unix)'.dependencies]\nx='1'",
            "[[test]]\nname='unit'",
            "[lib]\nproc-macro=true",
            "[[bin]]\nrequired-features=['a']",
        ] {
            let cargo = format!("[package]\nname='sample'\nversion='0.1.0'\n{addition}\n");
            assert!(
                matches!(
                    parse(&[("Cargo.toml", &cargo), ("src/lib.rs", "")]),
                    Err("unsupported_cargo_project")
                ),
                "{addition}"
            );
        }
        for target in [
            "src/bin/tool.rs",
            "tests/test.rs",
            "examples/demo.rs",
            "benches/bench.rs",
        ] {
            assert!(matches!(
                parse(&[("src/lib.rs", ""), (target, "fn main() {}")]),
                Err("unsupported_cargo_project")
            ));
        }
    }
    #[test]
    fn raw_cfg_identifiers_follow_the_same_module_closure_rules() {
        for source in [
            "#[r#cfg(any())] mod disabled;",
            "#![r#cfg(any())] mod disabled;",
        ] {
            assert!(
                matches!(
                    parse(&[
                        ("src/lib.rs", source),
                        ("src/disabled.rs", "pub fn hidden() {}")
                    ]),
                    Err("unsupported_rust_source")
                ),
                "{source}"
            );
        }
        assert!(
            parse(&[
                ("src/lib.rs", "#[r#cfg(r#test)] mod tests;"),
                ("src/tests.rs", "pub fn test_user() {}")
            ])
            .unwrap()
            .validate_query(Path::new("src/tests.rs"))
            .is_ok()
        );
        let no_test = json!({"crates":[{"root_module":"src/lib.rs","edition":"2021","deps":[],"cfg":[],"is_workspace_member":true}]});
        for cfg in ["#[r#cfg(test)]", "#[cfg(r#test)]", "#[r#cfg(r#test)]"] {
            assert!(
                parse(&[
                    ("rust-project.json", &no_test.to_string()),
                    ("src/lib.rs", &format!("{cfg} mod tests;")),
                    ("src/tests.rs", "pub fn test_user() {}")
                ])
                .is_err()
            );
        }
    }
    #[test]
    fn explicit_graph_schema_cfg_and_local_closure() {
        let graph = json!({"crates":[{"root_module":"src/lib.rs","edition":"2021","deps":[],"cfg":["test"],"is_workspace_member":true}]});
        assert!(
            parse(&[
                ("rust-project.json", &graph.to_string()),
                ("src/lib.rs", "#[cfg(test)] mod tests;"),
                ("src/tests.rs", "")
            ])
            .is_ok()
        );
        for (key, value) in [
            ("env", json!({"RUSTC_WRAPPER":"attack"})),
            ("proc_macro_dylib_path", json!("attack.so")),
            ("runnables", json!([])),
            ("target", json!("external")),
            ("sysroot", json!("/outside")),
        ] {
            let mut bad = graph.clone();
            bad["crates"][0][key] = value;
            assert!(
                parse(&[("rust-project.json", &bad.to_string()), ("src/lib.rs", "")]).is_err(),
                "{key}"
            );
        }
        for path in ["../outside.rs", "/tmp/outside.rs", ".git/input.rs"] {
            let mut bad = graph.clone();
            bad["crates"][0]["root_module"] = json!(path);
            assert!(parse(&[("rust-project.json", &bad.to_string()), ("src/lib.rs", "")]).is_err());
        }
        let mut no_test = graph;
        no_test["crates"][0]["cfg"] = json!([]);
        assert!(
            parse(&[
                ("rust-project.json", &no_test.to_string()),
                ("src/lib.rs", "#[cfg(test)] mod tests;"),
                ("src/tests.rs", "")
            ])
            .is_err()
        );
    }
}
