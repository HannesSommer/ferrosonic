//! The Settings media-cache readout: measured on every entry to the page, by
//! whichever route, and reachable on a terminal too short for the whole list.

mod common;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ferrosonic::app::state::Page;
use ferrosonic::app::App;
use ferrosonic::config::Config;
use serial_test::serial;

fn key(code: KeyCode) -> KeyEvent {
    let mut k = KeyEvent::new(code, KeyModifiers::NONE);
    k.kind = KeyEventKind::Press;
    k
}

fn click(x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

struct Fixture {
    app: App,
    _tempdir: tempfile::TempDir,
}

/// An app whose media cache already holds `mb` megabytes, as if the daemon had
/// filled it between visits to the Settings page.
async fn app_with_cached_mb(mb: usize) -> Fixture {
    let tempdir = common::tempdir();
    std::env::set_var("FERROSONIC_CONFIG_DIR", tempdir.path());
    std::env::set_var("FERROSONIC_CACHE_DIR", tempdir.path().join("cache"));
    let media = tempdir.path().join("cache").join("media");
    std::fs::create_dir_all(&media).expect("create media cache dir");
    std::fs::write(media.join("deadbeef.flac"), vec![0u8; mb * 1024 * 1024])
        .expect("write cached track");

    let mut config = Config::new();
    config.daemon = false;
    config.media_cache = true;
    Fixture {
        app: App::new(config),
        _tempdir: tempdir,
    }
}

#[tokio::test]
#[serial]
async fn f6_measures_the_cache_on_entry() {
    let mut fx = app_with_cached_mb(300).await;

    fx.app.handle_key(key(KeyCode::F(6))).await.unwrap();

    let usage = fx
        .app
        .client_state
        .read()
        .await
        .settings_state
        .media_cache_usage;
    assert_eq!(usage, Some(300 * 1024 * 1024));
}

/// Regression: clicking the Settings tab set the page but skipped the
/// measurement, so the readout stayed unset and the row showed no figure.
#[tokio::test]
#[serial]
async fn clicking_the_settings_tab_measures_the_cache_too() {
    let mut fx = app_with_cached_mb(300).await;
    // Lay out a frame so the header tab regions are populated for hit-testing.
    // Wide enough that the Settings tab falls inside the header's tab strip:
    // `region_at` reserves 30 columns on the right for the transport buttons.
    {
        let ds = fx.app.daemon_state.read().await.clone();
        let mut cs = fx.app.client_state.write().await;
        let _ = common::render(140, 40, &ds, &mut cs);
    }

    let tab_x = {
        let cs = fx.app.client_state.read().await;
        let header = cs.layout.header;
        (0..header.width)
            .find(|x| {
                ferrosonic::ui::header::Header::region_at(header, header.x + x, header.y)
                    == Some(ferrosonic::ui::header::HeaderRegion::Tab(Page::Settings))
            })
            .map(|x| cs.layout.header.x + x)
            .expect("a Settings tab region in the header")
    };
    fx.app.handle_mouse(click(tab_x, 0)).await.unwrap();

    let cs = fx.app.client_state.read().await;
    assert_eq!(cs.page, Page::Settings, "the click must switch pages");
    assert_eq!(
        cs.settings_state.media_cache_usage,
        Some(300 * 1024 * 1024),
        "reaching Settings by mouse must measure the cache, same as F6"
    );
}

#[tokio::test]
#[serial]
async fn the_usage_figure_is_rendered_on_the_clear_cache_row() {
    let mut fx = app_with_cached_mb(300).await;
    fx.app.handle_key(key(KeyCode::F(6))).await.unwrap();

    let ds = fx.app.daemon_state.read().await.clone();
    let mut cs = fx.app.client_state.write().await;
    let out = common::render(100, 40, &ds, &mut cs);

    assert!(out.contains("Clear Cache"), "the row must render:\n{out}");
    assert!(
        out.contains("300 MB used"),
        "the measured figure must reach the screen:\n{out}"
    );
}

/// The settings list is taller than a 24-row terminal. The rows below the fold
/// are still reachable by scrolling, but without a marker there is nothing to
/// tell the user the Media Cache section exists at all.
#[tokio::test]
#[serial]
async fn a_short_terminal_marks_the_rows_below_the_fold() {
    let mut fx = app_with_cached_mb(300).await;
    fx.app.handle_key(key(KeyCode::F(6))).await.unwrap();

    let ds = fx.app.daemon_state.read().await.clone();
    let mut cs = fx.app.client_state.write().await;

    let short = common::render(80, 24, &ds, &mut cs);
    assert!(
        !short.contains("Clear Cache"),
        "precondition: the section is off-screen at 80x24"
    );
    assert!(
        short.contains('▼'),
        "a truncated list must show there is more below:\n{short}"
    );

    let tall = common::render(100, 40, &ds, &mut cs);
    assert!(
        !tall.contains('▼') && !tall.contains('▲'),
        "a list that fits must not be marked as scrollable:\n{tall}"
    );
}

/// Scrolling down to the section reveals it, and the marker flips to show the
/// rows now hidden above.
#[tokio::test]
#[serial]
async fn scrolling_down_reveals_the_media_cache_section() {
    let mut fx = app_with_cached_mb(300).await;
    fx.app.handle_key(key(KeyCode::F(6))).await.unwrap();
    for _ in 0..12 {
        fx.app.handle_key(key(KeyCode::Down)).await.unwrap();
    }

    let ds = fx.app.daemon_state.read().await.clone();
    let mut cs = fx.app.client_state.write().await;
    let out = common::render(80, 24, &ds, &mut cs);

    assert!(
        out.contains("300 MB used"),
        "the Media Cache rows must be reachable on a short terminal:\n{out}"
    );
    assert!(
        out.contains('▲'),
        "with the top scrolled away the marker must point up:\n{out}"
    );
}
