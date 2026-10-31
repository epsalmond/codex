use super::*;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;

fn record(process_id: Option<i32>, cell_id: Option<&str>, ready: bool) -> PendingRecord {
    PendingRecord {
        id: "msg".to_owned(),
        call_id: "call".to_owned(),
        process_id,
        cell_id: cell_id.map(str::to_owned),
        command: process_id.map(|_| "sleep 60".to_owned()),
        created_at: 0,
        ready,
        wakes_owner: true,
    }
}

#[tokio::test]
async fn no_named_work_accepts_the_report() {
    let (_session, turn) = make_session_and_context().await;
    assert_eq!(
        pending_work_message(TerminalReportKind::Final, &[], &[], &turn),
        None
    );
}

#[tokio::test]
async fn finished_only_work_offers_delivery_without_stop_advice() {
    let (_session, turn) = make_session_and_context().await;
    assert_eq!(
        pending_work_message(
            TerminalReportKind::Final,
            &[record(None, Some("3"), /*ready*/ true)],
            &[],
            &turn,
        )
        .as_deref(),
        Some(
            "report_to_parent(kind=\"final\") was not recorded: your parent would not receive it until the work below has finished and its results have reached you.\nPending:\n- code cell 3 (finished; its result has not reached you yet)\nTo report, end your turn without calling report_to_parent; the results will be delivered to you, and you can report then."
        )
    );
}

#[tokio::test]
async fn stop_advice_names_only_running_kinds() {
    let (_session, turn) = make_session_and_context().await;
    let message = pending_work_message(
        TerminalReportKind::Blocked,
        &[
            record(None, Some("3"), /*ready*/ true),
            record(Some(1000), None, /*ready*/ false),
        ],
        &[],
        &turn,
    )
    .expect("running work rejects the report");
    let advice = message
        .split_once("To report")
        .map(|(_, advice)| advice)
        .expect("advice follows the list");
    assert!(
        message.contains("- process 1000: `sleep 60`\n"),
        "{message}"
    );
    assert!(
        advice.contains("`write_stdin` with chars \"\\u0003\" for processes"),
        "{advice}"
    );
    assert!(!advice.contains("code cells"), "{advice}");
}
