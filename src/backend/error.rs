//! Typed errors, one enum per service, composing into one [`BackendError`].
//!
//! Two rules make this safe to swap in under a live UI:
//!
//! 1. **Message strings are load-bearing.** [`PathError`]'s first seven
//!    variants reproduce `security::path_guard::PathGuardError` *verbatim*,
//!    including punctuation, because those strings reach the user in toasts.
//!    The golden test at the bottom pins them.
//! 2. **Cancellation is a variant, not a string.** The current engine returns
//!    `Err("cancelled")` and compares that literal at the UI boundary;
//!    [`BackendError::is_cancelled`] replaces that with something the compiler
//!    checks.

// PreviewError and the variants it composes are raised as of Stage 7, and
// TransferError as of the transfer engine. The rest — DirectoryError,
// MetadataError, and the mutating arms of FileError and PathError — belong to
// services that have not landed.
//
// `expect` rather than `allow`: once every variant is raised, this attribute
// itself starts erroring, which is the reminder to delete it.
#![expect(dead_code)]

use std::io;
use std::sync::Arc;

/// Path validation and authorization failures.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    // --- verbatim from PathGuardError; do not reword ---------------------
    #[error("path is empty")]
    Empty,
    #[error("path contains a null byte")]
    NullByte,
    #[error("path uses the reserved device name `{0}`")]
    ReservedName(String),
    #[error("path escapes its root through `..`")]
    Traversal,
    #[error("path must be absolute")]
    NotAbsolute,
    #[error("network (UNC) and device paths are not authorized")]
    UnauthorizedPrefix,
    #[error("path is outside the authorized storage roots")]
    OutsideRoots,

    // --- new -------------------------------------------------------------
    #[error("path is on the system deny list")]
    Denied,
    #[error("`{0}` escapes the operation's root")]
    OutsideScope(Arc<str>),
    #[error("{0}")]
    InvalidName(String),
    #[error("resolving {path}: {source}")]
    Resolve {
        path: Arc<str>,
        #[source]
        source: io::Error,
    },
}

/// Directory listing failures.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("reading {path}: {source}")]
    Io {
        path: Arc<str>,
        #[source]
        source: io::Error,
    },
    #[error("`{0}` is not a directory")]
    NotADirectory(Arc<str>),
    #[error("cancelled")]
    Cancelled,
}

/// Single-file operation failures.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("`{0}` already exists")]
    AlreadyExists(Arc<str>),
    #[error("refusing to read through a link: {0}")]
    IsSymlink(Arc<str>),
    #[error("`{0}` is not a regular file")]
    NotRegular(Arc<str>),
    #[error("{op} {path}: {source}")]
    Io {
        op: &'static str,
        path: Arc<str>,
        #[source]
        source: io::Error,
    },
    #[error("cancelled")]
    Cancelled,
}

/// Metadata and folder-size failures.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("reading metadata of {path}: {source}")]
    Io {
        path: Arc<str>,
        #[source]
        source: io::Error,
    },
    #[error("cancelled")]
    Cancelled,
}

/// Preview generation failures.
///
/// Read failures reuse [`FileError`] rather than restating them. The preview
/// engine opens files under the same symlink / regular-file policy as every
/// other reader, and a second copy of those strings is how they drift apart —
/// `preview_read_failures_match_the_storage_provider` pins that they do not.
///
/// Note what is *not* here: "too large" is not a failure. A file past the
/// decode caps is a successful classification with its own rendering, so it
/// stays a payload variant.
#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    File(#[from] FileError),
    /// The parser refused the input. Carries the provider's own wording, which
    /// is what already reaches the user today through `PreviewContent::Error`.
    #[error("{0}")]
    Undecodable(Arc<str>),
    #[error("cancelled")]
    Cancelled,
}

impl From<crate::backend::protocol::Cancelled> for PreviewError {
    fn from(_: crate::backend::protocol::Cancelled) -> Self {
        Self::Cancelled
    }
}

impl PreviewError {
    /// Whether this means "superseded" rather than "went wrong". A cancelled
    /// preview must leave the panel's loading state to the newer request that
    /// replaced it, and must never raise a toast.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled | Self::File(FileError::Cancelled))
    }
}

/// Which way a transfer moves data.
///
/// Named rather than a bare `bool` so a call site reads `TransferVerb::Move`
/// instead of `true`, and so the two words that reach the user ("copied",
/// "moved") come from one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferVerb {
    Copy,
    Move,
}

impl TransferVerb {
    pub fn is_move(self) -> bool {
        matches!(self, Self::Move)
    }

    /// Past participle, for "cannot be *copied* into itself".
    pub fn participle(self) -> &'static str {
        match self {
            Self::Copy => "copied",
            Self::Move => "moved",
        }
    }
}

impl std::fmt::Display for TransferVerb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.participle())
    }
}

/// Copy, move, and permanent-delete failures.
///
/// Composes [`PathError`] and [`FileError`] rather than restating them: every
/// node a transfer touches goes through the same authorization and the same
/// per-file reader as any other operation, so a second copy of those strings
/// is how they drift apart.
///
/// These messages are the *technical* form — they go to logs and to
/// [`BackendError::user_message`] as a last resort. The polished sentence the
/// user reads comes from `services::jobs::label::transfer_error`, which is
/// where every transfer string lives and is golden-tested.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    File(#[from] FileError),
    /// Copy or move of a directory into its own subtree.
    #[error("`{path}` cannot be {verb} into itself")]
    IntoItself { verb: TransferVerb, path: Arc<str> },
    /// A move whose destination already holds the source.
    #[error("`{0}` is already in the destination folder")]
    NoOpMove(Arc<str>),
    /// The destination volume cannot hold the planned bytes.
    #[error("the destination needs {needed} more bytes than it has")]
    InsufficientSpace { needed: u64, available: u64 },
    #[error("`{0}` cannot be written to")]
    NotWritable(Arc<str>),
    /// A move whose copy half succeeded but whose source removal did not. The
    /// data is safe; the user has two copies.
    #[error("copied `{path}`, but could not remove the source: {detail}")]
    SourceCleanup { path: Arc<str>, detail: Arc<str> },
    /// The transfer stopped for a decision and the channel closed without one —
    /// the window went away, or the app is quitting.
    #[error("no decision was made")]
    Abandoned,
    #[error("cancelled")]
    Cancelled,
}

impl From<crate::backend::protocol::Cancelled> for TransferError {
    fn from(_: crate::backend::protocol::Cancelled) -> Self {
        Self::Cancelled
    }
}

impl TransferError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled | Self::File(FileError::Cancelled))
    }
}

/// Drive and mount discovery failures.
#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error("enumerating drives: {0}")]
    Io(#[from] io::Error),
    /// A refresh was already running and there was no previous result to
    /// serve. Distinct from "no drives" — the caller may simply retry.
    #[error("drive enumeration is already in progress")]
    Busy,
    #[error("cancelled")]
    Cancelled,
}

/// Everything a backend operation can fail with.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Directory(#[from] DirectoryError),
    #[error(transparent)]
    File(#[from] FileError),
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(transparent)]
    Drive(#[from] DriveError),
    #[error(transparent)]
    Preview(#[from] PreviewError),
    #[error(transparent)]
    Transfer(#[from] TransferError),
    #[error("the backend is shutting down")]
    ShuttingDown,
}

impl BackendError {
    /// Whether this failure means "the user cancelled" rather than "something
    /// went wrong". Replaces comparing against the `"cancelled"` literal.
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Directory(DirectoryError::Cancelled)
            | Self::File(FileError::Cancelled)
            | Self::Metadata(MetadataError::Cancelled)
            | Self::Drive(DriveError::Cancelled)
            | Self::Preview(PreviewError::Cancelled)
            // A preview whose *read* was cancelled is still a cancellation;
            // without this arm a superseded selection would raise a toast.
            | Self::Preview(PreviewError::File(FileError::Cancelled))
            | Self::Transfer(TransferError::Cancelled)
            | Self::Transfer(TransferError::File(FileError::Cancelled)) => true,
            // A shutdown drain is not a user-visible failure either; treating
            // it as cancellation keeps quit from raising an error toast.
            Self::ShuttingDown => true,
            _ => false,
        }
    }

    /// The text a toast shows.
    ///
    /// Sanitized because these strings embed filenames, and a filename is
    /// attacker-controlled: without this, a file named with a bidi override
    /// could rewrite the rest of the message as it renders.
    pub fn user_message(&self) -> String {
        crate::security::git_text::sanitize_git_text(&self.to_string(), USER_MESSAGE_CAP, false)
    }
}

/// Upper bound on a user-facing error string, in characters.
const USER_MESSAGE_CAP: usize = 300;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::path_guard::PathGuardError;

    /// The seven inherited variants must render exactly as `PathGuardError`
    /// does today, or path failures change what the user reads.
    #[test]
    fn path_error_messages_match_the_guard_verbatim() {
        let pairs: Vec<(PathError, PathGuardError)> = vec![
            (PathError::Empty, PathGuardError::Empty),
            (PathError::NullByte, PathGuardError::NullByte),
            (
                PathError::ReservedName("CON".into()),
                PathGuardError::ReservedName("CON".into()),
            ),
            (PathError::Traversal, PathGuardError::Traversal),
            (PathError::NotAbsolute, PathGuardError::NotAbsolute),
            (
                PathError::UnauthorizedPrefix,
                PathGuardError::UnauthorizedPrefix,
            ),
            (PathError::OutsideRoots, PathGuardError::OutsideRoots),
        ];
        for (new, old) in pairs {
            assert_eq!(new.to_string(), old.to_string(), "message drifted");
        }
    }

    /// `PreviewError` deliberately reuses `FileError` for read failures instead
    /// of restating them. This runs the *real* `LocalProvider::open_read`
    /// against the two inputs it refuses and asserts the wrapped variants
    /// render identically — so when Stage 7's `preview::read` replaces that
    /// call, the strings the user sees cannot drift.
    #[test]
    fn preview_read_failures_match_the_storage_provider() {
        let dir = std::env::temp_dir().join("piku-preview-error-golden");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        // A directory is not a regular file.
        let got = crate::storage::local()
            .open_read(&dir)
            .expect_err("a directory must be refused");
        let expected = PreviewError::File(FileError::NotRegular(
            dir.display().to_string().as_str().into(),
        ));
        assert_eq!(format!("{got:#}"), expected.to_string());

        // A symlink is refused before it is followed.
        #[cfg(unix)]
        {
            let target = dir.join("real.txt");
            let link = dir.join("link.txt");
            let _ = std::fs::write(&target, b"x");
            std::os::unix::fs::symlink(&target, &link).expect("symlink");
            let got = crate::storage::local()
                .open_read(&link)
                .expect_err("a symlink must be refused");
            let expected = PreviewError::File(FileError::IsSymlink(
                link.display().to_string().as_str().into(),
            ));
            assert_eq!(format!("{got:#}"), expected.to_string());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancellation_is_recognized_through_the_wrapper() {
        assert!(BackendError::from(DirectoryError::Cancelled).is_cancelled());
        assert!(BackendError::from(FileError::Cancelled).is_cancelled());
        assert!(BackendError::from(MetadataError::Cancelled).is_cancelled());
        assert!(BackendError::ShuttingDown.is_cancelled());
        assert!(BackendError::from(PreviewError::Cancelled).is_cancelled());
        // A superseded selection cancels mid-read; that is not a toast.
        assert!(BackendError::from(PreviewError::File(FileError::Cancelled)).is_cancelled());
        assert!(
            !BackendError::from(PreviewError::Undecodable("not a readable zip".into()))
                .is_cancelled(),
            "a parser refusal is a real failure"
        );
        assert!(BackendError::from(TransferError::Cancelled).is_cancelled());
        // A cancelled copy cancels inside `copy_file`, one layer down.
        assert!(BackendError::from(TransferError::File(FileError::Cancelled)).is_cancelled());
        assert!(
            !BackendError::from(TransferError::NoOpMove("a.txt".into())).is_cancelled(),
            "a refused move is a real failure"
        );
        assert!(!BackendError::from(PathError::Traversal).is_cancelled());
        assert!(
            !BackendError::from(FileError::AlreadyExists("a.txt".into())).is_cancelled(),
            "a real failure must not be mistaken for cancellation"
        );
    }

    /// `TransferError`'s own messages are the technical form. They still reach
    /// the user when nothing more specific is available, so they are pinned.
    #[test]
    fn transfer_error_messages_are_pinned() {
        let cases: Vec<(TransferError, &str)> = vec![
            (
                TransferError::IntoItself {
                    verb: TransferVerb::Copy,
                    path: "photos".into(),
                },
                "`photos` cannot be copied into itself",
            ),
            (
                TransferError::IntoItself {
                    verb: TransferVerb::Move,
                    path: "photos".into(),
                },
                "`photos` cannot be moved into itself",
            ),
            (
                TransferError::NoOpMove("report.pdf".into()),
                "`report.pdf` is already in the destination folder",
            ),
            (
                TransferError::InsufficientSpace {
                    needed: 4096,
                    available: 10,
                },
                "the destination needs 4096 more bytes than it has",
            ),
            (
                TransferError::NotWritable("/mnt/ro".into()),
                "`/mnt/ro` cannot be written to",
            ),
            (
                TransferError::SourceCleanup {
                    path: "photos".into(),
                    detail: "permission denied".into(),
                },
                "copied `photos`, but could not remove the source: permission denied",
            ),
            (TransferError::Abandoned, "no decision was made"),
            (TransferError::Cancelled, "cancelled"),
        ];
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected, "message drifted");
            // And the wrapper is transparent, so a toast reads the same either
            // way.
            assert_eq!(BackendError::from(error).to_string(), expected);
        }
    }

    #[test]
    fn transfer_verbs_name_themselves() {
        assert!(TransferVerb::Move.is_move());
        assert!(!TransferVerb::Copy.is_move());
        assert_eq!(TransferVerb::Copy.to_string(), "copied");
        assert_eq!(TransferVerb::Move.to_string(), "moved");
    }

    #[test]
    fn transparent_wrapping_preserves_the_inner_message() {
        let inner = FileError::NotRegular("/dev/null".into());
        let expected = inner.to_string();
        assert_eq!(BackendError::from(inner).to_string(), expected);
    }

    #[test]
    fn path_errors_compose_into_service_errors_transparently() {
        let e = DirectoryError::from(PathError::OutsideRoots);
        assert_eq!(
            e.to_string(),
            "path is outside the authorized storage roots"
        );
        assert_eq!(
            BackendError::from(e).to_string(),
            "path is outside the authorized storage roots"
        );
    }

    #[test]
    fn user_messages_strip_control_and_bidi_characters() {
        // A filename carrying a right-to-left override must not be able to
        // reorder the message it is embedded in.
        let hostile = FileError::AlreadyExists("in\u{202E}gpj.exe".into());
        let msg = BackendError::from(hostile).user_message();
        assert!(!msg.contains('\u{202E}'), "bidi override survived: {msg:?}");
        assert!(msg.contains("already exists"));
    }

    #[test]
    fn user_messages_are_capped() {
        let long = "a".repeat(5_000);
        let msg = BackendError::from(FileError::AlreadyExists(long.into())).user_message();
        assert!(
            msg.chars().count() <= USER_MESSAGE_CAP + 1,
            "message not capped: {} chars",
            msg.chars().count()
        );
    }

    #[test]
    fn io_errors_keep_their_source_and_context() {
        let e = FileError::Io {
            op: "opening",
            path: "/tmp/x".into(),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        };
        assert_eq!(e.to_string(), "opening /tmp/x: denied");
        assert!(std::error::Error::source(&e).is_some());
    }
}
