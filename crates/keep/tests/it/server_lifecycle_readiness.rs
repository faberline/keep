use keep::{AppState, KvEngine};
use std::sync::Arc;

#[test]
fn state_reports_draining_through_server_lifecycle_readiness() {
    let state = AppState::new(Arc::new(KvEngine::with_shards(16)));
    assert!(!server_lifecycle::Readiness::is_draining(&state));
    state.start_drain();
    assert!(server_lifecycle::Readiness::is_draining(&state));
}
