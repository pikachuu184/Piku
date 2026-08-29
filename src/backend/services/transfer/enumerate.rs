//! The scan pass: byte totals, item counts, and every collision — all before
//! the first byte moves.
//!
//! # Why scan at all
//!
//! Two reasons, and the second is the important one.
//!
//! A progress bar needs a denominator. That is the obvious reason and the cheap
//! one.
//!
//! The real reason is that **a collision discovered mid-transfer cannot be
//! answered once.** Today's engine renames silently precisely because it has
//! nowhere to ask: it is 12 000 files into a copy with no way to show a
//! decision surface that means anything. Knowing the collisions up front is what
//! turns "8 conflicts found · Replace · Apply to all" into a single click
//! instead of 8 000 dialogs.
//!
//! # Walking without recursing
//!
//! An explicit worklist rather than recursion. The tree depth is attacker-
//! controlled — a crafted archive extracted earlier, or just a deeply nested
//! `node_modules` — and `#![deny(clippy::panic)]` does nothing about a stack
//! overflow. Symlinks are counted but never followed, so the walk cannot loop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::error::{PathError, TransferError};
use crate::backend::path::PathPolicy;
use crate::backend::services::transfer::conflict::{Conflict, NameClash, Side, unique_destination};

/// Collisions carried to the UI in one batch.
///
/// The modal is a list a person reads, and nobody reads the 4 000th row. Past
/// this the true count is still reported — "1,204 conflicts found" — and a
/// blanket answer covers all of them; only *per-entry* answers are limited to
/// what is shown. Also bounds the destination→index map to 256 entries instead
/// of one per colliding file.
const MAX_SHOWN_CLASHES: usize = 256;

/// One top-level item the job was asked to transfer.
///
/// Only top-level entries appear here. The recursive descent happens in
/// `copy.rs`, re-authorizing as it goes; materializing every node of a
/// 400 000-file tree into a `Vec` first would cost more memory than the copy.
#[derive(Clone, Debug)]
pub struct PlanItem {
    pub source: PathBuf,
    /// `destination.join(source.file_name())`.
    pub target: PathBuf,
    pub is_dir: bool,
}

/// What the scan concluded.
#[derive(Debug)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    pub total_bytes: u64,
    pub total_items: u64,
    /// The collisions to show, at most [`MAX_SHOWN_CLASHES`] of them.
    pub conflicts: Vec<Conflict>,
    /// Every collision found, including those past the display cap.
    pub clash_count: u64,
    /// Destination path → index into [`Plan::conflicts`], so the copy pass can
    /// find the answer that belongs to the node it is about to write.
    clash_index: HashMap<PathBuf, usize>,
}

impl Plan {
    /// The conflict index for a destination, if it was one of the ones shown.
    ///
    /// `None` means either "no collision here" or "collided but past the display
    /// cap" — both of which the caller handles the same way: fall back to the
    /// standing policy, and ask if there isn't one.
    pub fn clash_at(&self, target: &Path) -> Option<usize> {
        self.clash_index.get(target).copied()
    }

    pub fn has_conflicts(&self) -> bool {
        self.clash_count > 0
    }
}

/// Scan `sources`, planning delivery into `destination`.
///
/// `destination` is `None` for a permanent delete, which collides with nothing
/// and therefore only needs totals.
///
/// A top-level source that fails authorization fails the whole job — the user
/// selected it, so silently dropping it would report success for work that never
/// happened. A *child* that fails authorization is skipped and not counted,
/// which is what today's engine does and what keeps the totals honest against a
/// copy pass that skips it too.
pub fn enumerate(
    policy: &PathPolicy,
    sources: &[PathBuf],
    destination: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<Plan, TransferError> {
    let mut plan = Plan {
        items: Vec::with_capacity(sources.len()),
        total_bytes: 0,
        total_items: 0,
        conflicts: Vec::new(),
        clash_count: 0,
        clash_index: HashMap::new(),
    };

    for source in sources {
        if cancel.load(Ordering::Relaxed) {
            return Err(TransferError::Cancelled);
        }
        // Top-level: authorize or fail.
        policy.validate(source)?;
        let metadata = std::fs::symlink_metadata(source).map_err(|source_error| {
            TransferError::Path(PathError::Resolve {
                path: crate::security::text::sanitize_path(source).into(),
                source: source_error,
            })
        })?;

        // A path with no final component is a filesystem root. It cannot be
        // given a name at a destination, and walking it would count the whole
        // disk, so it is refused rather than silently reinterpreted.
        let Some(name) = source.file_name() else {
            return Err(TransferError::Path(PathError::InvalidName(
                "a filesystem root cannot be transferred".to_owned(),
            )));
        };
        let target = destination.map(|dir| dir.join(name));

        if let Some(target) = &target {
            plan.items.push(PlanItem {
                source: source.clone(),
                target: target.clone(),
                is_dir: metadata.is_dir() && !metadata.is_symlink(),
            });
        }

        walk(
            policy,
            source,
            target.as_deref(),
            destination,
            &mut plan,
            cancel,
        )?;
    }

    Ok(plan)
}

/// Count one subtree and record its collisions.
fn walk(
    policy: &PathPolicy,
    root: &Path,
    root_target: Option<&Path>,
    destination: Option<&Path>,
    plan: &mut Plan,
    cancel: &AtomicBool,
) -> Result<(), TransferError> {
    // (source, its destination) pairs still to visit.
    let mut stack: Vec<(PathBuf, Option<PathBuf>)> =
        vec![(root.to_path_buf(), root_target.map(Path::to_path_buf))];

    while let Some((path, target)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(TransferError::Cancelled);
        }
        // Unauthorized children are skipped without counting, because the copy
        // pass skips them the same way. Counting them would leave a job stuck at
        // 98% forever.
        if policy.validate(&path).is_err() {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };

        plan.total_items += 1;
        let is_dir = metadata.is_dir() && !metadata.is_symlink();
        if metadata.is_file() {
            plan.total_bytes += metadata.len();
        }

        if let (Some(target), Some(destination)) = (&target, destination) {
            note_clash(policy, &metadata, target, destination, plan);
        }

        // Symlinks are neither followed nor copied, so there is nothing beneath
        // them to count.
        if !is_dir {
            continue;
        }
        let Ok(children) = std::fs::read_dir(&path) else {
            continue;
        };
        for child in children.flatten() {
            let child_path = child.path();
            let child_target = match (&target, child_path.file_name()) {
                (Some(dir), Some(name)) => Some(dir.join(name)),
                _ => None,
            };
            stack.push((child_path, child_target));
        }
    }

    Ok(())
}

/// Record a collision at `target`, if there is one.
///
/// A directory landing on a directory is **not** a collision: that is a merge,
/// which is what every file manager does and what the user expects when they
/// drop `assets/` onto a folder that already has an `assets/`. Everything else
/// that already exists is a collision — including a symlink, which
/// `copy_file` refuses to write through and which `Replace` therefore has to
/// remove rather than overwrite.
fn note_clash(
    policy: &PathPolicy,
    source_meta: &std::fs::Metadata,
    target: &Path,
    destination: &Path,
    plan: &mut Plan,
) {
    let Ok(existing_meta) = std::fs::symlink_metadata(target) else {
        return; // Nothing there.
    };

    let source_is_dir = source_meta.is_dir() && !source_meta.is_symlink();
    let existing_is_dir = existing_meta.is_dir() && !existing_meta.is_symlink();
    if source_is_dir && existing_is_dir {
        return; // A merge, not a clash.
    }

    plan.clash_count += 1;
    if plan.conflicts.len() >= MAX_SHOWN_CLASHES {
        return;
    }

    let index = plan.conflicts.len();
    let relative = target.strip_prefix(destination).unwrap_or(target);
    let keep_both = unique_destination(policy, target);

    plan.conflicts.push(Conflict::NameTaken(NameClash {
        index,
        relative: crate::security::text::sanitize_path(relative).into(),
        existing_path: crate::security::text::sanitize_path(target).into(),
        source: Side::of(source_meta),
        existing: Side::of(&existing_meta),
        keep_both: keep_both
            .file_name()
            .map(|name| crate::security::text::sanitize_label(&name.to_string_lossy()))
            .unwrap_or_default()
            .into(),
    }));
    plan.clash_index.insert(target.to_path_buf(), index);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("piku-enumerate-{name}"));
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

    fn run(sources: &[PathBuf], destination: Option<&Path>) -> Plan {
        enumerate(&policy(), sources, destination, &AtomicBool::new(false)).expect("scan")
    }

    #[test]
    fn totals_count_every_node_and_every_byte() {
        let fx = Fixture::new("totals");
        let src = fx.dir("src");
        fx.file("src/a.txt", 100);
        fx.file("src/nested/b.txt", 250);
        let dest = fx.dir("dest");

        let plan = run(&[src], Some(&dest));
        // src, a.txt, nested, b.txt
        assert_eq!(plan.total_items, 4);
        assert_eq!(plan.total_bytes, 350);
        assert_eq!(plan.items.len(), 1, "one top-level entry");
        assert!(plan.items[0].is_dir);
        assert_eq!(plan.items[0].target, dest.join("src"));
    }

    #[test]
    fn an_empty_destination_yields_no_conflicts() {
        let fx = Fixture::new("clean");
        let a = fx.file("a.txt", 10);
        let dest = fx.dir("dest");

        let plan = run(&[a], Some(&dest));
        assert!(!plan.has_conflicts());
        assert_eq!(plan.clash_count, 0);
        assert!(plan.conflicts.is_empty());
    }

    /// The whole point: the collision is known before anything is written, and
    /// it carries what the row needs to render.
    #[test]
    fn a_taken_name_is_a_conflict_with_both_sides_described() {
        let fx = Fixture::new("clash");
        let source = fx.file("report.pdf", 100);
        let dest = fx.dir("dest");
        fx.file("dest/report.pdf", 4_000);

        let plan = run(&[source], Some(&dest));
        assert_eq!(plan.clash_count, 1);
        assert_eq!(plan.conflicts.len(), 1);

        let Conflict::NameTaken(clash) = &plan.conflicts[0] else {
            panic!("expected a name clash, got {:?}", plan.conflicts[0]);
        };
        assert_eq!(clash.index, 0);
        assert_eq!(&*clash.relative, "report.pdf");
        assert_eq!(clash.source.size, 100);
        assert_eq!(clash.existing.size, 4_000);
        assert!(!clash.existing.is_dir);
        // The modal shows what Keep Both would produce, rather than promising a
        // surprise.
        assert_eq!(&*clash.keep_both, "report (2).pdf");
        assert_eq!(plan.clash_at(&dest.join("report.pdf")), Some(0));
    }

    /// Dropping `assets/` onto a folder that already has `assets/` is a merge.
    /// Reporting it as a conflict would put a modal in front of the single most
    /// ordinary thing a user does with a file manager.
    #[test]
    fn a_directory_landing_on_a_directory_merges_silently() {
        let fx = Fixture::new("merge");
        let src = fx.dir("assets");
        fx.file("assets/logo.png", 10);
        let dest = fx.dir("dest");
        fx.dir("dest/assets");

        let plan = run(&[src], Some(&dest));
        assert_eq!(plan.clash_count, 0, "a merge was reported as a conflict");
    }

    /// ...but a file colliding *inside* that merge is a real conflict, and it
    /// must be found by the scan rather than mid-copy.
    #[test]
    fn a_collision_nested_inside_a_merge_is_still_found_up_front() {
        let fx = Fixture::new("nested-clash");
        let src = fx.dir("assets");
        fx.file("assets/logo.png", 10);
        let dest = fx.dir("dest");
        fx.dir("dest/assets");
        fx.file("dest/assets/logo.png", 99);

        let plan = run(&[src], Some(&dest));
        assert_eq!(plan.clash_count, 1);
        let Conflict::NameTaken(clash) = &plan.conflicts[0] else {
            panic!("expected a name clash");
        };
        assert_eq!(&*clash.relative, "assets/logo.png");
        assert_eq!(plan.clash_at(&dest.join("assets/logo.png")), Some(0));
    }

    #[test]
    fn a_directory_landing_on_a_file_is_a_conflict() {
        let fx = Fixture::new("dir-on-file");
        let src = fx.dir("notes");
        let dest = fx.dir("dest");
        fx.file("dest/notes", 5);

        let plan = run(&[src], Some(&dest));
        assert_eq!(plan.clash_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_destination_counts_as_a_collision() {
        // `copy_file` refuses to write through a link, so a link at the
        // destination must not be treated as "nothing there".
        let fx = Fixture::new("dest-symlink");
        let source = fx.file("a.txt", 10);
        let dest = fx.dir("dest");
        let elsewhere = fx.file("elsewhere.txt", 1);
        std::os::unix::fs::symlink(&elsewhere, dest.join("a.txt")).expect("symlink");

        let plan = run(&[source], Some(&dest));
        assert_eq!(plan.clash_count, 1, "a destination symlink was ignored");
    }

    #[cfg(unix)]
    #[test]
    fn a_source_symlink_is_counted_but_never_followed() {
        let fx = Fixture::new("src-symlink");
        let outside = fx.dir("outside");
        fx.file("outside/secret.txt", 1_000_000);
        let src = fx.dir("src");
        std::os::unix::fs::symlink(&outside, src.join("link")).expect("symlink");
        let dest = fx.dir("dest");

        let plan = run(&[src], Some(&dest));
        // src and the link itself, and nothing from beyond it.
        assert_eq!(plan.total_items, 2);
        assert_eq!(
            plan.total_bytes, 0,
            "the walk followed a link out of the tree"
        );
    }

    #[test]
    fn a_delete_needs_no_destination_and_finds_no_conflicts() {
        let fx = Fixture::new("delete");
        let src = fx.dir("doomed");
        fx.file("doomed/a.txt", 42);

        let plan = run(&[src], None);
        assert_eq!(plan.total_items, 2);
        assert_eq!(plan.total_bytes, 42);
        assert!(plan.items.is_empty(), "a delete plans no targets");
        assert!(!plan.has_conflicts());
    }

    #[test]
    fn the_shown_conflicts_are_capped_but_the_count_is_honest() {
        let fx = Fixture::new("many");
        let src = fx.dir("src");
        let dest = fx.dir("dest");
        let overflow = 20;
        for i in 0..(MAX_SHOWN_CLASHES + overflow) {
            fx.file(&format!("src/f{i}.txt"), 1);
            fx.file(&format!("dest/f{i}.txt"), 1);
        }
        // The destination already has `src`, so the two directories merge.
        fx.dir("dest/src");
        for i in 0..(MAX_SHOWN_CLASHES + overflow) {
            fx.file(&format!("dest/src/f{i}.txt"), 1);
        }

        let plan = run(&[src], Some(&dest));
        assert_eq!(plan.conflicts.len(), MAX_SHOWN_CLASHES);
        assert_eq!(plan.clash_count, (MAX_SHOWN_CLASHES + overflow) as u64);
    }

    #[test]
    fn cancellation_stops_the_scan() {
        let fx = Fixture::new("cancel");
        let src = fx.dir("src");
        fx.file("src/a.txt", 1);
        let dest = fx.dir("dest");

        let cancel = AtomicBool::new(true);
        let error = enumerate(&policy(), &[src], Some(&dest), &cancel)
            .expect_err("a cancelled scan must not return a plan");
        assert!(error.is_cancelled());
    }

    /// A source the user selected but the policy refuses must fail the job, not
    /// vanish from it — otherwise the transfer reports success for work it never
    /// did.
    #[test]
    fn an_unauthorized_top_level_source_fails_the_job() {
        let fx = Fixture::new("denied");
        let dest = fx.dir("dest");
        let relative = PathBuf::from("not/absolute.txt");

        let error = enumerate(&policy(), &[relative], Some(&dest), &AtomicBool::new(false))
            .expect_err("an unauthorized source must fail");
        assert!(!error.is_cancelled());
    }

    /// A root has no final component, so it cannot be given a name at the
    /// destination. Refusing it is the point: walking it would count the whole
    /// filesystem before reporting a total nobody asked for.
    #[test]
    fn a_filesystem_root_is_refused_rather_than_walked() {
        let fx = Fixture::new("root");
        let dest = fx.dir("dest");
        let root = if cfg!(windows) {
            PathBuf::from("C:\\")
        } else {
            PathBuf::from("/")
        };

        let error = enumerate(&policy(), &[root], Some(&dest), &AtomicBool::new(false))
            .expect_err("a filesystem root must be refused");
        assert!(!error.is_cancelled());
    }

    /// Hostile filenames reach the modal, so the strings the plan carries are
    /// already sanitized.
    #[test]
    fn conflict_rows_carry_no_reordering_characters() {
        let fx = Fixture::new("hostile");
        let name = "in\u{202E}gpj.exe";
        let source = fx.file(name, 10);
        let dest = fx.dir("dest");
        fx.file(&format!("dest/{name}"), 20);

        let plan = run(&[source], Some(&dest));
        let Conflict::NameTaken(clash) = &plan.conflicts[0] else {
            panic!("expected a name clash");
        };
        for text in [&clash.relative, &clash.existing_path, &clash.keep_both] {
            assert!(
                !text.contains('\u{202E}'),
                "a bidi override reached the UI: {text:?}"
            );
        }
        assert_eq!(&*clash.relative, "ingpj.exe");
    }
}
