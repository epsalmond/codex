use super::*;
use crate::agent::types::AgentMetadata;
use pretty_assertions::assert_eq;

fn agent(path: &str, live_status: Option<AgentStatus>) -> ListedAgent {
    ListedAgent {
        thread_id: ThreadId::new(),
        metadata: AgentMetadata {
            agent_path: Some(AgentPath::try_from(path).expect("valid agent path")),
            ..Default::default()
        },
        live_status,
    }
}

fn names(agents: Vec<&ListedAgent>) -> Vec<String> {
    agents.into_iter().map(agent_name).collect()
}

#[test]
fn idle_selection_skips_busy_subtrees_and_covered_descendants() {
    let descendants = vec![
        // Unloaded: idle, with an idle child that closing it covers.
        agent("/root/unloaded", None),
        agent(
            "/root/unloaded/child",
            Some(AgentStatus::Completed(Some("done".to_string()))),
        ),
        // Idle itself, but a descendant is still running.
        agent("/root/parent", Some(AgentStatus::Interrupted)),
        agent("/root/parent/worker", Some(AgentStatus::Running)),
        agent(
            "/root/parent/done",
            Some(AgentStatus::Errored("boom".to_string())),
        ),
        agent("/root/waiting", Some(AgentStatus::Waiting)),
        agent("/root/starting", Some(AgentStatus::PendingInit)),
    ];

    assert_eq!(
        names(idle_subtree_roots(&descendants)),
        vec!["/root/unloaded", "/root/parent/done"]
    );
}

#[test]
fn topmost_drops_duplicates_and_selected_descendants() {
    let parent = agent("/root/a", None);
    let child = agent("/root/a/b", None);
    let sibling = agent("/root/ab", None);

    assert_eq!(
        names(topmost(vec![&child, &parent, &sibling, &parent])),
        vec!["/root/a", "/root/ab"]
    );
}
