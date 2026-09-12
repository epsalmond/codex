use super::ArtifactStore;
use super::MAX_READ_BYTES;
use super::RecoveryReadBudget;
use super::recovery_read_budget;
use codex_utils_output_truncation::approx_tokens_from_byte_count;
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
        store
            .read(&uri, /*start_byte*/ None, MAX_READ_BYTES)
            .unwrap(),
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

    assert!(
        store
            .read("artifact://../0", /*start_byte*/ None, MAX_READ_BYTES)
            .is_err()
    );
    assert!(
        store
            .read(
                "artifact://0/other",
                /*start_byte*/ None,
                MAX_READ_BYTES
            )
            .is_err()
    );
    assert!(
        store
            .read(&uri, /*start_byte*/ None, MAX_READ_BYTES)
            .is_err()
    );
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
    let mut page = store
        .read(&uri, /*start_byte*/ None, MAX_READ_BYTES)
        .unwrap();
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
        page = store.read(&uri, Some(offset), MAX_READ_BYTES).unwrap();
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
        .read(&uri, Some((MAX_READ_BYTES + 1) as u64), MAX_READ_BYTES)
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
            MAX_READ_BYTES,
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
        destination
            .read(&uri, /*start_byte*/ None, MAX_READ_BYTES)
            .unwrap(),
        format!("fork content\n[artifact source: {uri}; final page]")
    );
}

#[test]
fn recovery_read_budget_uses_the_ceiling_when_headroom_is_ample() {
    for remaining in [None, Some(i64::MAX), Some(100_000)] {
        assert_eq!(
            recovery_read_budget(remaining).unwrap(),
            RecoveryReadBudget {
                max_bytes: MAX_READ_BYTES,
                clamped_by_remaining_tokens: None,
            },
            "{remaining:?}"
        );
    }
    // Exactly enough headroom for a full page still reports "not clamped":
    // 50% of 1_536 tokens is 768 tokens, which is MAX_READ_BYTES bytes.
    assert_eq!(
        recovery_read_budget(Some(1_536)).unwrap().max_bytes,
        MAX_READ_BYTES
    );
}

#[test]
fn recovery_read_budget_clamps_to_remaining_headroom() {
    // 50% of 400 tokens is 200 tokens => 800 bytes, under the 3 KiB ceiling
    // and over the 512-byte floor.
    let budget = recovery_read_budget(Some(400)).unwrap();
    assert_eq!(
        budget,
        RecoveryReadBudget {
            max_bytes: 800,
            clamped_by_remaining_tokens: Some(400),
        }
    );
    let notice = budget
        .truncation_notice()
        .expect("clamped page has a notice");
    assert!(notice.contains("800 bytes"), "{notice}");
    assert!(notice.contains("remaining"), "{notice}");
    assert!(notice.contains("start_byte"), "{notice}");
    // The unclamped case explains nothing, because nothing context-specific
    // happened.
    assert_eq!(
        recovery_read_budget(Some(100_000))
            .unwrap()
            .truncation_notice(),
        None
    );
}

#[test]
fn recovery_read_budget_refuses_when_headroom_is_below_the_floor() {
    // 50% of 200 tokens is 100 tokens => 400 bytes, under MIN_READ_BYTES.
    for remaining in [Some(0), Some(200), Some(-5)] {
        let error = recovery_read_budget(remaining)
            .expect_err("a sub-floor budget must be an actionable error");
        assert!(error.contains("not enough context"), "{error}");
        assert!(error.contains("/shake"), "{error}");
    }
}

#[test]
fn bounded_read_truncates_with_a_continuation_marker_and_fits_the_budget() {
    let temp = TempDir::new().unwrap();
    let store = ArtifactStore {
        root: temp.path().join("thread"),
    };
    let uri = store.save(&"z".repeat(MAX_READ_BYTES * 4), "tool").unwrap();

    let budget = recovery_read_budget(Some(400)).unwrap();
    let page = store
        .read(&uri, /*start_byte*/ None, budget.max_bytes)
        .unwrap();

    let body = page
        .split_once("\n[artifact source:")
        .map(|(body, _)| body)
        .expect("a truncated page carries a continuation marker");
    assert_eq!(body.len(), budget.max_bytes);
    assert!(
        page.contains(&format!(
            "[artifact source: {uri}; more content; use start_byte={}]",
            budget.max_bytes
        )),
        "{page}"
    );
    // The whole model-facing result, marker and clamp notice included, fits in
    // the headroom the budget was derived from.
    let notice = budget
        .truncation_notice()
        .expect("clamped page has a notice");
    let full = format!("{page}\n{notice}");
    assert!(
        approx_tokens_from_byte_count(full.len()) < 400,
        "recovered page is {} bytes, which does not fit ~400 tokens of headroom",
        full.len()
    );
}

#[test]
fn read_never_exceeds_the_ceiling_even_when_asked_for_more() {
    let temp = TempDir::new().unwrap();
    let store = ArtifactStore {
        root: temp.path().join("thread"),
    };
    let uri = store.save(&"y".repeat(MAX_READ_BYTES * 4), "tool").unwrap();

    let page = store.read(&uri, /*start_byte*/ None, usize::MAX).unwrap();

    let body = page.split_once("\n[artifact source:").unwrap().0;
    assert_eq!(body.len(), MAX_READ_BYTES);
}
