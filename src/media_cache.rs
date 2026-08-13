//! On-disk cache of downloaded media files.
//!
//! Tracks are streamed from the Subsonic server with `format=raw`, so the
//! bytes on the wire are the bytes of the original file. That makes them
//! trivially cacheable: a completed download is a byte-identical copy of the
//! server's file, and handing mpv the local path instead of the stream URL
//! preserves bit-perfect playback while removing the network from the path.
//!
//! # Invariants
//!
//! - **A file under the cache root is always complete.** Downloads land in a
//!   `NamedTempFile` in the same directory and are `rename`d into place only
//!   after the transfer finishes and matches the server's `Content-Length`.
//!   A partial download is never observable, so mpv can never read a truncated
//!   file and mistake its early EOF for the end of the track.
//! - **Eviction is least-recently-used**, approximated by file mtime, which
//!   [`MediaCache::lookup`] bumps on every hit. Independent of `atime`, which
//!   `relatime` mounts do not reliably update.
//! - **The cache is advisory.** Every operation degrades to a miss on error;
//!   nothing here can fail playback, only make it fall back to streaming.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use tracing::{debug, info, warn};

use crate::config::Config;

/// Prefix for in-progress downloads, so they are excluded from usage
/// accounting and eviction and can be swept if a crash leaks them.
const TEMP_PREFIX: &str = ".download-";

/// Extension used when the server reports no file suffix for a track.
const DEFAULT_SUFFIX: &str = "dat";

/// Abort a download whose stream has produced no bytes for this long. Bounds a
/// stalled fill without capping the total time a large lossless file may take.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Age past which a leaked in-progress download file is swept at startup.
const TEMP_SWEEP_AGE: Duration = Duration::from_hours(1);

/// Why a media-cache store did not produce a cached file.
///
/// Every variant is non-fatal: the caller streams from the server instead.
#[derive(Debug, thiserror::Error)]
pub enum MediaCacheError {
    /// Caching is switched off in the config.
    #[error("media cache is disabled")]
    Disabled,

    /// A download for this track is already running.
    #[error("a download for this track is already in flight")]
    AlreadyInFlight,

    /// Filesystem operation failed.
    #[error("media cache IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The transfer itself failed.
    #[error("media download failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The server answered with a non-success status.
    #[error("server returned HTTP {0}")]
    Status(u16),

    /// Fewer bytes arrived than `Content-Length` promised; the partial file is
    /// discarded rather than published.
    #[error("download truncated: expected {expected} bytes, got {got}")]
    Truncated {
        /// Byte count the server promised.
        expected: u64,
        /// Byte count actually received.
        got: u64,
    },

    /// A single track larger than the whole cache would evict everything else
    /// on every play, so it is never stored.
    #[error("track is {size} bytes, larger than the {capacity} byte cache")]
    TooLarge {
        /// Size of the track.
        size: u64,
        /// Configured cache capacity.
        capacity: u64,
    },

    /// The caller flipped the cancel flag mid-transfer.
    #[error("download cancelled")]
    Cancelled,
}

/// One cached file, as seen by usage accounting and eviction.
struct Entry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

/// Removes its key from the in-flight set on drop, so a task aborted
/// mid-download cannot leave a track permanently un-cacheable.
struct InFlightGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    key: String,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        let mut guard = match self.set.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.remove(&self.key);
    }
}

/// A size-capped, LRU-evicted directory of downloaded tracks.
///
/// Enabled state and capacity are atomics rather than plain fields so the
/// running daemon can apply a settings change without rebuilding the cache or
/// taking a write lock on the shared instance.
#[derive(Debug)]
pub struct MediaCache {
    root: PathBuf,
    enabled: AtomicBool,
    capacity_bytes: AtomicU64,
    /// Keys with a download running, so a preload and a play of the same
    /// track fetch it once rather than racing two identical transfers.
    in_flight: Mutex<HashSet<String>>,
    http: reqwest::Client,
}

impl MediaCache {
    /// Build a cache rooted at `root`.
    ///
    /// The directory is not created here; [`Self::store`] creates it on the
    /// first write, so a disabled cache never touches the filesystem.
    #[must_use]
    pub fn new(root: PathBuf, enabled: bool, capacity_bytes: u64) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // No overall timeout: a lossless track on a slow link can take
            // minutes legitimately. Stalls are caught per-chunk instead.
            .build()
            .unwrap_or_default();
        Self {
            root,
            enabled: AtomicBool::new(enabled),
            capacity_bytes: AtomicU64::new(capacity_bytes),
            in_flight: Mutex::new(HashSet::new()),
            http,
        }
    }

    /// Build a cache from the persisted settings, rooted at
    /// [`crate::config::paths::media_cache_dir`].
    ///
    /// Falls back to a disabled cache when no cache directory can be
    /// determined, so a platform without XDG paths degrades to streaming.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let Some(root) = crate::config::paths::media_cache_dir() else {
            warn!("No cache directory available; media caching disabled");
            return Self::new(PathBuf::new(), false, 0);
        };
        Self::new(
            root,
            config.media_cache,
            config.media_cache_capacity_bytes(),
        )
    }

    /// Directory holding the cached files.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether lookups and stores are active.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire) && !self.root.as_os_str().is_empty()
    }

    /// Switch caching on or off; takes effect on the next lookup or store.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Release);
    }

    /// Current size cap in bytes.
    #[must_use]
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes.load(Ordering::Acquire)
    }

    /// Set the size cap. The caller applies it with [`Self::evict_to_capacity`];
    /// lowering the cap does not delete anything on its own.
    pub fn set_capacity_bytes(&self, bytes: u64) {
        self.capacity_bytes.store(bytes, Ordering::Release);
    }

    /// Path a track would occupy, whether or not it is cached.
    ///
    /// The song ID is hashed rather than used verbatim: Subsonic IDs are
    /// server-chosen opaque strings that may contain `/`, `..`, or characters
    /// the local filesystem rejects, and a hash makes every key a safe,
    /// fixed-width single path component.
    ///
    /// ```
    /// use ferrosonic::media_cache::MediaCache;
    /// let c = MediaCache::new("/tmp/fs-cache".into(), true, 1024);
    /// let p = c.entry_path("../../etc/passwd", Some("flac"));
    /// assert_eq!(p.parent(), Some(std::path::Path::new("/tmp/fs-cache")));
    /// assert!(p.extension().is_some_and(|e| e == "flac"));
    /// ```
    #[must_use]
    pub fn entry_path(&self, song_id: &str, suffix: Option<&str>) -> PathBuf {
        let digest = md5::compute(song_id.as_bytes());
        self.root
            .join(format!("{:x}.{}", digest, sanitize_suffix(suffix)))
    }

    /// Path of the cached file for this track, or `None` on a miss.
    ///
    /// A hit bumps the file's mtime so eviction sees it as recently used.
    /// Zero-length files are treated as misses and removed: they carry no
    /// audio and would otherwise make mpv report a zero-duration track.
    #[must_use]
    pub fn lookup(&self, song_id: &str, suffix: Option<&str>) -> Option<PathBuf> {
        if !self.is_enabled() {
            return None;
        }
        let path = self.entry_path(song_id, suffix);
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() {
            return None;
        }
        if meta.len() == 0 {
            debug!("Dropping zero-length cache entry {}", path.display());
            let _ = std::fs::remove_file(&path);
            return None;
        }
        touch(&path);
        Some(path)
    }

    /// Download `url` into the cache and return the resulting path.
    ///
    /// Nothing is published until the transfer completes and matches the
    /// server's `Content-Length`, so a failure here leaves the cache exactly
    /// as it was. `cancel` is polled between chunks; flipping it aborts the
    /// transfer and discards the partial file.
    ///
    /// # Errors
    /// Returns a [`MediaCacheError`] if caching is off, a download for this
    /// track is already running, the transfer fails or is truncated, the track
    /// does not fit the cache, or a filesystem operation fails.
    pub async fn store(
        &self,
        url: &str,
        song_id: &str,
        suffix: Option<&str>,
        cancel: Option<&AtomicBool>,
    ) -> Result<PathBuf, MediaCacheError> {
        if !self.is_enabled() {
            return Err(MediaCacheError::Disabled);
        }
        let path = self.entry_path(song_id, suffix);

        // Claim the key before any IO so a preload and a play of the same
        // track cannot both start a transfer. The guard releases it on every
        // exit path, including an aborted task.
        let key = song_id.to_string();
        {
            let mut in_flight = match self.in_flight.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !in_flight.insert(key.clone()) {
                return Err(MediaCacheError::AlreadyInFlight);
            }
        }
        let _guard = InFlightGuard {
            set: &self.in_flight,
            key,
        };

        // Re-check under the claim: a transfer that finished between the
        // caller's lookup and here means there is nothing left to do.
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() > 0) {
            return Ok(path);
        }

        self.download_to(url, &path, cancel).await?;
        self.evict_to_capacity_excluding(Some(&path));
        Ok(path)
    }

    /// Stream `url` to a temp file beside `dest`, then rename it into place.
    async fn download_to(
        &self,
        url: &str,
        dest: &Path,
        cancel: Option<&AtomicBool>,
    ) -> Result<(), MediaCacheError> {
        use futures::StreamExt;
        use std::io::Write;

        std::fs::create_dir_all(&self.root)?;

        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(MediaCacheError::Status(status.as_u16()));
        }

        let capacity = self.capacity_bytes();
        let expected = resp.content_length();
        // Refuse oversized tracks up front so we never spend the bandwidth on
        // a file that eviction would immediately delete.
        if let Some(len) = expected {
            if len > capacity {
                return Err(MediaCacheError::TooLarge {
                    size: len,
                    capacity,
                });
            }
        }

        let temp = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempfile_in(&self.root)?;
        let mut written: u64 = 0;
        let mut stream = resp.bytes_stream();

        // Scoped so the file handle is flushed and closed before the rename.
        {
            let file = temp.as_file();
            let mut writer = std::io::BufWriter::new(file);
            loop {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    return Err(MediaCacheError::Cancelled);
                }
                let Ok(next) = tokio::time::timeout(STALL_TIMEOUT, stream.next()).await else {
                    return Err(MediaCacheError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "media download stalled",
                    )));
                };
                let Some(chunk) = next else { break };
                let chunk = chunk?;
                writer.write_all(&chunk)?;
                written += chunk.len() as u64;
                // Stop a chunked response with no Content-Length from
                // overrunning the cache; a mid-transfer cap breach is
                // indistinguishable from a track that never fits.
                if written > capacity {
                    return Err(MediaCacheError::TooLarge {
                        size: written,
                        capacity,
                    });
                }
            }
            writer.flush()?;
        }

        // A short read is the one failure that could otherwise be published as
        // a good entry, and mpv would read its EOF as the end of the track.
        if let Some(len) = expected {
            if written != len {
                return Err(MediaCacheError::Truncated {
                    expected: len,
                    got: written,
                });
            }
        }
        if written == 0 {
            return Err(MediaCacheError::Truncated {
                expected: expected.unwrap_or(0),
                got: 0,
            });
        }

        // Durability before visibility: the rename must not be able to expose
        // a file whose contents are still only in the page cache.
        temp.as_file().sync_all()?;
        temp.persist(dest)
            .map_err(|e| MediaCacheError::Io(e.error))?;
        crate::io_util::fsync_parent_dir(dest);
        debug!("Cached {} ({} KB)", dest.display(), written / 1024);
        Ok(())
    }

    /// Adopt an already-downloaded file into the cache by copying it in.
    ///
    /// For the prebuffer path, which fetches the whole track to a temp file
    /// before mpv reads it: the bytes are already local, so re-downloading
    /// them to fill the cache would compete with playback for the same link.
    /// A local copy costs disk IO instead of a second transfer.
    ///
    /// Blocking; run it off the async runtime. `src` is left untouched, since
    /// mpv is still reading it.
    ///
    /// # Errors
    /// Returns a [`MediaCacheError`] if caching is off, a store for this track
    /// is already running, `src` is empty or does not fit the cache, or a
    /// filesystem operation fails.
    pub fn insert_from_file(
        &self,
        src: &Path,
        song_id: &str,
        suffix: Option<&str>,
    ) -> Result<PathBuf, MediaCacheError> {
        if !self.is_enabled() {
            return Err(MediaCacheError::Disabled);
        }
        let dest = self.entry_path(song_id, suffix);

        let key = song_id.to_string();
        {
            let mut in_flight = match self.in_flight.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !in_flight.insert(key.clone()) {
                return Err(MediaCacheError::AlreadyInFlight);
            }
        }
        let _guard = InFlightGuard {
            set: &self.in_flight,
            key,
        };

        if std::fs::metadata(&dest).is_ok_and(|m| m.is_file() && m.len() > 0) {
            return Ok(dest);
        }

        let size = std::fs::metadata(src)?.len();
        if size == 0 {
            return Err(MediaCacheError::Truncated {
                expected: 0,
                got: 0,
            });
        }
        let capacity = self.capacity_bytes();
        if size > capacity {
            return Err(MediaCacheError::TooLarge { size, capacity });
        }

        std::fs::create_dir_all(&self.root)?;
        let mut temp = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempfile_in(&self.root)?;
        let copied = std::io::copy(&mut std::fs::File::open(src)?, temp.as_file_mut())?;
        // A short copy means `src` was truncated under us (the prebuffer temp
        // file is unlinked once mpv is done with it); publishing it would put
        // a partial track in the cache forever.
        if copied != size {
            return Err(MediaCacheError::Truncated {
                expected: size,
                got: copied,
            });
        }
        temp.as_file().sync_all()?;
        temp.persist(&dest)
            .map_err(|e| MediaCacheError::Io(e.error))?;
        crate::io_util::fsync_parent_dir(&dest);
        self.evict_to_capacity_excluding(Some(&dest));
        debug!(
            "Adopted {} into the cache ({} KB)",
            dest.display(),
            size / 1024
        );
        Ok(dest)
    }

    /// Total bytes held by completed cache entries.
    ///
    /// In-progress downloads are excluded: they are not yet cache content and
    /// counting them would make a large fill look like an over-capacity cache.
    #[must_use]
    pub fn usage_bytes(&self) -> u64 {
        self.entries().iter().map(|e| e.size).sum()
    }

    /// Number of completed cache entries.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries().len()
    }

    /// Completed entries, unordered. Empty when the root does not exist.
    fn entries(&self) -> Vec<Entry> {
        scan_entries(&self.root)
    }

    /// Delete least-recently-used entries until usage fits the capacity.
    /// Returns the bytes freed.
    pub fn evict_to_capacity(&self) -> u64 {
        self.evict_to_capacity_excluding(None)
    }

    /// [`Self::evict_to_capacity`], but never evicts `keep`.
    ///
    /// A just-stored track is the newest entry and so the last eviction
    /// candidate anyway; pinning it explicitly means a cache whose capacity
    /// was lowered below one track's size still returns a usable path rather
    /// than deleting the file it just handed back.
    fn evict_to_capacity_excluding(&self, keep: Option<&Path>) -> u64 {
        let capacity = self.capacity_bytes();
        let mut entries = self.entries();
        let mut usage: u64 = entries.iter().map(|e| e.size).sum();
        if usage <= capacity {
            return 0;
        }

        // Oldest mtime first: `lookup` bumps mtime on every hit, so this is
        // least-recently-used order.
        entries.sort_by_key(|e| e.modified);

        let mut freed: u64 = 0;
        for entry in entries {
            if usage <= capacity {
                break;
            }
            if keep.is_some_and(|k| k == entry.path) {
                continue;
            }
            match std::fs::remove_file(&entry.path) {
                Ok(()) => {
                    usage = usage.saturating_sub(entry.size);
                    freed += entry.size;
                }
                Err(e) => warn!("Could not evict {}: {}", entry.path.display(), e),
            }
        }
        if freed > 0 {
            info!(
                "Media cache evicted {} KB to stay under {} KB",
                freed / 1024,
                capacity / 1024
            );
        }
        freed
    }

    /// Delete every cached file. Returns `(files removed, bytes freed)`.
    ///
    /// In-progress downloads are left alone: they live in temp files that are
    /// unlinked when their transfer ends, and their eventual rename lands in a
    /// cache the user has just emptied, which is harmless.
    pub fn clear(&self) -> (usize, u64) {
        let mut files = 0;
        let mut bytes = 0;
        for entry in self.entries() {
            match std::fs::remove_file(&entry.path) {
                Ok(()) => {
                    files += 1;
                    bytes += entry.size;
                }
                Err(e) => warn!("Could not remove {}: {}", entry.path.display(), e),
            }
        }
        info!("Media cache cleared: {} files, {} KB", files, bytes / 1024);
        (files, bytes)
    }

    /// Remove in-progress download files left behind by a killed process.
    ///
    /// `NamedTempFile` unlinks on drop, so this only ever finds files whose
    /// owner died without unwinding. The age gate keeps it from deleting a
    /// live transfer belonging to another running instance.
    pub fn sweep_stale_downloads(&self) {
        let Ok(dir) = std::fs::read_dir(&self.root) else {
            return;
        };
        let now = SystemTime::now();
        for entry in dir.flatten() {
            if !entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age > TEMP_SWEEP_AGE);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Completed cache entries under `root`, unordered. Empty when `root` does not
/// exist, so an unused cache reads as empty rather than as an error.
fn scan_entries(root: &Path) -> Vec<Entry> {
    let Ok(dir) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        out.push(Entry {
            path: entry.path(),
            size: meta.len(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    out
}

/// Bytes held by the media cache at its default location.
///
/// For the TUI, which shows the figure but does not own the cache: the daemon
/// holds the [`MediaCache`], and both processes are on the same machine and
/// resolve the same path. `None` when no cache directory can be determined.
#[must_use]
pub fn default_usage_bytes() -> Option<u64> {
    let root = crate::config::paths::media_cache_dir()?;
    Some(scan_entries(&root).iter().map(|e| e.size).sum())
}

/// Human-readable byte count for UI readouts.
///
/// ```
/// use ferrosonic::media_cache::format_size;
/// assert_eq!(format_size(0), "0 MB");
/// assert_eq!(format_size(512 * 1024 * 1024), "512 MB");
/// assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
/// ```
#[must_use]
pub fn format_size(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        // Integer maths: the crate denies precision-losing float casts on
        // values this large, and one decimal is all the readout needs.
        let tenths = bytes * 10 / GB;
        format!("{}.{} GB", tenths / 10, tenths % 10)
    } else {
        format!("{} MB", bytes / MB)
    }
}

/// Best-effort mtime bump marking a cache entry as just used.
///
/// Failures are ignored: a read-only or exotic filesystem costs eviction
/// accuracy, never correctness.
fn touch(path: &Path) {
    let now = SystemTime::now();
    let times = std::fs::FileTimes::new()
        .set_modified(now)
        .set_accessed(now);
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_times(times);
    }
}

/// Reduce a server-reported file suffix to a safe, bounded extension.
///
/// The extension is cosmetic for playback (mpv probes container format from
/// content), but keeping the real one makes the cache directory legible and
/// helps mpv with formats that have no distinctive magic bytes.
fn sanitize_suffix(suffix: Option<&str>) -> String {
    let cleaned: String = suffix
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .flat_map(char::to_lowercase)
        .collect();
    if cleaned.is_empty() {
        DEFAULT_SUFFIX.to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    // disallowed_methods: `std::fs::write` is banned for production writes,
    // which must be atomic. These are fixture files in a per-test tempdir,
    // where a torn write is not a failure mode that exists.
    #![allow(clippy::disallowed_methods)]

    use super::*;

    fn cache(dir: &tempfile::TempDir, capacity: u64) -> MediaCache {
        MediaCache::new(dir.path().to_path_buf(), true, capacity)
    }

    #[test]
    fn suffix_is_sanitized_to_a_safe_extension() {
        assert_eq!(sanitize_suffix(Some("FLAC")), "flac");
        assert_eq!(sanitize_suffix(Some("mp3")), "mp3");
        assert_eq!(sanitize_suffix(None), DEFAULT_SUFFIX);
        assert_eq!(sanitize_suffix(Some("")), DEFAULT_SUFFIX);
        assert_eq!(sanitize_suffix(Some("../..")), DEFAULT_SUFFIX);
        assert_eq!(sanitize_suffix(Some("a/b.c")), "abc");
        assert_eq!(
            sanitize_suffix(Some("averylongsuffix")),
            "averylon",
            "suffix is bounded so a hostile value cannot blow up the name"
        );
    }

    #[test]
    fn entry_path_confines_hostile_ids_to_the_cache_root() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        for id in ["../../etc/passwd", "a/b/c", "..", "", "\0weird"] {
            let p = c.entry_path(id, Some("flac"));
            assert_eq!(
                p.parent(),
                Some(dir.path()),
                "id {id:?} escaped the cache root: {}",
                p.display()
            );
        }
    }

    #[test]
    fn entry_path_is_stable_and_distinct_per_song() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        assert_eq!(
            c.entry_path("s1", Some("flac")),
            c.entry_path("s1", Some("flac"))
        );
        assert_ne!(
            c.entry_path("s1", Some("flac")),
            c.entry_path("s2", Some("flac"))
        );
    }

    #[test]
    fn lookup_misses_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        std::fs::write(c.entry_path("s1", Some("flac")), b"audio").unwrap();
        assert!(c.lookup("s1", Some("flac")).is_some());
        c.set_enabled(false);
        assert!(
            c.lookup("s1", Some("flac")).is_none(),
            "a disabled cache must not serve files it still holds"
        );
    }

    #[test]
    fn lookup_drops_zero_length_entries() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        let p = c.entry_path("s1", Some("flac"));
        std::fs::write(&p, b"").unwrap();
        assert!(c.lookup("s1", Some("flac")).is_none());
        assert!(!p.exists(), "empty entry must be removed, not just skipped");
    }

    #[test]
    fn usage_ignores_in_progress_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        std::fs::write(c.entry_path("s1", Some("flac")), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join(format!("{TEMP_PREFIX}abc")), vec![0u8; 500]).unwrap();
        assert_eq!(c.usage_bytes(), 100);
        assert_eq!(c.entry_count(), 1);
    }

    #[test]
    fn eviction_removes_least_recently_used_first() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 250);
        let old = c.entry_path("old", Some("flac"));
        let mid = c.entry_path("mid", Some("flac"));
        let new = c.entry_path("new", Some("flac"));
        for p in [&old, &mid, &new] {
            std::fs::write(p, vec![0u8; 100]).unwrap();
        }
        // Explicit mtimes: the writes above are too close together to order.
        let base = SystemTime::now() - Duration::from_hours(1);
        for (p, offset) in [(&old, 0), (&mid, 60), (&new, 120)] {
            let t = base + Duration::from_secs(offset);
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(t))
                .unwrap();
        }

        let freed = c.evict_to_capacity();

        assert_eq!(freed, 100, "one 100-byte entry frees 300 -> 200 under 250");
        assert!(!old.exists(), "oldest entry must be evicted first");
        assert!(mid.exists());
        assert!(new.exists());
    }

    #[test]
    fn lookup_bumps_mtime_so_a_hit_survives_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 150);
        let old = c.entry_path("old", Some("flac"));
        let new = c.entry_path("new", Some("flac"));
        for p in [&old, &new] {
            std::fs::write(p, vec![0u8; 100]).unwrap();
        }
        let base = SystemTime::now() - Duration::from_hours(1);
        for (p, offset) in [(&old, 0), (&new, 60)] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new().set_modified(base + Duration::from_secs(offset)),
                )
                .unwrap();
        }

        // Touch the older entry, making it the most recently used.
        assert!(c.lookup("old", Some("flac")).is_some());
        c.evict_to_capacity();

        assert!(
            old.exists(),
            "a cache hit must protect an entry from eviction"
        );
        assert!(!new.exists(), "the now-oldest entry is the eviction target");
    }

    #[test]
    fn eviction_is_a_noop_under_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        let p = c.entry_path("s1", Some("flac"));
        std::fs::write(&p, vec![0u8; 100]).unwrap();
        assert_eq!(c.evict_to_capacity(), 0);
        assert!(p.exists());
    }

    #[test]
    fn clear_removes_everything_and_reports_totals() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        std::fs::write(c.entry_path("s1", Some("flac")), vec![0u8; 10]).unwrap();
        std::fs::write(c.entry_path("s2", Some("mp3")), vec![0u8; 20]).unwrap();

        let (files, bytes) = c.clear();

        assert_eq!((files, bytes), (2, 30));
        assert_eq!(c.usage_bytes(), 0);
    }

    #[test]
    fn missing_root_reads_as_an_empty_cache() {
        let c = MediaCache::new(PathBuf::from("/nonexistent/ferrosonic-test"), true, 1024);
        assert_eq!(c.usage_bytes(), 0);
        assert_eq!(c.clear(), (0, 0));
        assert!(c.lookup("s1", Some("flac")).is_none());
    }

    #[test]
    fn empty_root_disables_the_cache() {
        let c = MediaCache::new(PathBuf::new(), true, 1024);
        assert!(
            !c.is_enabled(),
            "no resolvable cache dir must read as disabled even when configured on"
        );
    }

    #[test]
    fn sweep_removes_only_aged_download_temporaries() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        let fresh = dir.path().join(format!("{TEMP_PREFIX}fresh"));
        let stale = dir.path().join(format!("{TEMP_PREFIX}stale"));
        let entry = c.entry_path("s1", Some("flac"));
        for p in [&fresh, &stale, &entry] {
            std::fs::write(p, b"x").unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - TEMP_SWEEP_AGE - Duration::from_secs(60)),
            )
            .unwrap();

        c.sweep_stale_downloads();

        assert!(!stale.exists(), "aged temp file must be swept");
        assert!(fresh.exists(), "a live transfer's temp file must survive");
        assert!(entry.exists(), "cache entries are not temp files");
    }

    #[tokio::test]
    async fn store_refuses_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        c.set_enabled(false);
        let err = c
            .store("http://127.0.0.1:1/x", "s1", Some("flac"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaCacheError::Disabled));
    }

    #[tokio::test]
    async fn store_returns_the_existing_path_without_refetching() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        let p = c.entry_path("s1", Some("flac"));
        std::fs::write(&p, b"already here").unwrap();

        // An unroutable URL: reaching the network at all would fail the call.
        let got = c
            .store("http://127.0.0.1:1/x", "s1", Some("flac"), None)
            .await
            .unwrap();

        assert_eq!(got, p);
        assert_eq!(std::fs::read(&p).unwrap(), b"already here");
    }

    #[tokio::test]
    async fn store_leaves_no_partial_file_when_the_fetch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        let err = c
            .store("http://127.0.0.1:1/x", "s1", Some("flac"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaCacheError::Http(_)), "got {err:?}");
        assert!(!c.entry_path("s1", Some("flac")).exists());
        assert_eq!(c.usage_bytes(), 0, "no temp file may be left behind");
    }

    #[tokio::test]
    async fn in_flight_key_is_released_after_a_failed_store() {
        let dir = tempfile::tempdir().unwrap();
        let c = cache(&dir, 1024);
        for _ in 0..2 {
            let err = c
                .store("http://127.0.0.1:1/x", "s1", Some("flac"), None)
                .await
                .unwrap_err();
            assert!(
                !matches!(err, MediaCacheError::AlreadyInFlight),
                "a finished store must release its key: {err:?}"
            );
        }
    }
}
