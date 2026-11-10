//! Purges uploads that were started but never completed (closed tab, network error...),
//! so the bucket and the `media` table never accumulate orphans.

use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::{config::MediaConfig, modules::media::MediaService};

const BATCH_SIZE: i64 = 100;

pub async fn run(media: MediaService, config: MediaConfig, shutdown: CancellationToken) {
    let mut ticker = interval(config.cleanup_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {
                match media.purge_abandoned(config.pending_ttl, BATCH_SIZE).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(purged = n, "media cleanup: abandoned uploads removed"),
                    Err(e) => tracing::warn!(error = ?e, "media cleanup failed"),
                }
            }
        }
    }
    tracing::debug!("media cleanup worker stopped");
}
