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

    /// Counts open descendants of the root session, including idle/error threads.
    /// Local dismissal affects navigation only; backend archives leave the counts.
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

    /// Hides inactive descendants locally until they next become running.
    /// Each row is independent, so an idle parent's running grandchild remains visible.
    pub(crate) fn dismiss_inactive_subagents(&mut self, root: ThreadId) {
        let inactive = self
            .session_descendants(root)
            .filter(|(_, entry)| !entry.is_running)
            .map(|(thread_id, _)| thread_id)
            .collect::<Vec<_>>();
        self.dismissed_threads.extend(inactive);
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
