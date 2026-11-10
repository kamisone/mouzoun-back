//! Background workers. Each runs until the shutdown token is cancelled.
//! Jobs are safe to run on several API instances at once (row locking with SKIP LOCKED).

mod media_cleanup;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

pub fn spawn_all(state: &AppState, shutdown: CancellationToken) -> Vec<JoinHandle<()>> {
    vec![tokio::spawn(media_cleanup::run(
        state.media.clone(),
        state.config.media.clone(),
        shutdown,
    ))]
}
