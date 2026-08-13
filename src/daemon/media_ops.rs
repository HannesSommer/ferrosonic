//! Media-cache integration: resolve each play to a local file or a stream
//! URL, and fill the cache in the background on a miss.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::daemon::core::{DaemonCore, PlayMode, PlaybackSource};
use crate::media_cache::MediaCacheError;
use crate::subsonic::models::Child;

impl DaemonCore {
    /// Pick the source for `song`, whose signed stream URL is `stream_url`.
    ///
    /// A cache hit returns the local path and touches nothing else. A miss
    /// returns the stream URL, and for [`PlayMode::Direct`] starts a background
    /// download so mpv can begin playing immediately over the network while the
    /// copy lands on disk for next time. That fill costs a second transfer of
    /// the track on its first play; it is the price of never delaying playback.
    ///
    /// [`PlayMode::Buffered`] gets no background fill: that path already
    /// downloads the whole track before mpv reads it, and a concurrent second
    /// transfer of the same bytes would compete with it for the link and delay
    /// the very playback the user is waiting on. The prebuffer hands its
    /// finished file to [`Self::adopt_prebuffered_file`] instead.
    pub(super) fn resolve_playback_source(
        self: &Arc<Self>,
        song: &Child,
        stream_url: String,
        mode: PlayMode,
    ) -> PlaybackSource {
        if !self.media_cache.is_enabled() {
            return PlaybackSource::Remote(stream_url);
        }
        if let Some(path) = self.media_cache.lookup(&song.id, song.suffix.as_deref()) {
            debug!("Media cache hit for {} ({})", song.title, path.display());
            return PlaybackSource::Cached(path);
        }
        if mode == PlayMode::Direct {
            self.spawn_cache_fill(song, &stream_url);
        }
        PlaybackSource::Remote(stream_url)
    }

    /// Copy a completed prebuffer temp file into the media cache.
    ///
    /// Runs on the blocking pool and after mpv has the file, so a large copy
    /// never sits between the download finishing and playback starting.
    pub(super) fn adopt_prebuffered_file(
        self: &Arc<Self>,
        temp_path: std::path::PathBuf,
        song_id: String,
        suffix: Option<String>,
        title: String,
    ) {
        if !self.media_cache.is_enabled() {
            return;
        }
        let cache = self.media_cache.clone();
        tokio::task::spawn_blocking(move || {
            match cache.insert_from_file(&temp_path, &song_id, suffix.as_deref()) {
                Ok(path) => info!("Cached {} to {}", title, path.display()),
                Err(MediaCacheError::AlreadyInFlight | MediaCacheError::Disabled) => {}
                Err(e) => warn!("Could not cache {}: {}", title, e),
            }
        });
    }

    /// Download `song` into the media cache in the background.
    ///
    /// Fire-and-forget: the task never touches playback state, so a failed or
    /// superseded fill can only cost a cache entry, never a track. Concurrent
    /// calls for the same song collapse to one transfer inside the cache's
    /// in-flight set.
    pub(super) fn spawn_cache_fill(self: &Arc<Self>, song: &Child, stream_url: &str) {
        if !self.media_cache.is_enabled() {
            return;
        }
        if self.shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let cache = self.media_cache.clone();
        let url = stream_url.to_string();
        let song_id = song.id.clone();
        let suffix = song.suffix.clone();
        let title = song.title.clone();
        tokio::spawn(async move {
            match cache.store(&url, &song_id, suffix.as_deref(), None).await {
                Ok(path) => info!("Cached {} to {}", title, path.display()),
                // Routine outcomes on a busy queue, not problems worth a log
                // line at info: a preload and a play racing the same track, or
                // the user switching the feature off mid-transfer.
                Err(MediaCacheError::AlreadyInFlight | MediaCacheError::Disabled) => {}
                Err(MediaCacheError::Cancelled) => debug!("Cache fill cancelled for {title}"),
                Err(e) => warn!("Could not cache {}: {}", title, e),
            }
        });
    }

    /// Apply a changed cache configuration to the running cache.
    ///
    /// Turning the cache off leaves the files in place so the user can turn it
    /// back on without re-downloading; [`Self::clear_media_cache`] is the
    /// explicit way to reclaim the space.
    pub(super) fn apply_media_cache_config(self: &Arc<Self>, enabled: bool, capacity_bytes: u64) {
        self.media_cache.set_enabled(enabled);
        self.media_cache.set_capacity_bytes(capacity_bytes);
        if enabled {
            self.media_cache.evict_to_capacity();
        }
    }

    /// Delete every cached media file. Returns `(files removed, bytes freed)`.
    ///
    /// Safe during playback: mpv holds an open descriptor on any file it is
    /// reading, so on unix the inode survives until it closes.
    pub fn clear_media_cache(self: &Arc<Self>) -> (usize, u64) {
        self.media_cache.clear()
    }

    /// Bytes currently held by the media cache.
    #[must_use]
    pub fn media_cache_usage(self: &Arc<Self>) -> u64 {
        self.media_cache.usage_bytes()
    }
}
