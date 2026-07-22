//! Append-only audit trail for every mutating filesystem operation.
//!
//! Deliberately mutation-only: reads (directory listings, previews, bounded
//! `read_head` calls) are not recorded — they are high-volume, harmless, and
//! logging them would drown the trail that matters.

use std::io::Write;
use std::path::Path;

use chrono::Local;

/// Rotate the log once it grows past this; one previous generation is kept.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Best-effort size-based rotation: `audit.log` → `audit.log.1` (replacing
/// any older generation). Failures are ignored — same policy as the append.
fn rotate_if_needed(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() < MAX_LOG_BYTES {
        return;
    }
    let old = path.with_extension("log.1");
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::rename(path, &old);
}

/// Record a mutating operation. Best effort: auditing must never block or
/// fail the operation itself.
pub fn record(op: &str, src: &Path, dst: Option<&Path>, ok: bool, detail: &str) {
    tracing::info!(
        target: "piku::audit",
        op,
        src = %src.display(),
        dst = dst.map(|d| d.display().to_string()).unwrap_or_default(),
        ok,
        detail,
    );

    let line = serde_json::json!({
        "time": Local::now().to_rfc3339(),
        "op": op,
        "src": src.display().to_string(),
        "dst": dst.map(|d| d.display().to_string()),
        "ok": ok,
        "detail": detail,
    });

    let path = crate::state::persistence::data_dir().join("audit.log");
    rotate_if_needed(&path);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}
