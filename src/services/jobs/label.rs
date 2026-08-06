//! Every user-visible string the transfer engine produces, in one place.
//!
//! These are pure functions over plain data — no `Window`, no `Context`, no
//! filesystem. That is deliberate: the backend re-architecture replaces the
//! engine underneath, and the golden table in this module's tests is the proof
//! that the toasts a user sees did not drift while it happened. Change a
//! string here only when you mean to change what the user reads.

use std::path::Path;

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
pub fn into_itself(is_move: bool, source: &Path) -> String {
    format!(
        "Cannot {} “{}” into itself",
        if is_move { "move" } else { "copy" },
        file_label(source)
    )
}

/// A move whose copy half succeeded but whose source removal did not — the
/// data is safe, but the user has two copies.
pub fn source_cleanup_failed(source: &Path, error: &dyn std::fmt::Display) -> String {
    format!(
        "Copied, but could not remove source “{}”: {error}",
        file_label(source)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
