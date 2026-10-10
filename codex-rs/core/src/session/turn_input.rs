//! Handles reply-bearing turn-input operations.
//!
//! This is the one place Core decides whether submitted input starts a turn,
//! steers an active turn, or is rejected. It replies after that decision; it
//! does not wait for user-prompt hooks, updating the in-memory model context,
//! rollout persistence, or sampling.
//!
//! Persistent thread settings apply on Started and Steered. Turn start
//! options only update turn context on Started; input provenance follows each request.
//! Host shutdown admission is checked before reserving or starting a new turn.
//! Parent-delegated subagent input bypasses drain; automatic starts remain gated.
//! Realtime drain refusals are returned to the fanout for ordered session teardown.

use super::TurnInput;
use super::session::Session;
use super::session::SessionConfiguration;
use super::session::SessionSettingsUpdate;
use super::thread_settings;
use super::turn_context::NewTurnContextOptions;
use super::turn_context::TurnContext;
use super::wake_assignments::UnstartedTurnAssignment;
use crate::WithTurnExtensionData;
use crate::agent::control::WorkAdmissionError;
use crate::state::ActiveTurn;
use crate::state::TurnState;
use crate::tasks::RegularTask;
use crate::tasks::TaskStartOutcome;
use crate::tasks::TurnReservation;
use crate::tasks::TurnStartHoldPoint;
use crate::tasks::wait_if_turn_start_held;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::UserInputOrigin;
use codex_protocol::config_types::ModeKind;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::NonSteerableTurnKind;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::turn_input::NotSubmittedReason;
use codex_protocol::turn_input::TurnInput as SubmittedTurnInput;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;
use uuid::Uuid;

/// Longest time explicit input waits for a starting turn (slot reserved, task not installed yet)
/// before replacing it. A start normally takes milliseconds. Input runs on the submission loop,
/// so the bound keeps a reservation that never installs its task from also blocking later ops
/// such as `Interrupt` and `Shutdown`.
const STARTING_TURN_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

/// How many more times explicit input whose reservation another turn took before its task
/// started looks again for a turn to steer into or a free slot.
const LOST_RESERVATION_RETRIES: usize = 2;

#[cfg(test)]
#[path = "turn_input_tests.rs"]
mod tests;

/// Why input is starting a turn; shared by admission and input delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnStartKind {
    User,
    Automatic,
    Recovery,
}

impl TurnStartKind {
    fn permits_mode(self, mode: ModeKind) -> bool {
        match self {
            Self::User | Self::Recovery => true,
            Self::Automatic => mode != ModeKind::Plan,
        }
    }

    /// Automatic work may neither leave an existing Plan mode nor enter it.
    fn permits_settings(
        self,
        current: &SessionConfiguration,
        proposed: &SessionConfiguration,
    ) -> bool {
        self.permits_mode(current.step_settings.collaboration_mode.mode)
            && self.permits_mode(proposed.step_settings.collaboration_mode.mode)
    }
}

/// Input `start_or_steer` or `steer` still has to deliver.
enum ExplicitInput {
    /// As submitted, with its additional context not merged yet.
    Submitted {
        input: Box<SubmittedTurnInput>,
        additional_context: BTreeMap<String, AdditionalContextEntry>,
    },
    /// Prepared for a start whose reservation was lost before its task was installed: additional
    /// context merged and acceptance order reserved.
    Prepared(Vec<TurnInput>),
}

/// Thread settings and start-only options prepared before Core knows whether
/// turn input starts or steers.
///
/// Thread settings are validated up front but only applied after Core accepts
/// the input. Start-only options are only consumed by `apply_started`.
struct PreparedTurnInputSettings {
    thread_settings_update: Option<SessionSettingsUpdate>,
    start_options: TurnStartOptions,
}

impl PreparedTurnInputSettings {
    /// Validates turn-input settings without applying them so rejected input
    /// leaves the thread unchanged.
    async fn prepare(
        session: &Session,
        thread_settings: impl Into<WithTurnExtensionData<ThreadSettingsOverrides>>,
        start_options: TurnStartOptions,
    ) -> CodexResult<Self> {
        let thread_settings = thread_settings.into();
        let thread_settings_update = if thread_settings.request
            == ThreadSettingsOverrides::default()
            && thread_settings.turn_extension_init.is_none()
        {
            None
        } else {
            let updates = thread_settings::prepare_update(thread_settings);
            session
                .preview_settings(&updates)
                .await
                .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
            Some(updates)
        };
        Ok(Self {
            thread_settings_update,
            start_options,
        })
    }

    /// Takes these settings for a start and leaves what another attempt of the same input needs
    /// if that start loses its reservation: the start options, without the persistent settings
    /// the start applies.
    fn take_for_start(&mut self) -> Self {
        let retry = Self {
            thread_settings_update: None,
            start_options: self.start_options.clone(),
        };
        std::mem::replace(self, retry)
    }

    fn required_active_final_output_json_schema(&self) -> Option<&Value> {
        self.start_options.final_output_json_schema.as_ref()
    }

    /// Applies persistent settings and start-only options before creating a
    /// new turn context. Returns `None` if admission rejects the candidate,
    /// without committing its settings.
    async fn apply_started(
        self,
        session: &Arc<Session>,
        submission_id: String,
        kind: TurnStartKind,
    ) -> CodexResult<Option<Arc<TurnContext>>> {
        let TurnStartOptions {
            turn_trigger,
            final_output_json_schema,
            service_tier,
            parent_turn_id,
            root_turn_id,
            cyber_access_program,
        } = self.start_options;
        let emit_thread_settings_applied = self.thread_settings_update.is_some();
        let _settings_guard = if emit_thread_settings_applied {
            Some(thread_settings::acquire_persistence_lock(session).await)
        } else {
            None
        };
        let mut updates = self.thread_settings_update.unwrap_or_default();
        updates.service_tier_for_turn = service_tier;

        let options = NewTurnContextOptions {
            final_output_json_schema,
            cyber_access_program,
        };
        let turn_context = match kind {
            TurnStartKind::User | TurnStartKind::Recovery => Some(
                session
                    .new_turn_with_sub_id(submission_id.clone(), updates, options)
                    .await?,
            ),
            TurnStartKind::Automatic => {
                session
                    .new_turn_with_sub_id_if(
                        submission_id.clone(),
                        updates,
                        options,
                        |current, proposed| kind.permits_settings(current, proposed),
                    )
                    .await?
            }
        };
        let Some((turn_context, settings_snapshot)) = turn_context else {
            return Ok(None);
        };
        if let Some(turn_trigger) = turn_trigger {
            turn_context
                .turn_metadata_state
                .set_turn_trigger(turn_trigger);
        }
        if emit_thread_settings_applied {
            thread_settings::emit_applied(session, submission_id, settings_snapshot).await;
        }
        if let Some(parent_turn_id) = parent_turn_id {
            turn_context
                .turn_metadata_state
                .set_parent_turn_id(parent_turn_id);
        }
        if let Some(root_turn_id) = root_turn_id {
            turn_context
                .turn_metadata_state
                .set_root_turn_id(root_turn_id);
        }
        Ok(Some(turn_context))
    }

    /// Applies only persistent settings after steering succeeds. The active
    /// turn keeps its existing context; subsequent turns see the update.
    async fn apply_steered(self, session: &Session, submission_id: String) -> CodexResult<()> {
        let Some(thread_settings_update) = self.thread_settings_update else {
            return Ok(());
        };
        thread_settings::apply_update(session, submission_id, thread_settings_update)
            .await
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))
    }
}

#[tracing::instrument(
    name = "codex.turn_input",
    level = "trace",
    skip_all,
    fields(conversation.id = %session.thread_id, turn.id)
)]
pub(super) async fn handle(
    session: &Arc<Session>,
    request: impl Into<WithTurnExtensionData<TurnInputRequest>>,
    mode: TurnInputMode,
    submission_id: String,
) -> CodexResult<TurnInputSubmission> {
    let request = request.into();
    let result = match mode {
        TurnInputMode::StartOrSteer => start_or_steer(session, request, submission_id).await,
        TurnInputMode::StartIfIdle => {
            let kind = match &request.request.input {
                SubmittedTurnInput::UserInput { content, .. } if !content.is_empty() => {
                    TurnStartKind::User
                }
                SubmittedTurnInput::UserInput { .. }
                | SubmittedTurnInput::ResponseItem(_)
                | SubmittedTurnInput::InterAgentCommunication(_) => TurnStartKind::Automatic,
            };
            start_if_idle(
                session,
                request,
                submission_id,
                kind,
                /*expected_previous_turn_id*/ None,
            )
            .await
        }
        TurnInputMode::ContinueIfIdle {
            expected_previous_turn_id,
        } => {
            if !matches!(&request.request.input, SubmittedTurnInput::ResponseItem(_)) {
                return Err(CodexErr::InvalidRequest(
                    "continuation requires internal response input".to_string(),
                ));
            }
            start_if_idle(
                session,
                request,
                submission_id,
                TurnStartKind::Recovery,
                Some(expected_previous_turn_id),
            )
            .await
        }
        TurnInputMode::Steer { expected_turn_id } => {
            steer(session, request, expected_turn_id, submission_id).await
        }
    };
    // Link this request's trace to the accepted turn, which may have an older trace.
    if let Ok(TurnInputSubmission::Started { turn_id } | TurnInputSubmission::Steered { turn_id }) =
        &result
    {
        tracing::Span::current().record("turn.id", turn_id);
    }
    result
}

#[tracing::instrument(
    name = "codex.turn_input",
    level = "trace",
    skip_all,
    fields(conversation.id = %session.thread_id, turn.id)
)]
pub(super) async fn handle_recovery(
    session: &Arc<Session>,
    thread_settings: impl Into<WithTurnExtensionData<ThreadSettingsOverrides>>,
    start_options: TurnStartOptions,
    submission_id: String,
) -> CodexResult<TurnInputSubmission> {
    let WithTurnExtensionData {
        request: thread_settings,
        turn_extension_init,
    } = thread_settings.into();
    let request = TurnInputRequest::user_input(Vec::new())
        .with_thread_settings(thread_settings)
        .on_start(TurnStartOptions {
            turn_trigger: Some("retry".to_string()),
            ..start_options
        });
    let result = start_if_idle(
        session,
        WithTurnExtensionData {
            request,
            turn_extension_init,
        },
        submission_id,
        TurnStartKind::Recovery,
        /*expected_previous_turn_id*/ None,
    )
    .await;
    if let Ok(TurnInputSubmission::Started { turn_id }) = &result {
        tracing::Span::current().record("turn.id", turn_id);
    }
    result
}

/// Maps a reserved start's outcome to its submission. Only a turn whose task was installed is
/// reported as started.
fn started_submission(
    outcome: TaskStartOutcome,
    turn_id: String,
) -> CodexResult<TurnInputSubmission> {
    Ok(match outcome {
        TaskStartOutcome::Started => TurnInputSubmission::Started { turn_id },
        TaskStartOutcome::Rejected(error) => TurnInputSubmission::NotSubmitted {
            reason: not_submitted_reason(&error),
        },
        // Another turn took the slot, or an interrupt cleared it, before the task was installed.
        // The turn ended with `TurnAborted` and only reserved mail went back to the mailbox, so
        // the caller keeps its own input instead of losing it with a turn that never ran.
        TaskStartOutcome::Aborted | TaskStartOutcome::Lost(_) => {
            TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::NotIdle,
            }
        }
    })
}

fn not_submitted_reason(error: &WorkAdmissionError) -> NotSubmittedReason {
    match error {
        WorkAdmissionError::Closed => NotSubmittedReason::ServerDraining,
        WorkAdmissionError::CapacityReached => NotSubmittedReason::RootTurnCapacityReached,
    }
}

async fn start_or_steer(
    session: &Arc<Session>,
    request: WithTurnExtensionData<TurnInputRequest>,
    submission_id: String,
) -> CodexResult<TurnInputSubmission> {
    let WithTurnExtensionData {
        request,
        turn_extension_init,
    } = request;
    let TurnInputRequest {
        input,
        thread_settings,
        start,
        additional_context,
        responsesapi_client_metadata,
        ..
    } = request;
    let origin = UserInputOrigin::from_turn_trigger(start.turn_trigger.as_deref());
    match &input {
        SubmittedTurnInput::UserInput { .. }
        | SubmittedTurnInput::ResponseItem(ResponseItem::FunctionCallOutput {
            call_id: None,
            ..
        }) => {}
        _ => {
            return Err(CodexErr::InvalidRequest(
                "only user input or standalone function-call outputs can start or steer a turn"
                    .to_string(),
            ));
        }
    }
    let mut settings = PreparedTurnInputSettings::prepare(
        session,
        WithTurnExtensionData {
            request: thread_settings,
            turn_extension_init,
        },
        start,
    )
    .await?;
    // MAv1 sends explicit input to spawned agents as part of an existing
    // parent's work. Client RPCs are gated separately by the host.
    let is_delegated_input = settings.start_options.parent_turn_id.is_some()
        && matches!(
            session
                .state
                .lock()
                .await
                .session_configuration
                .session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        );
    let mut input = ExplicitInput::Submitted {
        input: Box::new(input),
        additional_context,
    };
    let mut retries_left = LOST_RESERVATION_RETRIES;
    // Input never races a starting turn, one that reserved the slot without installing its task
    // yet, such as an automatic wake binding its assignment. It steers into that turn once it
    // runs, or starts after the turn releases the slot. A finishing turn, or a start that outlives
    // the wait, is replaced as before. A free slot is reserved before this turn binds its
    // assignment. One deadline covers every attempt, so retries cannot stretch how long this
    // input holds the submission loop.
    let deadline = Instant::now() + STARTING_TURN_WAIT_TIMEOUT;
    loop {
        let (turn_state, assignment_guard, _admission) = loop {
            match session
                .steer_input(
                    &mut input,
                    /*expected_turn_id*/ None,
                    settings.required_active_final_output_json_schema(),
                    responsesapi_client_metadata.clone(),
                    origin,
                )
                .await
            {
                Ok(turn_id) => {
                    settings.apply_steered(session, submission_id).await?;
                    return Ok(TurnInputSubmission::Steered { turn_id });
                }
                Err(NotSubmittedReason::NoActiveTurn) => {}
                Err(reason) => return Ok(TurnInputSubmission::NotSubmitted { reason }),
            }
            let mut task_installed = {
                let mut active_turn = session.active_turn.lock().await;
                let starting = match active_turn.as_ref() {
                    Some(turn) if turn.task.is_some() => continue,
                    Some(turn) if !turn.finishing && Instant::now() < deadline => {
                        Some(turn.task_installed.subscribe())
                    }
                    None | Some(_) => None,
                };
                match starting {
                    Some(task_installed) => task_installed,
                    None => {
                        let admission = if is_delegated_input {
                            None
                        } else {
                            let Some(admission) = session.services.extensions.admit_turn_start()
                            else {
                                return Ok(TurnInputSubmission::NotSubmitted {
                                    reason: NotSubmittedReason::ServerDraining,
                                });
                            };
                            Some(admission)
                        };
                        // From here until its task is installed, every exit gives back the
                        // assignment this turn holds, including one handed over below.
                        let assignment_guard =
                            session.guard_unstarted_turn_assignment(&submission_id);
                        // A replaced start's own bind, release or task start later finds this
                        // reservation and leaves it alone. It binds only while it holds the slot,
                        // under this lock, so handing its binding over here, under the same lock,
                        // leaves no window in which either start can bind against the other.
                        if let Some(replaced_turn_id) = active_turn
                            .take()
                            .filter(|turn| !turn.finishing)
                            .and_then(|turn| turn.reserved_turn_id)
                        {
                            session.services.local_agent_runtime.hand_off_wake_turn(
                                session.thread_id,
                                &replaced_turn_id,
                                &submission_id,
                            );
                        }
                        let reserved = active_turn.insert(ActiveTurn {
                            reserved_turn_id: Some(submission_id.clone()),
                            ..ActiveTurn::default()
                        });
                        break (
                            Arc::clone(&reserved.turn_state),
                            assignment_guard,
                            admission,
                        );
                    }
                }
            };
            // The sender closes when the starting turn releases the slot.
            let _ = timeout_at(deadline, task_installed.changed()).await;
        };
        if let Some(submission) = start_reserved(
            session,
            ReservedExplicitStart {
                turn_state,
                assignment_guard,
                settings: settings.take_for_start(),
                submission_id: &submission_id,
                origin,
                responsesapi_client_metadata: responsesapi_client_metadata.as_ref(),
            },
            &mut input,
        )
        .await?
        {
            return Ok(submission);
        }
        // Another turn took the slot, or an interrupt cleared it, before this turn's task was
        // installed. Nothing announced this turn, so look again for a turn to steer into or a
        // free slot rather than hand the caller back its input.
        if retries_left == 0 {
            return Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::NotIdle,
            });
        }
        retries_left -= 1;
    }
}

/// What `start_reserved` needs besides the input it may hand back.
struct ReservedExplicitStart<'a> {
    turn_state: Arc<tokio::sync::Mutex<TurnState>>,
    assignment_guard: UnstartedTurnAssignment<'a>,
    settings: PreparedTurnInputSettings,
    submission_id: &'a str,
    origin: UserInputOrigin,
    responsesapi_client_metadata: Option<&'a HashMap<String, String>>,
}

/// Starts explicit input in the slot `start_or_steer` reserved for it. Returns `None` if the
/// reservation is replaced or released before the task is installed: no `TurnAborted` announces
/// the turn, and `input` holds what is left to deliver.
async fn start_reserved(
    session: &Arc<Session>,
    start: ReservedExplicitStart<'_>,
    input: &mut ExplicitInput,
) -> CodexResult<Option<TurnInputSubmission>> {
    let ReservedExplicitStart {
        turn_state,
        assignment_guard,
        settings,
        submission_id,
        origin,
        responsesapi_client_metadata,
    } = start;
    let turn_context = match settings
        .apply_started(session, submission_id.to_string(), TurnStartKind::User)
        .await
    {
        Ok(Some(turn_context)) => turn_context,
        Ok(None) => unreachable!("explicit user input can enter Plan mode"),
        Err(error) => {
            session.clear_reserved_idle_turn(&turn_state).await;
            return Err(error);
        }
    };
    let held =
        wait_if_turn_start_held(session.thread_id, TurnStartHoldPoint::InputBeforeBind).await;
    match session
        .bind_in_reservation(&turn_state, || {
            if held.injected_failure() {
                return Err(CodexErr::InvalidRequest(
                    "turn start failed at a test hold point".to_string(),
                ));
            }
            session.bind_wake_assignment(&turn_context, /*allow_new_generation*/ true)
        })
        .await
    {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            session.clear_reserved_idle_turn(&turn_state).await;
            return Err(error);
        }
        // Replaced or interrupted before it bound anything.
        None => return Ok(None),
    }
    if let Err(error) = session.register_root_turn_lifecycle(turn_context.as_ref()) {
        tracing::warn!(%error, "root turn admission rejected by lifecycle coordinator");
        session.clear_reserved_idle_turn(&turn_state).await;
        return Ok(Some(TurnInputSubmission::NotSubmitted {
            reason: not_submitted_reason(&error),
        }));
    }
    if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
        turn_context
            .turn_metadata_state
            .set_responsesapi_client_metadata(responsesapi_client_metadata.clone());
    }
    session
        .maybe_emit_model_warnings_for_turn(turn_context.as_ref())
        .await;
    let task_input = match std::mem::replace(input, ExplicitInput::Prepared(Vec::new())) {
        ExplicitInput::Submitted {
            input: submitted,
            additional_context,
        } => {
            let submitted = *submitted;
            let has_explicit_input = match &submitted {
                SubmittedTurnInput::UserInput { content, .. } => {
                    turn_context.session_telemetry.user_prompt(content);
                    !content.is_empty()
                }
                SubmittedTurnInput::ResponseItem(_)
                | SubmittedTurnInput::InterAgentCommunication(_) => true,
            };
            let mut task_input = merge_additional_context_input(session, additional_context).await;
            if has_explicit_input {
                task_input.push(
                    pending_turn_input(session, submitted, &turn_context.sub_id, origin).await,
                );
            }
            task_input
        }
        ExplicitInput::Prepared(task_input) => task_input,
    };
    let resumes_wakeups = task_input
        .iter()
        .any(|item| matches!(item, TurnInput::UserInput { .. }))
        && session.input_queue.wakeups_paused();
    session.clear_connector_selection().await;
    if resumes_wakeups {
        // The reservation above keeps held mail from starting a wake turn once the pause clears.
        session.resume_paused_wakeups().await;
    }
    let _held =
        wait_if_turn_start_held(session.thread_id, TurnStartHoldPoint::InputBeforeInstall).await;
    let reservation = TurnReservation {
        turn_state,
        mail_start_options: TurnStartOptions::default(),
        return_input_if_lost: true,
    };
    Ok(
        match session
            .start_task(turn_context, task_input, RegularTask::new(), reservation)
            .await
        {
            TaskStartOutcome::Started => {
                assignment_guard.started();
                Some(TurnInputSubmission::Started {
                    turn_id: submission_id.to_string(),
                })
            }
            TaskStartOutcome::Rejected(error) => Some(TurnInputSubmission::NotSubmitted {
                reason: not_submitted_reason(&error),
            }),
            TaskStartOutcome::Lost(task_input) => {
                *input = ExplicitInput::Prepared(task_input);
                None
            }
            // A reservation that asks for its input back is never aborted this way.
            TaskStartOutcome::Aborted => Some(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::NotIdle,
            }),
        },
    )
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "the previous turn check and idle reservation must be atomic"
)]
async fn start_if_idle(
    session: &Arc<Session>,
    request: WithTurnExtensionData<TurnInputRequest>,
    submission_id: String,
    kind: TurnStartKind,
    expected_previous_turn_id: Option<String>,
) -> CodexResult<TurnInputSubmission> {
    let WithTurnExtensionData {
        request,
        turn_extension_init,
    } = request;
    let TurnInputRequest {
        input,
        thread_settings,
        start,
        additional_context,
        responsesapi_client_metadata,
        ..
    } = request;
    let origin = UserInputOrigin::from_turn_trigger(start.turn_trigger.as_deref());
    if session.input_queue.has_trigger_turn_mailbox_items().await {
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PendingTriggerTurn,
        });
    }
    // Preserve current-Plan rejection before reservation and settings errors.
    // The commit-time decision also checks the proposed mode.
    if kind == TurnStartKind::Automatic
        && !kind.permits_mode(session.collaboration_mode().await.mode)
    {
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PlanMode,
        });
    }

    let _admission = session.services.extensions.admit_turn_start();
    // A one-shot review delegate completes its already-running parent's work.
    // Its explicit input carries parent lineage; automatic starts do not qualify.
    if _admission.is_none()
        && !(kind == TurnStartKind::User
            && start.parent_turn_id.is_some()
            && matches!(
                session
                    .state
                    .lock()
                    .await
                    .session_configuration
                    .session_source,
                SessionSource::SubAgent(SubAgentSource::Review)
            ))
    {
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::ServerDraining,
        });
    }

    let turn_state = {
        let mut active_turn = session.active_turn.lock().await;
        if active_turn.is_some() {
            return Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::NotIdle,
            });
        }
        if let Some(expected) = expected_previous_turn_id
            && session.state.lock().await.last_started_turn_id.as_ref() != Some(&expected)
        {
            return Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::Superseded,
            });
        }
        let active_turn = active_turn.insert(ActiveTurn {
            reserved_turn_id: Some(submission_id.clone()),
            ..ActiveTurn::default()
        });
        Arc::clone(&active_turn.turn_state)
    };
    // Every exit before the task is installed gives back the assignment this turn binds.
    let assignment_guard = session.guard_unstarted_turn_assignment(&submission_id);

    if session.input_queue.has_trigger_turn_mailbox_items().await {
        session.clear_reserved_idle_turn(&turn_state).await;
        session.maybe_start_turn_for_pending_work().await;
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PendingTriggerTurn,
        });
    }

    let settings = match PreparedTurnInputSettings::prepare(
        session,
        WithTurnExtensionData {
            request: thread_settings,
            turn_extension_init,
        },
        start,
    )
    .await
    {
        Ok(settings) => settings,
        Err(error) => {
            session.clear_reserved_idle_turn(&turn_state).await;
            return Err(error);
        }
    };
    let turn_context = match settings
        .apply_started(session, submission_id.clone(), kind)
        .await
    {
        Ok(Some(turn_context)) => turn_context,
        Ok(None) => {
            session.clear_reserved_idle_turn(&turn_state).await;
            return Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::PlanMode,
            });
        }
        Err(error) => {
            session.clear_reserved_idle_turn(&turn_state).await;
            return Err(error);
        }
    };
    let allow_new_generation = kind != TurnStartKind::Automatic
        || !matches!(
            &input,
            SubmittedTurnInput::InterAgentCommunication(communication)
                if communication.id.as_ref().is_some_and(|id| id.as_str().starts_with("amsg_"))
        );
    let _held =
        wait_if_turn_start_held(session.thread_id, TurnStartHoldPoint::InputBeforeBind).await;
    match session
        .bind_in_reservation(&turn_state, || {
            session.bind_wake_assignment(&turn_context, allow_new_generation)
        })
        .await
    {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            session.clear_reserved_idle_turn(&turn_state).await;
            return Err(error);
        }
        // Replaced or interrupted before it bound anything. The caller keeps its input.
        None => {
            return Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::NotIdle,
            });
        }
    }
    if let Err(error) = session.register_root_turn_lifecycle(turn_context.as_ref()) {
        tracing::warn!(%error, "root turn admission rejected by lifecycle coordinator");
        session.clear_reserved_idle_turn(&turn_state).await;
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: not_submitted_reason(&error),
        });
    }
    if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
        turn_context
            .turn_metadata_state
            .set_responsesapi_client_metadata(responsesapi_client_metadata);
    }
    session
        .maybe_emit_model_warnings_for_turn(turn_context.as_ref())
        .await;

    let mut task_input = merge_additional_context_input(session, additional_context).await;
    match kind {
        TurnStartKind::User => {
            session.clear_connector_selection().await;
            if let SubmittedTurnInput::UserInput { content, .. } = &input {
                turn_context.session_telemetry.user_prompt(content);
            }
            session.resume_paused_wakeups().await;
            task_input.push(pending_turn_input(session, input, &turn_context.sub_id, origin).await);
        }
        TurnStartKind::Automatic | TurnStartKind::Recovery => {
            // Empty automatic user input resumes sampling without a new message.
            if !matches!(&input, SubmittedTurnInput::UserInput { .. }) {
                session
                    .input_queue
                    .extend_pending_input_for_turn_state(
                        turn_state.as_ref(),
                        vec![
                            pending_turn_input(session, input, &turn_context.sub_id, origin).await,
                        ],
                    )
                    .await;
            }
        }
    }
    let _held =
        wait_if_turn_start_held(session.thread_id, TurnStartHoldPoint::InputBeforeInstall).await;
    let reservation = TurnReservation {
        turn_state,
        mail_start_options: TurnStartOptions::default(),
        return_input_if_lost: false,
    };
    let outcome = session
        .start_task(turn_context, task_input, RegularTask::new(), reservation)
        .await;
    if matches!(outcome, TaskStartOutcome::Started) {
        assignment_guard.started();
    }
    started_submission(outcome, submission_id)
}

async fn steer(
    session: &Arc<Session>,
    request: WithTurnExtensionData<TurnInputRequest>,
    expected_turn_id: String,
    submission_id: String,
) -> CodexResult<TurnInputSubmission> {
    let WithTurnExtensionData {
        request,
        turn_extension_init,
    } = request;
    let TurnInputRequest {
        input,
        thread_settings,
        start,
        additional_context,
        responsesapi_client_metadata,
        ..
    } = request;
    let origin = UserInputOrigin::from_turn_trigger(start.turn_trigger.as_deref());
    if !matches!(&input, SubmittedTurnInput::UserInput { .. }) {
        return Err(CodexErr::InvalidRequest(
            "only user input can steer a turn".to_string(),
        ));
    }
    let settings = PreparedTurnInputSettings::prepare(
        session,
        WithTurnExtensionData {
            request: thread_settings,
            turn_extension_init,
        },
        start,
    )
    .await?;
    let mut input = ExplicitInput::Submitted {
        input: Box::new(input),
        additional_context,
    };
    match session
        .steer_input(
            &mut input,
            Some(expected_turn_id.as_str()),
            settings.required_active_final_output_json_schema(),
            responsesapi_client_metadata,
            origin,
        )
        .await
    {
        Ok(turn_id) => {
            settings.apply_steered(session, submission_id).await?;
            Ok(TurnInputSubmission::Steered { turn_id })
        }
        Err(reason) => Ok(TurnInputSubmission::NotSubmitted { reason }),
    }
}

impl Session {
    /// Called under the active-turn lock before running any task or lifecycle callback.
    pub(crate) async fn record_started_turn(&self, turn_id: &str) {
        self.state.lock().await.last_started_turn_id = Some(turn_id.to_string());
    }

    pub(crate) async fn route_realtime_text_input(
        self: &Arc<Self>,
        text: String,
    ) -> Result<(), &'static str> {
        let submission_id = Uuid::now_v7().to_string();
        let submission = handle(
            self,
            TurnInputRequest::user_input(vec![UserInput::Text {
                text,
                text_elements: Vec::new(),
            }])
            .on_start(TurnStartOptions {
                turn_trigger: Some("realtime".to_string()),
                ..Default::default()
            }),
            TurnInputMode::StartOrSteer,
            submission_id.clone(),
        )
        .await;
        match submission {
            Ok(TurnInputSubmission::Started { .. } | TurnInputSubmission::Steered { .. }) => {}
            Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::ServerDraining,
            }) => {
                return Err("Server is draining; retry the turn after reconnecting");
            }
            Ok(TurnInputSubmission::NotSubmitted { reason }) => {
                self.send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(ErrorEvent {
                        misalignment: None,
                        message: format!("failed to submit turn input: {reason:?}"),
                        codex_error_info: Some(CodexErrorInfo::BadRequest),
                    }),
                })
                .await;
            }
            Err(error) => {
                self.send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(error.to_error_event(/*message_prefix*/ None)),
                })
                .await;
            }
        }
        Ok(())
    }

    /// Runs `bind` only while `turn_state` still holds the active-turn slot without a task, and
    /// returns `None` when it no longer does.
    ///
    /// Binding is synchronous, so the ownership check and the bind happen under one hold of the
    /// active-turn lock. Input that replaces a reservation hands that reservation's binding over
    /// under the same lock, so a reserving start either binds before it is replaced, and the
    /// replacing turn takes the binding over, or finds it was replaced and binds nothing.
    pub(crate) async fn bind_in_reservation<T>(
        &self,
        turn_state: &Arc<tokio::sync::Mutex<TurnState>>,
        bind: impl FnOnce() -> T,
    ) -> Option<T> {
        let active_turn = self.active_turn.lock().await;
        let owns_reservation = active_turn
            .as_ref()
            .is_some_and(|turn| turn.task.is_none() && Arc::ptr_eq(&turn.turn_state, turn_state));
        let bound = owns_reservation.then(bind);
        drop(active_turn);
        bound
    }

    pub(crate) async fn clear_reserved_idle_turn(
        &self,
        turn_state: &Arc<tokio::sync::Mutex<TurnState>>,
    ) {
        let mut active_turn_guard = self.active_turn.lock().await;
        if let Some(active_turn) = active_turn_guard.as_ref()
            && active_turn.task.is_none()
            && Arc::ptr_eq(&active_turn.turn_state, turn_state)
        {
            *active_turn_guard = None;
        }
    }

    /// Inject additional user input or a standalone tool output into the active turn.
    ///
    /// Returns the active turn id when accepted.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    async fn steer_input(
        &self,
        input: &mut ExplicitInput,
        expected_turn_id: Option<&str>,
        required_final_output_json_schema: Option<&Value>,
        responsesapi_client_metadata: Option<HashMap<String, String>>,
        origin: UserInputOrigin,
    ) -> Result<String, NotSubmittedReason> {
        let mut active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_mut() else {
            return Err(NotSubmittedReason::NoActiveTurn);
        };

        let Some(active_task) = active_turn.task.as_ref() else {
            return Err(NotSubmittedReason::NoActiveTurn);
        };
        let active_turn_id = &active_task.turn_context.sub_id;

        if let Some(expected_turn_id) = expected_turn_id
            && expected_turn_id != active_turn_id
        {
            return Err(NotSubmittedReason::ExpectedTurnMismatch {
                expected: expected_turn_id.to_string(),
                actual: active_turn_id.clone(),
            });
        }

        match active_task.kind {
            crate::state::TaskKind::Regular => {}
            crate::state::TaskKind::Review => {
                return Err(NotSubmittedReason::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Review,
                });
            }
            crate::state::TaskKind::Compact => {
                return Err(NotSubmittedReason::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Compact,
                });
            }
        }

        let is_empty = match &*input {
            ExplicitInput::Submitted { input, .. } => matches!(
                input.as_ref(),
                SubmittedTurnInput::UserInput { content, .. } if content.is_empty()
            ),
            ExplicitInput::Prepared(items) => !items.iter().any(|item| {
                matches!(
                    item,
                    TurnInput::UserInput { .. } | TurnInput::FunctionCallOutput(_)
                )
            }),
        };
        if is_empty {
            return Err(NotSubmittedReason::EmptyInput);
        }
        // Compare JSON values directly instead of serialized schema text.
        // Value equality ignores object key order while preserving array and
        // scalar distinctions; broader JSON Schema equivalence is out of scope.
        if let Some(required_schema) = required_final_output_json_schema
            && active_task.turn_context.final_output_json_schema.as_ref() != Some(required_schema)
        {
            return Err(NotSubmittedReason::ActiveTurnOutputSchemaMismatch);
        }

        if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
            active_task
                .turn_context
                .turn_metadata_state
                .set_responsesapi_client_metadata(responsesapi_client_metadata);
        }

        let (pending_input, is_user_input) = match input {
            ExplicitInput::Submitted {
                input,
                additional_context,
            } => {
                let mut pending_input =
                    merge_additional_context_input(self, std::mem::take(additional_context)).await;
                let is_user_input = matches!(input.as_ref(), SubmittedTurnInput::UserInput { .. });
                let input = match input.as_mut() {
                    SubmittedTurnInput::UserInput { content, client_id } => {
                        active_task
                            .turn_context
                            .session_telemetry
                            .user_prompt(content);
                        TurnInput::UserInput {
                            content: std::mem::take(content),
                            client_id: client_id.clone(),
                            metadata: super::UserInputMetadata {
                                acceptance_order: Some(self.reserve_user_input_order().await),
                                origin,
                            },
                        }
                    }
                    input => pending_turn_input(self, input.clone(), active_turn_id, origin).await,
                };
                pending_input.push(input);
                (pending_input, is_user_input)
            }
            // Its context, acceptance order and telemetry were taken when it first started.
            ExplicitInput::Prepared(items) => {
                let items = std::mem::take(items);
                let is_user_input = items
                    .iter()
                    .any(|item| matches!(item, TurnInput::UserInput { .. }));
                (items, is_user_input)
            }
        };
        // A user message clears an Esc pause. Clearing it while this turn cannot take input yet
        // lets the turn drain the held results together with the message.
        let resumed_wakeups = is_user_input && self.input_queue.resume_wakeups();
        self.input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                active_turn.turn_state.as_ref(),
                pending_input,
            )
            .await;
        let active_turn_id = active_turn_id.clone();
        drop(active);
        if resumed_wakeups {
            self.emit_agent_wakeups_updated().await;
        }
        Ok(active_turn_id)
    }
}

async fn merge_additional_context_input(
    session: &Session,
    additional_context: BTreeMap<String, AdditionalContextEntry>,
) -> Vec<TurnInput> {
    let additional_context_input = {
        let mut state = session.state.lock().await;
        state.additional_context.merge(additional_context)
    };
    additional_context_input
        .into_iter()
        .map(|item| session.annotate_client_response_item(item))
        .map(TurnInput::ResponseItem)
        .collect()
}

async fn pending_turn_input(
    session: &Session,
    input: SubmittedTurnInput,
    turn_id: &str,
    origin: UserInputOrigin,
) -> TurnInput {
    match input {
        SubmittedTurnInput::UserInput { content, client_id } => TurnInput::UserInput {
            content,
            client_id,
            metadata: super::UserInputMetadata {
                acceptance_order: Some(session.reserve_user_input_order().await),
                origin,
            },
        },
        SubmittedTurnInput::ResponseItem(mut item)
            if matches!(
                &item,
                ResponseItem::FunctionCallOutput { call_id: None, .. }
            ) =>
        {
            Session::assign_missing_response_item_id(&mut item);
            let metadata = if let Some(messages) = session
                .services
                .local_agent_runtime
                .capture_sender_user_messages(&item, session.thread_id, turn_id)
                .await
            {
                Some(CodexHarnessMetadata {
                    user_input_order: Some(session.reserve_user_input_order().await),
                    sender_user_messages: Some(Box::new(messages)),
                    ..Default::default()
                })
            } else {
                None
            };
            TurnInput::FunctionCallOutput(ResponseItemEnvelope { item, metadata })
        }
        SubmittedTurnInput::ResponseItem(item) => TurnInput::ResponseItem(item.into()),
        SubmittedTurnInput::InterAgentCommunication(communication) => {
            TurnInput::InterAgentCommunication(communication)
        }
    }
}
