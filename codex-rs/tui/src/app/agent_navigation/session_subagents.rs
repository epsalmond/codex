//! Root session counts and local picker dismissal share the same descendant scope.
use super::AgentNavigationState;
use crate::multi_agents::AgentPickerThreadEntry;
use crate::multi_agents::SubagentCounts;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;

impl AgentNavigationState {
    /// Records backend spawn evidence for legacy descendants that lack agent paths.
    pub(crate) fn mark_subagent(&mut self, thread_id: ThreadId) {
        self.subagent_threads.insert(thread_id);
    }

    /// Records a live spawn whose first turn has not been observed yet.
    pub(crate) fn mark_spawned(&mut self, thread_id: ThreadId) {
        if !self
            .threads
            .get(&thread_id)
            .is_some_and(|entry| entry.is_running || entry.is_closed)
        {
            self.note_awaiting_first_turn(thread_id);
        }
    }

    /// Counts open descendants of the root session, including idle/error threads.
    /// Local dismissal affects navigation only; backend archives close agents and drop them.
    pub(crate) fn subagent_counts(&self, root: Option<ThreadId>) -> SubagentCounts {
        let Some(root) = root else {
            return SubagentCounts::default();
        };
        self.session_descendants(root)
            .filter(|(_, entry)| !entry.is_closed)
            .fold(SubagentCounts::default(), |mut counts, (_, entry)| {
                counts.total += 1;
                counts.running += usize::from(entry.is_running);
                counts
            })
    }

    /// Hides inactive descendants locally until they next become running or are messaged.
    /// Inactive means finished, errored, or closed; a spawned child awaiting its first turn
    /// is not inactive even though no turn has marked it running yet.
    /// Each row is independent, so an idle parent's running grandchild remains visible.
    pub(crate) fn dismiss_inactive_subagents(&mut self, root: ThreadId) {
        let inactive = self
            .session_descendants(root)
            .filter(|(thread_id, entry)| {
                entry.is_closed
                    || (!entry.is_running && !self.awaiting_first_turn.contains(thread_id))
            })
            .map(|(thread_id, _)| thread_id)
            .collect::<Vec<_>>();
        self.dismissed_threads.extend(inactive);
    }

    /// Open, idle descendants to close for "close all idle": rows that are not running or
    /// awaiting a first turn and have no such descendant. Archiving closes a whole subtree, so
    /// descendants of another target are left out.
    pub(crate) fn idle_close_targets(&self, root: ThreadId) -> Vec<ThreadId> {
        let open = self
            .session_descendants(root)
            .filter(|(_, entry)| !entry.is_closed)
            .collect::<Vec<_>>();
        let is_busy = |thread_id: &ThreadId, entry: &AgentPickerThreadEntry| {
            entry.is_running || self.awaiting_first_turn.contains(thread_id)
        };
        let busy_paths = open
            .iter()
            .filter(|(thread_id, entry)| is_busy(thread_id, entry))
            .filter_map(|(_, entry)| entry.agent_path.as_deref())
            .collect::<Vec<_>>();
        let idle =
            open.iter()
                .filter(|(thread_id, entry)| !is_busy(thread_id, entry))
                .filter(|(_, entry)| {
                    entry.agent_path.as_deref().is_none_or(|path| {
                        !busy_paths.iter().any(|busy| is_path_ancestor(path, busy))
                    })
                })
                .collect::<Vec<_>>();
        let idle_paths = idle
            .iter()
            .filter_map(|(_, entry)| entry.agent_path.as_deref())
            .collect::<Vec<_>>();
        let mut targets = idle
            .iter()
            .filter(|(_, entry)| {
                entry.agent_path.as_deref().is_none_or(|path| {
                    !idle_paths.iter().any(|other| is_path_ancestor(other, path))
                })
            })
            .map(|(thread_id, _)| *thread_id)
            .collect::<Vec<_>>();
        targets.sort_by_key(|thread_id| self.order.iter().position(|id| id == thread_id));
        targets
    }

    /// Descendants of the root session in `subtrees`, by agent path, including the roots.
    pub(crate) fn session_subtree_threads(
        &self,
        root: ThreadId,
        subtrees: &[ThreadId],
    ) -> Vec<ThreadId> {
        let subtree_paths = subtrees
            .iter()
            .filter_map(|thread_id| self.get(thread_id))
            .filter_map(|entry| entry.agent_path.as_deref())
            .collect::<Vec<_>>();
        self.session_descendants(root)
            .filter(|(thread_id, entry)| {
                subtrees.contains(thread_id)
                    || entry.agent_path.as_deref().is_some_and(|path| {
                        subtree_paths
                            .iter()
                            .any(|subtree| is_path_ancestor(subtree, path))
                    })
            })
            .map(|(thread_id, _)| thread_id)
            .collect()
    }

    /// Whether `thread_id` is a subagent in the root session's tree.
    pub(crate) fn is_session_descendant(&self, root: ThreadId, thread_id: ThreadId) -> bool {
        self.session_descendants(root)
            .any(|(descendant, _)| descendant == thread_id)
    }

    fn session_descendants(
        &self,
        root: ThreadId,
    ) -> impl Iterator<Item = (ThreadId, &AgentPickerThreadEntry)> {
        let root_path = self
            .get(&root)
            .and_then(|entry| entry.agent_path.as_deref())
            .unwrap_or(AgentPath::ROOT);
        let descendant_prefix = format!("{root_path}/");
        self.threads
            .iter()
            .map(|(id, entry)| (*id, entry))
            .filter(move |(thread_id, entry)| {
                *thread_id != root
                    && !self.picker_excluded_threads.contains(thread_id)
                    && entry.agent_path.as_deref().map_or_else(
                        || self.subagent_threads.contains(thread_id),
                        |path| path.starts_with(&descendant_prefix),
                    )
            })
    }
}

/// Whether `ancestor` is a strict ancestor of `path` in the agent-path hierarchy.
fn is_path_ancestor(ancestor: &str, path: &str) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}
