//! Resolves helper binaries that tests need from another package in this Cargo workspace.
//!
//! Cargo only exports `CARGO_BIN_EXE_<name>` for binaries of the package under test, so a test
//! that needs another package's binary falls back to the uplifted `target/<profile>/<name>`.
//! `cargo test -p <package>` does not build that file, and when several checkouts share one
//! `CARGO_TARGET_DIR` it belongs to whichever checkout built it last. These helpers fail loudly
//! in both cases instead of letting a test silently skip or run another checkout's binary.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::SystemTime;

use crate::cargo_bin;
use crate::cargo_bin_env_keys;

static RESOLVED: OnceLock<Mutex<HashMap<String, Result<PathBuf, String>>>> = OnceLock::new();

/// Returns the path to binary `name` from workspace package `package`, or panics with the
/// exact build command when it is missing or was not built from this checkout's sources.
///
/// An explicit `CARGO_BIN_EXE_<name>` (set by Cargo for the package under test, by Bazel, or by
/// the caller) is trusted as-is. Otherwise the uplifted binary must have a Cargo dep-info file
/// (`<name>.d`) whose workspace sources live in this checkout and are not newer than the binary.
pub fn workspace_bin(name: &str, package: &str) -> PathBuf {
    let resolved = RESOLVED.get_or_init(Mutex::default);
    let mut resolved = match resolved.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let result = resolved
        .entry(name.to_owned())
        .or_insert_with(|| resolve_workspace_bin(name, package))
        .clone();
    drop(resolved);
    match result {
        Ok(path) => path,
        Err(message) => panic!("{message}"),
    }
}

fn resolve_workspace_bin(name: &str, package: &str) -> Result<PathBuf, String> {
    let build_command = format!("cargo build -p {package} --bin {name}");
    let explicit = cargo_bin_env_keys(name)
        .iter()
        .any(|key| std::env::var_os(key).is_some());
    let path = cargo_bin(name).map_err(|err| {
        let err = err.to_string();
        let err = err.trim_end();
        format!(
            "test helper binary `{name}` is not available: {err}. Build it from codex-rs/ with \
             the same CARGO_TARGET_DIR before running these tests: `{build_command}`"
        )
    })?;
    if explicit {
        return Ok(path);
    }
    check_built_from_workspace(&path, name, workspace_root()).map_err(|reason| {
        format!(
            "test helper binary {} is stale: {reason}. Rebuild it from this checkout's codex-rs/ \
             with the same CARGO_TARGET_DIR: `{build_command}` (or set CARGO_BIN_EXE_{name} to a \
             binary you trust)",
            path.display()
        )
    })?;
    Ok(path)
}

fn workspace_root() -> &'static Path {
    // This crate lives at <workspace>/utils/cargo-bin.
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.ancestors().nth(2).unwrap_or(manifest_dir)
}

/// Uses the dep-info file Cargo writes next to every uplifted binary to verify that the binary
/// was built from `workspace` and that none of its workspace sources changed afterwards. A
/// missing dep-info file is not an error, because only Cargo writes one.
fn check_built_from_workspace(bin: &Path, name: &str, workspace: &Path) -> Result<(), String> {
    let Some(dep_info_path) = bin.parent().map(|dir| dir.join(format!("{name}.d"))) else {
        return Ok(());
    };
    let Ok(dep_info) = fs::read_to_string(&dep_info_path) else {
        return Ok(());
    };
    let deps = parse_dep_info(&dep_info);
    let canonical_workspace = workspace.canonicalize().ok();
    let in_workspace = |dep: &Path| {
        dep.starts_with(workspace)
            || canonical_workspace
                .as_deref()
                .is_some_and(|canonical| dep.starts_with(canonical))
    };
    let workspace_deps: Vec<&PathBuf> = deps.iter().filter(|dep| in_workspace(dep)).collect();
    if workspace_deps.is_empty() {
        // Prefer a source path over a build-script output inside the target dir.
        let target_dir = bin.parent().unwrap_or(bin);
        let Some(foreign) = deps
            .iter()
            .filter(|dep| dep.is_absolute())
            .min_by_key(|dep| dep.starts_with(target_dir))
        else {
            return Ok(());
        };
        return Err(format!(
            "{} lists no sources under {}, so it was built from another checkout (for example \
             {})",
            dep_info_path.display(),
            workspace.display(),
            foreign.display()
        ));
    }

    let built_at =
        modified(bin).ok_or_else(|| format!("cannot read mtime of {}", bin.display()))?;
    for dep in workspace_deps {
        match modified(dep) {
            Some(changed_at) if changed_at <= built_at => {}
            Some(_) => return Err(format!("{} changed after it was built", dep.display())),
            None => return Err(format!("its source {} no longer exists", dep.display())),
        }
    }
    Ok(())
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// Parses the first rule of a Makefile-style dep-info file (`target: dep dep ...`), where Cargo
/// escapes spaces inside paths as `\ `.
fn parse_dep_info(contents: &str) -> Vec<PathBuf> {
    let Some(line) = contents.lines().next() else {
        return Vec::new();
    };
    let Some((_target, deps)) = line.split_once(": ") else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut chars = deps.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            ' ' => {
                if !current.is_empty() {
                    paths.push(PathBuf::from(std::mem::take(&mut current)));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        paths.push(PathBuf::from(current));
    }
    paths
}
