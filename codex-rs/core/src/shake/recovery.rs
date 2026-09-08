use uuid::Uuid;

pub(super) fn recovery_placeholder(
    save: &mut dyn FnMut(&str, &str) -> Option<String>,
    original: &str,
    tokens: usize,
    label: &str,
) -> Option<String> {
    let uri = save(original, label)?;
    Some(format!(
        "[shaken ~{tokens} tokens from {label} (recover: {uri})]"
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
    let Some((_, id)) = rest.rsplit_once(" (recover: artifact://") else {
        return false;
    };
    valid_artifact_id(id)
}

fn valid_artifact_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok()
}
