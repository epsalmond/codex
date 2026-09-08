use super::ArtifactStore;
use super::MAX_READ_BYTES;
use pretty_assertions::assert_eq;
use std::fs;
use tempfile::TempDir;

#[test]
fn saves_atomically_and_reads_bounded_ranges() {
    let temp = TempDir::new().unwrap();
    let store = ArtifactStore {
        root: temp.path().join("thread"),
    };
    let uri = store
        .save("one\ntwo\nthree\n", "../../hostile/tool")
        .unwrap();

    assert!(uri.starts_with("artifact://"));
    assert_eq!(
        store.read(&uri, /*start_byte*/ None).unwrap(),
        format!("one\ntwo\nthree\n\n[artifact source: {uri}; final page]")
    );
    let id = uri.strip_prefix("artifact://").unwrap();
    assert!(
        temp.path()
            .join(format!("thread/{id}.hostile_tool.log"))
            .exists()
    );
}

#[test]
fn rejects_cross_thread_and_traversal_uris() {
    let temp = TempDir::new().unwrap();
    let source = ArtifactStore {
        root: temp.path().join("thread"),
    };
    let store = ArtifactStore {
        root: temp.path().join("other-thread"),
    };
    let uri = source.save("secret", "tool").unwrap();

    assert!(store.read("artifact://../0", /*start_byte*/ None).is_err());
    assert!(
        store
            .read("artifact://0/other", /*start_byte*/ None)
            .is_err()
    );
    assert!(store.read(&uri, /*start_byte*/ None).is_err());
}

#[test]
fn byte_reads_are_bounded_and_recoverable_at_utf8_boundaries() {
    let temp = TempDir::new().unwrap();
    let store = ArtifactStore {
        root: temp.path().join("thread"),
    };
    let mut content = "a".repeat(MAX_READ_BYTES - 1);
    content.push_str("é🙂 mixed\n");
    content.push_str(&"αβγ✨ mixed text\n".repeat(/*n*/ 2_000));
    let uri = store.save(&content, "utf8").unwrap();
    let mut page = store.read(&uri, /*start_byte*/ None).unwrap();
    let mut recovered = String::new();
    let mut page_count = 0;
    loop {
        page_count += 1;
        let (body, marker) = page
            .rsplit_once("\n[artifact source: ")
            .expect("every page should have a source marker");
        assert!(body.len() <= MAX_READ_BYTES);
        recovered.push_str(body);
        let marker = marker
            .strip_suffix(']')
            .expect("source marker should end with ]");
        assert!(marker.starts_with(&format!("{uri}; ")));
        if marker == format!("{uri}; final page") {
            break;
        }
        let offset = marker
            .strip_prefix(&format!("{uri}; more content; use start_byte="))
            .expect("continuation marker should contain the next offset")
            .parse::<u64>()
            .unwrap();
        page = store.read(&uri, Some(offset)).unwrap();
    }
    assert!(page_count > 1);
    assert_eq!(recovered, content);
}

#[test]
fn rejects_mid_character_offsets_on_large_utf8_artifacts() {
    let temp = TempDir::new().unwrap();
    let store = ArtifactStore {
        root: temp.path().join("thread"),
    };
    let content = format!(
        "{}é{}",
        "a".repeat(MAX_READ_BYTES),
        "b".repeat(MAX_READ_BYTES)
    );
    let uri = store.save(&content, "utf8").unwrap();

    let error = store
        .read(&uri, Some((MAX_READ_BYTES + 1) as u64))
        .unwrap_err();
    assert!(error.contains("character boundary"));
}

#[test]
fn rejects_corrupt_utf8_artifacts() {
    let temp = TempDir::new().unwrap();
    let store = ArtifactStore {
        root: temp.path().join("thread"),
    };
    fs::create_dir_all(&store.root).unwrap();
    fs::write(
        store.root.join("8c1f5d8d9d2d4e2f9f84a9d1d5cce0f1.tool.log"),
        [0xff, 0xfe],
    )
    .unwrap();

    let error = store
        .read(
            "artifact://8c1f5d8d9d2d4e2f9f84a9d1d5cce0f1",
            /*start_byte*/ None,
        )
        .unwrap_err();
    assert!(error.contains("invalid UTF-8"));
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

    destination.copy_from(&source).unwrap();

    assert_eq!(
        destination.read(&uri, /*start_byte*/ None).unwrap(),
        format!("fork content\n[artifact source: {uri}; final page]")
    );
}
