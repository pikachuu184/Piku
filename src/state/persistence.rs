//! Atomic JSON persistence in the per-user application data directory.
//! State files hold paths and layout only — never credentials.

use std::fs;
use std::path::PathBuf;

use anyhow::Context as _;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn data_dir() -> PathBuf {
    let dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("piku");
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn state_path(name: &str) -> PathBuf {
    data_dir().join(name)
}

/// How many times to attempt the final replace, and how long to wait between
/// tries. Covers a transient Windows sharing violation without stalling a
/// shutdown save for long.
const RENAME_ATTEMPTS: u32 = 3;
const RENAME_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

/// Atomic write: serialize to a temp file, then rename over the target so a
/// crash can never leave a truncated state file.
pub fn save_json<T: Serialize>(name: &str, value: &T) -> anyhow::Result<()> {
    crate::app::diagnostics::assert_not_rendering("persistence::save_json");
    let path = state_path(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;

    // `rename` replaces the destination atomically on POSIX, and on Windows
    // `fs::rename` maps to a replacing move. Deleting the target first (as
    // this used to) opens a window where a crash loses the state file
    // outright — the exact failure the temp-then-rename dance exists to
    // prevent.
    //
    // Windows can still transiently refuse the replace with a sharing
    // violation if another process (an AV scanner, a previous instance
    // shutting down) has the file open, so retry briefly before giving up.
    let mut last_err = None;
    for attempt in 0..RENAME_ATTEMPTS {
        match fs::rename(&tmp, &path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_err = Some(error);
                if attempt + 1 < RENAME_ATTEMPTS {
                    std::thread::sleep(RENAME_BACKOFF);
                }
            }
        }
    }
    // Leave the temp file behind rather than deleting it: it holds the only
    // copy of the new state, and the next save overwrites it anyway.
    Err(last_err
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("rename failed"))
        .context(format!("replacing {}", path.display())))
}

/// Upper bound on any state file we are willing to read into memory. PIKU
/// never writes files anywhere near this size; anything larger is corrupt or
/// tampered with, and callers fall back to defaults.
const MAX_STATE_FILE_BYTES: u64 = 8 * 1024 * 1024;

pub fn load_json<T: DeserializeOwned>(name: &str) -> anyhow::Result<T> {
    crate::app::diagnostics::assert_not_rendering("persistence::load_json");
    let path = state_path(name);
    let len = fs::metadata(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    if len > MAX_STATE_FILE_BYTES {
        anyhow::bail!("state file {} is too large ({len} bytes)", path.display());
    }
    let json = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&json)?)
}
