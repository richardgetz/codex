use crate::thread_state::ThreadState;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) async fn clear_router_tick(thread_state: &Arc<Mutex<ThreadState>>) {
    thread_state.lock().await.cancel_router_tick();
}
