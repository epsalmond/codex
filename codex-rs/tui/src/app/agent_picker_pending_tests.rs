use super::*;
use pretty_assertions::assert_eq;

const NOW: i64 = 1_800_000_000;

fn job(command: &str, age: i64, finished: bool) -> AgentPendingJob {
    AgentPendingJob {
        command: command.to_string(),
        started_at: NOW - age,
        finished,
        wakes_owner: true,
    }
}

#[test]
fn idle_label_counts_pending_jobs_only_once_listed() {
    let mut silent = job("c", 3, false);
    silent.wakes_owner = false;
    let jobs = [job("a", 1, false), job("b", 2, true), silent];
    assert_eq!(
        [
            idle_status_label(/*jobs*/ None),
            idle_status_label(Some(&[])),
            idle_status_label(Some(&jobs[..1])),
            idle_status_label(Some(&jobs)),
            idle_status_label(Some(&jobs[2..])),
        ],
        [
            "checking pending work",
            "idle · nothing pending",
            "waiting on 1 job",
            "waiting on 3 jobs",
            "waiting on 1 job",
        ]
    );
}

#[test]
fn pending_job_labels_use_command_then_process_then_cell() {
    let completion = |command: Option<&str>, process_id: Option<&str>, cell_id: Option<&str>| {
        pending_job(ThreadPendingCompletion {
            title: None,
            is_finite: true,
            id: "msg_1".to_string(),
            call_id: "call_1".to_string(),
            process_id: process_id.map(str::to_string),
            cell_id: cell_id.map(str::to_string),
            command: command.map(str::to_string),
            created_at: NOW,
            status: ThreadPendingCompletionStatus::Ready,
            wakes_owner: true,
        })
    };
    assert_eq!(
        completion(Some("bash -lc 'agent-job -- make'"), Some("7"), None),
        AgentPendingJob {
            command: "agent-job -- make".to_string(),
            started_at: NOW,
            finished: true,
            wakes_owner: true,
        }
    );
    assert_eq!(completion(None, Some("7"), None).command, "process 7");
    assert_eq!(
        completion(None, None, Some("cell-2")).command,
        "code cell cell-2"
    );
}

#[test]
fn pending_job_details_list_command_and_elapsed() {
    let details = AgentPickerRowDetails {
        context: None,
        jobs: vec![
            job("agent-job -- wait-for-build -- cargo build", 125, false),
            job("wait-for-status pr 222", 3_725, false),
            job("sleep 5", 30, true),
            AgentPendingJob {
                wakes_owner: false,
                ..job("npm test", 14, false)
            },
        ],
    };
    insta::assert_snapshot!(details.lines(NOW).join("\n"));
}

#[test]
fn pending_job_label_prefers_a_human_title_and_ignores_a_blank_title() {
    let completion = |title: &str| ThreadPendingCompletion {
        title: Some(title.into()),
        is_finite: true,
        id: "msg_1".into(),
        call_id: "call_1".into(),
        process_id: Some("7".into()),
        cell_id: None,
        command: Some("bash -lc 'cargo test'".into()),
        created_at: NOW,
        status: ThreadPendingCompletionStatus::Running,
        wakes_owner: true,
    };
    assert_eq!(
        pending_job(completion("Verify pending work lifecycle")).command,
        "Verify pending work lifecycle"
    );
    assert_eq!(pending_job(completion("  ")).command, "cargo test");
}
