#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use predicates::str::contains;
use wiremock::MockServer;

fn exec_sse_response() -> String {
    responses::sse(vec![
        responses::ev_response_created("resp-last-message"),
        responses::ev_assistant_message("msg-last-message", "final answer"),
        responses::ev_completed("resp-last-message"),
    ])
}

/// A failed `--output-last-message` write must fail the run so automation
/// can detect the missing output file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exits_non_zero_when_last_message_write_fails() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let output_path = test.cwd_path().join("missing-dir").join("last-message.txt");

    for json_flag in [None, Some("--json")] {
        let _response_mock = responses::mount_sse_once(&server, exec_sse_response()).await;
        test.cmd_with_server(&server)
            .arg("--skip-git-repo-check")
            .args(json_flag)
            .arg("--output-last-message")
            .arg(&output_path)
            .arg("write the last message")
            .assert()
            .code(1)
            .stderr(contains("failed to write last message file"));
    }

    assert!(!output_path.exists());
    Ok(())
}
