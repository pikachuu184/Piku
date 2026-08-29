//! Every user-visible string the transfer engine produces, in one place.
//!
//! These are pure functions over plain data — no `Window`, no `Context`, no
//! filesystem. That is deliberate: the backend re-architecture replaces the
//! engine underneath, and the golden table in this module's tests is the proof
//! that the toasts a user sees did not drift while it happened. Change a
//! string here only when you mean to change what the user reads.

use std::path::Path;
use std::time::Duration;

use crate::backend::error::TransferError;
use crate::backend::services::transfer::conflict::{Conflict, NameClash, Side};
use crate::core::format::{format_size, format_time};
use crate::services::jobs::job::{Job, JobKind, JobStatus};

/// The sentinel a worker returns when it stopped because the user cancelled.
/// Matched by [`is_cancelled`] rather than compared inline at call sites.
pub const CANCELLED: &str = "cancelled";

/// The toast shown for a cancelled job.
pub const CANCELLED_TOAST: &str = "Operation cancelled";

/// Whether a worker's failure string means "the user cancelled" rather than
/// "something went wrong".
pub fn is_cancelled(error: &str) -> bool {
    error == CANCELLED
}

/// The display name for a path: its final component, falling back to the whole
/// path when there isn't one (a bare root).
pub fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `""` for one, `"s"` for any other count.
pub fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

// ---------------------------------------------------------------------------
// Job titles (shown in the status bar while the job runs)
// ---------------------------------------------------------------------------

/// Title for a copy or move: the kind's verb plus either the single item's
/// name or a count.
pub fn transfer_title(verb: &str, first: &Path, count: usize) -> String {
    if count == 1 {
        format!("{verb} “{}”", file_label(first))
    } else {
        format!("{verb} {count} items")
    }
}

pub fn delete_title(first: &Path, count: usize) -> String {
    if count == 1 {
        format!("Deleting “{}”", file_label(first))
    } else {
        format!("Deleting {count} items")
    }
}

pub fn rename_title(from: &Path) -> String {
    format!("Renaming “{}”", file_label(from))
}

pub fn create_title(path: &Path) -> String {
    format!("Creating “{}”", file_label(path))
}

pub fn fetch_title(remote: &str) -> String {
    format!("Fetching “{remote}”")
}

// ---------------------------------------------------------------------------
// Success messages (shown as a toast when the job finishes)
// ---------------------------------------------------------------------------

pub fn transfer_summary(is_move: bool, count: usize) -> String {
    let verb = if is_move { "Moved" } else { "Copied" };
    format!("{verb} {count} item{}", plural(count))
}

pub fn delete_summary(count: usize) -> String {
    format!("Moved {count} item{} to the Recycle Bin", plural(count))
}

pub fn rename_summary(to: &Path) -> String {
    format!("Renamed to “{}”", file_label(to))
}

pub fn create_summary(path: &Path) -> String {
    format!("Created “{}”", file_label(path))
}

pub fn fetch_summary(remote: &str, updated_refs: usize) -> String {
    format!(
        "Fetched “{remote}” — {updated_refs} ref{} updated",
        plural(updated_refs)
    )
}

// ---------------------------------------------------------------------------
// Failure messages
// ---------------------------------------------------------------------------

/// Refusing to copy or move a directory into its own subtree.
///
/// `allow`, not `expect`: only the golden table below calls this now, and it is
/// what pins the engine's spelling — `transfer_error(&TransferError::IntoItself)`
/// renders from an already-sanitized display path, this renders from a `Path`, and
/// the test asserts the two agree. An `expect` would be unfulfilled in the binary.
#[allow(
    dead_code,
    reason = "the reference spelling the golden table pins against"
)]
pub fn into_itself(is_move: bool, source: &Path) -> String {
    into_itself_named(is_move, &file_label(source))
}

/// A move whose copy half succeeded but whose source removal did not — the
/// data is safe, but the user has two copies.
///
/// Test-only for the same reason as [`into_itself`].
#[allow(
    dead_code,
    reason = "the reference spelling the golden table pins against"
)]
pub fn source_cleanup_failed(source: &Path, error: &dyn std::fmt::Display) -> String {
    source_cleanup_named(&file_label(source), error)
}

// The two sentences above also arrive as `TransferError` variants from the
// engine, which carries an already-sanitized display path rather than a `Path`.
// Both spellings go through these, so the wording has one home and the golden
// table pins one string per condition rather than two that look alike.

fn into_itself_named(is_move: bool, name: &str) -> String {
    format!(
        "Cannot {} “{name}” into itself",
        if is_move { "move" } else { "copy" }
    )
}

fn source_cleanup_named(name: &str, error: &dyn std::fmt::Display) -> String {
    format!("Copied, but could not remove source “{name}”: {error}")
}

/// The last component of an already-sanitized display path.
///
/// Purely textual — it splits on both separators regardless of platform,
/// because the string may have come from a path this machine did not create,
/// and it is never used to reach the filesystem. Anything path-shaped that is
/// going to be *opened* goes through
/// [`PathPolicy`](crate::backend::path::PathPolicy), not this.
fn leaf(display_path: &str) -> &str {
    let trimmed = display_path.trim_end_matches(['/', '\\']);
    match trimmed.rsplit_once(['/', '\\']) {
        // A trailing separator was all there was: a root, so say the root.
        Some((_, "")) | None => display_path,
        Some((_, last)) => last,
    }
}

/// The polished sentence for a transfer failure.
///
/// [`TransferError`]'s own `Display` is the technical form that goes to logs and
/// to `BackendError::user_message`; this is what a toast says. `error.rs`'s doc
/// comment points here by name, and the golden table below is why that pointer
/// can be trusted.
///
/// Every path this touches is already through `security::text`, because the
/// engine sanitizes at construction — see `NameClash`. Nothing here re-escapes
/// anything, and nothing here has a raw path to escape.
pub fn transfer_error(error: &TransferError) -> String {
    match error {
        // The path and file layers already own their wording, and it is
        // golden-tested where it is defined. Restating it here would be a
        // second copy that drifts.
        TransferError::Path(inner) => inner.to_string(),
        TransferError::File(inner) => inner.to_string(),
        TransferError::IntoItself { verb, path } => into_itself_named(verb.is_move(), leaf(path)),
        TransferError::NoOpMove(path) => {
            format!("“{}” is already in that folder", leaf(path))
        }
        TransferError::InsufficientSpace { needed, available } => format!(
            "Not enough space — {} more is needed, and {} is free",
            format_size(*needed),
            format_size(*available)
        ),
        TransferError::NotWritable(path) => {
            format!("“{}” cannot be written to", leaf(path))
        }
        TransferError::SourceCleanup { path, detail } => source_cleanup_named(leaf(path), detail),
        // The window went away, or the app is quitting, while a job sat waiting
        // for an answer. Named as the job's own outcome rather than as a
        // mechanism, because "the oneshot closed" means nothing to anyone.
        TransferError::Abandoned => "Stopped without an answer".to_string(),
        // Reachable only if a cancellation is reported as a failure rather than
        // as `Cancelled`. Same string either way, so the two cannot read
        // differently.
        TransferError::Cancelled => CANCELLED_TOAST.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Permanent delete
//
// Its own set of strings, and every one of them says "permanently". The trash
// is the default everywhere else in the app, so the only defence against a user
// reading past this is that it does not look like the sentence they have
// dismissed a hundred times.
// ---------------------------------------------------------------------------

pub fn delete_permanent_title(first: &Path, count: usize) -> String {
    if count == 1 {
        format!("Deleting “{}” permanently", file_label(first))
    } else {
        format!("Deleting {count} items permanently")
    }
}

pub fn delete_permanent_summary(count: usize) -> String {
    format!("Permanently deleted {count} item{}", plural(count))
}

/// The trash confirmation's body. Takes a display name rather than a path
/// because the explorer already has one on `FsEntry`.
pub fn delete_description(first_name: &str, count: usize) -> String {
    if count == 1 {
        format!("“{first_name}” will be moved to the Recycle Bin.")
    } else {
        format!("{count} items will be moved to the Recycle Bin.")
    }
}

/// The permanent-delete confirmation's body. The second sentence is the whole
/// point of the dialog.
pub fn delete_permanent_description(first_name: &str, count: usize) -> String {
    if count == 1 {
        format!("“{first_name}” will be deleted permanently. This cannot be undone.")
    } else {
        format!("{count} items will be deleted permanently. This cannot be undone.")
    }
}

// ---------------------------------------------------------------------------
// Numbers a card renders
// ---------------------------------------------------------------------------

/// Grouped by thousands, because `1032 files` and `10321 files` are the same
/// shape at a glance and a transfer's whole job is telling you how big it is.
pub fn thousands(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// Bytes per second. `—` at zero, matching [`format_time`]'s empty rendering:
/// a rate of nothing is a rate not yet known, and `0 B/s` reads like a stall.
pub fn throughput(bytes_per_second: u64) -> String {
    if bytes_per_second == 0 {
        return "—".to_string();
    }
    format!("{}/s", format_size(bytes_per_second))
}

/// `MM:SS` under an hour, `H:MM:SS` above it.
///
/// Clock-shaped rather than `4 min 31 s`, because it sits beside a moving
/// number and a settling ETA that changes every 200 ms — fixed-width digits
/// stay readable while they move.
pub fn duration_label(remaining: Duration) -> String {
    let total = remaining.as_secs();
    // Past a day the figure is noise: it comes from a median over eight
    // samples, and no honest rendering of it is a countdown.
    if total >= 24 * 3600 {
        return "over a day".to_string();
    }
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// The ETA segment. `—` until the rate ring has enough samples to mean
/// anything — see `backend::services::transfer::rate`.
pub fn eta_label(eta: Option<Duration>) -> String {
    match eta {
        Some(remaining) => format!("{} remaining", duration_label(remaining)),
        None => "—".to_string(),
    }
}

/// The card's numeric row: `6.4 GB / 8.2 GB · 812 / 1,032 items · 118 MB/s ·
/// 00:14 remaining`.
///
/// Every segment is dropped when it would be a lie rather than shown as a zero.
/// A job still scanning has no totals, a queued job has no rate, and a rate ring
/// with two samples has no ETA; `0 B / 0 B · 0 B/s · 00:00 remaining` claims all
/// three.
pub fn progress_line(job: &Job) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(4);
    if job.total_bytes > 0 {
        parts.push(format!(
            "{} / {}",
            format_size(job.done_bytes),
            format_size(job.total_bytes)
        ));
    }
    if job.total_items > 0 {
        parts.push(format!(
            "{} / {} item{}",
            thousands(job.done_items as u64),
            thousands(job.total_items as u64),
            plural(job.total_items)
        ));
    }
    // Only while bytes are actually moving. A paused job's last reading is not
    // its current one, and its status already says so.
    if job.is_moving() && job.throughput > 0 {
        parts.push(throughput(job.throughput));
    }
    if let Some(remaining) = job.eta.filter(|_| job.is_moving()) {
        parts.push(eta_label(Some(remaining)));
    }
    parts.join(" · ")
}

// ---------------------------------------------------------------------------
// States and aggregates
// ---------------------------------------------------------------------------

/// The status word on a card.
///
/// Title case, unlike `TransferState::label`, which is lowercase because it goes
/// into structured log fields. Two audiences, two spellings, and the one that
/// reaches a person lives here with the rest.
pub fn status_label(status: &JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "Queued",
        JobStatus::Running => "Running",
        JobStatus::Pausing => "Pausing…",
        JobStatus::Paused => "Paused",
        JobStatus::WaitingForInput => "Waiting for input",
        JobStatus::Done => "Completed",
        JobStatus::Failed(_) => "Failed",
        JobStatus::Cancelled => "Cancelled",
    }
}

/// Fixed order, so the popover's summary does not reshuffle itself between
/// ticks.
const GERUNDS: [&str; 6] = [
    "copying", "moving", "deleting", "renaming", "creating", "fetching",
];

fn gerund_index(kind: JobKind) -> usize {
    match kind {
        JobKind::Copy => 0,
        JobKind::Move => 1,
        // The popover does not shout "permanently". The confirmation already
        // did, and by here the answer is given.
        JobKind::Delete | JobKind::DeletePermanent => 2,
        JobKind::Rename => 3,
        JobKind::NewFolder | JobKind::NewFile => 4,
        JobKind::GitFetch => 5,
    }
}

/// The popover's one-line headline: `2 copying · 1 waiting`.
///
/// Finished jobs are not counted. They are history, and the status bar is about
/// what is happening now.
pub fn activity_summary(jobs: &[Job]) -> String {
    let mut running = [0usize; GERUNDS.len()];
    let (mut waiting, mut paused, mut queued) = (0usize, 0usize, 0usize);
    for job in jobs {
        match &job.status {
            JobStatus::Running | JobStatus::Pausing => running[gerund_index(job.kind)] += 1,
            JobStatus::WaitingForInput => waiting += 1,
            JobStatus::Paused => paused += 1,
            JobStatus::Queued => queued += 1,
            JobStatus::Done | JobStatus::Failed(_) | JobStatus::Cancelled => {}
        }
    }

    let mut parts: Vec<String> = Vec::new();
    for (index, count) in running.iter().enumerate() {
        if *count > 0 {
            parts.push(format!("{count} {}", GERUNDS[index]));
        }
    }
    for (count, word) in [(waiting, "waiting"), (paused, "paused"), (queued, "queued")] {
        if count > 0 {
            parts.push(format!("{count} {word}"));
        }
    }

    if parts.is_empty() {
        return "No transfers".to_string();
    }
    parts.join(" · ")
}

/// The transfer center's dialog title — static, so it cannot go stale in the
/// dialog layer's builder. See [`center_title`] for the live version, which the
/// center renders itself.
pub const TRANSFERS: &str = "Transfers";

/// The transfer center's own header line.
pub fn center_title(active: usize) -> String {
    if active == 0 {
        TRANSFERS.to_string()
    } else {
        format!("{TRANSFERS} · {active} active")
    }
}

// ---------------------------------------------------------------------------
// Conflicts
// ---------------------------------------------------------------------------

pub fn conflict_heading(count: usize) -> String {
    format!("{count} conflict{} found", plural(count))
}

/// One conflict's headline. The name/size/time comparison is rendered
/// structurally beside it, so this is the sentence, not the data.
pub fn conflict_summary(conflict: &Conflict) -> String {
    match conflict {
        Conflict::NameTaken(clash) => format!("“{}” already exists", clash.relative),
        Conflict::InsufficientSpace { needed, available } => format!(
            "Not enough space — {} more is needed, and {} is free",
            format_size(*needed),
            format_size(*available)
        ),
        Conflict::Denied { path, detail } => format!("“{path}” was refused: {detail}"),
    }
}

/// What `Keep Both` will actually write, shown next to the button so the
/// resolved name is a promise rather than a surprise.
pub fn keep_both_hint(clash: &NameClash) -> String {
    format!("Saves it as “{}”", clash.keep_both)
}

/// One side of a clash: how big it is and when it last changed.
///
/// The two things a user compares to decide which copy they want. A directory
/// says so instead of reporting a size, because [`Side::of`] does not walk it and
/// `0 B` would read as an empty folder rather than as "not measured".
pub fn side_label(side: &Side) -> String {
    let size = if side.is_dir {
        "Folder".to_string()
    } else {
        format_size(side.size)
    };
    format!("{size} · {}", format_time(side.modified))
}

/// The conflict surface's dialog title.
///
/// Static, unlike [`conflict_heading`]. The dialog layer rebuilds a dialog's
/// title only when `Root` renders, so a count up there would freeze while the
/// engine kept re-asking; the live count is the first line of the content.
pub const CONFLICT_TITLE: &str = "Resolve conflicts";

/// The four per-entry answers, exactly as the buttons spell them.
pub const REPLACE: &str = "Replace";
pub const SKIP: &str = "Skip";
pub const KEEP_BOTH: &str = "Keep Both";
pub const RENAME: &str = "Rename";

/// Confirms a typed rename for one row.
pub const SAVE: &str = "Save";

/// The blanket-answer checkbox.
pub const APPLY_TO_ALL: &str = "Apply to all conflicts";

/// The conflict surface's footer buttons. `CANCEL_TRANSFER` says which cancel
/// this is: the surface can also just be dismissed, which leaves the job waiting.
pub const APPLY: &str = "Apply";
pub const CANCEL_TRANSFER: &str = "Cancel transfer";

/// Waves a whole-job block through — a space shortfall, or a refused path.
pub const CONTINUE_ANYWAY: &str = "Continue anyway";

/// Which side of a clash a figure belongs to.
pub const INCOMING: &str = "New";
pub const EXISTING: &str = "Existing";

/// Why the blanket answer is the only one on offer.
///
/// A [`Decision`](crate::backend::services::transfer::conflict::Decision) is
/// delivered once per ask, and only `All` clears a whole-job block as well as the
/// name clashes — so when a job is stopped by both, per-row answers cannot settle
/// it. Saying that is better than a disabled button with no explanation.
pub const CONFLICT_ALL_REQUIRED: &str =
    "This job is blocked as a whole, so one answer has to cover every conflict.";

/// How many rows still have no answer, for the footer beside a disabled `Apply`.
pub fn conflict_remaining(count: usize) -> String {
    if count == 1 {
        "1 still needs an answer".to_string()
    } else {
        format!("{count} still need an answer")
    }
}

// ---------------------------------------------------------------------------
// The two transfer surfaces
// ---------------------------------------------------------------------------

/// The controls that apply to every job at once, on both surfaces.
pub const PAUSE_ALL: &str = "Pause All";
pub const RESUME_ALL: &str = "Resume All";

/// Opens the transfer center from the popover.
pub const DETAILS: &str = "Details";

/// Opens the conflict surface for a job that stopped for one.
pub const RESOLVE: &str = "Resolve";

/// Drops finished jobs from the store. History, not work — so this destroys
/// nothing on disk, which is why it needs no confirmation.
pub const CLEAR_FINISHED: &str = "Clear finished";

/// The transfer center's group headings.
///
/// Two of them read the same as their [`status_label`] — deliberately, since a
/// card in the `Queued` group says `Queued` — but they are separate constants
/// because one names a section and the other names one job's state, and a future
/// change to either should not silently move the other.
pub const GROUP_ACTIVE: &str = "Active";
pub const GROUP_WAITING: &str = "Waiting for input";
pub const GROUP_QUEUED: &str = "Queued";
pub const GROUP_FINISHED: &str = "Finished";

/// The center with nothing in it.
pub const CENTER_EMPTY_TITLE: &str = "No transfers";
pub const CENTER_EMPTY_HINT: &str = "Copies, moves, and deletes appear here while they run.";

/// A job that stopped for a decision the user has since dismissed, or that
/// finished while its conflict surface was open.
pub const NOT_WAITING: &str = "This transfer is no longer waiting for an answer.";

/// The kebab's `Retry` on a permanent delete. The first run went through
/// `dialogs::confirm_delete_permanent`; a menu item is not a licence to skip it,
/// so the same sentence is asked again. See
/// [`JobKind::needs_reconfirm`](super::JobKind::needs_reconfirm).
pub const RETRY_PERMANENT_TITLE: &str = "Run this permanent delete again?";
pub const RETRY_PERMANENT_BODY: &str =
    "Anything it removes is deleted permanently. This cannot be undone.";
pub const RETRY_PERMANENT_OK: &str = "Delete permanently";

/// The count of items a finished job stepped over by policy, for its row in the
/// finished group. Not the same as [`failed_note`]: these were skipped because
/// the user said to skip them.
pub fn skipped_note(count: u64) -> String {
    format!("{count} item{} skipped", plural(count as usize))
}

/// The count of items a finished job could not handle. Not the job's own
/// failure — see [`Job::failures`].
pub fn failed_note(count: usize) -> String {
    format!("{count} item{} failed", plural(count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::backend::services::transfer::conflict::{ConflictPolicy, Side};

    /// The golden table. Every entry is a string a user can actually see.
    ///
    /// This test exists to fail loudly if the backend rewrite changes what the
    /// UI says. If you are here because it broke: either you changed a string
    /// on purpose (update the expectation and say so in the commit) or you
    /// changed it by accident (fix the code, not the test).
    #[test]
    fn golden_strings() {
        let one = PathBuf::from("/home/u/report.pdf");
        let dir = PathBuf::from("/home/u/photos");

        // Titles
        assert_eq!(transfer_title("Copying", &one, 1), "Copying “report.pdf”");
        assert_eq!(transfer_title("Copying", &one, 4), "Copying 4 items");
        assert_eq!(transfer_title("Moving", &one, 1), "Moving “report.pdf”");
        assert_eq!(transfer_title("Moving", &one, 9), "Moving 9 items");
        assert_eq!(delete_title(&one, 1), "Deleting “report.pdf”");
        assert_eq!(delete_title(&one, 3), "Deleting 3 items");
        assert_eq!(rename_title(&one), "Renaming “report.pdf”");
        assert_eq!(create_title(&dir), "Creating “photos”");
        assert_eq!(fetch_title("origin"), "Fetching “origin”");

        // Success messages
        assert_eq!(transfer_summary(false, 1), "Copied 1 item");
        assert_eq!(transfer_summary(false, 12), "Copied 12 items");
        assert_eq!(transfer_summary(true, 1), "Moved 1 item");
        assert_eq!(transfer_summary(true, 0), "Moved 0 items");
        assert_eq!(delete_summary(1), "Moved 1 item to the Recycle Bin");
        assert_eq!(delete_summary(5), "Moved 5 items to the Recycle Bin");
        assert_eq!(rename_summary(&one), "Renamed to “report.pdf”");
        assert_eq!(create_summary(&one), "Created “report.pdf”");
        assert_eq!(
            fetch_summary("origin", 1),
            "Fetched “origin” — 1 ref updated"
        );
        assert_eq!(
            fetch_summary("upstream", 0),
            "Fetched “upstream” — 0 refs updated"
        );

        // Failures
        assert_eq!(into_itself(false, &dir), "Cannot copy “photos” into itself");
        assert_eq!(into_itself(true, &dir), "Cannot move “photos” into itself");
        assert_eq!(
            source_cleanup_failed(&dir, &"permission denied"),
            "Copied, but could not remove source “photos”: permission denied"
        );

        // Cancellation
        assert_eq!(CANCELLED, "cancelled");
        assert_eq!(CANCELLED_TOAST, "Operation cancelled");
        assert!(is_cancelled("cancelled"));
        assert!(!is_cancelled("Cancelled"));
        assert!(!is_cancelled("permission denied"));
    }

    /// The transfer subsystem's half of the table. Same contract: these are
    /// sentences a user reads, so a change here is a change to the product.
    #[test]
    fn golden_transfer_strings() {
        let one = PathBuf::from("/home/u/report.pdf");

        // Permanent delete. Every one of these says the word.
        assert_eq!(
            delete_permanent_title(&one, 1),
            "Deleting “report.pdf” permanently"
        );
        assert_eq!(
            delete_permanent_title(&one, 7),
            "Deleting 7 items permanently"
        );
        assert_eq!(delete_permanent_summary(1), "Permanently deleted 1 item");
        assert_eq!(delete_permanent_summary(4), "Permanently deleted 4 items");
        assert_eq!(
            delete_permanent_description("report.pdf", 1),
            "“report.pdf” will be deleted permanently. This cannot be undone."
        );
        assert_eq!(
            delete_permanent_description("report.pdf", 3),
            "3 items will be deleted permanently. This cannot be undone."
        );
        // Byte-identical to what `dialogs::confirm_delete` built inline before
        // it moved here.
        assert_eq!(
            delete_description("report.pdf", 1),
            "“report.pdf” will be moved to the Recycle Bin."
        );
        assert_eq!(
            delete_description("report.pdf", 3),
            "3 items will be moved to the Recycle Bin."
        );

        // Numbers
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_032), "1,032");
        assert_eq!(thousands(12_345_678), "12,345,678");
        assert_eq!(throughput(0), "—");
        assert_eq!(throughput(118 * 1024 * 1024), "118 MB/s");
        assert_eq!(throughput(1024), "1.0 KB/s");
        assert_eq!(duration_label(Duration::from_secs(14)), "00:14");
        assert_eq!(duration_label(Duration::from_secs(271)), "04:31");
        assert_eq!(duration_label(Duration::from_secs(3599)), "59:59");
        assert_eq!(duration_label(Duration::from_secs(3753)), "1:02:33");
        assert_eq!(duration_label(Duration::from_secs(86_399)), "23:59:59");
        assert_eq!(duration_label(Duration::from_secs(86_400)), "over a day");
        assert_eq!(eta_label(None), "—");
        assert_eq!(eta_label(Some(Duration::from_secs(14))), "00:14 remaining");

        // States
        assert_eq!(status_label(&JobStatus::Queued), "Queued");
        assert_eq!(status_label(&JobStatus::Running), "Running");
        assert_eq!(status_label(&JobStatus::Pausing), "Pausing…");
        assert_eq!(status_label(&JobStatus::Paused), "Paused");
        assert_eq!(
            status_label(&JobStatus::WaitingForInput),
            "Waiting for input"
        );
        assert_eq!(status_label(&JobStatus::Done), "Completed");
        assert_eq!(status_label(&JobStatus::Failed("x".into())), "Failed");
        assert_eq!(status_label(&JobStatus::Cancelled), "Cancelled");

        // Aggregates
        assert_eq!(TRANSFERS, "Transfers");
        assert_eq!(center_title(0), "Transfers");
        assert_eq!(center_title(3), "Transfers · 3 active");
        assert_eq!(conflict_heading(1), "1 conflict found");
        assert_eq!(conflict_heading(8), "8 conflicts found");
        assert_eq!(skipped_note(1), "1 item skipped");
        assert_eq!(skipped_note(9), "9 items skipped");
        assert_eq!(failed_note(1), "1 item failed");
        assert_eq!(failed_note(2), "2 items failed");

        // The conflict surface. Every one of these is a button a user clicks or a
        // sentence explaining why they cannot.
        assert_eq!(CONFLICT_TITLE, "Resolve conflicts");
        assert_eq!(REPLACE, "Replace");
        assert_eq!(SKIP, "Skip");
        assert_eq!(KEEP_BOTH, "Keep Both");
        assert_eq!(RENAME, "Rename");
        assert_eq!(SAVE, "Save");
        assert_eq!(APPLY, "Apply");
        assert_eq!(APPLY_TO_ALL, "Apply to all conflicts");
        assert_eq!(CANCEL_TRANSFER, "Cancel transfer");
        assert_eq!(CONTINUE_ANYWAY, "Continue anyway");
        assert_eq!(INCOMING, "New");
        assert_eq!(EXISTING, "Existing");
        assert_eq!(
            CONFLICT_ALL_REQUIRED,
            "This job is blocked as a whole, so one answer has to cover every conflict."
        );
        assert_eq!(conflict_remaining(1), "1 still needs an answer");
        assert_eq!(conflict_remaining(4), "4 still need an answer");

        // The two surfaces' shared controls and headings.
        assert_eq!(PAUSE_ALL, "Pause All");
        assert_eq!(RESUME_ALL, "Resume All");
        assert_eq!(DETAILS, "Details");
        assert_eq!(RESOLVE, "Resolve");
        assert_eq!(CLEAR_FINISHED, "Clear finished");
        assert_eq!(GROUP_ACTIVE, "Active");
        assert_eq!(GROUP_QUEUED, "Queued");
        assert_eq!(GROUP_FINISHED, "Finished");
        // Reads the same as the state it groups, and is checked against it here
        // so the two cannot drift apart unnoticed.
        assert_eq!(GROUP_WAITING, "Waiting for input");
        assert_eq!(GROUP_WAITING, status_label(&JobStatus::WaitingForInput));
        assert_eq!(GROUP_QUEUED, status_label(&JobStatus::Queued));
        assert_eq!(CENTER_EMPTY_TITLE, "No transfers");
        // The same words `activity_summary` uses for the same condition.
        assert_eq!(CENTER_EMPTY_TITLE, activity_summary(&[]));
        assert_eq!(
            CENTER_EMPTY_HINT,
            "Copies, moves, and deletes appear here while they run."
        );
        assert_eq!(
            NOT_WAITING,
            "This transfer is no longer waiting for an answer."
        );

        // The retry confirmation. Its OK button reads exactly like the dialog the
        // first run went through, because it is the same act.
        assert_eq!(RETRY_PERMANENT_TITLE, "Run this permanent delete again?");
        assert_eq!(
            RETRY_PERMANENT_BODY,
            "Anything it removes is deleted permanently. This cannot be undone."
        );
        assert_eq!(RETRY_PERMANENT_OK, "Delete permanently");

        // `modified: None` on purpose: `format_time` renders in the local zone,
        // and a literal expectation here would pass in one timezone only.
        let stamp = std::time::SystemTime::UNIX_EPOCH;
        assert_eq!(
            side_label(&Side {
                size: 24 * 1024 * 1024,
                modified: None,
                is_dir: false,
            }),
            "24.0 MB · —"
        );
        // A folder's size was never measured, and `0 B` would claim it was.
        assert_eq!(
            side_label(&Side {
                size: 0,
                modified: None,
                is_dir: true,
            }),
            "Folder · —"
        );
        assert_eq!(
            side_label(&Side {
                size: 1024,
                modified: Some(stamp),
                is_dir: false,
            }),
            format!("1.0 KB · {}", format_time(Some(stamp)))
        );
    }

    /// The failure sentences, including the ones that have to match the
    /// pre-engine wording exactly.
    #[test]
    fn golden_transfer_errors() {
        use crate::backend::error::TransferVerb;

        // The same two conditions the old path reported, arriving as typed
        // errors from the engine. Identical strings, or a user who upgraded
        // sees the wording change for no reason.
        assert_eq!(
            transfer_error(&TransferError::IntoItself {
                verb: TransferVerb::Copy,
                path: "/home/u/photos".into(),
            }),
            into_itself(false, Path::new("/home/u/photos"))
        );
        assert_eq!(
            transfer_error(&TransferError::IntoItself {
                verb: TransferVerb::Move,
                path: "/home/u/photos".into(),
            }),
            "Cannot move “photos” into itself"
        );
        assert_eq!(
            transfer_error(&TransferError::SourceCleanup {
                path: "/home/u/photos".into(),
                detail: "permission denied".into(),
            }),
            source_cleanup_failed(Path::new("/home/u/photos"), &"permission denied")
        );

        assert_eq!(
            transfer_error(&TransferError::NoOpMove("/home/u/photos".into())),
            "“photos” is already in that folder"
        );
        assert_eq!(
            transfer_error(&TransferError::NotWritable("/mnt/backup".into())),
            "“backup” cannot be written to"
        );
        assert_eq!(
            transfer_error(&TransferError::InsufficientSpace {
                needed: 2 * 1024 * 1024 * 1024,
                available: 512 * 1024 * 1024,
            }),
            "Not enough space — 2.0 GB more is needed, and 512 MB is free"
        );
        assert_eq!(
            transfer_error(&TransferError::Abandoned),
            "Stopped without an answer"
        );
        // One string for cancellation, wherever it is reported from.
        assert_eq!(transfer_error(&TransferError::Cancelled), CANCELLED_TOAST);

        // The path and file layers keep their own wording, verbatim.
        let denied = crate::backend::error::PathError::Denied;
        let expected = denied.to_string();
        assert_eq!(transfer_error(&TransferError::Path(denied)), expected);
        let missing = crate::backend::error::FileError::NotRegular("/dev/null".into());
        let expected = missing.to_string();
        assert_eq!(transfer_error(&TransferError::File(missing)), expected);
    }

    /// The engine hands over a display path, and the sentence wants a name. A
    /// root has no name but still has to render as something.
    #[test]
    fn leaf_survives_both_separators_and_a_bare_root() {
        assert_eq!(leaf("/home/u/report.pdf"), "report.pdf");
        assert_eq!(leaf("C:\\Users\\u\\report.pdf"), "report.pdf");
        // A trailing separator is not a name.
        assert_eq!(leaf("/home/u/photos/"), "photos");
        assert_eq!(leaf("report.pdf"), "report.pdf");
        // Roots: nothing to take, so the whole thing stands.
        assert_eq!(leaf("/"), "/");
        assert_eq!(leaf("C:\\"), "C:\\");
        assert_eq!(leaf(""), "");
    }

    #[test]
    fn the_numeric_row_drops_a_segment_rather_than_claiming_a_zero() {
        let mut job = Job::queued(
            1,
            JobKind::Copy,
            "Copying “photos”".into(),
            vec![PathBuf::from("/home/u/photos")],
            Some(PathBuf::from("/backup")),
            ConflictPolicy::Ask,
        );
        // Queued: nothing scanned, nothing moving, nothing to say.
        assert_eq!(progress_line(&job), "");

        // Scanned, and running.
        job.status = JobStatus::Running;
        job.total_bytes = 8_804_682_957;
        job.done_bytes = 6_871_947_674;
        job.total_items = 1_032;
        job.done_items = 812;
        job.throughput = 118 * 1024 * 1024;
        job.eta = Some(Duration::from_secs(14));
        assert_eq!(
            progress_line(&job),
            "6.4 GB / 8.2 GB · 812 / 1,032 items · 118 MB/s · 00:14 remaining"
        );

        // Paused: the last reading is not the current one, and the status word
        // is what says so.
        job.status = JobStatus::Paused;
        assert_eq!(progress_line(&job), "6.4 GB / 8.2 GB · 812 / 1,032 items");

        // A single-item job says "item", not "items".
        job.status = JobStatus::Running;
        job.total_items = 1;
        job.done_items = 0;
        job.throughput = 0;
        job.eta = None;
        assert_eq!(progress_line(&job), "6.4 GB / 8.2 GB · 0 / 1 item");
    }

    #[test]
    fn the_popover_headline_counts_only_live_jobs() {
        let copying = |id: u64, status: JobStatus| {
            let mut job = Job::new(id, JobKind::Copy, "t".into());
            job.status = status;
            job
        };

        assert_eq!(activity_summary(&[]), "No transfers");
        assert_eq!(
            activity_summary(&[
                copying(1, JobStatus::Done),
                copying(2, JobStatus::Cancelled)
            ]),
            "No transfers",
            "history was counted as activity"
        );

        // The plan's own example.
        assert_eq!(
            activity_summary(&[
                copying(1, JobStatus::Running),
                copying(2, JobStatus::Running),
                copying(3, JobStatus::WaitingForInput),
            ]),
            "2 copying · 1 waiting"
        );

        // Order is fixed by kind, then by the stopped states, so the line does
        // not reshuffle between 200 ms ticks.
        let mut moving = Job::new(4, JobKind::Move, "t".into());
        moving.status = JobStatus::Running;
        let mut fetching = Job::new(5, JobKind::GitFetch, "t".into());
        fetching.status = JobStatus::Running;
        assert_eq!(
            activity_summary(&[
                copying(6, JobStatus::Queued),
                fetching,
                copying(7, JobStatus::Paused),
                moving,
                copying(8, JobStatus::Running),
            ]),
            "1 copying · 1 moving · 1 fetching · 1 paused · 1 queued"
        );

        // Pausing is still moving bytes, so it counts as the work it is doing.
        assert_eq!(
            activity_summary(&[copying(9, JobStatus::Pausing)]),
            "1 copying"
        );
    }

    #[test]
    fn golden_conflict_strings() {
        let clash = NameClash {
            index: 3,
            relative: "raw/DSC_0041.NEF".into(),
            existing_path: "/backup/photos/raw/DSC_0041.NEF".into(),
            source: Side::default(),
            existing: Side::default(),
            keep_both: "DSC_0041 (2).NEF".into(),
        };
        assert_eq!(
            conflict_summary(&Conflict::NameTaken(clash.clone())),
            "“raw/DSC_0041.NEF” already exists"
        );
        assert_eq!(keep_both_hint(&clash), "Saves it as “DSC_0041 (2).NEF”");

        // Same shortfall, same sentence, whether it arrives as a conflict the
        // user can override or as the failure of a job that could not.
        let space = Conflict::InsufficientSpace {
            needed: 2 * 1024 * 1024 * 1024,
            available: 512 * 1024 * 1024,
        };
        assert_eq!(
            conflict_summary(&space),
            transfer_error(&TransferError::InsufficientSpace {
                needed: 2 * 1024 * 1024 * 1024,
                available: 512 * 1024 * 1024,
            })
        );
        assert_eq!(
            conflict_summary(&space),
            "Not enough space — 2.0 GB more is needed, and 512 MB is free"
        );

        assert_eq!(
            conflict_summary(&Conflict::Denied {
                path: "/backup/photos".into(),
                detail: "permission denied".into(),
            }),
            "“/backup/photos” was refused: permission denied"
        );
    }

    #[test]
    fn file_label_falls_back_to_the_whole_path_for_a_root() {
        assert_eq!(file_label(Path::new("/home/u/a.txt")), "a.txt");
        // A bare root has no final component.
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        assert_eq!(file_label(Path::new(root)), root);
    }

    #[test]
    fn plural_switches_only_at_one() {
        assert_eq!(plural(0), "s");
        assert_eq!(plural(1), "");
        assert_eq!(plural(2), "s");
    }
}
