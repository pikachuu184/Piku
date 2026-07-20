//! Append-only audit trail for every mutating filesystem operation.

use std::io::Write;
use std::path::Path;

use chrono::Local;

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
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}
