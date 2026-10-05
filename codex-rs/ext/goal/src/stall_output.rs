//! Trusted unified-exec renderer adapter. Stdout remains an opaque user value.

use serde_json::Map;
use serde_json::Value;

pub(super) fn terminal_text(text: &str) -> Option<Value> {
    let (header, stdout) = text.split_once("\nOutput:\n")?;
    let mut lines = header.lines().peekable();
    let mut result = Map::new();
    if let Some(chunk) = lines.peek()?.strip_prefix("Chunk ID: ") {
        result.insert("chunk_id".to_owned(), Value::String(chunk.to_owned()));
        lines.next();
    }
    let elapsed = lines
        .next()?
        .strip_prefix("Wall time: ")?
        .strip_suffix(" seconds")?
        .parse::<f64>()
        .ok()?;
    if !elapsed.is_finite() || elapsed < 0.0 {
        return None;
    }
    result.insert("wall_time_seconds".to_owned(), Value::from(elapsed));
    if let Some(exit) = lines
        .peek()
        .and_then(|line| line.strip_prefix("Process exited with code "))
    {
        result.insert(
            "exit_code".to_owned(),
            Value::from(exit.parse::<i32>().ok()?),
        );
        lines.next();
    }
    if let Some(session) = lines
        .peek()
        .and_then(|line| line.strip_prefix("Process running with session ID "))
    {
        result.insert(
            "session_id".to_owned(),
            Value::from(session.parse::<i32>().ok()?),
        );
        lines.next();
    }
    if let Some(tokens) = lines
        .peek()
        .and_then(|line| line.strip_prefix("Original token count: "))
    {
        result.insert(
            "original_token_count".to_owned(),
            Value::from(tokens.parse::<u64>().ok()?),
        );
        lines.next();
    }
    if lines.next().is_some() {
        return None;
    }
    result.insert("output".to_owned(), Value::String(stdout.to_owned()));
    Some(Value::Object(result))
}
