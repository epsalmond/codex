use super::*;
use pretty_assertions::assert_eq;

const NOW: i64 = 1_800_000_000;

fn job(command: &str, age: i64, finished: bool) -> AgentPendingJob {
    AgentPendingJob {
        command: command.to_string(),
        started_at: NOW - age,
        finished,
    }
}

#[test]
fn idle_label_counts_pending_jobs_only_once_listed() {
    let jobs = [job("a", 1, false), job("b", 2, true)];
    assert_eq!(
        [
            idle_status_label(/*jobs*/ None),
            idle_status_label(Some(&[])),
            idle_status_label(Some(&jobs[..1])),
            idle_status_label(Some(&jobs)),
        ],
        [
            "idle",
            "idle · nothing pending",
            "idle · waiting on 1 job",
            "idle · waiting on 2 jobs",
        ]
    );
}

#[test]
fn pending_job_labels_use_command_then_process_then_cell() {
    let completion = |command: Option<&str>, process_id: Option<&str>, cell_id: Option<&str>| {
        pending_job(ThreadPendingCompletion {
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
        ],
    };
    insta::assert_snapshot!(details.lines(NOW).join("\n"));
}
