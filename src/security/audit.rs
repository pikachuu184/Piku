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

/// Upper bound on the `op` name, in characters. Every call site passes a
/// literal, so this bounds nothing untrusted — it is here so no field reaches
/// the log without a cap.
const MAX_OP_CHARS: usize = 64;

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

impl Record {
    /// Build a record with every attacker-influenced field already sanitized.
    ///
    /// Split out of [`record`] so the sanitizing — the part that carries a
    /// security claim — can be exercised without the process-wide sink.
    fn sanitized(
        seq: u64,
        op: &str,
        src: &Path,
        dst: Option<&Path>,
        ok: bool,
        detail: &str,
    ) -> Self {
        use crate::security::text::sanitize_display;
        Self {
            seq,
            time: Local::now().to_rfc3339(),
            op: sanitize_display(op, MAX_OP_CHARS, false),
            src: clean_path(src),
            dst: dst.map(clean_path),
            ok,
            detail: sanitize_display(detail, MAX_DETAIL_CHARS, false),
        }
    }

    /// The record as the single JSON line that goes in the log.
    fn to_line(&self) -> String {
        serde_json::json!({
            "seq": self.seq,
            "time": self.time,
            "op": self.op,
            "src": self.src,
            "dst": self.dst,
            "ok": self.ok,
            "detail": self.detail,
        })
        .to_string()
    }
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

/// Allocate the next sequence number.
///
/// Taken when the record is *created*, not when it is written, so a record the
/// queue drops still leaves a visible gap.
fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// The directory the log lives in, or `None` if there is no per-user data
/// directory.
///
/// Computes a path and creates nothing, which is what lets a test assert where
/// the log may land without writing anywhere.
fn log_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("piku"))
}

/// Where the log lives, or `None` if there is nowhere it may go.
///
/// `persistence::data_dir()` falls back to the process working directory when
/// the platform cannot supply one. That fallback is fine for state files but
/// not for an audit log: it would scatter records containing absolute paths
/// into whatever directory the app was launched from.
///
/// `None` is a normal outcome, not a malfunction. On Linux `dirs::data_dir()`
/// is `$XDG_DATA_HOME` when that is absolute and `$HOME/.local/share`
/// otherwise, so a process started with no `HOME` has no data directory at all,
/// and one confined by a sandbox may have one it cannot create or write. Either
/// way auditing turns itself off. That is why the tests below never come
/// through here: asserting on the real log made them fail wherever this
/// correctly returned `None`.
fn log_path() -> Option<PathBuf> {
    let dir = log_dir()?;
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
                    let _ = writeln!(file, "{}", record.to_line());
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
    let record = Record::sanitized(next_seq(), op, src, dst, ok, detail);
    let seq = record.seq;

    tracing::info!(
        target: "piku::audit",
        seq,
        op = %record.op,
        src = %record.src,
        dst = %record.dst.as_deref().unwrap_or_default(),
        ok = record.ok,
        detail = %record.detail,
    );

    let Some(sink) = sink() else {
        return;
    };
    // `try_send`: if the writer is wedged on a stuck disk, drop the record
    // rather than stall the filesystem operation that produced it. The gap is
    // visible in `seq`, which is why `seq` is allocated before the send.
    if sink.tx.try_send(Message::Write(Box::new(record))).is_err() {
        tracing::warn!(target: "piku::audit", seq, "audit queue full; record dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log of this suite's own: its own file in the temp directory, with its
    /// own writer thread.
    ///
    /// These tests deliberately do **not** go through [`sink`] or [`record`].
    /// That sink is a process-wide `OnceLock` over the user's real data
    /// directory, which made the suite both destructive and
    /// environment-dependent. Destructive because `cargo test` appended its
    /// records to the machine's actual audit trail — a security artifact — where
    /// they outnumbered the real ones. Environment-dependent because a run whose
    /// [`log_path`] is `None` gets no sink at all, so [`record`] correctly drops
    /// every record and each assertion here collapsed to `0 == n`, reporting an
    /// unwritable data directory as six code failures.
    ///
    /// The code under test is unchanged: [`writer_loop`], [`open_append`],
    /// [`Record::sanitized`] and [`Record::to_line`] are the production
    /// functions, driven over a file this suite owns. Only the location differs.
    /// What that leaves uncovered is the three lines of [`record`] that hand a
    /// built record to the global sink; exercising those means writing to the
    /// machine's real trail, which is the thing being fixed.
    struct TestLog {
        dir: PathBuf,
        path: PathBuf,
        /// `Option` only so [`Drop`] can close the channel before joining:
        /// dropping the last sender is what ends [`writer_loop`].
        tx: Option<std::sync::mpsc::SyncSender<Message>>,
        writer: Option<std::thread::JoinHandle<()>>,
    }

    impl TestLog {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join("piku-audit-tests").join(tag);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            let path = dir.join("audit.log");

            let (tx, rx) = std::sync::mpsc::sync_channel::<Message>(QUEUE_DEPTH);
            let for_writer = path.clone();
            let writer = std::thread::Builder::new()
                .name(format!("piku-audit-test-{tag}"))
                .spawn(move || writer_loop(&for_writer, &rx))
                .expect("writer thread");

            Self {
                dir,
                path,
                tx: Some(tx),
                writer: Some(writer),
            }
        }

        fn tx(&self) -> &std::sync::mpsc::SyncSender<Message> {
            self.tx
                .as_ref()
                .expect("the channel is closed only by Drop")
        }

        /// Sanitize and queue one record the way [`record`] does.
        ///
        /// `send`, not `try_send`: a test that silently dropped a record would
        /// assert against a short log and blame the writer.
        fn record(&self, op: &str, src: &Path, dst: Option<&Path>, ok: bool, detail: &str) {
            let record = Record::sanitized(next_seq(), op, src, dst, ok, detail);
            self.tx()
                .send(Message::Write(Box::new(record)))
                .expect("queue a record");
        }

        /// Every line in the log, parsed, once the writer has drained.
        ///
        /// A line that is not valid JSON fails here rather than being filtered
        /// out, so a forged or half-written line cannot hide from the counts
        /// below — which is what makes `len()` a claim about the file and not
        /// merely about the records that happened to parse.
        fn lines(&self) -> Vec<serde_json::Value> {
            let (tx, rx) = std::sync::mpsc::channel();
            self.tx().send(Message::Sync(tx)).expect("queue a sync");
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .expect("writer drained the queue");

            let text = std::fs::read_to_string(&self.path).unwrap_or_default();
            text.lines()
                .map(|line| {
                    serde_json::from_str(line)
                        .unwrap_or_else(|error| panic!("log line is not JSON ({error}): {line:?}"))
                })
                .collect()
        }
    }

    impl Drop for TestLog {
        fn drop(&mut self) {
            // Close the channel first: that is what ends the writer's `recv`
            // loop. Joining before the directory goes away keeps a record in
            // flight from recreating the file after it is removed.
            drop(self.tx.take());
            if let Some(writer) = self.writer.take() {
                let _ = writer.join();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_record_round_trips_with_its_fields() {
        let log = TestLog::new("roundtrip");
        log.record(
            "move_file",
            Path::new("/tmp/a.txt"),
            Some(Path::new("/tmp/b.txt")),
            true,
            "",
        );

        let found = log.lines();
        assert_eq!(found.len(), 1, "expected exactly one record");
        let r = &found[0];
        assert_eq!(r["op"], "move_file");
        assert_eq!(r["src"], "/tmp/a.txt");
        assert_eq!(r["dst"], "/tmp/b.txt");
        assert_eq!(r["ok"], true);
        assert!(r["seq"].as_u64().is_some());
        assert!(r["time"].as_str().is_some());
    }

    /// A filename is attacker-controlled. Without sanitizing, a newline in one
    /// would forge an extra record — so this asserts on the number of lines in
    /// the file, not just on what the field ended up holding.
    #[test]
    fn path_text_cannot_forge_a_record() {
        let log = TestLog::new("forge");
        let hostile = "/tmp/a\n{\"op\":\"forged\"}\n\u{202E}evil\u{1B}[31m";
        log.record("delete_file", Path::new(hostile), None, false, "");

        let found = log.lines();
        assert_eq!(found.len(), 1, "a hostile path added a line: {found:#?}");
        assert_eq!(found[0]["op"], "delete_file", "the forged op won");
        let src = found[0]["src"].as_str().unwrap();
        assert!(!src.contains('\n'), "newline survived: {src:?}");
        assert!(!src.contains('\u{202E}'), "bidi override survived");
        assert!(!src.contains('\u{1B}'), "escape survived");
    }

    #[test]
    fn detail_is_capped() {
        let log = TestLog::new("cap");
        log.record(
            "delete_file",
            Path::new("/tmp/a"),
            None,
            false,
            &"x".repeat(5_000),
        );

        let found = log.lines();
        assert_eq!(found.len(), 1);
        let detail = found[0]["detail"].as_str().unwrap();
        // The cap plus the one ellipsis `sanitize_display` appends when it cuts.
        assert!(
            detail.chars().count() <= MAX_DETAIL_CHARS + 1,
            "detail not capped: {} chars",
            detail.chars().count()
        );
    }

    /// Increasing, not consecutive: [`SEQ`] is process-wide and the other tests
    /// draw from it in parallel. That is the property the log actually promises
    /// — a gap is visible — and asserting consecutiveness would make the test
    /// depend on which tests ran alongside it.
    #[test]
    fn sequence_numbers_are_monotonic_so_gaps_are_visible() {
        let log = TestLog::new("seq");
        for _ in 0..5 {
            log.record("copy_file", Path::new("/tmp/a"), None, true, "");
        }

        let found = log.lines();
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
    fn a_new_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let log = TestLog::new("perms-new");
        log.record("delete_file", Path::new("/tmp/a"), None, true, "");
        assert_eq!(log.lines().len(), 1, "nothing was written to stat");

        let mode = std::fs::metadata(&log.path)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "audit log is group/world accessible: {:o}",
            mode & 0o777
        );
    }

    /// `OpenOptions::mode` applies at creation only, so a log left at 0644 by an
    /// earlier version would stay readable by every local account forever.
    /// [`open_append`] tightens it on the way in — a claim its comment makes and
    /// nothing checked.
    #[cfg(unix)]
    #[test]
    fn an_existing_lax_log_is_tightened_on_open() {
        use std::os::unix::fs::PermissionsExt as _;
        let log = TestLog::new("perms-tighten");
        std::fs::write(&log.path, b"{\"seq\":0}\n").expect("seed a log");
        std::fs::set_permissions(&log.path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen it");

        drop(open_append(&log.path).expect("open"));

        let mode = std::fs::metadata(&log.path)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "a 0644 log was left readable: {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn concurrent_writers_do_not_interleave_or_lose_lines() {
        let log = TestLog::new("concurrent");
        std::thread::scope(|scope| {
            for t in 0..8 {
                let log = &log;
                scope.spawn(move || {
                    for i in 0..10 {
                        log.record(
                            "copy_file",
                            Path::new(&format!("/tmp/t{t}-{i}")),
                            None,
                            true,
                            "",
                        );
                    }
                });
            }
        });

        let found = log.lines();
        assert_eq!(found.len(), 80, "records lost or corrupted by interleaving");
        let mut seqs: Vec<u64> = found
            .iter()
            .map(|r| r["seq"].as_u64().expect("seq"))
            .collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 80, "two records shared a sequence number");
    }

    /// The one claim about the real location that holds without writing there:
    /// the log lives under the per-user data directory or nowhere at all. The
    /// `None` case is the documented refusal — no data directory means auditing
    /// is off, not a log dropped wherever the process started.
    #[test]
    fn the_log_never_lands_outside_the_data_directory() {
        match (log_dir(), dirs::data_dir()) {
            (Some(dir), Some(data)) => {
                assert!(dir.starts_with(&data), "{dir:?} is outside {data:?}");
                assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("piku"));
            }
            (None, None) => {}
            (dir, data) => panic!("log_dir() {dir:?} disagrees with data_dir() {data:?}"),
        }
    }
}
