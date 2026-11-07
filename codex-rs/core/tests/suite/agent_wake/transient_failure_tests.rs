//! A wake-triggered root turn that fails with a transient model error (capacity or rate limit) must
//! leave its children running. Their reports wait for the root's next turn.

use super::*;
use codex_protocol::protocol::CodexErrorInfo;
use core_test_support::streaming_sse::start_routed_streaming_sse_server;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::sync::Mutex;
use test_case::test_case;

const CHILDREN: [&str; 3] = ["quick", "racer", "worker"];

fn task_message(task: &str) -> String {
    format!("work on {task}")
}

fn failure_chunks(kind: &str, gate: oneshot::Receiver<()>) -> Vec<StreamingSseChunk> {
    let error = |code: &str, message: &str| {
        vec![json!({
            "type": "response.failed",
            "response": {
                "id": "root-wake",
                "status": "failed",
                "error": {"code": code, "message": message},
            },
        })]
    };
    let events = match kind {
        "overloaded" => error(
            "server_is_overloaded",
            "Selected model is at capacity. Please try a different model.",
        ),
        "rate_limited" => error("rate_limit_exceeded", "Rate limit reached."),
        other => panic!("unknown failure kind {other}"),
    };
    vec![
        chunk(ev_response_created("root-wake")),
        gated_chunk(gate, events),
    ]
}

#[test_case("overloaded", CodexErrorInfo::ServerOverloaded; "server_overloaded")]
#[test_case("rate_limited", CodexErrorInfo::RateLimitExceeded; "rate_limited")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_wake_turn_failure_keeps_children_and_their_reports(
    kind: &str,
    expected_error: CodexErrorInfo,
) -> Result<()> {
    let (release_racer, racer_gate) = oneshot::channel();
    let (release_worker, worker_gate) = oneshot::channel();
    let (release_failure, failure_gate) = oneshot::channel();
    let spawn_calls = CHILDREN
        .iter()
        .map(|task| {
            chunk(ev_function_call_with_namespace(
                &format!("spawn-{task}"),
                NAMESPACE,
                "spawn_agent",
                &json!({
                    "task_name": task,
                    "message": task_message(task),
                    "fork_turns": "none",
                })
                .to_string(),
            ))
        })
        .collect::<Vec<_>>();
    let root_route = vec![
        std::iter::once(chunk(ev_response_created("root-spawn")))
            .chain(spawn_calls)
            .chain(std::iter::once(chunk(ev_completed("root-spawn"))))
            .collect(),
        final_answer_chunks("root-delegated", "delegated"),
        failure_chunks(kind, failure_gate),
        final_answer_chunks("root-followup", "summary"),
    ];
    let routed_children = Arc::new(Mutex::new(HashMap::<&'static str, String>::new()));
    let routed_for_matcher = Arc::clone(&routed_children);
    let (server, _) = start_routed_streaming_sse_server(
        vec![
            root_route,
            vec![final_answer_chunks("quick", "quick result")],
            vec![vec![gated_chunk(
                racer_gate,
                vec![
                    ev_response_created("racer"),
                    final_answer_item("racer-msg", "racer result"),
                    ev_completed("racer"),
                ],
            )]],
            vec![vec![gated_chunk(
                worker_gate,
                vec![
                    ev_response_created("worker"),
                    final_answer_item("worker-msg", "worker result"),
                    ev_completed("worker"),
                ],
            )]],
        ],
        move |_, body| {
            // Only root requests carry the user's prompt; children start from their task message.
            let text = std::str::from_utf8(body).ok()?;
            if text.contains(ROOT_PROMPT) {
                return Some(0);
            }
            let index = CHILDREN
                .iter()
                .position(|task| text.contains(&task_message(task)))?;
            let body: Value = serde_json::from_slice(body).ok()?;
            routed_for_matcher
                .lock()
                .expect("routes lock")
                .insert(CHILDREN[index], request_thread_id(&body)?.to_owned());
            Some(index + 1)
        },
    )
    .await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_session_source(SessionSource::Cli)
        .with_config(|config| configure_multi_agent(config, /*agent_polling*/ None))
        .build_with_streaming_server(&server)
        .await?;
    let root = test.session_configured.thread_id.to_string();
    let root_bodies = || async {
        streaming_request_bodies(&server)
            .await
            .into_iter()
            .filter(|body| request_thread_id(body) == Some(root.as_str()))
            .collect::<Vec<_>>()
    };

    submit_user_text(&test.codex, ROOT_PROMPT).await;
    // The quick child's report ends the root's sleep; the wake turn's request is then held.
    let deadline = Instant::now() + Duration::from_secs(20);
    while root_bodies().await.len() < 3 {
        assert!(
            Instant::now() < deadline,
            "the quick report never woke the root"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Drain the user turn's and the wake turn's starts so later checks see only new turns.
    for _ in 0..2 {
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnStarted(_))
        })
        .await;
    }
    let child_thread = |task: &str| {
        let id = routed_children.lock().expect("routes lock")[task].clone();
        ThreadId::from_string(&id).expect("child thread id")
    };
    let worker = test
        .thread_manager
        .get_thread(child_thread("worker"))
        .await?;
    let racer = test
        .thread_manager
        .get_thread(child_thread("racer"))
        .await?;

    // A child completes while the failing wake turn is in flight.
    release_racer.send(()).expect("racer request is held");
    wait_for_turn_complete(&racer).await;
    assert_no_turn_starts(&test.codex, SETTLE).await;

    release_failure.send(()).expect("wake request is held");
    let EventMsg::TurnComplete(failed) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(
        failed.error.and_then(|error| error.codex_error_info),
        Some(expected_error)
    );
    assert!(
        worker.wait_until_terminated().now_or_never().is_none(),
        "a transient root failure must not stop a working child"
    );
    assert!(
        test.thread_manager
            .list_thread_ids()
            .await
            .contains(&child_thread("worker")),
        "the working child stays registered under its path"
    );

    // The working child finishes after the failure; its report waits for the next root turn.
    release_worker.send(()).expect("worker request is held");
    wait_for_turn_complete(&worker).await;
    assert_no_turn_starts(&test.codex, SETTLE).await;

    submit_user_text(&test.codex, "what did the workers find?").await;
    let EventMsg::TurnComplete(followup) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(followup.error, None);
    let bodies = root_bodies().await;
    assert_eq!(bodies.len(), 4);
    assert_eq!(
        (
            deliveries(&bodies, "quick result"),
            deliveries(&bodies, "racer result"),
            deliveries(&bodies, "worker result"),
        ),
        (1, 1, 1)
    );
    server.shutdown().await;
    Ok(())
}
