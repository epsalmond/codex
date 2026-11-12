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

fn turn_idle_ids(agents: &[ListedAgent]) -> HashSet<ThreadId> {
    agents
        .iter()
        .filter(|agent| agent.is_idle())
        .map(|agent| agent.thread_id)
        .collect()
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
        names(idle_subtree_roots(
            &descendants,
            &turn_idle_ids(&descendants)
        )),
        vec!["/root/unloaded", "/root/parent/done"]
    );
}

#[test]
fn idle_selection_preserves_finite_work_and_its_idle_ancestors() {
    let descendants = vec![
        agent("/root/parent", Some(AgentStatus::Interrupted)),
        agent(
            "/root/parent/running_job",
            Some(AgentStatus::Completed(None)),
        ),
        agent("/root/parent/ready_job", Some(AgentStatus::Completed(None))),
        agent("/root/parent/idle_sibling", None),
        agent("/root/unknown", Some(AgentStatus::Completed(None))),
        agent("/root/idle", Some(AgentStatus::Completed(None))),
    ];
    // Runtime inspection withholds agents with retained running/ready finite records, and
    // agents whose runtime could not be inspected, even though their turns are idle.
    let idle = HashSet::from([
        descendants[0].thread_id,
        descendants[3].thread_id,
        descendants[5].thread_id,
    ]);
    assert_eq!(
        names(idle_subtree_roots(&descendants, &idle)),
        vec!["/root/parent/idle_sibling", "/root/idle"]
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
