use super::ArtifactStore;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[test]
fn saves_atomically_with_a_sanitized_label() {
    let temp = TempDir::new().unwrap();
    let store = ArtifactStore {
        root: temp.path().join("thread"),
    };
    let uri = store
        .save("one\ntwo\nthree\n", "../../hostile/tool")
        .unwrap();

    assert!(uri.starts_with("artifact://"));
    let id = uri.strip_prefix("artifact://").unwrap();
    let path = temp.path().join(format!("thread/{id}.hostile_tool.log"));
    assert!(path.exists());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "one\ntwo\nthree\n");
}

#[test]
fn copies_only_regular_artifacts_for_forks() {
    let temp = TempDir::new().unwrap();
    let source = ArtifactStore {
        root: temp.path().join("source"),
    };
    let destination = ArtifactStore {
        root: temp.path().join("destination"),
    };
    let uri = source.save("fork content", "tool").unwrap();
    let id = uri.strip_prefix("artifact://").unwrap();

    destination.copy_from(&source).unwrap();

    let copied = destination.destination_path(id, "tool");
    assert_eq!(std::fs::read_to_string(copied).unwrap(), "fork content");
}

/// The placeholder names the artifact's on-disk path directly, with no
/// recovery tool to resolve a URI through, so the path must be usable as-is
/// by a shell tool regardless of how `CODEX_HOME` was spelled. `for_thread`
/// resolves an existing relative `codex_home` at construction, so the resulting
/// placeholder path must be absolute without creating the artifact directory.
#[test]
fn for_thread_canonicalizes_to_an_absolute_root() {
    let temp = TempDir::new().unwrap();
    let store = ArtifactStore::for_thread(temp.path(), "thread-1");
    let path = store.destination_path("00000000000000000000000000000000", "tool");
    assert!(path.is_absolute(), "expected an absolute path: {path:?}");
    assert!(!temp.path().join("artifacts/thread-1").exists());
}
