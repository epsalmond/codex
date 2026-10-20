//! The one view of an agent's spawned descendants that every surface shares.
//!
//! Membership and lifecycle come from the persisted spawn edges, so idle children the
//! residency policy unloaded still count as open. Live status is overlaid for loaded threads.
//! Loaded threads with no persisted edge (ephemeral trees, no state DB) count as open.

use super::LocalAgentControl;
use crate::agent::types::AgentMetadata;
use crate::agent::types::ListedAgent;
use crate::agent::types::SpawnedDescendant;
use crate::agent::types::ThreadSpawnEdgeStatus;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use std::collections::HashMap;
use std::collections::HashSet;

impl ThreadManagerState {
    /// List every spawned descendant of `root_thread_id`, open and closed, breadth-first.
    ///
    /// An agent is `Closed` when its own edge or any edge above it is closed.
    pub(crate) async fn list_spawned_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<Vec<SpawnedDescendant>> {
        let mut edges = Vec::<(ThreadId, ThreadSpawnEdgeStatus)>::new();
        let mut seen = HashSet::from([root_thread_id]);
        if let Some(agent_graph_store) = self.agent_graph_store() {
            let load_error =
                |err| CodexErr::Fatal(format!("failed to load thread-spawn descendants: {err}"));
            let all = agent_graph_store
                .list_thread_spawn_descendants(root_thread_id, /*status_filter*/ None)
                .await
                .map_err(load_error)?;
            let open = agent_graph_store
                .list_thread_spawn_descendants(root_thread_id, Some(ThreadSpawnEdgeStatus::Open))
                .await
                .map_err(load_error)?
                .into_iter()
                .collect::<HashSet<_>>();
            for thread_id in all {
                if seen.insert(thread_id) {
                    let status = if open.contains(&thread_id) {
                        ThreadSpawnEdgeStatus::Open
                    } else {
                        ThreadSpawnEdgeStatus::Closed
                    };
                    edges.push((thread_id, status));
                }
            }
        }

        // Loaded children whose edge was never persisted.
        let mut live_children = HashMap::<ThreadId, Vec<ThreadId>>::new();
        for (parent, child) in self.list_live_thread_spawn_edges().await {
            live_children.entry(parent).or_default().push(child);
        }
        for children in live_children.values_mut() {
            children.sort_by_key(ToString::to_string);
        }
        let closed = edges
            .iter()
            .filter(|(_, status)| *status == ThreadSpawnEdgeStatus::Closed)
            .map(|(thread_id, _)| *thread_id)
            .collect::<HashSet<_>>();
        let mut queue = std::collections::VecDeque::from([(root_thread_id, false)]);
        while let Some((parent, parent_closed)) = queue.pop_front() {
            for child in live_children.remove(&parent).unwrap_or_default() {
                let child_closed = parent_closed || closed.contains(&child);
                if seen.insert(child) {
                    let status = if child_closed {
                        ThreadSpawnEdgeStatus::Closed
                    } else {
                        ThreadSpawnEdgeStatus::Open
                    };
                    edges.push((child, status));
                }
                queue.push_back((child, child_closed));
            }
        }

        let loaded = {
            let threads = self.threads.read().await;
            edges
                .iter()
                .map(|(thread_id, _)| {
                    threads
                        .get(thread_id)
                        .filter(|thread| !thread.session_source.is_internal())
                        .cloned()
                })
                .collect::<Vec<_>>()
        };
        let mut descendants = Vec::with_capacity(edges.len());
        for ((thread_id, edge_status), thread) in edges.into_iter().zip(loaded) {
            let live_status = match thread {
                Some(thread) => Some(thread.agent_status().await),
                None => None,
            };
            descendants.push(SpawnedDescendant {
                thread_id,
                edge_status,
                live_status,
            });
        }
        Ok(descendants)
    }
}

impl LocalAgentControl {
    /// List the tree root and its open descendants, filtered by task-path prefix.
    ///
    /// Open descendants come from [`ThreadManagerState::list_spawned_descendants`], so an idle
    /// agent that was unloaded is listed with no live status instead of disappearing.
    pub(crate) async fn list_open_agents(
        &self,
        resolved_prefix: Option<&AgentPath>,
    ) -> CodexResult<Vec<ListedAgent>> {
        let state = self.runtime.upgrade()?;
        let root_path = AgentPath::root();
        let Some(root_thread_id) = self.runtime.registry.agent_id_for_path(&root_path) else {
            return Ok(Vec::new());
        };
        let matches_prefix = |agent_path: Option<&AgentPath>| {
            resolved_prefix.is_none_or(|prefix| super::agent_matches_prefix(agent_path, prefix))
        };

        let mut agents = Vec::new();
        for descendant in state.list_spawned_descendants(root_thread_id).await? {
            if descendant.edge_status != ThreadSpawnEdgeStatus::Open {
                continue;
            }
            let metadata = self
                .get_agent_metadata(descendant.thread_id)
                .unwrap_or_else(|| AgentMetadata {
                    agent_id: Some(descendant.thread_id),
                    ..Default::default()
                });
            if !matches_prefix(metadata.agent_path.as_ref()) {
                continue;
            }
            agents.push(ListedAgent {
                thread_id: descendant.thread_id,
                metadata,
                live_status: descendant.live_status,
            });
        }
        agents.sort_by(|left, right| {
            left.metadata
                .agent_path
                .as_deref()
                .unwrap_or_default()
                .cmp(right.metadata.agent_path.as_deref().unwrap_or_default())
                .then_with(|| left.thread_id.to_string().cmp(&right.thread_id.to_string()))
        });

        if matches_prefix(Some(&root_path))
            && let Ok(root_thread) = state.get_thread(root_thread_id).await
        {
            agents.insert(
                0,
                ListedAgent {
                    thread_id: root_thread_id,
                    metadata: AgentMetadata {
                        agent_id: Some(root_thread_id),
                        agent_path: Some(root_path),
                        ..Default::default()
                    },
                    live_status: Some(root_thread.agent_status().await),
                },
            );
        }
        Ok(agents)
    }
}
