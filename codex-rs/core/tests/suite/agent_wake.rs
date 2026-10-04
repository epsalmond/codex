//! Wake mode (`multi_agent_v2.agent_polling = "disabled"`): the root ends its turn while children
//! work, and a child's report starts the root's next turn. These tests count root model requests,
//! the cost that `wait_agent` polling used to add, and check that each result is consumed once.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use codex_core::CodexThread;
use codex_core::GuardedShutdownOutcome;
use codex_core::StartIfIdleSubmission;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_core::WorkObservation;
use codex_core::config::Config;
use codex_core::config::Constrained;
use codex_extension_items::sleep::SleepItem;
use codex_features::AgentPolling;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentWakeupsUpdatedEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SessionSource;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_reasoning_item_added;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::user_message_item;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio::time::timeout_at;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const NAMESPACE: &str = "collaboration";
const SPAWN_CALL_ID: &str = "spawn-call-1";
const ROOT_PROMPT: &str = "delegate the worker task";
const FINAL_ANSWER: &str = "Message Type: FINAL_ANSWER";
const CHILD_DELAY: Duration = Duration::from_secs(1);
/// Long enough for a spurious wake turn to have issued its request.
const SETTLE: Duration = Duration::from_millis(750);

fn configure_multi_agent(config: &mut Config, agent_polling: Option<AgentPolling>) {
    for feature in [Feature::Collab, Feature::MultiAgentV2] {
        config
            .features
            .enable(feature)
            .expect("test config should allow feature update");
    }
    if let Some(agent_polling) = agent_polling {
        config.multi_agent_v2.agent_polling = agent_polling;
    }
    config.multi_agent_v2.min_wait_timeout_ms = 100;
    config.model_provider.request_max_retries = Some(0);
    config.model_provider.stream_max_retries = Some(0);
    config.model_provider.supports_websockets = false;
}

async fn build_multi_agent(
    server: &MockServer,
    agent_polling: Option<AgentPolling>,
) -> Result<TestCodex> {
    build_multi_agent_with_source(server, agent_polling, SessionSource::Cli).await
}

async fn build_multi_agent_with_source(
    server: &MockServer,
    agent_polling: Option<AgentPolling>,
    session_source: SessionSource,
) -> Result<TestCodex> {
    test_codex()
        .with_model("koffing")
        .with_session_source(session_source)
        .with_config(move |config| configure_multi_agent(config, agent_polling))
        .build(server)
        .await
}

fn request_json(req: &wiremock::Request) -> Option<Value> {
    let is_zstd = req
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("zstd"));
    let body = if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&req.body)).ok()?
    } else {
        req.body.clone()
    };
    serde_json::from_slice(&body).ok()
}

fn request_thread_id(body: &Value) -> Option<&str> {
    body["client_metadata"]["thread_id"].as_str()
}

/// Matches `/responses` requests from the root thread (`is_root`) or from any other thread.
fn from_thread(
    root: ThreadId,
    is_root: bool,
    predicate: impl Fn(&str) -> bool + Send + Sync + 'static,
) -> impl Fn(&wiremock::Request) -> bool + Send + Sync + 'static {
    let root = root.to_string();
    move |req: &wiremock::Request| {
        request_json(req).is_some_and(|body| {
            (request_thread_id(&body) == Some(root.as_str())) == is_root
                && predicate(&body.to_string())
        })
    }
}

async fn root_request_bodies(server: &MockServer, root: ThreadId) -> Vec<Value> {
    let root = root.to_string();
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|req| req.url.path().ends_with("/responses"))
        .filter_map(request_json)
        .filter(|body| request_thread_id(body) == Some(root.as_str()))
        .collect()
}

fn agent_message_texts(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "agent_message")
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|content| content["text"].as_str().map(str::to_string))
        .collect()
}

fn user_texts(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "message" && item["role"] == "user")
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|content| content["text"].as_str().map(str::to_string))
        .collect()
}

/// Counts how many root requests carry `needle` inside an `agent_message` input for the first
/// time, i.e. how many times the harness delivered it.
fn deliveries(bodies: &[Value], needle: &str) -> usize {
    let mut seen = 0;
    let mut delivered = 0;
    for body in bodies {
        let count = agent_message_texts(body)
            .iter()
            .filter(|text| text.contains(needle))
            .count();
        if count > seen {
            delivered += count - seen;
            seen = count;
        }
    }
    delivered
}

async fn submit_user_text(codex: &CodexThread, text: &str) {
    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }]))
        .await
        .expect("submit user input");
}

async fn wait_for_turn_complete(codex: &CodexThread) {
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
}

/// Drains events for `window` and fails if any turn starts.
async fn assert_no_turn_starts(codex: &CodexThread, window: Duration) {
    let deadline = Instant::now() + window;
    while let Ok(event) = timeout_at(deadline, codex.next_event()).await {
        let event = event.expect("event stream should stay open");
        assert!(
            !matches!(event.msg, EventMsg::TurnStarted(_)),
            "unexpected turn start"
        );
    }
}

/// Simulates a child report arriving at the root's mailbox.
async fn submit_child_mail(codex: &CodexThread, author: &str, text: &str, trigger_turn: bool) {
    codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from(author).expect("author path should parse"),
                AgentPath::root(),
                Vec::new(),
                text.to_string(),
                trigger_turn,
            ),
            start_options: Default::default(),
        })
        .await
        .expect("submit child mail");
}

/// Submits a child report, then waits for the root to process it. The barrier wait discards
/// earlier events, so use `submit_child_mail` when a test needs an event the mail emits.
async fn deliver_child_mail(codex: &CodexThread, author: &str, text: &str, trigger_turn: bool) {
    submit_child_mail(codex, author, text, trigger_turn).await;
    codex
        .submit(Op::RealtimeConversationListVoices)
        .await
        .expect("submit barrier");
    wait_for_event(codex, |event| {
        matches!(event, EventMsg::RealtimeConversationListVoicesResponse(_))
    })
    .await;
}

async fn wait_for_wakeups(codex: &CodexThread, paused: bool, queued_results: u32) {
    let expected = AgentWakeupsUpdatedEvent {
        paused,
        queued_results,
    };
    wait_for_event(
        codex,
        |event| matches!(event, EventMsg::AgentWakeupsUpdated(update) if *update == expected),
    )
    .await;
}

async fn mount_spawn_then_status(server: &MockServer, root: ThreadId) {
    let spawn_args = json!({"message": "do the work", "task_name": "worker"}).to_string();
    mount_sse_once_match(
        server,
        from_thread(
            root,
            /*is_root*/ true,
            |body| !body.contains(SPAWN_CALL_ID),
        ),
        sse(vec![
            ev_response_created("root-spawn"),
            ev_function_call_with_namespace(SPAWN_CALL_ID, NAMESPACE, "spawn_agent", &spawn_args),
            ev_completed("root-spawn"),
        ]),
    )
    .await;
}

fn root_before_report(body: &str) -> bool {
    body.contains(SPAWN_CALL_ID) && !body.contains(FINAL_ANSWER)
}

async fn mount_child_answer(server: &MockServer, root: ThreadId, answer: &str) {
    mount_response_once_match(
        server,
        from_thread(root, /*is_root*/ false, |_| true),
        sse_response(sse(vec![
            ev_response_created("child"),
            ev_assistant_message("child-msg", answer),
            ev_completed("child"),
        ]))
        .set_delay(CHILD_DELAY),
    )
    .await;
}

/// The regression harness: in wake mode the root costs one request per child report, however
/// long the child runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_root_sleeps_until_child_reports() -> Result<()> {
    let server = start_mock_server().await;
    let test = build_multi_agent(&server, None).await?;
    let root = test.session_configured.thread_id;
    mount_spawn_then_status(&server, root).await;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, root_before_report),
        sse(vec![
            ev_response_created("root-status"),
            ev_assistant_message("root-status-msg", "worker is running"),
            ev_completed("root-status"),
        ]),
    )
    .await;
    mount_child_answer(&server, root, "child done").await;
    mount_sse_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ true,
            |body| body.contains(FINAL_ANSWER),
        ),
        sse(vec![
            ev_response_created("root-wake"),
            ev_assistant_message("root-wake-msg", "integrated"),
            ev_completed("root-wake"),
        ]),
    )
    .await;

    test.submit_turn(ROOT_PROMPT).await?;
    let delegating_requests = root_request_bodies(&server, root).await.len();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    wait_for_turn_complete(&test.codex).await;
    assert_no_turn_starts(&test.codex, SETTLE).await;

    let bodies = root_request_bodies(&server, root).await;
    let wake_requests = &bodies[delegating_requests..];
    assert_eq!(
        (delegating_requests, wake_requests.len()),
        (2, 1),
        "the root should issue exactly one request after delegating: the wake"
    );
    assert!(wake_requests[0].to_string().contains(FINAL_ANSWER));
    assert_eq!(deliveries(&bodies, "child done"), 1);
    Ok(())
}

/// Baseline for the harness: with `wait_agent`, the same child costs one root request per poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_agent_mode_polls_while_child_runs() -> Result<()> {
    assert_wait_agent_polls_while_child_runs(SessionSource::Cli, Some(AgentPolling::Enabled)).await
}

/// `agent_polling = "enabled"` keeps an Exec root on `wait_agent` polling, as before exec drained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_mode_polls_while_child_runs_when_polling_is_enabled() -> Result<()> {
    assert_wait_agent_polls_while_child_runs(SessionSource::Exec, Some(AgentPolling::Enabled)).await
}

async fn assert_wait_agent_polls_while_child_runs(
    session_source: SessionSource,
    agent_polling: Option<AgentPolling>,
) -> Result<()> {
    let server = start_mock_server().await;
    let test = build_multi_agent_with_source(&server, agent_polling, session_source).await?;
    let root = test.session_configured.thread_id;
    mount_spawn_then_status(&server, root).await;
    let polls = Arc::new(AtomicUsize::new(0));
    let poll_counter = Arc::clone(&polls);
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(from_thread(root, /*is_root*/ true, root_before_report))
        .respond_with(move |_: &wiremock::Request| {
            let poll = poll_counter.fetch_add(1, Ordering::SeqCst);
            let response_id = format!("root-poll-{poll}");
            sse_response(sse(vec![
                ev_response_created(&response_id),
                ev_function_call_with_namespace(
                    &format!("wait-call-{poll}"),
                    NAMESPACE,
                    "wait_agent",
                    &json!({"timeout_ms": 100}).to_string(),
                ),
                ev_completed(&response_id),
            ]))
        })
        .mount(&server)
        .await;
    mount_child_answer(&server, root, "child done").await;
    mount_sse_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ true,
            |body| body.contains(FINAL_ANSWER),
        ),
        sse(vec![
            ev_response_created("root-final"),
            ev_assistant_message("root-final-msg", "integrated"),
            ev_completed("root-final"),
        ]),
    )
    .await;

    test.submit_turn(ROOT_PROMPT).await?;
    assert_no_turn_starts(&test.codex, SETTLE).await;

    let bodies = root_request_bodies(&server, root).await;
    assert!(
        polls.load(Ordering::SeqCst) > 1,
        "wait_agent should poll more than once while the child runs"
    );
    assert_eq!(bodies.len(), 2 + polls.load(Ordering::SeqCst));
    assert_eq!(deliveries(&bodies, "child done"), 1);
    Ok(())
}

/// A child that ends its turn with a question wakes the root, and so does its answer to the
/// root's follow-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_help_round_trip_wakes_root_twice() -> Result<()> {
    const QUESTION: &str = "which color should I use?";
    const FOLLOWUP: &str = "use blue";
    const ANSWER: &str = "painted it blue";
    let server = start_mock_server().await;
    let test = build_multi_agent(&server, Some(AgentPolling::Disabled)).await?;
    let root = test.session_configured.thread_id;
    mount_spawn_then_status(&server, root).await;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, root_before_report),
        sse(vec![
            ev_response_created("root-status"),
            ev_assistant_message("root-status-msg", "worker is running"),
            ev_completed("root-status"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ false,
            |body| !body.contains(FOLLOWUP),
        ),
        sse_response(sse(vec![
            ev_response_created("child-question"),
            ev_assistant_message("child-question-msg", QUESTION),
            ev_completed("child-question"),
        ]))
        .set_delay(CHILD_DELAY),
    )
    .await;
    let followup_args = json!({"target": "worker", "message": FOLLOWUP}).to_string();
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, |body| {
            body.contains(QUESTION) && !body.contains("followup-call")
        }),
        sse(vec![
            ev_response_created("root-wake-1"),
            ev_function_call_with_namespace(
                "followup-call",
                NAMESPACE,
                "followup_task",
                &followup_args,
            ),
            ev_completed("root-wake-1"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, |body| {
            body.contains("followup-call") && !body.contains(ANSWER)
        }),
        sse(vec![
            ev_response_created("root-replied"),
            ev_assistant_message("root-replied-msg", "answered the worker"),
            ev_completed("root-replied"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        from_thread(root, /*is_root*/ false, |body| body.contains(FOLLOWUP)),
        sse_response(sse(vec![
            ev_response_created("child-answer"),
            ev_assistant_message("child-answer-msg", ANSWER),
            ev_completed("child-answer"),
        ]))
        .set_delay(CHILD_DELAY),
    )
    .await;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, |body| body.contains(ANSWER)),
        sse(vec![
            ev_response_created("root-wake-2"),
            ev_assistant_message("root-wake-2-msg", "done"),
            ev_completed("root-wake-2"),
        ]),
    )
    .await;

    test.submit_turn(ROOT_PROMPT).await?;
    for _ in 0..2 {
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnStarted(_))
        })
        .await;
        wait_for_turn_complete(&test.codex).await;
    }
    assert_no_turn_starts(&test.codex, SETTLE).await;

    let bodies = root_request_bodies(&server, root).await;
    assert_eq!(
        (
            bodies.len(),
            deliveries(&bodies, QUESTION),
            deliveries(&bodies, ANSWER)
        ),
        (5, 1, 1)
    );
    Ok(())
}

/// A child's `send_message` is queue-only: it waits for the root's next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_progress_message_starts_no_root_turn() -> Result<()> {
    let server = start_mock_server().await;
    let test = build_multi_agent(&server, Some(AgentPolling::Disabled)).await?;
    let root = test.session_configured.thread_id;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, |_| true),
        sse(vec![
            ev_response_created("root-1"),
            ev_assistant_message("root-1-msg", "done"),
            ev_completed("root-1"),
        ]),
    )
    .await;

    test.submit_turn(ROOT_PROMPT).await?;
    deliver_child_mail(
        &test.codex,
        "/root/worker",
        "progress update",
        /*trigger_turn*/ false,
    )
    .await;
    assert_no_turn_starts(&test.codex, SETTLE).await;

    assert_eq!(root_request_bodies(&server, root).await.len(), 1);
    Ok(())
}

fn chunk(event: Value) -> StreamingSseChunk {
    StreamingSseChunk {
        gate: None,
        body: sse(vec![event]),
    }
}

fn gated_chunk(gate: oneshot::Receiver<()>, events: Vec<Value>) -> StreamingSseChunk {
    StreamingSseChunk {
        gate: Some(gate),
        body: sse(events),
    }
}

fn final_answer_chunks(response_id: &str, text: &str) -> Vec<StreamingSseChunk> {
    vec![
        chunk(ev_response_created(response_id)),
        chunk(final_answer_item(&format!("{response_id}-msg"), text)),
        chunk(ev_completed(response_id)),
    ]
}

fn final_answer_item(id: &str, text: &str) -> Value {
    json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "id": id,
            "content": [{"type": "output_text", "text": text}],
            "phase": "final_answer",
        }
    })
}

async fn build_streaming_wake_mode(server: &StreamingSseServer) -> Arc<CodexThread> {
    test_codex()
        .with_model("gpt-5.4")
        .with_session_source(SessionSource::Cli)
        .with_config(|config| configure_multi_agent(config, Some(AgentPolling::Disabled)))
        .build_with_streaming_server(server)
        .await
        .expect("build streaming Codex test session")
        .codex
}

async fn streaming_request_bodies(server: &StreamingSseServer) -> Vec<Value> {
    server
        .requests()
        .await
        .iter()
        .map(|body| serde_json::from_slice(body).expect("parse request"))
        .collect()
}

/// A report that arrives while the root is working is delivered in that turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_report_during_active_turn_is_delivered_in_that_turn() {
    let (release_tx, release_rx) = oneshot::channel();
    let first = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_reasoning_item_added("reason-1", &["thinking"])),
        gated_chunk(
            release_rx,
            vec![
                ev_reasoning_item("reason-1", &["thinking"], &[]),
                ev_completed("resp-1"),
            ],
        ),
    ];
    let (server, _completions) =
        start_streaming_sse_server(vec![first, final_answer_chunks("resp-2", "integrated")]).await;
    let codex = build_streaming_wake_mode(&server).await;

    submit_user_text(&codex, ROOT_PROMPT).await;
    wait_for_event(&codex, |event| {
        matches!(
            event,
            EventMsg::ItemStarted(started) if matches!(started.item, TurnItem::Reasoning(_))
        )
    })
    .await;
    deliver_child_mail(
        &codex,
        "/root/worker",
        "child done",
        /*trigger_turn*/ true,
    )
    .await;
    let _ = release_tx.send(());
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    assert_eq!((bodies.len(), deliveries(&bodies, "child done")), (2, 1));
    server.shutdown().await;
}

/// Reports that land after the root's final answer produce exactly one wake turn, which carries
/// all of them.
#[test_case(&["child a done"]; "one report")]
#[test_case(&["child a done", "child b done"]; "two near-simultaneous reports")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_reports_after_final_answer_start_one_wake_turn(reports: &[&str]) {
    let (release_tx, release_rx) = oneshot::channel();
    let first = vec![
        chunk(ev_response_created("resp-1")),
        chunk(final_answer_item("resp-1-msg", "worker is running")),
        gated_chunk(release_rx, vec![ev_completed("resp-1")]),
    ];
    let (server, _completions) =
        start_streaming_sse_server(vec![first, final_answer_chunks("resp-2", "integrated")]).await;
    let codex = build_streaming_wake_mode(&server).await;

    submit_user_text(&codex, ROOT_PROMPT).await;
    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::AgentMessage(message) if message.message == "worker is running")
    })
    .await;
    for (index, report) in reports.iter().enumerate() {
        let author = format!("/root/worker_{index}");
        deliver_child_mail(&codex, &author, report, /*trigger_turn*/ true).await;
    }
    let _ = release_tx.send(());
    wait_for_turn_complete(&codex).await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnStarted(_))).await;
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        reports
            .iter()
            .map(|report| deliveries(&bodies[1..], report))
            .collect::<Vec<_>>(),
        vec![1; reports.len()]
    );
    server.shutdown().await;
}

/// Esc pauses automatic wakeups: held reports wait for the next user message, which starts one
/// turn carrying both the user's text and the reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_interrupt_holds_reports_until_next_user_message() {
    const NEXT_PROMPT: &str = "what did the workers find?";
    let (_hold_tx, hold_rx) = oneshot::channel::<()>();
    let first = vec![
        chunk(ev_response_created("resp-1")),
        gated_chunk(hold_rx, vec![ev_completed("resp-1")]),
    ];
    let (server, _completions) =
        start_streaming_sse_server(vec![first, final_answer_chunks("resp-2", "summary")]).await;
    let codex = build_streaming_wake_mode(&server).await;

    submit_user_text(&codex, ROOT_PROMPT).await;
    server.wait_for_request_count(1).await;
    // Pending at Esc: arrives mid-request, before any item boundary could deliver it.
    deliver_child_mail(
        &codex,
        "/root/worker_a",
        "report a",
        /*trigger_turn*/ true,
    )
    .await;
    codex.submit(Op::Interrupt).await.expect("interrupt");
    wait_for_wakeups(&codex, /*paused*/ true, /*queued_results*/ 1).await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnAborted(_))).await;

    // Arrives while paused; the wakeups update shows the root processed it.
    submit_child_mail(
        &codex,
        "/root/worker_b",
        "report b",
        /*trigger_turn*/ true,
    )
    .await;
    wait_for_wakeups(&codex, /*paused*/ true, /*queued_results*/ 2).await;
    assert_no_turn_starts(&codex, SETTLE).await;
    assert_eq!(server.requests().await.len(), 1);

    submit_user_text(&codex, NEXT_PROMPT).await;
    wait_for_wakeups(&codex, /*paused*/ false, /*queued_results*/ 0).await;
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    assert_eq!(bodies.len(), 2);
    let resumed = &bodies[1];
    assert_eq!(
        (
            user_texts(resumed).iter().any(|text| text == NEXT_PROMPT),
            deliveries(&bodies, "report a"),
            deliveries(&bodies, "report b"),
        ),
        (true, 1, 1)
    );
    server.shutdown().await;
}

/// Pauses wakeups on an idle root, as Esc does, and waits for the pause to land.
async fn pause_idle_root(codex: &CodexThread) {
    codex.submit(Op::Interrupt).await.expect("interrupt");
    wait_for_wakeups(codex, /*paused*/ true, /*queued_results*/ 0).await;
}

async fn hold_child_report(codex: &CodexThread, author: &str, text: &str, queued_results: u32) {
    submit_child_mail(codex, author, text, /*trigger_turn*/ true).await;
    wait_for_wakeups(codex, /*paused*/ true, queued_results).await;
}

/// A user message steered into a running turn clears the pause, carries the held report into
/// that turn, and lets later reports wake the root again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_steered_message_clears_pause() {
    const STEER: &str = "also check the logs";
    let (release_tx, release_rx) = oneshot::channel();
    let automatic = vec![
        chunk(ev_response_created("resp-1")),
        gated_chunk(release_rx, vec![ev_completed("resp-1")]),
    ];
    let (server, _completions) = start_streaming_sse_server(vec![
        automatic,
        final_answer_chunks("resp-2", "checked"),
        final_answer_chunks("resp-3", "integrated"),
    ])
    .await;
    let codex = build_streaming_wake_mode(&server).await;

    pause_idle_root(&codex).await;
    hold_child_report(
        &codex,
        "/root/worker_a",
        "report a",
        /*queued_results*/ 1,
    )
    .await;
    let submission = codex
        .start_turn_if_idle(TurnInputRequest::new(TurnInput::ResponseItem(
            user_message_item("automatic input"),
        )))
        .await
        .expect("start automatic turn");
    assert!(matches!(submission, StartIfIdleSubmission::Started { .. }));
    server.wait_for_request_count(1).await;
    submit_user_text(&codex, STEER).await;
    wait_for_wakeups(&codex, /*paused*/ false, /*queued_results*/ 0).await;
    let _ = release_tx.send(());
    wait_for_turn_complete(&codex).await;

    submit_child_mail(
        &codex,
        "/root/worker_b",
        "report b",
        /*trigger_turn*/ true,
    )
    .await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnStarted(_))).await;
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    assert_eq!(
        (
            bodies.len(),
            deliveries(&bodies[..1], "report a"),
            user_texts(&bodies[1]).iter().any(|text| text == STEER),
            deliveries(&bodies[..2], "report a"),
            deliveries(&bodies, "report a"),
            deliveries(&bodies, "report b"),
        ),
        (3, 0, true, 1, 1, 1)
    );
    server.shutdown().await;
}

/// Queue-only mail still wakes a durable sleep while paused, but that turn leaves held reports
/// for the next user message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_durable_sleep_while_paused_holds_reports() {
    const NEXT_PROMPT: &str = "what did the workers find?";
    let (server, _completions) = start_streaming_sse_server(vec![
        final_answer_chunks("resp-1", "noted"),
        final_answer_chunks("resp-2", "summary"),
    ])
    .await;
    let codex = build_streaming_wake_mode(&server).await;

    pause_idle_root(&codex).await;
    codex.thread_extension_data().insert(SleepItem {
        id: "clock-wait-1".to_string(),
        duration_ms: 60_000,
    });
    hold_child_report(
        &codex,
        "/root/worker_a",
        "report a",
        /*queued_results*/ 1,
    )
    .await;
    assert_no_turn_starts(&codex, SETTLE).await;

    submit_child_mail(
        &codex,
        "/root/worker_b",
        "progress b",
        /*trigger_turn*/ false,
    )
    .await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnStarted(_))).await;
    wait_for_turn_complete(&codex).await;
    submit_user_text(&codex, NEXT_PROMPT).await;
    wait_for_wakeups(&codex, /*paused*/ false, /*queued_results*/ 0).await;
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    assert_eq!(
        (
            bodies.len(),
            deliveries(&bodies[..1], "progress b"),
            deliveries(&bodies[..1], "report a"),
            user_texts(&bodies[1])
                .iter()
                .any(|text| text == NEXT_PROMPT),
            deliveries(&bodies, "report a"),
        ),
        (2, 1, 0, true, 1)
    );
    server.shutdown().await;
}

/// Esc right as a wake turn starts returns its report to the mailbox, where the pause holds it
/// for the next user message. The report is delivered exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_interrupt_at_wake_turn_start_keeps_report() {
    const NEXT_PROMPT: &str = "what did the worker find?";
    let (server, _completions) = start_streaming_sse_server(vec![
        final_answer_chunks("resp-1", "summary"),
        final_answer_chunks("resp-2", "summary again"),
    ])
    .await;
    let codex = build_streaming_wake_mode(&server).await;

    submit_child_mail(
        &codex,
        "/root/worker_a",
        "report a",
        /*trigger_turn*/ true,
    )
    .await;
    codex.submit(Op::Interrupt).await.expect("interrupt");
    // The interrupt may abort the wake turn or land before it starts; both must hold the report.
    while timeout_at(Instant::now() + SETTLE, codex.next_event())
        .await
        .is_ok()
    {}

    submit_user_text(&codex, NEXT_PROMPT).await;
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    let last = bodies.last().expect("the user turn should issue a request");
    assert_eq!(
        (
            user_texts(last).iter().any(|text| text == NEXT_PROMPT),
            deliveries(&bodies, "report a"),
        ),
        (true, 1)
    );
    server.shutdown().await;
}

/// Cancelling an approval aborts the turn as Esc does: the report waits for the next user
/// message, which delivers it exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_approval_abort_holds_reports_until_next_user_message() {
    const NEXT_PROMPT: &str = "what did the worker find?";
    let exec_args = json!({"cmd": "/usr/bin/touch wake-approval"}).to_string();
    let first = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_function_call(
            "approval-call",
            "exec_command",
            &exec_args,
        )),
        chunk(ev_completed("resp-1")),
    ];
    let (server, _completions) =
        start_streaming_sse_server(vec![first, final_answer_chunks("resp-2", "summary")]).await;
    let codex = test_codex()
        .with_model("gpt-5.4")
        .with_session_source(SessionSource::Cli)
        .with_config(|config| {
            configure_multi_agent(config, Some(AgentPolling::Disabled));
            config.permissions.approval_policy =
                Constrained::allow_any(AskForApproval::UnlessTrusted);
        })
        .build_with_streaming_server(&server)
        .await
        .expect("build streaming Codex test session")
        .codex;

    submit_user_text(&codex, ROOT_PROMPT).await;
    let EventMsg::ExecApprovalRequest(approval) = wait_for_event(&codex, |event| {
        matches!(event, EventMsg::ExecApprovalRequest(_))
    })
    .await
    else {
        unreachable!("wait_for_event returns the matched event");
    };
    deliver_child_mail(
        &codex,
        "/root/worker",
        "report a",
        /*trigger_turn*/ true,
    )
    .await;
    codex
        .submit(Op::ExecApproval {
            id: approval.effective_approval_id(),
            turn_id: None,
            decision: ReviewDecision::Abort,
        })
        .await
        .expect("abort approval");
    wait_for_wakeups(&codex, /*paused*/ true, /*queued_results*/ 1).await;
    assert_no_turn_starts(&codex, SETTLE).await;
    assert_eq!(server.requests().await.len(), 1);

    submit_user_text(&codex, NEXT_PROMPT).await;
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    assert_eq!(
        (
            bodies.len(),
            user_texts(&bodies[1])
                .iter()
                .any(|text| text == NEXT_PROMPT),
            deliveries(&bodies, "report a"),
        ),
        (2, true, 1)
    );
    server.shutdown().await;
}

/// A prompt that a UserPromptSubmit hook blocks returns the held report to the mailbox, and the
/// same turn's next request delivers it exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_blocked_prompt_keeps_held_report() -> Result<()> {
    const BLOCKED_PROMPT: &str = "blocked prompt";
    let server = start_mock_server().await;
    let test = test_codex()
        .with_model("koffing")
        .with_session_source(SessionSource::Cli)
        .with_pre_build_hook(|home| {
            let script_path = home.join("block_prompt_hook.py");
            std::fs::write(
                &script_path,
                "import json, sys\njson.load(sys.stdin)\nprint(json.dumps({\"decision\": \"block\", \"reason\": \"blocked by hook\"}))\n",
            )
            .expect("write hook script");
            let hooks = json!({"hooks": {"UserPromptSubmit": [{"hooks": [{
                "type": "command",
                "command": format!("python3 {}", script_path.display()),
            }]}]}});
            std::fs::write(home.join("hooks.json"), hooks.to_string()).expect("write hooks.json");
        })
        .with_config(|config| {
            configure_multi_agent(config, Some(AgentPolling::Disabled));
            trust_discovered_hooks(config);
        })
        .build(&server)
        .await?;
    let root = test.session_configured.thread_id;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, |_| true),
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("resp-1-msg", "summary"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let codex = &test.codex;

    pause_idle_root(codex).await;
    hold_child_report(
        codex,
        "/root/worker_a",
        "report a",
        /*queued_results*/ 1,
    )
    .await;
    submit_user_text(codex, BLOCKED_PROMPT).await;
    wait_for_turn_complete(codex).await;
    assert_no_turn_starts(codex, SETTLE).await;

    let bodies = root_request_bodies(&server, root).await;
    assert_eq!(
        (
            bodies.len(),
            bodies
                .iter()
                .any(|body| user_texts(body).iter().any(|text| text == BLOCKED_PROMPT)),
            deliveries(&bodies, "report a"),
        ),
        (1, false, 1)
    );
    Ok(())
}

/// A report delivered after a wake request was sampled is included once in the next accepted
/// request, even when the current turn finishes before that report can be sampled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn report_arriving_after_sampling_snapshot_is_accepted_once_on_following_request() {
    let (release_first_response_tx, release_first_response_rx) = oneshot::channel();
    let (server, _completions) = start_streaming_sse_server(vec![
        vec![
            chunk(ev_response_created("late-report-first")),
            gated_chunk(
                release_first_response_rx,
                vec![ev_completed("late-report-first")],
            ),
        ],
        final_answer_chunks("late-report-followup", "processed the late report"),
    ])
    .await;
    let codex = build_streaming_wake_mode(&server).await;

    submit_child_mail(
        &codex,
        "/root/worker_a",
        "first report",
        /*trigger_turn*/ true,
    )
    .await;
    server.wait_for_request_count(1).await;
    let first_request = streaming_request_bodies(&server).await.remove(0);
    assert_eq!(deliveries(&[first_request], "first report"), 1);

    // The first request body has already crossed its sampling boundary. Queue the second report
    // while its response remains open, then wait for the session to process that mailbox item.
    deliver_child_mail(
        &codex,
        "/root/worker_b",
        "late report",
        /*trigger_turn*/ true,
    )
    .await;
    assert_eq!(server.requests().await.len(), 1);

    let _ = release_first_response_tx.send(());
    timeout_at(
        Instant::now() + Duration::from_secs(10),
        server.wait_for_request_count(2),
    )
    .await
    .expect("the late report should be delivered by another accepted request");
    wait_for_event(
        &codex,
        |event| matches!(event, EventMsg::AgentMessage(message) if message.message == "processed the late report"),
    )
    .await;
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    assert_eq!(deliveries(&bodies[1..2], "late report"), 1);
    assert_eq!(deliveries(&bodies, "late report"), 1);
    server.shutdown().await;
}

/// Builds a wake-mode root whose next turn runs pre-turn compaction after a 500k-token response.
async fn build_compacting_wake_mode(server: &StreamingSseServer) -> Arc<CodexThread> {
    test_codex()
        .with_model("gpt-5.4")
        .with_session_source(SessionSource::Cli)
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            configure_multi_agent(config, Some(AgentPolling::Disabled));
            config.model_auto_compact_token_limit = Some(100_000);
            let _ = config.features.enable(Feature::RemoteCompactionV2);
            let _ = config.features.disable(Feature::EnableRequestCompression);
        })
        .build_with_streaming_server(server)
        .await
        .expect("build streaming Codex test session")
        .codex
}

fn is_compaction(body: &Value) -> bool {
    body["input"].as_array().is_some_and(|input| {
        input
            .iter()
            .any(|item| item["type"] == "compaction_trigger")
    })
}

/// `/compact` replacing a wake turn takes over the report that turn had not read yet: the wake
/// turn is held in its pre-turn compaction, before it drains its input. Compaction does not read
/// the report either, so a wake turn after the compaction delivers it, exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_compact_replacing_turn_keeps_report() {
    let (_hold_tx, hold_rx) = oneshot::channel::<()>();
    let (server, _completions) = start_streaming_sse_server(vec![
        vec![
            chunk(ev_response_created("resp-1")),
            chunk(final_answer_item("resp-1-msg", "worker is running")),
            chunk(ev_completed_with_tokens(
                "resp-1", /*total_tokens*/ 500_000,
            )),
        ],
        // The wake turn's pre-turn compaction, held until `/compact` replaces the turn.
        vec![gated_chunk(hold_rx, vec![ev_completed("resp-2")])],
        vec![
            chunk(json!({
                "type": "response.output_item.done",
                "item": {"type": "compaction", "encrypted_content": "COMPACTED"},
            })),
            chunk(ev_completed_with_tokens("resp-3", /*total_tokens*/ 50)),
        ],
        final_answer_chunks("resp-4", "integrated"),
    ])
    .await;
    let codex = build_compacting_wake_mode(&server).await;

    submit_user_text(&codex, ROOT_PROMPT).await;
    wait_for_turn_complete(&codex).await;
    submit_child_mail(
        &codex,
        "/root/worker",
        "report a",
        /*trigger_turn*/ true,
    )
    .await;
    server.wait_for_request_count(2).await;
    codex.submit(Op::Compact).await.expect("compact");
    timeout_at(
        Instant::now() + Duration::from_secs(10),
        server.wait_for_request_count(4),
    )
    .await
    .expect("the returned report should wake the root");
    wait_for_event(
        &codex,
        |event| matches!(event, EventMsg::AgentMessage(message) if message.message == "integrated"),
    )
    .await;
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    assert_eq!(
        (
            bodies.len(),
            bodies.iter().map(is_compaction).collect::<Vec<_>>(),
            deliveries(&bodies[..3], "report a"),
            deliveries(&bodies, "report a"),
        ),
        (4, vec![false, true, true, false], 0, 1)
    );
    server.shutdown().await;
}

/// `/compact` replacing the user turn that ends an Esc pause takes over the results that turn
/// had not read yet: the user turn is held in its pre-turn compaction, before it records its
/// input. Compaction does not read the results either, so a wake turn after the compaction
/// delivers them, exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_compact_replacing_resumed_user_turn_keeps_report() {
    const NEXT_PROMPT: &str = "what did the worker find?";
    let (_hold_tx, hold_rx) = oneshot::channel::<()>();
    let (server, _completions) = start_streaming_sse_server(vec![
        vec![
            chunk(ev_response_created("resp-1")),
            chunk(final_answer_item("resp-1-msg", "worker is running")),
            chunk(ev_completed_with_tokens(
                "resp-1", /*total_tokens*/ 500_000,
            )),
        ],
        // The user turn's pre-turn compaction, held until `/compact` replaces the turn.
        vec![gated_chunk(hold_rx, vec![ev_completed("resp-2")])],
        vec![
            chunk(json!({
                "type": "response.output_item.done",
                "item": {"type": "compaction", "encrypted_content": "COMPACTED"},
            })),
            chunk(ev_completed_with_tokens("resp-3", /*total_tokens*/ 50)),
        ],
        final_answer_chunks("resp-4", "integrated"),
    ])
    .await;
    let codex = build_compacting_wake_mode(&server).await;

    submit_user_text(&codex, ROOT_PROMPT).await;
    wait_for_turn_complete(&codex).await;
    pause_idle_root(&codex).await;
    hold_child_report(
        &codex,
        "/root/worker",
        "report a",
        /*queued_results*/ 1,
    )
    .await;
    submit_user_text(&codex, NEXT_PROMPT).await;
    server.wait_for_request_count(2).await;
    codex.submit(Op::Compact).await.expect("compact");
    timeout_at(
        Instant::now() + Duration::from_secs(10),
        server.wait_for_request_count(4),
    )
    .await
    .expect("the returned report should wake the root");
    wait_for_event(
        &codex,
        |event| matches!(event, EventMsg::AgentMessage(message) if message.message == "integrated"),
    )
    .await;
    wait_for_turn_complete(&codex).await;
    assert_no_turn_starts(&codex, SETTLE).await;

    let bodies = streaming_request_bodies(&server).await;
    assert_eq!(
        (
            bodies.len(),
            bodies.iter().map(is_compaction).collect::<Vec<_>>(),
            deliveries(&bodies[..3], "report a"),
            deliveries(&bodies, "report a"),
        ),
        (4, vec![false, true, true, false], 0, 1)
    );
    server.shutdown().await;
}

/// The root decides whether a completion wakes it: a root that keeps `wait_agent` reads the
/// report as queue-only mail even when the child marked it as a wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_agent_root_keeps_child_reports_queue_only() -> Result<()> {
    let server = start_mock_server().await;
    let test = build_multi_agent(&server, Some(AgentPolling::Enabled)).await?;
    let root = test.session_configured.thread_id;
    for turn in ["root-1", "root-2"] {
        mount_sse_once_match(
            &server,
            from_thread(root, /*is_root*/ true, |_| true),
            sse(vec![
                ev_response_created(turn),
                ev_assistant_message(&format!("{turn}-msg"), "done"),
                ev_completed(turn),
            ]),
        )
        .await;
    }

    deliver_child_mail(
        &test.codex,
        "/root/worker",
        "child done",
        /*trigger_turn*/ true,
    )
    .await;
    assert_no_turn_starts(&test.codex, SETTLE).await;
    // Queue-only mail pending at a user turn's start reaches the model on the following turn.
    test.submit_turn(ROOT_PROMPT).await?;
    test.submit_turn("anything new?").await?;

    let bodies = root_request_bodies(&server, root).await;
    assert_eq!((bodies.len(), deliveries(&bodies, "child done")), (2, 1));
    Ok(())
}

async fn wait_for_turn_complete_id(codex: &CodexThread) -> String {
    let EventMsg::TurnComplete(completed) =
        wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await
    else {
        unreachable!("the predicate only accepts turn completion");
    };
    completed.turn_id
}

fn assert_not_closable(observation: &WorkObservation) {
    let snapshot = observation.snapshot();
    assert!(!snapshot.quiescent, "{snapshot:?}");
    // A running child can advance the revision between the read and the close; either refusal
    // keeps the scope open.
    let outcome = observation.shutdown_if_quiescent(&snapshot.revision);
    assert!(
        matches!(
            outcome,
            GuardedShutdownOutcome::NotQuiescent(_) | GuardedShutdownOutcome::StaleRevision(_)
        ),
        "{snapshot:?} -> {outcome:?}"
    );
}

/// The premature-shutdown race: an Exec root that ends its turn while a child works must stay
/// open until the wake turn's output is forwarded, even after the first turn's output was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_wake_mode_stays_open_until_the_wake_turn_output_is_forwarded() -> Result<()> {
    let server = start_mock_server().await;
    let test = test_codex()
        .with_model("koffing")
        .with_session_source(SessionSource::Exec)
        .with_config(|config| {
            configure_multi_agent(config, None);
        })
        .build(&server)
        .await?;
    let root = test.session_configured.thread_id;
    mount_spawn_then_status(&server, root).await;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, root_before_report),
        sse(vec![
            ev_response_created("root-status"),
            ev_assistant_message("root-status-msg", "worker is running"),
            ev_completed("root-status"),
        ]),
    )
    .await;
    mount_child_answer(&server, root, "child done").await;
    mount_sse_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ true,
            |body| body.contains(FINAL_ANSWER),
        ),
        sse(vec![
            ev_response_created("root-wake"),
            ev_assistant_message("root-wake-msg", "integrated"),
            ev_completed("root-wake"),
        ]),
    )
    .await;

    let observation = test
        .codex
        .work_observation()
        .expect("an Exec root in wake mode is observable");
    let (initial, mut updates) = observation.subscribe();
    assert!(initial.quiescent);

    submit_user_text(&test.codex, ROOT_PROMPT).await;
    let first_turn = wait_for_turn_complete_id(&test.codex).await;
    assert_not_closable(&observation);
    test.codex.note_root_turn_output_forwarded(&first_turn);
    let waiting = observation.snapshot();
    assert_eq!(
        (waiting.active_root_turns, waiting.pending_terminal_outputs),
        (0, 0)
    );
    assert!(waiting.running_finite_work >= 2, "{waiting:?}");
    assert_not_closable(&observation);

    let wake_turn = wait_for_turn_complete_id(&test.codex).await;
    assert_ne!(wake_turn, first_turn);
    assert_eq!(observation.snapshot().pending_terminal_outputs, 1);
    assert_not_closable(&observation);
    assert!(!updates.borrow_and_update().quiescent);

    test.codex.note_root_turn_output_forwarded(&wake_turn);
    let drained = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = updates.borrow_and_update().clone();
            if snapshot.quiescent {
                return snapshot;
            }
            updates.changed().await.expect("observation remains alive");
        }
    })
    .await?;
    let GuardedShutdownOutcome::Closed(closed) =
        observation.shutdown_if_quiescent(&drained.revision)
    else {
        panic!("a drained Exec tree should close at its current revision");
    };
    assert!(closed.closed);

    // The wake assignment binds before root admission; a rejected turn must not reopen the tree.
    let rejected = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "after close".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    assert!(
        matches!(rejected, TurnInputSubmission::NotSubmitted { .. }),
        "{rejected:?}"
    );
    assert!(observation.snapshot().closed);
    assert_eq!(
        deliveries(&root_request_bodies(&server, root).await, "child done"),
        1
    );
    Ok(())
}
