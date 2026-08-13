use std::path::PathBuf;

/// Honors `FERROSONIC_CONFIG_DIR` for tests; XDG otherwise.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os("FERROSONIC_CONFIG_DIR") {
        return Some(PathBuf::from(override_path));
    }
    dirs::config_dir().map(|p| p.join("ferrosonic"))
}

/// Path of `config.toml` under the XDG config dir.
#[must_use]
pub fn config_file() -> Option<PathBuf> {
    config_dir().map(|p| p.join("config.toml"))
}

/// Path of the user themes directory.
#[must_use]
pub fn themes_dir() -> Option<PathBuf> {
    config_dir().map(|p| p.join("themes"))
}

/// Path of the daemon log file.
#[must_use]
pub fn log_file() -> Option<PathBuf> {
    config_dir().map(|p| p.join("ferrosonic.log"))
}

/// Path of the mpv IPC socket under the runtime dir.
#[must_use]
pub fn mpv_socket_path() -> PathBuf {
    // Prefer $XDG_RUNTIME_DIR (per-user, mode 0700) when present;
    // otherwise UID-scope the /tmp path so two users on the same host
    // do not collide on the shared socket.
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let rt = PathBuf::from(rt);
        if rt.exists() {
            return rt.join("ferrosonic-mpv.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("ferrosonic-mpv-{uid}.sock"))
}

/// Path of the persisted queue snapshot.
#[must_use]
pub fn queue_file() -> Option<PathBuf> {
    config_dir().map(|p| p.join("queue.json"))
}

/// Honors `FERROSONIC_CACHE_DIR` for tests; XDG otherwise.
///
/// Separate from [`config_dir`] because cached media is regenerable bulk data:
/// it belongs under `$XDG_CACHE_HOME`, where it is not swept up by config
/// backups and can be deleted at any time without losing user state.
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os("FERROSONIC_CACHE_DIR") {
        return Some(PathBuf::from(override_path));
    }
    dirs::cache_dir().map(|p| p.join("ferrosonic"))
}

/// Directory holding cached media files, one per track.
#[must_use]
pub fn media_cache_dir() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("media"))
}

/// Create the media cache directory if missing and return it.
///
/// On unix the directory is created `0o700`: cached media reveals what the
/// user listens to, so it stays owner-only like the config dir.
///
/// # Errors
/// Returns an error if the directory path cannot be determined or created.
pub fn ensure_media_cache_dir() -> std::io::Result<PathBuf> {
    let dir = media_cache_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine cache directory",
        )
    })?;

    if !dir.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&dir)?;
    }

    Ok(dir)
}

/// Create the config directory if missing and return it.
///
/// # Errors
/// Returns an error if the config directory cannot be created.
pub fn ensure_config_dir() -> std::io::Result<PathBuf> {
    let dir = config_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine config directory",
        )
    })?;

    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }

    Ok(dir)
}
