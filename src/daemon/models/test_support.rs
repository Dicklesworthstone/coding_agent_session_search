//! Test-only publication through the real slot and loader gate. No production
//! model injection or native-loader bypass is exposed by the daemon.
use super::*;

impl ModelManager {
    pub(in crate::daemon) fn install_test_embedder(&self, model: Arc<dyn Embedder>) {
        self.embedder.unload();
        self.embedder.acquire_with(|| Ok::<_, ()>((model, "test embedder".to_owned()))).unwrap();
    }

    pub(in crate::daemon) fn install_test_reranker(&self, model: Arc<dyn Reranker>) {
        self.reranker.unload();
        self.reranker.acquire_with(|| Ok::<_, ()>((model, "test reranker".to_owned()))).unwrap();
    }
}
