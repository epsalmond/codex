/// `save` returns the absolute on-disk path of the file it wrote the elided
/// region to, so the placeholder can name it for a shell tool (`grep`,
/// `awk`, …). There is no tool to recover an artifact through: see
/// `docs/shake.md` for why.
pub(super) fn recovery_placeholder(
    save: &mut dyn FnMut(&str, &str) -> Option<String>,
    original: &str,
    tokens: usize,
    label: &str,
) -> Option<String> {
    let abs_path = save(original, label)?;
    Some(format!(
        "[shaken ~{tokens} tokens from {label}. original: {abs_path}]"
    ))
}

pub(super) fn is_artifact_recovery_output(text: &str) -> bool {
    text.lines().any(is_shaken_artifact_marker)
}

fn is_shaken_artifact_marker(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("[shaken ~") else {
        return false;
    };
    let Some((_, rest)) = rest.split_once(" tokens from ") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(']') else {
        return false;
    };
    let Some((_label, abs_path)) = rest.split_once(". original: ") else {
        return false;
    };
    !abs_path.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_carries_the_original_file_path() {
        let mut save = |_content: &str, _label: &str| {
            Some(
                "/home/user/.codex/artifacts/thread-1/00000000000000000000000000000000.tool_output.log"
                    .to_string(),
            )
        };
        let placeholder = recovery_placeholder(&mut save, "original content", 123, "tool output")
            .expect("save succeeded, so a placeholder must be produced");

        assert_eq!(
            placeholder,
            "[shaken ~123 tokens from tool output. original: \
             /home/user/.codex/artifacts/thread-1/00000000000000000000000000000000.tool_output.log]"
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
