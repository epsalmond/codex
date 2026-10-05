//! Cold loading must distinguish orphaned durable turns from live work.

use super::ROLE_MODEL;
use super::ROLE_NAME;
use super::body_contains;
use super::configure_multi_agent_v2_with_role;
use super::mount_root_collaboration_call;
use super::request_has_model;
use anyhow::Context;
use anyhow::Result;
use codex_core::SuspendTurnOutcome;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::ListTurnsParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::SortDirection;
use codex_thread_store::StoredTurnItemsView;
use codex_thread_store::StoredTurnStatus;
use core_test_support::responses::ev_completed;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Mutex;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::oneshot;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::matchers::method;
use wiremock::matchers::path;

const TASK: &str = "unfinished durable child task";
const QUEUED: &str = "queued context for the interrupted child";
const FOLLOWUP: &str = "explicit follow-up for the interrupted child";

#[derive(Clone, Copy)]
enum ResumeCase {
    Followup,
    CompactedFollowup,
}

#[test_case(ResumeCase::Followup; "unfinished turn")]
#[test_case(ResumeCase::CompactedFollowup; "compacted unfinished turn")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_resume_reports_interrupted_without_starting_queued_child(
    resume_case: ResumeCase,
) -> Result<()> {
    let server = start_mock_server().await;
    let (requested, request_received) = oneshot::channel();
    let requested = Mutex::new(Some(requested));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(|request: &wiremock::Request| {
            request_has_model(request, ROLE_MODEL) && body_contains(request, TASK)
        })
        .respond_with(move |_: &wiremock::Request| {
            if let Some(requested) = requested.lock().expect("child request signal").take() {
                let _ = requested.send(());
            }
            sse_response(sse(vec![ev_completed("unfinished-child")]))
                .set_delay(Duration::from_secs(/*secs*/ 60))
        })
        .up_to_n_times(/*n*/ 1)
        .mount(&server)
        .await;
    mount_root_collaboration_call(
        &server,
        "start unfinished child",
        "spawn-unfinished-child",
        "spawn_agent",
        &json!({ "message": TASK, "task_name": "worker", "agent_type": ROLE_NAME, "fork_turns": "none" }).to_string(),
    )
    .await;
    let configure = |config: &mut codex_core::config::Config| {
        configure_multi_agent_v2_with_role(config);
        config
            .features
            .enable(Feature::Sqlite)
            .expect("enable SQLite");
    };
    let initial = test_codex()
        .with_config(configure)
        .build_with_auto_env(&server)
        .await?;
    initial.submit_turn("start unfinished child").await?;
    timeout(Duration::from_secs(/*secs*/ 10), request_received).await??;
    let root = initial.session_configured.thread_id;
    let child_id = initial
        .thread_manager
        .list_agent_subtree_thread_ids(root)
        .await?
        .into_iter()
        .find(|id| *id != root)
        .context("spawned child")?;
    let child = initial.thread_manager.get_thread(child_id).await?;
    let started = wait_for_event(child.as_ref(), |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    let EventMsg::TurnStarted(started) = started else {
        unreachable!()
    };
    let original_turn = started.turn_id;
    assert_eq!(child.agent_status().await, AgentStatus::Running);

    mount_root_collaboration_call(
        &server,
        "queue active child",
        "queue-active-child",
        "send_message",
        &json!({ "target": "worker", "message": "context for the active child" }).to_string(),
    )
    .await;
    initial.submit_turn("queue active child").await?;
    assert_eq!(child.agent_status().await, AgentStatus::Running);

    // The internal session handoff stops execution and closes its writer without a
    // terminal turn event. Unlike graceful shutdown, it leaves a genuine durable orphan.
    let (reply, suspended) = oneshot::channel();
    child.submit(Op::SuspendTurnAndShutdown { reply }).await?;
    assert_eq!(
        timeout(Duration::from_secs(/*secs*/ 10), suspended).await???,
        SuspendTurnOutcome::Suspended {
            turn_id: original_turn.clone()
        }
    );
    initial
        .thread_manager
        .remove_thread(&child_id)
        .await
        .context("remove suspended child")?;

    let turn_params = ListTurnsParams {
        thread_id: child_id,
        include_archived: true,
        cursor: None,
        page_size: 1,
        sort_direction: SortDirection::Desc,
        items_view: StoredTurnItemsView::NotLoaded,
    };
    let turns = initial.thread_store.list_turns(turn_params.clone()).await?;
    assert_eq!(
        turns
            .turns
            .iter()
            .map(|turn| (&turn.turn_id, turn.status))
            .collect::<Vec<_>>(),
        vec![(&original_turn, StoredTurnStatus::InProgress)]
    );
    let history_params = LoadThreadHistoryParams {
        thread_id: child_id,
        include_archived: true,
    };
    let history = initial
        .thread_store
        .load_latest_model_context(history_params.clone())
        .await?;
    assert!(!history.items.iter().any(|item| matches!(
        item,
        RolloutItem::EventMsg(EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_))
    )));

    if matches!(resume_case, ResumeCase::CompactedFollowup) {
        // A real compaction boundary makes the startup suffix omit TurnStarted.
        // Durable turn metadata must still determine the resumed runtime status.
        initial
            .thread_store
            .resume_thread(codex_thread_store::ResumeThreadParams {
                thread_id: child_id,
                rollout_path: child.rollout_path(),
                history: None,
                include_archived: true,
                metadata: codex_thread_store::ThreadPersistenceMetadata {
                    cwd: Some(initial.config.cwd.to_path_buf()),
                    model_provider: initial.config.model_provider_id.clone(),
                    memory_mode: codex_protocol::protocol::ThreadMemoryMode::Enabled,
                },
            })
            .await?;
        initial
            .thread_store
            .append_items(AppendThreadItemsParams {
                thread_id: child_id,
                items: vec![RolloutItem::Compacted(serde_json::from_value(json!({
                    "message": "unfinished task checkpoint",
                    "replacement_history": [],
                    "window_number": 1,
                }))?)],
            })
            .await?;
        initial.thread_store.flush_thread(child_id).await?;
        initial.thread_store.shutdown_thread(child_id).await?;
        let suffix = initial
            .thread_store
            .load_latest_model_context(history_params)
            .await?;
        assert!(
            !suffix
                .items
                .iter()
                .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
        );
    }

    let resumed = test_codex()
        .with_config(configure)
        .restart(&server, &initial)
        .await?;
    assert!(resumed.thread_manager.get_thread(child_id).await.is_err());
    mount_root_collaboration_call(
        &server,
        "queue interrupted child",
        "queue-interrupted-child",
        "send_message",
        &json!({ "target": "worker", "message": QUEUED }).to_string(),
    )
    .await;
    resumed.submit_turn("queue interrupted child").await?;
    let loaded = resumed.thread_manager.get_thread(child_id).await?;
    assert_eq!(loaded.agent_status().await, AgentStatus::Interrupted);
    assert!(loaded.interrupted_turn().await.is_none());
    let child_request_count = |requests: Vec<wiremock::Request>| {
        requests
            .iter()
            .filter(|request| request_has_model(request, ROLE_MODEL))
            .count()
    };
    assert_eq!(
        child_request_count(
            server
                .received_requests()
                .await
                .context("captured requests")?
        ),
        1
    );
    assert_eq!(
        resumed.thread_store.list_turns(turn_params).await?.turns[0].status,
        StoredTurnStatus::InProgress
    );

    let resumed_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, ROLE_MODEL) && body_contains(request, QUEUED)
        },
        sse(vec![ev_completed("resumed-child")]),
    )
    .await;
    mount_root_collaboration_call(
        &server,
        "follow up interrupted child",
        "followup-interrupted-child",
        "followup_task",
        &json!({ "target": "worker", "message": FOLLOWUP }).to_string(),
    )
    .await;
    resumed.submit_turn("follow up interrupted child").await?;
    let completed = wait_for_event(loaded.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(completed) = completed else {
        unreachable!()
    };
    let requests = resumed_request
        .requests()
        .into_iter()
        .filter(|request| {
            request.body_json()["client_metadata"]["thread_id"] == json!(child_id)
                && request.body_contains_text(QUEUED)
        })
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let messages = request.body_json()["input"]
        .as_array()
        .context("model input")?
        .iter()
        .filter(|item| item["type"] == "agent_message")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        messages
            .iter()
            .filter(|item| item.to_string().contains(QUEUED))
            .count(),
        1
    );
    assert!(request.body_contains_text(FOLLOWUP));
    assert_ne!(completed.turn_id, original_turn);
    assert_eq!(
        child_request_count(
            server
                .received_requests()
                .await
                .context("captured requests")?
        ),
        2
    );
    Ok(())
}
