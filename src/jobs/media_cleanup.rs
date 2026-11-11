//! Deletes the files of uploads started but never completed (closed tab, network error...).

use crate::modules::media::MediaService;

pub async fn run_once(media: MediaService) -> anyhow::Result<()> {
    let purged = media.purge_abandoned(100).await?;
    if purged > 0 {
        tracing::info!(purged, "media cleanup: abandoned uploads removed");
    }
    Ok(())
}
