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

/// Atomic write: serialize to a temp file, then rename over the target so a
/// crash can never leave a truncated state file.
pub fn save_json<T: Serialize>(name: &str, value: &T) -> anyhow::Result<()> {
    let path = state_path(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    if path.exists() {
        fs::remove_file(&path).ok();
    }
    fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Upper bound on any state file we are willing to read into memory. PIKU
/// never writes files anywhere near this size; anything larger is corrupt or
/// tampered with, and callers fall back to defaults.
const MAX_STATE_FILE_BYTES: u64 = 8 * 1024 * 1024;

pub fn load_json<T: DeserializeOwned>(name: &str) -> anyhow::Result<T> {
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
