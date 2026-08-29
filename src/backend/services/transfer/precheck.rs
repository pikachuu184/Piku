//! Everything that has to be true *before* the destructive half of a transfer
//! begins.
//!
//! # Two kinds of "no"
//!
//! A precheck failure is either something the user can reasonably override or
//! something they cannot, and conflating the two is how a file manager either
//! nags about nothing or destroys data.
//!
//! * **Hard** — returned as [`TransferError`]. Copying a directory into its own
//!   subtree, moving something to where it already is, a destination that is not
//!   a writable directory. There is no answer that makes these work.
//! * **Overridable** — returned as a [`Conflict`] that puts the job in
//!   `WaitingForInput`. Only free space qualifies today: "available bytes" is a
//!   guess (compression, sparse files, another process writing at the same time),
//!   so the honest move is to stop and say so rather than to refuse.
//!
//! # Why the destination gets resolved
//!
//! Today's engine checks `dest_dir.starts_with(source)` on normalized paths.
//! That catches `a/` → `a/b/` and it catches `dest/../src` aliases. It does not
//! catch `a/link` → `a/`, because lexically `a/link/x` does not start with `a/`
//! until the link is followed. [`ScopePolicy`] resolves both sides and compares
//! real paths, so a symlinked destination inside the source tree is refused.
//!
//! The *sources* are deliberately left unresolved for the copy itself. Resolving
//! a symlinked source would turn "copy this link" into "copy whatever it points
//! at, from wherever that is" — the exact substitution `copy_file` refuses. The
//! resolved spelling is used for comparisons and then dropped.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::error::{FileError, PathError, TransferError, TransferVerb};
use crate::backend::path::{PathPolicy, ScopePolicy};
use crate::backend::services::transfer::conflict::Conflict;
use crate::backend::services::transfer::job::TransferKind;
use crate::security::text::sanitize_path;

/// Headroom demanded on top of the planned bytes.
///
/// A destination filled to the last byte is a broken system, not a successful
/// copy: directory entries, journal writes, and the temp files of whatever else
/// is running all need somewhere to go. 64 MB is small enough to be invisible on
/// any modern volume and large enough to matter on the ones where it doesn't.
const SPACE_MARGIN: u64 = 64 * 1024 * 1024;

/// Distinguishes concurrent write probes within one process.
static PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Names tried before giving up on finding a free probe filename.
const PROBE_ATTEMPTS: u32 = 8;

/// What the precheck settled.
#[derive(Clone, Debug)]
pub struct Approved {
    /// Normalized and authorized, in the spelling the copy will walk — links
    /// intact.
    pub sources: Vec<PathBuf>,
    /// Resolved and re-authorized. `None` for a permanent delete.
    pub destination: Option<PathBuf>,
}

/// Authorize the operation's shape. Blocking: resolves paths and probes the
/// destination.
pub fn approve(
    policy: &PathPolicy,
    kind: TransferKind,
    sources: &[PathBuf],
    destination: Option<&Path>,
) -> Result<Approved, TransferError> {
    if sources.is_empty() {
        return Err(TransferError::Path(PathError::Empty));
    }

    let mut validated = Vec::with_capacity(sources.len());
    for source in sources {
        validated.push(policy.validate(source)?.into_path_buf());
    }

    let Some(destination) = destination else {
        // A permanent delete has no destination, so none of what follows
        // applies. The sources are authorized and that is the whole check.
        return Ok(Approved {
            sources: validated,
            destination: None,
        });
    };
    // Past this point there is a destination, so there is a verb. Taking it
    // here rather than defaulting inside the error constructor means a delete
    // can never produce a message about copying.
    let verb = kind
        .verb()
        .ok_or(TransferError::NoOpMove(sanitize_path(destination).into()))?;

    // Resolved, so a symlinked destination is authorized as its real target
    // rather than as its spelling.
    let resolved_dest = policy.resolve(destination)?;
    if !resolved_dest.as_path().is_dir() {
        return Err(TransferError::NotWritable(
            sanitize_path(resolved_dest.as_path()).into(),
        ));
    }

    for source in &validated {
        // Lexical first: cheap, and it catches the `dest/../src` aliases that a
        // resolve would silently normalize away.
        if resolved_dest.as_path().starts_with(source) {
            return Err(into_itself(verb, source));
        }
        // Then the real comparison, which is what catches a *source* reached
        // through a link: `link → real/src` shares no prefix with
        // `real/src/inner`, so only resolving both sides sees the containment.
        // A source that will not resolve — a broken link, a path that vanished
        // between the UI click and here — leaves the lexical check as the only
        // one, which is the same protection today's engine has.
        if let Ok(scope) = ScopePolicy::new(policy, source)
            && scope.contains(&resolved_dest)
        {
            return Err(into_itself(verb, source));
        }

        if matches!(kind, TransferKind::Move) {
            let already_there = policy
                .resolve(source)
                .ok()
                .and_then(|resolved| resolved.parent().map(Path::to_path_buf))
                .is_some_and(|parent| parent == resolved_dest.as_path());
            if already_there {
                return Err(TransferError::NoOpMove(sanitize_path(source).into()));
            }
        }
    }

    probe_writable(policy, resolved_dest.as_path())?;

    Ok(Approved {
        sources: validated,
        destination: Some(resolved_dest.into_path_buf()),
    })
}

/// Whether the destination volume can hold `planned_bytes`.
///
/// Runs after the scan, because that is when the number exists. Returns the
/// overridable block, or `None` when there is room — or when the volume cannot
/// be identified at all, which is not the same as "no room" and must not be
/// reported as it.
pub fn check_space(destination: &Path, planned_bytes: u64) -> Option<Conflict> {
    let available = available_bytes(destination)?;
    let needed = planned_bytes.saturating_add(SPACE_MARGIN);
    if needed <= available {
        return None;
    }
    Some(Conflict::InsufficientSpace {
        needed: needed.saturating_sub(available),
        available,
    })
}

fn into_itself(verb: TransferVerb, source: &Path) -> TransferError {
    TransferError::IntoItself {
        verb,
        path: sanitize_path(source).into(),
    }
}

/// Available bytes on the volume holding `path`.
///
/// The mount table comes from the same enumeration the sidebar uses
/// (`fs_service::list_drives`), so free space has one source of truth and this
/// stage adds no crate. It is called directly rather than through
/// [`DriveService`](crate::backend::services::drive::DriveService) because that
/// handle is async and cached for 30 seconds: a space check wants the number
/// now, and wants it fresh, since the previous job in the queue may have just
/// consumed the headroom.
///
/// The longest matching mount point wins, so a volume mounted beneath another
/// (`/` and `/home` on separate partitions) is attributed correctly.
fn available_bytes(path: &Path) -> Option<u64> {
    crate::services::fs_service::list_drives()
        .into_iter()
        .filter(|drive| path.starts_with(&drive.mount))
        .max_by_key(|drive| drive.mount.as_os_str().len())
        .map(|drive| drive.available)
}

/// Confirm the destination accepts writes, by making one.
///
/// `metadata().permissions().readonly()` is not this check: it misses read-only
/// mounts, POSIX group and other bits, ACLs, and immutable flags. Creating a file
/// is the only answer the kernel actually stands behind, and finding out here
/// beats finding out 40 GB in.
///
/// It writes with `fs` directly rather than through `StorageProvider`, which
/// would emit a `create_file` and a `remove_after_move` audit record per job for
/// something that leaves no trace. The path is still authorized through the
/// policy, and every component of it is authored here — nothing untrusted goes
/// into the name.
fn probe_writable(policy: &PathPolicy, destination: &Path) -> Result<(), TransferError> {
    let pid = std::process::id();
    let mut last: Option<std::io::Error> = None;

    for _ in 0..PROBE_ATTEMPTS {
        let n = PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = destination.join(format!(".piku-write-probe-{pid}-{n}"));
        let candidate = policy.validate(&candidate)?.into_path_buf();

        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                drop(file);
                let _ = std::fs::remove_file(&candidate);
                return Ok(());
            }
            // Someone else holds this name. Not a permission problem — try the
            // next one rather than reporting the destination unwritable.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last = Some(error);
            }
            Err(error) => {
                last = Some(error);
                break;
            }
        }
    }

    let path: std::sync::Arc<str> = sanitize_path(destination).into();
    match last {
        Some(error) => Err(TransferError::File(FileError::Io {
            op: "writing to",
            path,
            source: error,
        })),
        None => Err(TransferError::NotWritable(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("piku-precheck-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("fixture root");
            Self { root }
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.root.join(rel);
            std::fs::create_dir_all(&path).expect("dir");
            path
        }

        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent");
            }
            std::fs::write(&path, vec![b'x'; bytes]).expect("file");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn policy() -> PathPolicy {
        PathPolicy::with_system_roots()
    }

    #[test]
    fn a_plain_copy_into_a_sibling_directory_is_approved() {
        let fx = Fixture::new("plain");
        let src = fx.dir("src");
        fx.file("src/a.txt", 10);
        let dest = fx.dir("dest");

        let approved = approve(
            &policy(),
            TransferKind::Copy,
            std::slice::from_ref(&src),
            Some(&dest),
        )
        .expect("a sibling copy must be approved");
        assert_eq!(approved.sources, vec![src]);
        assert!(approved.destination.is_some());
    }

    #[test]
    fn copying_a_directory_into_its_own_subtree_is_refused() {
        let fx = Fixture::new("descendant");
        let src = fx.dir("src");
        let inner = fx.dir("src/inner");

        let error = approve(&policy(), TransferKind::Copy, &[src], Some(&inner))
            .expect_err("a self-descendant copy must be refused");
        assert!(matches!(error, TransferError::IntoItself { .. }));
    }

    #[test]
    fn copying_a_directory_into_itself_is_refused() {
        let fx = Fixture::new("itself");
        let src = fx.dir("src");

        let error = approve(
            &policy(),
            TransferKind::Copy,
            std::slice::from_ref(&src),
            Some(&src),
        )
        .expect_err("copying into itself must be refused");
        assert!(matches!(error, TransferError::IntoItself { .. }));
    }

    /// The case the lexical check cannot see. `gateway` is a link that lands
    /// inside `src/`, so `src → gateway` would recurse into itself, one level
    /// deeper each pass, until the disk fills. It is *resolving the destination*
    /// that catches this — the link's own spelling shares no prefix with `src`.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_destination_inside_the_source_is_refused() {
        let fx = Fixture::new("symlink-descendant");
        let src = fx.dir("src");
        let inner = fx.dir("src/inner");
        let link = fx.root.join("gateway");
        std::os::unix::fs::symlink(&inner, &link).expect("symlink");

        // Lexically, `gateway` is a sibling of `src` and shares no prefix.
        assert!(!link.starts_with(&src));

        let error = approve(&policy(), TransferKind::Copy, &[src], Some(&link))
            .expect_err("a symlinked self-descendant must be refused");
        assert!(matches!(error, TransferError::IntoItself { .. }));
    }

    /// The case that needs [`ScopePolicy`] specifically: the *source* is the
    /// link. Both sides are already resolved-or-normalized and still share no
    /// prefix, so only resolving the source as a scope root sees the
    /// containment.
    #[cfg(unix)]
    #[test]
    fn a_source_reached_through_a_link_still_contains_its_own_subtree() {
        let fx = Fixture::new("symlink-source");
        let real = fx.dir("real/src");
        let inner = fx.dir("real/src/inner");
        let link = fx.root.join("shortcut");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        // Neither the lexical check nor resolving the destination alone helps:
        // `real/src/inner` does not start with `shortcut`.
        assert!(!inner.starts_with(&link));

        let error = approve(&policy(), TransferKind::Copy, &[link], Some(&inner))
            .expect_err("a linked source's own subtree must be refused");
        assert!(matches!(error, TransferError::IntoItself { .. }));
    }

    /// `dest/../src` normalizes to `src`, which is why the lexical check runs on
    /// normalized paths.
    #[test]
    fn an_alias_spelling_of_the_source_is_refused() {
        let fx = Fixture::new("alias");
        let src = fx.dir("src");
        let dest = fx.dir("dest");
        let alias = dest.join("..").join("src");

        let error = approve(&policy(), TransferKind::Copy, &[src], Some(&alias))
            .expect_err("an alias of the source must be refused");
        assert!(matches!(error, TransferError::IntoItself { .. }));
    }

    #[test]
    fn moving_something_to_where_it_already_is_is_refused() {
        let fx = Fixture::new("noop-move");
        let file = fx.file("a.txt", 10);
        let home = fx.root.clone();

        let error = approve(&policy(), TransferKind::Move, &[file], Some(&home))
            .expect_err("a no-op move must be refused");
        assert!(matches!(error, TransferError::NoOpMove(_)));
    }

    /// The same shape is a *duplicate*, which is legitimate — the conflict
    /// machinery gives it a `(2)` name. Refusing it would break Ctrl+C Ctrl+V.
    #[test]
    fn copying_something_to_where_it_already_is_is_allowed() {
        let fx = Fixture::new("duplicate");
        let file = fx.file("a.txt", 10);
        let home = fx.root.clone();

        approve(&policy(), TransferKind::Copy, &[file], Some(&home))
            .expect("a duplicate in place must be allowed");
    }

    #[test]
    fn a_destination_that_is_a_file_is_refused() {
        let fx = Fixture::new("dest-is-file");
        let src = fx.dir("src");
        let dest = fx.file("dest.txt", 1);

        let error = approve(&policy(), TransferKind::Copy, &[src], Some(&dest))
            .expect_err("a file destination must be refused");
        assert!(matches!(error, TransferError::NotWritable(_)));
    }

    #[test]
    fn a_destination_that_does_not_exist_is_refused() {
        let fx = Fixture::new("dest-missing");
        let src = fx.dir("src");
        let dest = fx.root.join("nowhere");

        approve(&policy(), TransferKind::Copy, &[src], Some(&dest))
            .expect_err("a missing destination must be refused");
    }

    #[test]
    fn a_delete_needs_no_destination() {
        let fx = Fixture::new("delete");
        let doomed = fx.file("doomed.txt", 5);

        let approved = approve(
            &policy(),
            TransferKind::DeletePermanent,
            std::slice::from_ref(&doomed),
            None,
        )
        .expect("a delete must be approved without a destination");
        assert_eq!(approved.sources, vec![doomed]);
        assert!(approved.destination.is_none());
    }

    #[test]
    fn an_empty_source_set_is_refused() {
        let fx = Fixture::new("empty");
        let dest = fx.dir("dest");
        approve(&policy(), TransferKind::Copy, &[], Some(&dest))
            .expect_err("an empty transfer must be refused");
    }

    /// The probe must leave nothing behind. A stray dotfile per paste would be a
    /// visible bug within a day of use.
    #[test]
    fn the_write_probe_leaves_no_trace() {
        let fx = Fixture::new("probe");
        let src = fx.dir("src");
        let dest = fx.dir("dest");

        let before: Vec<PathBuf> = std::fs::read_dir(&dest)
            .expect("read dest")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(before.is_empty());

        approve(&policy(), TransferKind::Copy, &[src], Some(&dest)).expect("approved");

        let after: Vec<PathBuf> = std::fs::read_dir(&dest)
            .expect("read dest")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(after.is_empty(), "the probe left {after:?} behind");
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_destination_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let fx = Fixture::new("readonly");
        let src = fx.dir("src");
        let dest = fx.dir("dest");
        let mut perms = std::fs::metadata(&dest).expect("metadata").permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&dest, perms).expect("chmod");

        let result = approve(&policy(), TransferKind::Copy, &[src], Some(&dest));

        // Restore before asserting, so a failure does not leave an undeletable
        // fixture behind.
        let mut perms = std::fs::metadata(&dest).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).expect("chmod back");

        // Running as root, or on a filesystem that ignores mode bits, defeats
        // the setup entirely. Skip rather than assert something the environment
        // cannot demonstrate — a test that passes for the wrong reason is worse
        // than one that does not run.
        let Err(error) = result else {
            return;
        };
        assert!(
            matches!(
                error,
                TransferError::NotWritable(_) | TransferError::File(_)
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_unidentifiable_volume_is_not_reported_as_full() {
        // A path under no known mount point yields no answer, and "no answer"
        // must never render as "insufficient space".
        let nowhere = Path::new("relative/not/a/mount");
        assert!(check_space(nowhere, u64::MAX / 2).is_none());
    }

    #[test]
    fn a_request_for_more_than_the_volume_holds_is_blocked() {
        let fx = Fixture::new("space");
        let dest = fx.dir("dest");

        let block = check_space(&dest, u64::MAX - SPACE_MARGIN)
            .expect("asking for the whole address space must block");
        let Conflict::InsufficientSpace { needed, available } = block else {
            panic!("expected an insufficient-space block, got {block:?}");
        };
        assert!(needed > 0);
        assert!(available < u64::MAX - SPACE_MARGIN);
    }

    #[test]
    fn a_small_transfer_onto_a_real_volume_is_not_blocked() {
        let fx = Fixture::new("room");
        let dest = fx.dir("dest");
        assert!(
            check_space(&dest, 1_024).is_none(),
            "a 1 KB copy was reported as too large"
        );
    }

    /// The margin is part of the contract: a transfer that exactly fills the
    /// volume still blocks.
    ///
    /// Both legs re-query free space, which drifts on a live system, so the
    /// "allowed" leg asks for a margin less than it could — the assertion is
    /// about the margin existing, not about the byte count holding still.
    #[test]
    fn the_margin_is_demanded_on_top_of_the_planned_bytes() {
        let fx = Fixture::new("margin");
        let dest = fx.dir("dest");
        let Some(available) = available_bytes(&dest) else {
            return; // No identifiable mount in this environment.
        };
        assert!(
            check_space(&dest, available).is_some(),
            "a copy that exactly fills the volume was allowed"
        );
        assert!(
            check_space(&dest, available.saturating_sub(SPACE_MARGIN * 2)).is_none(),
            "a copy leaving twice the margin was blocked"
        );
    }
}
