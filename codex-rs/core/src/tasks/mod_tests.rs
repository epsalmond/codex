use super::SessionTask;
use super::SessionTaskResult;
use super::TASK_COMPACT_METRIC;
use super::UserShellCommandTask;
use super::emit_compact_metric;
use super::emit_turn_memory_metric;
use super::emit_turn_network_proxy_metric;
use crate::session::TurnInput;
use crate::session::multi_agents::ChildReportMode;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_session_source_and_rx;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_otel::TURN_TOKEN_USAGE_METRIC;
use codex_otel::TURN_TOOL_CALL_METRIC;
use codex_otel::TURN_UNIFIED_EXEC_RUNNING_PROCESSES_METRIC;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::Metric;
use opentelemetry_sdk::metrics::data::MetricData;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct PendingTask;

impl SessionTask for PendingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.pending"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _turn_context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        std::future::pending().await
    }
}

fn test_session_telemetry() -> SessionTelemetry {
    let exporter = InMemoryMetricExporter::default();
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory("test", "codex-core", env!("CARGO_PKG_VERSION"), exporter)
            .with_runtime_reader(),
    )
    .expect("in-memory metrics client");
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-5.4",
        "gpt-5.4",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "tty".to_string(),
        SessionSource::Cli,
    )
    .with_metrics_without_metadata_tags(metrics)
}

fn find_metric<'a>(resource_metrics: &'a ResourceMetrics, name: &str) -> &'a Metric {
    for scope_metrics in resource_metrics.scope_metrics() {
        for metric in scope_metrics.metrics() {
            if metric.name() == name {
                return metric;
            }
        }
    }
    panic!("metric {name} missing");
}

fn attributes_to_map<'a>(
    attributes: impl Iterator<Item = &'a KeyValue>,
) -> BTreeMap<String, String> {
    attributes
        .map(|kv| (kv.key.as_str().to_string(), kv.value.as_str().to_string()))
        .collect()
}

fn metric_point(resource_metrics: &ResourceMetrics, name: &str) -> (BTreeMap<String, String>, u64) {
    let metric = find_metric(resource_metrics, name);
    match metric.data() {
        AggregatedMetrics::U64(data) => match data {
            MetricData::Sum(sum) => {
                let points: Vec<_> = sum.data_points().collect();
                assert_eq!(points.len(), 1);
                let point = points[0];
                (attributes_to_map(point.attributes()), point.value())
            }
            _ => panic!("unexpected counter aggregation"),
        },
        _ => panic!("unexpected counter data type"),
    }
}

#[derive(Clone, Copy)]
enum UsageScenario {
    ResponseOnly,
    ResponseAfterStepSwitch,
    CompactThenResponse,
    CompactThenStop,
    NoResponse,
}

#[test_case::test_case(UsageScenario::ResponseOnly; "response_only")]
#[test_case::test_case(UsageScenario::ResponseAfterStepSwitch; "response_after_step_switch")]
#[test_case::test_case(UsageScenario::CompactThenResponse; "compact_then_response")]
#[test_case::test_case(UsageScenario::CompactThenStop; "compact_then_stop")]
#[test_case::test_case(UsageScenario::NoResponse; "no_response")]
#[tokio::test]
async fn turn_completion_metrics_follow_model_switch(scenario: UsageScenario) {
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory(
            "test",
            "codex-core",
            env!("CARGO_PKG_VERSION"),
            InMemoryMetricExporter::default(),
        )
        .with_runtime_reader(),
    )
    .expect("in-memory metrics client");
    let (mut session, mut turn_context, _receiver) = make_session_and_context_with_rx().await;
    let session_telemetry = session
        .services
        .session_telemetry
        .clone()
        .with_metrics(metrics.clone());
    Arc::get_mut(&mut session)
        .expect("session should be uniquely owned")
        .services
        .session_telemetry = session_telemetry.clone();
    Arc::get_mut(&mut turn_context)
        .expect("turn context should be uniquely owned")
        .session_telemetry = session_telemetry;

    let next_model = if turn_context.model_info().slug == "gpt-5.4" {
        "gpt-5.2"
    } else {
        "gpt-5.4"
    };
    let previous_context = turn_context;
    let turn_context = Arc::new(
        previous_context
            .with_model(next_model.to_string(), &session.services.models_manager)
            .await,
    );
    // Earlier session usage must not leak into this turn's per-model samples.
    session
        .update_token_usage_info(
            &previous_context,
            Some(&TokenUsage {
                input_tokens: 1_000,
                total_tokens: 1_000,
                ..TokenUsage::default()
            }),
        )
        .await
        .expect("earlier session usage should be recorded");
    session
        .spawn_task(Arc::clone(&turn_context), Vec::new(), PendingTask)
        .await;
    let mut expected_usage = Vec::new();
    if matches!(
        scenario,
        UsageScenario::CompactThenResponse | UsageScenario::CompactThenStop
    ) {
        let compaction_usage = TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 30,
            cache_write_input_tokens: 20,
            output_tokens: 70,
            reasoning_output_tokens: 40,
            total_tokens: 170,
            codex_rollout_budget_units: None,
        };
        // Local pre-turn compaction records usage with the previous model's context.
        session
            .update_token_usage_info(&previous_context, Some(&compaction_usage))
            .await
            .expect("compaction usage should be recorded");
        expected_usage.push((previous_context.model_info().slug.clone(), compaction_usage));
    }
    if matches!(
        scenario,
        UsageScenario::ResponseOnly
            | UsageScenario::ResponseAfterStepSwitch
            | UsageScenario::CompactThenResponse
    ) {
        let response_context = if matches!(scenario, UsageScenario::ResponseAfterStepSwitch) {
            &previous_context
        } else {
            &turn_context
        };
        let response_usage = TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 3,
            cache_write_input_tokens: 2,
            output_tokens: 7,
            reasoning_output_tokens: 4,
            total_tokens: 17,
            codex_rollout_budget_units: None,
        };
        // Multiple requests for one model must still produce one turn histogram sample.
        for _ in 0..2 {
            session
                .record_token_usage_info(
                    &turn_context,
                    &response_context.initial_settings,
                    Some(&response_usage),
                )
                .await
                .expect("response usage should be recorded");
        }
        let mut total_usage = response_usage.clone();
        total_usage.add_assign(&response_usage);
        expected_usage.push((response_context.model_info().slug.clone(), total_usage));
    }
    if matches!(scenario, UsageScenario::NoResponse) {
        expected_usage.push((next_model.to_string(), TokenUsage::default()));
    }

    let task_result = if matches!(scenario, UsageScenario::CompactThenStop) {
        // A PostCompact hook can stop the turn before the new model samples.
        Err(CodexErr::TurnAborted)
    } else {
        Ok(None)
    };
    session.on_task_finished(turn_context, task_result).await;

    let snapshot = metrics.snapshot().expect("runtime metrics snapshot");
    let token_usage_metric = find_metric(&snapshot, TURN_TOKEN_USAGE_METRIC);
    let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = token_usage_metric.data() else {
        panic!("expected token usage histogram");
    };
    let mut token_usage_points = histogram
        .data_points()
        .map(|point| {
            let mut attributes = attributes_to_map(point.attributes());
            (
                (
                    attributes
                        .remove("token_type")
                        .expect("token usage metric should include a token type"),
                    attributes
                        .remove("model")
                        .expect("token usage metric should include a model"),
                ),
                point.count(),
                point.sum(),
            )
        })
        .collect::<Vec<_>>();
    let mut expected_points = expected_usage
        .into_iter()
        .flat_map(|(model, usage)| {
            [
                ("cache_write_input", usage.cache_write_input_tokens),
                ("cached_input", usage.cached_input()),
                ("input", usage.input_tokens),
                ("output", usage.output_tokens),
                ("reasoning_output", usage.reasoning_output_tokens),
                ("total", usage.total_tokens),
            ]
            .map(|(token_type, value)| ((token_type.to_string(), model.clone()), 1, value as f64))
        })
        .collect::<Vec<_>>();
    token_usage_points.sort_by(|left, right| left.0.cmp(&right.0));
    expected_points.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(token_usage_points, expected_points);

    let tool_call_metric = find_metric(&snapshot, TURN_TOOL_CALL_METRIC);
    let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = tool_call_metric.data() else {
        panic!("expected tool call histogram");
    };
    let tool_call_models = histogram
        .data_points()
        .map(|point| {
            attributes_to_map(point.attributes())
                .remove("model")
                .expect("tool call metric should include a model")
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_call_models, vec![next_model.to_string()]);

    let counter_models = [
        TURN_MEMORY_METRIC,
        TURN_NETWORK_PROXY_METRIC,
        TURN_UNIFIED_EXEC_RUNNING_PROCESSES_METRIC,
    ]
    .into_iter()
    .map(|name| {
        let (mut attributes, _value) = metric_point(&snapshot, name);
        (
            name.to_string(),
            attributes
                .remove("model")
                .expect("turn counter should include a model"),
        )
    })
    .collect::<BTreeMap<_, _>>();
    assert_eq!(
        counter_models,
        BTreeMap::from([
            (TURN_MEMORY_METRIC.to_string(), next_model.to_string()),
            (
                TURN_NETWORK_PROXY_METRIC.to_string(),
                next_model.to_string(),
            ),
            (
                TURN_UNIFIED_EXEC_RUNNING_PROCESSES_METRIC.to_string(),
                next_model.to_string(),
            ),
        ])
    );
}

#[test]
fn emit_turn_network_proxy_metric_records_active_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ true,
        ("tmp_mem_enabled", "true"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "true".to_string()),
            ("tmp_mem_enabled".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn emit_turn_network_proxy_metric_records_inactive_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ false,
        ("tmp_mem_enabled", "false"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "false".to_string()),
            ("tmp_mem_enabled".to_string(), "false".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_read_allowed_with_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ true,
        /*has_citations*/ true,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "true".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "true".to_string()),
            ("read_allowed".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_config_disabled_without_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ false,
        /*has_citations*/ false,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "false".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "false".to_string()),
            ("read_allowed".to_string(), "false".to_string()),
        ])
    );
}

#[test]
fn emit_compact_metric_records_manual_remote_v2() {
    let session_telemetry = test_session_telemetry();

    emit_compact_metric(&session_telemetry, "remote_v2", /*manual*/ true);

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TASK_COMPACT_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("manual".to_string(), "true".to_string()),
            ("type".to_string(), "remote_v2".to_string()),
        ])
    );
}

#[test]
fn emit_compact_metric_records_auto_local() {
    let session_telemetry = test_session_telemetry();

    emit_compact_metric(&session_telemetry, "local", /*manual*/ false);

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TASK_COMPACT_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("manual".to_string(), "false".to_string()),
            ("type".to_string(), "local".to_string()),
        ])
    );
}

/// Compaction, review and user shell commands never read the input that `start_task` hands them.
/// In wake mode they return its agent mail, in order, for a wake turn to deliver. A regular turn
/// records unread mail instead, so a turn that ended before reading it cannot restart on the same
/// mail. Outside wake mode every task records it, as upstream does.
#[tokio::test]
async fn finished_task_returns_unread_mail_only_in_wake_mode_when_not_read() {
    struct UnreadInputTask(TaskKind);

    impl SessionTask for UnreadInputTask {
        fn kind(&self) -> TaskKind {
            self.0
        }

        fn span_name(&self) -> &'static str {
            "session_task.unread_input"
        }

        async fn run(
            self: Arc<Self>,
            _session: Arc<Session>,
            _ctx: Arc<TurnContext>,
            _input: Vec<TurnInput>,
            _cancellation_token: CancellationToken,
        ) -> SessionTaskResult {
            Ok(None)
        }
    }

    const UNREAD: [(&str, bool); 2] = [("unread progress", false), ("unread report", true)];
    let unread = || {
        UNREAD.map(|(content, trigger_turn)| {
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("agent path"),
                AgentPath::root(),
                Vec::new(),
                content.to_string(),
                trigger_turn,
            )
        })
    };
    /// Returns the mail left in the mailbox and how many unread items history recorded.
    async fn finish_with_unread_mail(
        task: impl SessionTask,
        mode: ChildReportMode,
        unread: [InterAgentCommunication; 2],
    ) -> (Vec<TurnInput>, usize) {
        let session_source = if mode == ChildReportMode::WakeOnReport {
            SessionSource::Cli
        } else {
            SessionSource::Exec
        };
        let (session, turn_context, rx) =
            make_session_and_context_with_auth_and_config_and_session_source_and_rx(
                CodexAuth::from_api_key("Test API Key"),
                Vec::new(),
                session_source,
                |config| {
                    config
                        .features
                        .enable(Feature::MultiAgentV2)
                        .expect("test config should allow feature update");
                    config.multi_agent_v2.agent_polling = if mode == ChildReportMode::WaitAgent {
                        codex_features::AgentPolling::Enabled
                    } else {
                        codex_features::AgentPolling::Disabled
                    };
                },
            )
            .await;
        for communication in unread {
            session
                .input_queue
                .enqueue_mailbox_communication(communication, Default::default())
                .await;
        }
        session.spawn_task(turn_context, Vec::new(), task).await;
        // Holds returned mail so no real wake turn starts. `start_task` has already handed the
        // mail to the task, which has not run yet on this single-threaded runtime.
        session.input_queue.pause_wakeups();
        while !matches!(
            rx.recv().await.expect("event").msg,
            EventMsg::TurnComplete(_)
        ) {}
        session.input_queue.resume_wakeups();
        let history = session.clone_history().await;
        let recorded = history
            .raw_items()
            .filter(|item| {
                serde_json::to_string(item)
                    .expect("serialize history item")
                    .contains("unread ")
            })
            .count();
        (
            session.input_queue.drain_mailbox_input_items().await.0,
            recorded,
        )
    }

    let mut outcomes = Vec::new();
    for (kind, mode) in [
        (TaskKind::Regular, ChildReportMode::WakeOnReport),
        (TaskKind::Compact, ChildReportMode::WakeOnReport),
        (TaskKind::Review, ChildReportMode::WakeOnReport),
        (TaskKind::Compact, ChildReportMode::WaitAgent),
        (TaskKind::Review, ChildReportMode::WaitAgent),
    ] {
        outcomes.push(finish_with_unread_mail(UnreadInputTask(kind), mode, unread()).await);
    }
    for mode in [ChildReportMode::WakeOnReport, ChildReportMode::WaitAgent] {
        let shell = UserShellCommandTask::new("echo shell".to_string(), /*timeout_ms*/ None);
        outcomes.push(finish_with_unread_mail(shell, mode, unread()).await);
    }

    let returned = unread()
        .into_iter()
        .map(TurnInput::InterAgentCommunication)
        .collect::<Vec<_>>();
    let recorded = (Vec::new(), 2);
    assert_eq!(
        outcomes,
        vec![
            recorded.clone(),
            (returned.clone(), 0),
            (returned.clone(), 0),
            recorded.clone(),
            recorded.clone(),
            (returned, 0),
            recorded,
        ]
    );
}
