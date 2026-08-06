//! Append-only audit trail for every mutating filesystem operation.
//!
//! Deliberately mutation-only: reads (directory listings, previews, bounded
//! `read_head` calls) are not recorded — they are high-volume, harmless, and
//! logging them would drown the trail that matters.
//!
//! # Threat model, and what is deliberately absent
//!
//! This log answers "what did PIKU change on this machine, and when" for the
//! machine's own user. It is **not** tamper-evident, and no HMAC chain or
//! signature will be added: the file lives in the user's own data directory,
//! so anyone who can alter it can also alter the key, and a chain would only
//! create the appearance of a guarantee. Tamper evidence needs an off-host
//! sink, which is a different feature.
//!
//! What the log *does* guarantee:
//!
//! * **Owner-only permissions** (`0600` on unix; `%APPDATA%`'s per-user ACL
//!   covers Windows). It records absolute paths, which are private.
//! * **No writes outside the app data directory.** If the platform cannot tell
//!   us where that is, auditing turns itself off rather than dropping a log
//!   into whatever directory the process happened to start in.
//! * **Sanitized fields.** Paths and error text are attacker-influenced; a
//!   filename containing a newline or an ANSI escape must not be able to forge
//!   a record or repaint a terminal reading it.
//! * **A monotonic sequence number**, so a gap is visible.
//! * **Serialized writes.** One writer thread owns the file, which both keeps
//!   the open/append/rotate off the thread that performed the mutation and
//!   removes the check-then-rename race two concurrent rotations had.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Local;

/// Rotate the log once it grows past this; one previous generation is kept.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Upper bound on the free-text `detail` field, in characters. It carries OS
/// error strings, which embed attacker-chosen filenames.
const MAX_DETAIL_CHARS: usize = 512;

/// Upper bound on a recorded path, in characters.
const MAX_PATH_CHARS: usize = 4096;

/// Queue depth for the writer thread. Deep enough to absorb a burst from a
/// recursive delete; bounded so a stuck disk cannot grow it without limit.
const QUEUE_DEPTH: usize = 1024;

/// One record, already sanitized, on its way to the writer thread.
struct Record {
    seq: u64,
    time: String,
    op: String,
    src: String,
    dst: Option<String>,
    ok: bool,
    detail: String,
}

enum Message {
    Write(Box<Record>),
    /// Flush marker used by tests to wait for the queue to drain. Production
    /// code never waits on the audit trail, which is why this is test-only.
    #[cfg(test)]
    Sync(std::sync::mpsc::Sender<()>),
}

struct Sink {
    tx: std::sync::mpsc::SyncSender<Message>,
}

static SINK: OnceLock<Option<Sink>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(1);

/// Where the log lives, or `None` if there is no per-user data directory.
///
/// `persistence::data_dir()` falls back to the process working directory when
/// the platform cannot supply one. That fallback is fine for state files but
/// not for an audit log: it would scatter records containing absolute paths
/// into whatever directory the app was launched from.
fn log_path() -> Option<PathBuf> {
    let dir = dirs::data_dir()?.join("piku");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::error!(%error, "cannot create the app data directory; auditing is disabled");
        return None;
    }
    Some(dir.join("audit.log"))
}

fn sink() -> Option<&'static Sink> {
    SINK.get_or_init(|| {
        let path = log_path()?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Message>(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name("piku-audit".into())
            .spawn(move || writer_loop(&path, &rx))
            .map_err(|error| {
                tracing::error!(%error, "cannot start the audit writer; auditing is disabled");
            })
            .ok()?;
        Some(Sink { tx })
    })
    .as_ref()
}

/// The single writer. Owning the file here is what makes rotation race-free:
/// the size check and the rename are never interleaved with another writer.
fn writer_loop(path: &Path, rx: &std::sync::mpsc::Receiver<Message>) {
    while let Ok(message) = rx.recv() {
        match message {
            Message::Write(record) => {
                rotate_if_needed(path);
                if let Some(mut file) = open_append(path) {
                    let line = serde_json::json!({
                        "seq": record.seq,
                        "time": record.time,
                        "op": record.op,
                        "src": record.src,
                        "dst": record.dst,
                        "ok": record.ok,
                        "detail": record.detail,
                    });
                    let _ = writeln!(file, "{line}");
                }
            }
            #[cfg(test)]
            Message::Sync(reply) => {
                let _ = reply.send(());
            }
        }
    }
}

/// Open the log for appending, owner-only.
fn open_append(path: &Path) -> Option<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // The log holds absolute paths from the user's filesystem; other local
        // accounts have no business reading it.
        options.mode(0o600);
    }
    let file = options.open(path).ok()?;
    #[cfg(unix)]
    {
        // `mode()` above applies **only at creation**. A log written by an
        // earlier version (or with a laxer umask) is already on disk at 0644,
        // and would stay that way forever. Tighten it on open.
        use std::os::unix::fs::PermissionsExt as _;
        if let Ok(metadata) = file.metadata() {
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    Some(file)
}

/// Size-based rotation: `audit.log` → `audit.log.1`, one generation kept.
///
/// `fsync` here and only here. Per-line syncing costs roughly an order of
/// magnitude in write throughput and buys nothing against this threat model —
/// but losing the whole previous generation to a crash mid-rotation would.
fn rotate_if_needed(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() < MAX_LOG_BYTES {
        return;
    }
    if let Some(file) = open_append(path) {
        let _ = file.sync_all();
    }
    let old = path.with_extension("log.1");
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::rename(path, &old);
}

/// Sanitize a path for recording: lossy text, stripped of anything that could
/// forge a record or repaint a terminal, and capped.
fn clean_path(path: &Path) -> String {
    crate::security::text::sanitize_display(&path.to_string_lossy(), MAX_PATH_CHARS, false)
}

/// Record a mutating operation. Best effort: auditing must never block or
/// fail the operation itself.
///
/// Both sinks — the tracing event and the JSON line — receive **sanitized**
/// fields. Sanitizing only the JSON would still let a filename containing a
/// newline inject a fake line into a console or journal reading the tracing
/// side.
pub fn record(op: &str, src: &Path, dst: Option<&Path>, ok: bool, detail: &str) {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let op = crate::security::text::sanitize_display(op, 64, false);
    let src = clean_path(src);
    let dst = dst.map(clean_path);
    let detail = crate::security::text::sanitize_display(detail, MAX_DETAIL_CHARS, false);

    tracing::info!(
        target: "piku::audit",
        seq,
        op = %op,
        src = %src,
        dst = %dst.as_deref().unwrap_or_default(),
        ok,
        detail = %detail,
    );

    let Some(sink) = sink() else {
        return;
    };
    let record = Record {
        seq,
        time: Local::now().to_rfc3339(),
        op,
        src,
        dst,
        ok,
        detail,
    };
    // `try_send`: if the writer is wedged on a stuck disk, drop the record
    // rather than stall the filesystem operation that produced it. The gap is
    // visible in `seq`, which is why `seq` is allocated before the send.
    if sink.tx.try_send(Message::Write(Box::new(record))).is_err() {
        tracing::warn!(target: "piku::audit", seq, "audit queue full; record dropped");
    }
}

/// Block until every record queued so far has been written.
///
/// Test-only: production code never waits on the audit trail.
#[cfg(test)]
fn flush() {
    if let Some(sink) = sink() {
        let (tx, rx) = std::sync::mpsc::channel();
        if sink.tx.send(Message::Sync(tx)).is_ok() {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the live log. These tests share one process-wide sink (it is a
    /// `OnceLock` plus a thread), so they assert on records they can identify
    /// by a unique `op` rather than on the file's whole contents.
    fn lines_for(op: &str) -> Vec<serde_json::Value> {
        flush();
        let Some(path) = log_path() else {
            return Vec::new();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v.get("op").and_then(|o| o.as_str()) == Some(op))
            .collect()
    }

    /// A marker no other run can collide with.
    ///
    /// The log is append-only and persists between `cargo test` invocations,
    /// so an op string derived from anything reused across runs (a counter, a
    /// fixed name) matches records left by an *earlier* run and makes exact
    /// count assertions flaky.
    fn unique_op(tag: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("test.{tag}.{}.{nanos}", std::process::id())
    }

    #[test]
    fn a_record_round_trips_with_its_fields() {
        let op = unique_op("roundtrip");
        record(
            &op,
            Path::new("/tmp/a.txt"),
            Some(Path::new("/tmp/b.txt")),
            true,
            "",
        );
        let found = lines_for(&op);
        assert_eq!(found.len(), 1, "expected exactly one record");
        let r = &found[0];
        assert_eq!(r["src"], "/tmp/a.txt");
        assert_eq!(r["dst"], "/tmp/b.txt");
        assert_eq!(r["ok"], true);
        assert!(r["seq"].as_u64().is_some());
        assert!(r["time"].as_str().is_some());
    }

    /// A filename is attacker-controlled. Without sanitizing, a newline in one
    /// would let it forge an extra record.
    #[test]
    fn path_text_cannot_forge_a_record() {
        let op = unique_op("forge");
        let hostile = "/tmp/a\n{\"op\":\"forged\"}\n\u{202E}evil\u{1B}[31m";
        record(&op, Path::new(hostile), None, false, "");
        let found = lines_for(&op);
        assert_eq!(found.len(), 1);
        let src = found[0]["src"].as_str().unwrap();
        assert!(!src.contains('\n'), "newline survived: {src:?}");
        assert!(!src.contains('\u{202E}'), "bidi override survived");
        assert!(!src.contains('\u{1B}'), "escape survived");
        // And no forged record appeared.
        assert!(lines_for("forged").is_empty());
    }

    #[test]
    fn detail_is_capped() {
        let op = unique_op("cap");
        record(&op, Path::new("/tmp/a"), None, false, &"x".repeat(5_000));
        let found = lines_for(&op);
        assert_eq!(found.len(), 1);
        let detail = found[0]["detail"].as_str().unwrap();
        assert!(
            detail.chars().count() <= MAX_DETAIL_CHARS + 1,
            "detail not capped: {} chars",
            detail.chars().count()
        );
    }

    #[test]
    fn sequence_numbers_are_monotonic_so_gaps_are_visible() {
        let op = unique_op("seq");
        for _ in 0..5 {
            record(&op, Path::new("/tmp/a"), None, true, "");
        }
        let found = lines_for(&op);
        assert_eq!(found.len(), 5);
        let seqs: Vec<u64> = found
            .iter()
            .map(|r| r["seq"].as_u64().expect("seq"))
            .collect();
        for pair in seqs.windows(2) {
            assert!(pair[1] > pair[0], "sequence not increasing: {seqs:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let op = unique_op("perms");
        record(&op, Path::new("/tmp/a"), None, true, "");
        flush();
        let path = log_path().expect("data dir");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "audit log is group/world accessible: {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn concurrent_writers_do_not_interleave_or_lose_lines() {
        let op = unique_op("concurrent");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let op = op.clone();
                std::thread::spawn(move || {
                    for i in 0..10 {
                        record(&op, Path::new(&format!("/tmp/t{t}-{i}")), None, true, "");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("join");
        }
        let found = lines_for(&op);
        assert_eq!(found.len(), 80, "records lost or corrupted by interleaving");
    }
}
