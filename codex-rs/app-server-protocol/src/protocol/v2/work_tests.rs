use super::*;
use crate::ClientRequest;
use crate::RequestId;
use crate::ServerNotification;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn work_lifecycle_requests_and_notifications_keep_v2_wire_names() {
    let subscribe = ClientRequest::ThreadWorkSubscribe {
        request_id: RequestId::Integer(1),
        params: ThreadWorkSubscribeParams {
            thread_id: "root-1".to_string(),
        },
    };
    assert_eq!(subscribe.method_name(), "thread/subscribeWorkState");
    assert_eq!(
        serde_json::to_value(subscribe).unwrap(),
        json!({
            "method": "thread/subscribeWorkState",
            "id": 1,
            "params": { "threadId": "root-1" }
        })
    );
    assert_eq!(
        serde_json::to_value(ThreadWorkSubscribeResponse {
            outcome: ThreadWorkSubscribeOutcome::Unavailable,
            snapshot: None,
        })
        .unwrap(),
        json!({
            "outcome": "unavailable",
            "snapshot": null
        })
    );

    let shutdown = ClientRequest::ThreadWorkShutdownIfQuiescent {
        request_id: RequestId::Integer(2),
        params: ThreadWorkShutdownIfQuiescentParams {
            thread_id: "root-1".to_string(),
            revision: "incarnation:19".to_string(),
        },
    };
    assert_eq!(
        serde_json::to_value(shutdown).unwrap(),
        json!({
            "method": "thread/shutdownIfQuiescent",
            "id": 2,
            "params": {
                "threadId": "root-1",
                "revision": "incarnation:19"
            }
        })
    );

    let notification = ServerNotification::ThreadWorkUpdated(ThreadWorkUpdatedNotification {
        thread_id: "root-1".to_string(),
        snapshot: ThreadWorkSnapshot {
            revision: "incarnation:20".to_string(),
            outstanding_work: 1,
            running_finite_work: 1,
            pending_notifications: 0,
            active_root_turns: 0,
            pending_terminal_outputs: 0,
            output_forwarding_observed: true,
            closed: false,
            quiescent: false,
        },
    });
    assert_eq!(
        serde_json::to_value(notification).unwrap(),
        json!({
            "method": "thread/workStateChanged",
            "params": {
                "threadId": "root-1",
                "snapshot": {
                    "revision": "incarnation:20",
                    "outstandingWork": 1,
                    "runningFiniteWork": 1,
                    "pendingNotifications": 0,
                    "activeRootTurns": 0,
                    "pendingTerminalOutputs": 0,
                    "outputForwardingObserved": true,
                    "closed": false,
                    "quiescent": false
                }
            }
        })
    );
}
