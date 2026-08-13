//! Media cache wired into the daemon's playback paths: a hit hands mpv a
//! local file, a miss streams and fills the cache in the background, and the
//! settings setters apply to the running cache.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{song, TestDaemon};
use ferrosonic::daemon::core::PlayMode;
use serde_json::Value;
use serial_test::serial;

fn payload(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect()
}

/// Poll until `path` holds bytes, or give up. The cache fill runs in a
/// detached task, so there is no handle to await.
async fn wait_for_cached(path: &Path, timeout_ms: u64) -> bool {
    for _ in 0..(timeout_ms / 25) {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

async fn loaded_arg(td: &TestDaemon) -> Option<String> {
    td.fake_mpv
        .commands()
        .await
        .iter()
        .rev()
        .find(|c| c.first().and_then(Value::as_str) == Some("loadfile"))
        .and_then(|c| c.get(1).and_then(Value::as_str))
        .map(str::to_string)
}

#[tokio::test]
#[serial]
async fn miss_streams_from_the_server_and_fills_the_cache() {
    let td = TestDaemon::new().await;
    td.core.media_cache().set_enabled(true);
    let body = payload(64 * 1024);
    td.fake_subsonic
        .expect_stream_for("abc", body.clone())
        .await;

    {
        let mut s = td.state.write().await;
        s.queue.push(song("abc", "Track A"));
    }

    td.core
        .play_queue_position(0, PlayMode::Direct)
        .await
        .unwrap();

    let arg = loaded_arg(&td)
        .await
        .expect("mpv was given something to load");
    assert!(
        arg.starts_with("http"),
        "a cold cache must not delay playback; mpv streams while the fill runs, got {arg}"
    );

    let cached = td.core.media_cache().entry_path("abc", None);
    assert!(
        wait_for_cached(&cached, 5000).await,
        "the background fill must land a file at {}",
        cached.display()
    );
    assert_eq!(
        std::fs::read(&cached).unwrap(),
        body,
        "the cached copy must be byte-identical to the stream"
    );
}

#[tokio::test]
#[serial]
async fn hit_plays_the_local_file_without_touching_the_server() {
    let td = TestDaemon::new().await;
    let cache = td.core.media_cache();
    cache.set_enabled(true);
    let cached = cache.entry_path("abc", None);
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::write(&cached, payload(4096)).unwrap();

    {
        let mut s = td.state.write().await;
        s.queue.push(song("abc", "Track A"));
    }

    td.core
        .play_queue_position(0, PlayMode::Direct)
        .await
        .unwrap();

    assert_eq!(
        loaded_arg(&td).await.as_deref(),
        Some(cached.to_string_lossy().as_ref()),
        "a cached track must be loaded from disk, not re-streamed"
    );
    let streamed = td
        .fake_subsonic
        .received_requests()
        .await
        .iter()
        .any(|r| r.url.path() == "/rest/stream");
    assert!(!streamed, "a cache hit must issue no stream request at all");
}

#[tokio::test]
#[serial]
async fn buffered_mode_skips_prebuffering_a_cached_track() {
    let td = TestDaemon::new().await;
    let cache = td.core.media_cache();
    cache.set_enabled(true);
    let cached = cache.entry_path("abc", None);
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::write(&cached, payload(4096)).unwrap();

    {
        let mut s = td.state.write().await;
        s.queue.push(song("abc", "Track A"));
    }

    // Buffered exists to get a complete file on disk before mpv reads it. The
    // cache already did that, so the prebuffer temp file must not appear.
    td.core
        .play_queue_position(0, PlayMode::Buffered)
        .await
        .unwrap();

    let arg = loaded_arg(&td)
        .await
        .expect("mpv was given something to load");
    assert_eq!(
        arg,
        cached.to_string_lossy(),
        "a cached track must load directly, not via a prebuffer temp copy"
    );
    assert!(
        !arg.contains("ferrosonic-prebuf-"),
        "prebuffering a file already on disk is wasted work: {arg}"
    );
}

#[tokio::test]
#[serial]
async fn disabled_cache_streams_and_writes_nothing() {
    let td = TestDaemon::new().await;
    let cache = td.core.media_cache();
    assert!(!cache.is_enabled(), "media caching is off by default");
    td.fake_subsonic
        .expect_stream_for("abc", payload(4096))
        .await;

    {
        let mut s = td.state.write().await;
        s.queue.push(song("abc", "Track A"));
    }

    td.core
        .play_queue_position(0, PlayMode::Direct)
        .await
        .unwrap();

    let arg = loaded_arg(&td)
        .await
        .expect("mpv was given something to load");
    assert!(arg.starts_with("http"), "expected a stream URL, got {arg}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        cache.usage_bytes(),
        0,
        "a disabled cache must not write to disk"
    );
}

#[tokio::test]
#[serial]
async fn gapless_preload_appends_the_cached_next_track() {
    let td = TestDaemon::new().await;
    let cache = td.core.media_cache();
    cache.set_enabled(true);
    let next_cached = cache.entry_path("def", None);
    std::fs::create_dir_all(next_cached.parent().unwrap()).unwrap();
    std::fs::write(&next_cached, payload(4096)).unwrap();
    td.fake_subsonic
        .expect_stream_for("abc", payload(4096))
        .await;

    {
        let mut s = td.state.write().await;
        s.queue.push(song("abc", "Track A"));
        s.queue.push(song("def", "Track B"));
    }

    td.core
        .play_queue_position(0, PlayMode::Direct)
        .await
        .unwrap();

    let appended = td
        .fake_mpv
        .wait_for(5000, |cmds| {
            cmds.iter().any(|c| {
                c.first().and_then(Value::as_str) == Some("loadfile")
                    && c.get(2).and_then(Value::as_str) == Some("append")
                    && c.get(1).and_then(Value::as_str)
                        == Some(next_cached.to_string_lossy().as_ref())
            })
        })
        .await;
    assert!(
        appended,
        "the gapless preload must append the next track's cached file"
    );
}

#[tokio::test]
#[serial]
async fn enabling_the_cache_persists_and_activates_it() {
    let td = TestDaemon::new().await;

    td.core.set_media_cache(true).await.unwrap();

    assert!(td.core.media_cache().is_enabled());
    assert!(td.state.read().await.config.media_cache);
    let reloaded = ferrosonic::config::Config::load_default().unwrap();
    assert!(
        reloaded.media_cache,
        "the toggle must survive a daemon restart"
    );
}

#[tokio::test]
#[serial]
async fn size_setter_clamps_and_evicts_down_to_the_new_cap() {
    let td = TestDaemon::new().await;
    let cache = td.core.media_cache();
    td.core.set_media_cache(true).await.unwrap();

    std::fs::create_dir_all(cache.root()).unwrap();
    for id in ["a", "b", "c"] {
        std::fs::write(cache.entry_path(id, None), payload(80 * 1024 * 1024)).unwrap();
    }
    assert_eq!(cache.usage_bytes(), 240 * 1024 * 1024);

    // Below MEDIA_CACHE_MIN_MB: clamps up to 128 MB, which fits one 80 MB file.
    td.core.set_media_cache_size(1).await.unwrap();

    assert_eq!(
        td.state.read().await.config.media_cache_size_mb,
        ferrosonic::config::Config::MEDIA_CACHE_MIN_MB,
        "an out-of-range size must clamp, not be stored verbatim"
    );
    assert_eq!(
        cache.usage_bytes(),
        80 * 1024 * 1024,
        "lowering the cap must evict down to it"
    );
}

#[tokio::test]
#[serial]
async fn clearing_reports_what_it_removed() {
    let td = TestDaemon::new().await;
    let cache = td.core.media_cache();
    cache.set_enabled(true);
    std::fs::create_dir_all(cache.root()).unwrap();
    std::fs::write(cache.entry_path("a", None), payload(1000)).unwrap();
    std::fs::write(cache.entry_path("b", None), payload(2000)).unwrap();

    let (files, bytes) = td.core.clear_media_cache();

    assert_eq!((files, bytes), (2, 3000));
    assert_eq!(cache.usage_bytes(), 0);
}

#[tokio::test]
#[serial]
async fn disabling_the_cache_keeps_the_files_for_a_later_re_enable() {
    let td = TestDaemon::new().await;
    let cache = td.core.media_cache();
    td.core.set_media_cache(true).await.unwrap();
    std::fs::create_dir_all(cache.root()).unwrap();
    std::fs::write(cache.entry_path("a", None), payload(1000)).unwrap();

    td.core.set_media_cache(false).await.unwrap();

    assert!(!cache.is_enabled());
    assert_eq!(
        cache.usage_bytes(),
        1000,
        "switching off saves bandwidth, not disk; Clear Cache reclaims the space"
    );
    assert!(
        cache.lookup("a", None).is_none(),
        "a disabled cache must not serve files it still holds"
    );
}
