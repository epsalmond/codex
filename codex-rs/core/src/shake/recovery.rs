use uuid::Uuid;

/// `save` returns the artifact's `artifact://<id>` URI and the absolute
/// on-disk path of the file it was written to, so the placeholder can carry
/// both: the URI for `read_artifact`, and the path so the model can reach the
/// same content with a shell tool (`grep`, `awk`, …) when it only needs part
/// of it.
pub(super) fn recovery_placeholder(
    save: &mut dyn FnMut(&str, &str) -> Option<(String, String)>,
    original: &str,
    tokens: usize,
    label: &str,
) -> Option<String> {
    let (uri, abs_path) = save(original, label)?;
    Some(format!(
        "[shaken ~{tokens} tokens from {label} (recover: {uri}; file: {abs_path})]"
    ))
}

pub(super) fn is_artifact_recovery_output(text: &str) -> bool {
    text.lines()
        .any(|line| is_artifact_source_marker(line) || is_shaken_artifact_marker(line))
}

fn is_artifact_source_marker(line: &str) -> bool {
    let Some(rest) = line
        .strip_prefix("[artifact source: artifact://")
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    let Some((id, suffix)) = rest.split_once("; ") else {
        return false;
    };
    valid_artifact_id(id)
        && (suffix == "final page"
            || suffix
                .strip_prefix("more content; use start_byte=")
                .is_some_and(|offset| {
                    !offset.is_empty() && offset.bytes().all(|byte| byte.is_ascii_digit())
                }))
}

fn is_shaken_artifact_marker(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("[shaken ~") else {
        return false;
    };
    let Some((_, rest)) = rest.split_once(" tokens ") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(")]") else {
        return false;
    };
    // The URI is followed by "; file: <abs_path>"; split off the path from the
    // right so a path that happens to contain "; file: " itself (unlikely for
    // a real filesystem path) doesn't defeat the match.
    let Some((rest, _abs_path)) = rest.rsplit_once("; file: ") else {
        return false;
    };
    let Some((_, id)) = rest.rsplit_once(" (recover: artifact://") else {
        return false;
    };
    valid_artifact_id(id)
}

fn valid_artifact_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_carries_both_the_uri_and_the_abs_path() {
        let mut save = |_content: &str, _label: &str| {
            Some((
                "artifact://00000000000000000000000000000000".to_string(),
                "/home/user/.codex/artifacts/thread-1/00000000000000000000000000000000.tool_output.log"
                    .to_string(),
            ))
        };
        let placeholder = recovery_placeholder(&mut save, "original content", 123, "tool output")
            .expect("save succeeded, so a placeholder must be produced");

        assert_eq!(
            placeholder,
            "[shaken ~123 tokens from tool output (recover: artifact://00000000000000000000000000000000; \
             file: /home/user/.codex/artifacts/thread-1/00000000000000000000000000000000.tool_output.log)]"
        );
        assert!(
            is_shaken_artifact_marker(&placeholder),
            "the marker guard must still recognize a placeholder with a file path: {placeholder}"
        );
    }

    #[test]
    fn placeholder_is_none_when_save_fails() {
        let mut save = |_content: &str, _label: &str| None;
        assert_eq!(
            recovery_placeholder(&mut save, "original content", 123, "tool output"),
            None
        );
    }
}
