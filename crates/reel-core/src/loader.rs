//! Background loading of sources: one task per addon, results flow into the pool.

use crate::addon::{Addon, AddonClient};
use crate::pool::SourcePool;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio::time::timeout;

/// Generous: self-hosted addons can take a while; Stremio waits for them too.
pub const ADDON_TIMEOUT: Duration = Duration::from_secs(65);

/// Owns the in-flight addon requests. Dropping it cancels whatever is still pending.
pub struct SourceLoader {
    tasks: JoinSet<()>,
    pool: SourcePool,
}

impl SourceLoader {
    /// Starts fetching streams (and subtitles) for `video_id` from every addon that supports it.
    /// Returns immediately; use the pool to observe results.
    pub fn start(
        client: AddonClient,
        addons: &[Addon],
        kind: &str,
        video_id: &str,
        pool: SourcePool,
    ) -> Self {
        let mut tasks = JoinSet::new();
        let stream_addons: Vec<Addon> = addons
            .iter()
            .filter(|a| a.supports("stream", kind, video_id))
            .cloned()
            .collect();
        let sub_addons: Vec<Addon> = addons
            .iter()
            .filter(|a| a.supports("subtitles", kind, video_id))
            .cloned()
            .collect();
        for addon in stream_addons {
            let (client, pool) = (client.clone(), pool.clone());
            let (kind, id) = (kind.to_owned(), video_id.to_owned());
            pool.begin_task(addon.name());
            tasks.spawn(async move {
                match timeout(ADDON_TIMEOUT, client.streams(&addon, &kind, &id)).await {
                    Ok(Ok(streams)) => pool.add_streams(addon.name(), streams),
                    Ok(Err(e)) => tracing::warn!("addon {} failed: {e}", addon.name()),
                    Err(_) => tracing::warn!("addon {} timed out", addon.name()),
                }
                pool.task_done(addon.name());
            });
        }
        for addon in sub_addons {
            let (client, pool) = (client.clone(), pool.clone());
            let (kind, id) = (kind.to_owned(), video_id.to_owned());
            pool.begin_task(addon.name());
            tasks.spawn(async move {
                match timeout(ADDON_TIMEOUT, client.subtitles(&addon, &kind, &id)).await {
                    Ok(Ok(subs)) => pool.add_subtitles(subs),
                    Ok(Err(e)) => tracing::warn!("subtitle addon {} failed: {e}", addon.name()),
                    Err(_) => tracing::warn!("subtitle addon {} timed out", addon.name()),
                }
                pool.task_done(addon.name());
            });
        }
        Self { tasks, pool }
    }
}

impl Drop for SourceLoader {
    fn drop(&mut self) {
        self.tasks.abort_all();
        self.pool.cancel_pending();
    }
}
