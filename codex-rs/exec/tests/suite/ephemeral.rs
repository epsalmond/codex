#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use walkdir::WalkDir;
use wiremock::MockServer;

fn exec_sse_response() -> String {
    responses::sse(vec![
        responses::ev_response_created("resp-ephemeral"),
        responses::ev_assistant_message("msg-ephemeral", "ephemeral response"),
        responses::ev_completed("resp-ephemeral"),
    ])
}

fn session_rollout_count(home_path: &std::path::Path) -> usize {
    let sessions_dir = home_path.join("sessions");
    if !sessions_dir.exists() {
        return 0;
    }

    WalkDir::new(sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persists_rollout_file_by_default() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = responses::mount_sse_once(&server, exec_sse_response()).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("default persistence behavior")
        .assert()
        .code(0);

    assert_eq!(session_rollout_count(test.home_path()), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn does_not_persist_rollout_file_in_ephemeral_mode() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = responses::mount_sse_once(&server, exec_sse_response()).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--ephemeral")
        .arg("ephemeral behavior")
        .assert()
        .code(0);

    assert_eq!(session_rollout_count(test.home_path()), 0);
    Ok(())
}

/// The default drain subscribes to work state; a root without multi-agent v2 is not observable, so
/// exec falls back to its single-turn path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_work_observation_keeps_exec_startup_compatible() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = responses::mount_sse_once(&server, exec_sse_response()).await;

    let output = test
        .cmd_with_server(&server)
        .arg("-c")
        .arg("features.multi_agent_v2.enabled=false")
        .arg("--skip-git-repo-check")
        .arg("unobserved work lifecycle startup")
        .output()?;

    assert!(
        output.status.success(),
        "Exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ephemeral response"));
    Ok(())
}
