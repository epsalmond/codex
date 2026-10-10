//! Pauses the recovery metadata read after a definite missing-runtime send failure.

use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_thread_store as store;
use std::any::Any;
use std::sync::Arc;
use std::sync::Mutex;
use store::ThreadStore;
use tokio::sync::oneshot;

pub(super) struct ReadGate {
    pub thread_id: ThreadId,
    pub include_archived: bool,
    pub started: oneshot::Sender<()>,
    pub release: oneshot::Receiver<()>,
}

#[derive(Default)]
pub(super) struct GatedCompletionReadStore {
    inner: store::InMemoryThreadStore,
    pub gate: Mutex<Option<ReadGate>>,
}

macro_rules! delegate_store_methods {
    ($(fn $name:ident($param:ident: $params:ty) -> $result:ty;)*) => {
        $(fn $name(&self, $param: $params) -> store::ThreadStoreFuture<'_, $result> {
            self.inner.$name($param)
        })*
    };
}

impl ThreadStore for GatedCompletionReadStore {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn default_history_mode(&self) -> ThreadHistoryMode {
        self.inner.default_history_mode()
    }

    delegate_store_methods! {
        fn create_thread(params: store::CreateThreadParams) -> ();
        fn resume_thread(params: store::ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn append_items(params: store::AppendThreadItemsParams) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: store::LoadThreadHistoryParams) -> store::StoredThreadHistory;
        fn load_latest_model_context(params: store::LoadThreadHistoryParams) -> store::StoredModelContext;
        fn read_thread_by_rollout_path(params: store::ReadThreadByRolloutPathParams) -> store::StoredThread;
        fn list_threads(params: store::ListThreadsParams) -> store::ThreadPage;
        fn update_thread_metadata(params: store::UpdateThreadMetadataParams) -> Option<store::StoredThread>;
        fn archive_thread(params: store::ArchiveThreadParams) -> ();
        fn unarchive_thread(params: store::ArchiveThreadParams) -> store::StoredThread;
        fn delete_thread(params: store::DeleteThreadParams) -> ();
    }

    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: store::PersistContext,
    ) -> store::ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread_id, context)
    }

    fn read_thread(
        &self,
        params: store::ReadThreadParams,
    ) -> store::ThreadStoreFuture<'_, store::StoredThread> {
        Box::pin(async move {
            let gate = {
                let mut gate = self.gate.lock().expect("recovery read gate");
                if gate.as_ref().is_some_and(|gate| {
                    gate.thread_id == params.thread_id
                        && gate.include_archived == params.include_archived
                }) {
                    gate.take()
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                assert!(!params.include_history);
                gate.started.send(()).expect("recovery started receiver");
                gate.release.await.expect("release recovery metadata read");
            }
            self.inner.read_thread(params).await
        })
    }
}
