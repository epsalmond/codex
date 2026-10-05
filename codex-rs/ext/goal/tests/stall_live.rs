#![allow(dead_code)]
#[path = "../src/stall.rs"]
mod stall;
#[path = "../src/stall_live.rs"]
mod stall_live;
#[path = "../src/stall_observation.rs"]
mod stall_observation;
#[path = "../src/stall_settings.rs"]
mod stall_settings;

use codex_protocol::models::ResponseInputItem;
use pretty_assertions::assert_eq;
use serde_json::json;
use stall_live::output_value;
use stall_observation::CallKind;
use stall_observation::CallParent;
use stall_observation::TurnObserver;

#[test]
fn native_text_and_custom_content_use_the_same_model_visible_body_adapter() -> anyhow::Result<()> {
    let native: ResponseInputItem = serde_json::from_value(
        json!({"type":"function_call_output","call_id":"call","output":"{\"duration\":7,\"id\":\"user-data\"}"}),
    )?;
    assert_eq!(
        output_value("read", &native),
        Some(json!("{\"duration\":7,\"id\":\"user-data\"}"))
    );
    let custom: ResponseInputItem = serde_json::from_value(
        json!({"type":"custom_tool_call_output","call_id":"call","output":"native result"}),
    )?;
    assert_eq!(
        output_value("exec", &custom),
        Some(stall_observation::model_output(
            "exec",
            stall_observation::OutputBodyKind::Custom,
            json!("native result")
        ))
    );
    Ok(())
}

#[test]
fn private_mcp_metadata_does_not_supply_or_change_observable_work() -> anyhow::Result<()> {
    let body = |text: &str, private: &str| {
        serde_json::from_value::<ResponseInputItem>(json!({
            "type":"mcp_tool_call_output", "call_id":"call", "output":{"content":[{"type":"text","text":text}],"_meta":{"private":private}}
        }))
    };
    let first = output_value("mcp.read", &body("actual result", "one")?)
        .ok_or_else(|| anyhow::anyhow!("body unavailable"))?;
    let second = output_value("mcp.read", &body("actual result", "changed hidden data")?)
        .ok_or_else(|| anyhow::anyhow!("body unavailable"))?;
    assert_eq!(first, second);
    let empty = output_value("mcp.read", &body(" ", "looks like progress")?)
        .ok_or_else(|| anyhow::anyhow!("body unavailable"))?;
    let mut observer = TurnObserver::default();
    observer.automatic();
    observer.call(
        "call",
        "mcp.read",
        &json!({}),
        CallKind::Leaf,
        CallParent::Direct,
    );
    observer.outcome("call", "mcp.read", &empty);
    assert!(!observer.finish().complete);
    Ok(())
}

#[test]
fn owned_terminal_renderer_and_structured_result_share_digest_without_rewriting_stdout()
-> anyhow::Result<()> {
    for stdout in [
        "",
        "ack",
        "{\"id\":\"user-data\",\"duration\":7,\"status\":\"pending\"}",
    ] {
        let observation = |result| {
            let mut observer = TurnObserver::default();
            observer.automatic();
            observer.call(
                "call",
                "exec_command",
                &json!({"cmd":"read status"}),
                CallKind::Leaf,
                CallParent::Direct,
            );
            observer.outcome("call", "exec_command", &result);
            observer.finish()
        };
        let structured = json!({"exit_code":0,"output":stdout,"chunk_id":"other","wall_time_seconds":1.23,"original_token_count":10});
        let rendered: ResponseInputItem = serde_json::from_value(
            json!({"type":"function_call_output","call_id":"call","output":format!("Chunk ID: changeable\nWall time: 9.1234 seconds\nProcess exited with code 0\nOriginal token count: 10\nOutput:\n{stdout}")}),
        )?;
        let adapted =
            output_value("functions.exec_command", &rendered).expect("owned terminal body");
        assert_eq!(adapted["output"], json!(stdout));
        assert_eq!(observation(adapted), observation(structured));
    }
    Ok(())
}

#[test]
fn terminal_header_near_misses_and_user_text_do_not_lose_content() {
    for text in [
        "Wall time: NaN seconds\nOutput:\nack",
        "Wall time: 1.0000 seconds\nUnexpected metadata\nOutput:\nack",
        "Wall time: 1.0000 seconds\nProcess exited with code nope\nOutput:\nack",
    ] {
        assert_eq!(
            stall_observation::model_output(
                "exec_command",
                stall_observation::OutputBodyKind::Function,
                json!(text)
            ),
            json!(text)
        );
    }
    let text = "Chunk ID: user-data\nWall time: 1.0000 seconds\nOutput:\nack";
    assert_eq!(
        stall_observation::model_output(
            "read",
            stall_observation::OutputBodyKind::Function,
            json!(text)
        ),
        json!(text)
    );
}
