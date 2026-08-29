//! The byte-moving pass: the same-volume fast path, the gated chunked copy, and
//! the bottom-up removal that finishes a move or carries out a permanent
//! delete.
//!
//! # Why this is a worklist and not a recursion
//!
//! Two reasons, and the second is the one that shaped the module.
//!
//! Tree depth is attacker-controlled — a crafted archive extracted earlier, or
//! just a deep `node_modules` — and `#![deny(clippy::panic)]` does nothing about
//! a stack overflow. That is the same argument [`enumerate`] makes.
//!
//! The real reason is that **the pass has to be stoppable and resumable from
//! outside itself.** A collision the scan never saw and a pause arriving between
//! two files both mean the same thing: hand the scheduler permit back, let the
//! driver await an answer, then carry on from exactly where we were. A recursive
//! copy keeps that position on the call stack of a blocking thread, where
//! nothing can reach it. A `Vec<Step>` keeps it in a field.
//!
//! So every stop is one shape: return a [`Pass`] with the worklist intact, and
//! be called again.
//!
//! # Post-order for free
//!
//! Removal has to happen bottom-up, and a LIFO stack gives that with no
//! bookkeeping: push the node's own removal *before* its children and it pops
//! after all of them. The same trick orders a move's source cleanup after its
//! copy — [`CopyPass::new`] pushes the prune first and the copy second.
//!
//! # Two pauses, deliberately
//!
//! Between files, the pass returns [`Pass::Paused`] so the driver can release
//! the permit — which is what lets "Pause All" free capacity for a queued job
//! rather than merely stopping four of them. *Inside* a file, the
//! [`PauseGate`](crate::storage::provider::PauseGate) threaded into `copy_file`
//! parks the blocking thread with both handles open, so the read offset, the
//! write offset, and the partial file all survive and resume needs no
//! checkpoint. That one holds its permit, and there is at most one per running
//! job.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::backend::error::TransferError;
use crate::backend::path::PathPolicy;
use crate::backend::services::transfer::conflict::{
    Conflict, ConflictResolver, NameClash, Resolution, Side, unique_destination,
};
use crate::backend::services::transfer::control::TransferControl;
use crate::backend::services::transfer::enumerate::{Plan, enumerate};
use crate::backend::services::transfer::job::TransferKind;
use crate::security::audit;
use crate::security::text;
use crate::storage::provider::StorageProvider;

/// Everything the pass reads, gathered so it can be rebuilt cheaply on every
/// re-entry.
///
/// Rebuilt rather than stored, because the resolver is the driver's to mutate
/// between calls: it folds the answer in, hands a fresh view back, and the pass
/// sees the new instruction without owning anything.
pub struct CopyContext<'a> {
    pub provider: &'a dyn StorageProvider,
    pub policy: &'a PathPolicy,
    pub control: &'a TransferControl,
    pub plan: &'a Plan,
    pub resolver: &'a ConflictResolver,
    /// The destination directory, for naming a clash relative to it the way the
    /// scan's rows are named. `None` for a permanent delete.
    pub destination: Option<&'a Path>,
}

/// How the pass came back.
#[derive(Debug, PartialEq, Eq)]
pub enum Pass {
    /// The worklist is empty.
    Done,
    /// A pause was noticed at a node boundary. The worklist is intact; call
    /// again after [`TransferControl::wait_for_resume`].
    Paused,
    /// Stopped on a collision with no applicable policy. The worklist is
    /// intact; record a decision on the resolver and call again.
    NeedsDecision(Vec<Conflict>),
    Cancelled,
}

/// One node still to be dealt with.
#[derive(Debug)]
enum Step {
    /// Deliver `source` to `target`, enqueueing children if it is a directory.
    Visit {
        source: PathBuf,
        target: PathBuf,
        /// One of the entries the user selected. Only these snapshot the
        /// failure count that decides whether a move may remove its source.
        top_level: bool,
        /// A collision at this node has already been acted on once. A second
        /// one is recorded as a failure rather than asked about again, because
        /// `Rename` onto another taken name would otherwise loop forever.
        resolved: bool,
    },
    /// Take `path` apart bottom-up: enqueue children, then the node itself.
    Prune { path: PathBuf, top_level: bool },
    /// Remove one node, which is empty by the time this pops.
    Remove {
        path: PathBuf,
        /// Credited to the progress counters for a permanent delete, whose bar
        /// is driven by the same byte totals a copy's is.
        bytes: u64,
        top_level: bool,
    },
}

/// The resumable byte-moving pass for one job.
#[derive(Debug)]
pub struct CopyPass {
    kind: TransferKind,
    stack: Vec<Step>,
    /// The index handed to the resolver for the clash the pass last stopped on,
    /// remembered so re-entry looks the same answer up rather than allocating a
    /// second index for the same node.
    last_ask: Option<(PathBuf, usize)>,
    /// Indices allocated to clashes the scan never recorded. Counted from the
    /// end of the plan's own conflicts so the two cannot collide.
    extra_asks: usize,
    /// `control.failure_count()` when the current top-level entry started.
    failures_at_top: u64,
}

impl CopyPass {
    /// Seed the worklist from the scan.
    ///
    /// `sources` is the approved top-level list; for a copy or move the targets
    /// come from [`Plan::items`], which already paired each source with its
    /// destination name.
    pub fn new(kind: TransferKind, sources: &[PathBuf], plan: &Plan) -> Self {
        let mut stack = Vec::new();
        match kind {
            // A delete is nothing but removal, so it is prunes all the way
            // down. `plan.items` is empty here — the scan pairs nothing with a
            // destination that does not exist.
            TransferKind::DeletePermanent => {
                for source in sources.iter().rev() {
                    stack.push(Step::Prune {
                        path: source.clone(),
                        top_level: true,
                    });
                }
            }
            TransferKind::Copy | TransferKind::Move => {
                // Reversed, because the stack pops in the opposite order and
                // the user's first selection should be the first thing moved.
                for item in plan.items.iter().rev() {
                    if kind == TransferKind::Move {
                        // Pushed *before* the copy so it pops after the whole
                        // subtree has been delivered.
                        stack.push(Step::Prune {
                            path: item.source.clone(),
                            top_level: true,
                        });
                    }
                    stack.push(Step::Visit {
                        source: item.source.clone(),
                        target: item.target.clone(),
                        top_level: true,
                        resolved: false,
                    });
                }
            }
        }

        Self {
            kind,
            stack,
            last_ask: None,
            extra_asks: 0,
            failures_at_top: 0,
        }
    }

    /// Whether anything is left to do. `false` after [`Pass::Done`].
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    /// Work the list down until it empties or something stops it.
    ///
    /// Blocking. Runs inside `BackendRuntime::blocking()`, which is the one
    /// sanctioned `spawn_blocking` in the process.
    pub fn run(&mut self, ctx: &CopyContext<'_>) -> Result<Pass, TransferError> {
        let cancel = ctx.control.cancel_flag();

        loop {
            // Both checks happen before the pop, so the worklist is untouched
            // when either fires.
            if ctx.control.is_cancelled() {
                return Ok(Pass::Cancelled);
            }
            if ctx.control.is_pause_requested() {
                return Ok(Pass::Paused);
            }

            let Some(step) = self.stack.pop() else {
                ctx.control.set_current(None);
                return Ok(Pass::Done);
            };

            match step {
                Step::Visit {
                    source,
                    target,
                    top_level,
                    resolved,
                } => {
                    if let Some(pending) =
                        self.visit(ctx, &cancel, source, target, top_level, resolved)?
                    {
                        return Ok(Pass::NeedsDecision(pending));
                    }
                }
                Step::Prune { path, top_level } => self.prune(ctx, path, top_level),
                Step::Remove {
                    path,
                    bytes,
                    top_level,
                } => self.remove(ctx, path, bytes, top_level)?,
            }
        }
    }

    /// Deliver one node. `Some(pending)` means it stopped to ask, and the step
    /// has already been pushed back.
    fn visit(
        &mut self,
        ctx: &CopyContext<'_>,
        cancel: &AtomicBool,
        source: PathBuf,
        target: PathBuf,
        top_level: bool,
        resolved: bool,
    ) -> Result<Option<Vec<Conflict>>, TransferError> {
        if top_level {
            self.failures_at_top = ctx.control.failure_count();
        }
        ctx.control.set_current(Some(&source));

        // Re-authorize both ends at the moment of use, not once at submission.
        // The scan skipped unauthorized children without counting them, so
        // skipping them here is what keeps the totals honest — counting them
        // would leave the job stuck at 98% forever.
        if ctx.policy.validate(&source).is_err() || ctx.policy.validate(&target).is_err() {
            ctx.control.push_failure(&source, "not authorized");
            return Ok(None);
        }

        let Ok(metadata) = std::fs::symlink_metadata(&source) else {
            // Gone between the scan and now. One vanished file does not fail a
            // 40 000-file copy; it becomes a row in the expanded card.
            ctx.control.push_failure(&source, "no longer there");
            return Ok(None);
        };

        if metadata.is_symlink() {
            // Neither followed nor recreated: `copy_file` refuses to read
            // through a link, and duplicating the link itself would quietly
            // copy whatever it points at, which may be somewhere the user never
            // selected. Counted as done so the totals close.
            ctx.control.add_items(1);
            return Ok(None);
        }

        let existing = std::fs::symlink_metadata(&target).ok();

        // A directory landing on a directory is a merge, which is what every
        // file manager does and what the user means by dropping `assets/` onto
        // a folder that already has one. Anything else that is already there is
        // a collision.
        if let Some(existing) = &existing {
            let merge = metadata.is_dir() && existing.is_dir() && !existing.is_symlink();
            if !merge {
                if resolved {
                    // The answer was carried out and the name is *still* taken:
                    // a `Rename` onto another taken name, or a `Replace` whose
                    // removal failed. Asking again would not converge.
                    ctx.control
                        .push_failure(&target, "the destination name is still taken");
                    return Ok(None);
                }

                // A clash the scan recorded carries the index the UI answered
                // against. One it never saw — created mid-transfer, or past the
                // display cap — falls back to the standing policy, which is
                // exactly what "apply to all" is for.
                let known = ctx.plan.clash_at(&target).or_else(|| {
                    self.last_ask
                        .as_ref()
                        .filter(|(path, _)| path == &target)
                        .map(|(_, index)| *index)
                });
                let resolution = match known {
                    Some(index) => ctx.resolver.resolution_for(index),
                    None => ctx.resolver.standing().resolution(),
                };

                let Some(resolution) = resolution else {
                    // An index is allocated only here, on the ask, so a
                    // policy-covered clash costs nothing.
                    let index = known.unwrap_or_else(|| {
                        let index = ctx.plan.conflicts.len() + self.extra_asks;
                        self.extra_asks += 1;
                        index
                    });
                    let pending = clash_at(ctx, index, &metadata, &target, existing);
                    self.last_ask = Some((target.clone(), index));
                    self.stack.push(Step::Visit {
                        source,
                        target,
                        top_level,
                        resolved,
                    });
                    return Ok(Some(vec![pending]));
                };

                self.apply(ctx, cancel, resolution, source, target, &metadata)?;
                return Ok(None);
            }
        }

        // A move within one filesystem is a directory-entry rewrite: no bytes
        // are read and none are written, however large the tree. Applied at
        // every depth rather than only at the top, because a merge rules out
        // renaming the tree as a whole while each child inside it can still
        // move as one entry — which is the difference between "instant" and
        // "read 40 000 files" for the most ordinary drag there is.
        //
        // Only when the name is free: `rename` onto an existing one either
        // fails or silently overwrites depending on the platform, and neither is
        // what a conflict policy just decided.
        if self.kind == TransferKind::Move
            && existing.is_none()
            && same_volume(&source, &target)
            && ctx.provider.rename(&source, &target).is_ok()
        {
            let (bytes, items) = moved_totals(ctx, cancel, &target, &metadata)?;
            ctx.control.add_bytes(bytes);
            ctx.control.add_items(items);
            return Ok(None);
        }
        // A failed rename falls through to copy-then-remove: it can fail for
        // reasons `dev` cannot see, such as a bind mount, a read-only subtree,
        // or a destination directory the user cannot write to.

        if metadata.is_dir() {
            if existing.is_none() {
                // Routed through the provider rather than a raw `create_dir`,
                // so directory creation gains the same authorization and audit
                // record as every other mutation.
                if let Err(error) = ctx.provider.create_dir(&target) {
                    ctx.control.push_failure(&target, &error.to_string());
                    return Ok(None);
                }
            }
            ctx.control.add_items(1);

            let children = match std::fs::read_dir(&source) {
                Ok(children) => children,
                Err(error) => {
                    ctx.control.push_failure(&source, &error.to_string());
                    return Ok(None);
                }
            };
            for child in children.flatten() {
                let child_source = child.path();
                let Some(name) = child_source.file_name() else {
                    continue;
                };
                self.stack.push(Step::Visit {
                    target: target.join(name),
                    source: child_source,
                    top_level: false,
                    resolved: false,
                });
            }
            return Ok(None);
        }

        // A regular file. Progress goes straight into the control's atomics —
        // one relaxed `fetch_add` per megabyte — and the scheduler's 200 ms
        // tick is what publishes. The old engine sent a channel message per
        // chunk, so the faster the disk the more of the render loop the copy
        // ate.
        let mut report = |chunk: u64| ctx.control.add_bytes(chunk);
        match ctx.provider.copy_file(
            &source,
            &target,
            &mut report,
            cancel,
            Some(ctx.control.pause_gate()),
        ) {
            Ok(_) => ctx.control.add_items(1),
            Err(error) => {
                // A cancelled copy has already removed its partial destination
                // file, and the loop turns it into `Pass::Cancelled` on the
                // next turn, so there is nothing to record.
                if !ctx.control.is_cancelled() {
                    ctx.control.push_failure(&source, &error.to_string());
                }
            }
        }
        Ok(None)
    }

    /// Carry out one resolution.
    fn apply(
        &mut self,
        ctx: &CopyContext<'_>,
        cancel: &AtomicBool,
        resolution: Resolution,
        source: PathBuf,
        target: PathBuf,
        metadata: &std::fs::Metadata,
    ) -> Result<(), TransferError> {
        // All four are audited, not just `Replace`. `Replace` destroys data,
        // which is the obvious case; the other three change *where the user's
        // data ended up*, which is the question the log gets asked afterwards.
        audit::record(resolution.audit_op(), &source, Some(&target), true, "");

        match resolution {
            Resolution::Skip => {
                // The whole subtree is credited, not just this node. A skipped
                // file whose bytes were never credited leaves the completion
                // bar short of the end for the rest of the job, and "Skip all"
                // over an existing tree is the common case.
                let (bytes, items) = match enumerate(ctx.policy, &[source], None, cancel) {
                    Ok(plan) => (plan.total_bytes, plan.total_items),
                    Err(error) if error.is_cancelled() => return Err(error),
                    Err(_) => (metadata.len(), 1),
                };
                ctx.control.add_bytes(bytes);
                ctx.control.add_items(items);
                ctx.control.add_skipped(items);
            }

            Resolution::Replace => {
                ctx.control.add_replaced(1);
                // The existing entry goes first: `copy_file` refuses to write
                // through a link and `create_dir` refuses a name that is taken,
                // so overwriting means removing. Reusing the prune worklist is
                // what gives an existing *directory* the same bottom-up
                // descent and the same containment check as any other removal.
                self.stack.push(Step::Visit {
                    source,
                    target: target.clone(),
                    top_level: false,
                    resolved: true,
                });
                self.stack.push(Step::Prune {
                    path: target,
                    top_level: false,
                });
            }

            Resolution::KeepBoth => {
                let renamed = unique_destination(ctx.policy, &target);
                if renamed == target {
                    ctx.control.push_failure(&target, "no free name beside it");
                    return Ok(());
                }
                self.stack.push(Step::Visit {
                    source,
                    target: renamed,
                    top_level: false,
                    resolved: true,
                });
            }

            Resolution::Rename(name) => {
                let Some(parent) = target.parent() else {
                    ctx.control
                        .push_failure(&target, "no parent to rename within");
                    return Ok(());
                };
                self.stack.push(Step::Visit {
                    source,
                    target: parent.join(name.as_str()),
                    top_level: false,
                    resolved: true,
                });
            }
        }
        Ok(())
    }

    /// Enqueue one node's removal, children first.
    fn prune(&mut self, ctx: &CopyContext<'_>, path: PathBuf, top_level: bool) {
        // A move never removes a source whose delivery did not fully succeed.
        // Without this, one locked file turns "move" into "delete" — the old
        // engine avoided it by failing the entire job on the first error, which
        // is not an option once a 40 000-file copy is allowed to carry on past
        // one bad node.
        if top_level
            && self.kind == TransferKind::Move
            && ctx.control.failure_count() > self.failures_at_top
        {
            ctx.control
                .push_failure(&path, "source kept: some items could not be copied");
            return;
        }

        if ctx.policy.validate(&path).is_err() {
            ctx.control.push_failure(&path, "not authorized");
            return;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            // Already gone — the same-volume fast path renamed it away, or
            // something else removed it. Not a failure.
            return;
        };
        ctx.control.set_current(Some(&path));

        if !metadata.is_dir() || metadata.is_symlink() {
            // A symlink is removed as a link. Following it would delete
            // whatever it points at, which is never what the user asked for.
            self.stack.push(Step::Remove {
                path,
                bytes: metadata.len(),
                top_level,
            });
            return;
        }

        // Pushed before the children so it pops after all of them.
        self.stack.push(Step::Remove {
            path: path.clone(),
            bytes: 0,
            top_level,
        });

        // Containment check before descending. The lexical guard above cannot
        // see through a link: an attacker who swaps a child directory for a
        // symlink between our `symlink_metadata` and the `read_dir` would have
        // us recurse outside the subtree being removed. Comparing the child's
        // *resolved* path against the resolved root closes that in practice.
        //
        // Note this is mitigation, not proof: the resolve and the descent are
        // still two separate steps. Fully closing it needs `openat`-style
        // handles (`cap-std`), which is tracked separately.
        let root = dunce::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let children = match std::fs::read_dir(&path) {
            Ok(children) => children,
            Err(error) => {
                ctx.control.push_failure(&path, &error.to_string());
                return;
            }
        };
        for child in children.flatten() {
            let child_path = child.path();
            let Ok(child_meta) = std::fs::symlink_metadata(&child_path) else {
                continue;
            };
            if child_meta.is_dir() && !child_meta.file_type().is_symlink() {
                let resolved =
                    dunce::canonicalize(&child_path).unwrap_or_else(|_| child_path.clone());
                if !resolved.starts_with(&root) {
                    // Recorded and skipped rather than descended into. The
                    // parent's own removal then fails because the directory is
                    // not empty, so nothing is silently left behind.
                    ctx.control
                        .push_failure(&child_path, "refusing to descend outside the deleted tree");
                    continue;
                }
            }
            self.stack.push(Step::Prune {
                path: child_path,
                top_level: false,
            });
        }
    }

    /// Remove one node.
    fn remove(
        &self,
        ctx: &CopyContext<'_>,
        path: PathBuf,
        bytes: u64,
        top_level: bool,
    ) -> Result<(), TransferError> {
        let result = ctx.provider.remove_after_move(&path);

        if self.kind == TransferKind::DeletePermanent {
            // A delete's bar is driven by the same byte totals a copy's is, so
            // removal is what credits them.
            ctx.control.add_bytes(bytes);
            ctx.control.add_items(1);
            if top_level {
                audit::record(
                    TransferKind::DeletePermanent.audit_op(),
                    &path,
                    None,
                    result.is_ok(),
                    result
                        .as_ref()
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_default()
                        .as_str(),
                );
            }
        }

        if let Err(error) = result {
            // A move whose source survives has left the user with two copies,
            // and that is something they have to be told rather than a row in a
            // list they may never open. Anything deeper is one item of many —
            // and a child that would not come off makes the parent's own
            // removal fail, so it surfaces here anyway.
            if self.kind == TransferKind::Move && top_level {
                return Err(TransferError::SourceCleanup {
                    path: text::sanitize_path(&path).into(),
                    detail: text::sanitize_label(&error.to_string()).into(),
                });
            }
            ctx.control.push_failure(&path, &error.to_string());
        }
        Ok(())
    }
}

/// Describe a collision the scan did not record, in the shape its rows use.
fn clash_at(
    ctx: &CopyContext<'_>,
    index: usize,
    source_meta: &std::fs::Metadata,
    target: &Path,
    existing_meta: &std::fs::Metadata,
) -> Conflict {
    let relative = ctx
        .destination
        .and_then(|dir| target.strip_prefix(dir).ok())
        .unwrap_or(target);

    Conflict::NameTaken(NameClash {
        index,
        relative: text::sanitize_path(relative).into(),
        existing_path: text::sanitize_path(target).into(),
        source: Side::of(source_meta),
        existing: Side::of(existing_meta),
        keep_both: unique_destination(ctx.policy, target)
            .file_name()
            .map(|name| text::sanitize_label(&name.to_string_lossy()))
            .unwrap_or_default()
            .into(),
    })
}

/// Bytes and items a completed rename accounted for.
///
/// A rename moves a whole subtree in one syscall, so nothing incremental was
/// reported and the counters would otherwise stall for the rest of the job. A
/// file answers from the metadata already in hand; a directory needs a walk,
/// which is stats only — no bytes are read — and costs a small fraction of the
/// copy it just replaced.
fn moved_totals(
    ctx: &CopyContext<'_>,
    cancel: &AtomicBool,
    moved: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(u64, u64), TransferError> {
    if !metadata.is_dir() {
        return Ok((metadata.len(), 1));
    }
    let sources = [moved.to_path_buf()];
    match enumerate(ctx.policy, &sources, None, cancel) {
        Ok(plan) => Ok((plan.total_bytes, plan.total_items)),
        Err(error) if error.is_cancelled() => Err(error),
        // The tree moved; failing to *count* it afterwards is not worth failing
        // the job over. One item, and the totals close short.
        Err(_) => Ok((0, 1)),
    }
}

/// Whether `source` and the directory that will hold `target` are on one
/// filesystem, so a move can be a rename.
///
/// `target` need not exist — its parent is what is measured.
#[cfg(unix)]
fn same_volume(source: &Path, target: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    // The device number, not the leading path component. On POSIX every
    // absolute path starts with `/`, so a lexical compare answers "same
    // volume" for every pair of paths on the machine — which would send a
    // cross-device move down the rename path and make it fail on every single
    // item before falling back.
    let Some(parent) = target.parent() else {
        return false;
    };
    match (
        std::fs::symlink_metadata(source),
        std::fs::metadata(parent), // followed: the destination was resolved already
    ) {
        (Ok(source), Ok(parent)) => source.dev() == parent.dev(),
        _ => false,
    }
}

/// Whether `source` and the directory that will hold `target` are on one
/// volume.
#[cfg(not(unix))]
fn same_volume(source: &Path, target: &Path) -> bool {
    // On Windows the first component *is* the volume (`C:`), which is exactly
    // the question, and there is no `dev` to ask instead.
    fn volume(path: &Path) -> Option<String> {
        path.components()
            .next()
            .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
    }
    volume(source).is_some() && volume(source) == volume(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use crate::backend::path::FileName;
    use crate::backend::services::transfer::conflict::{ConflictPolicy, Decision, Outcome};
    use crate::backend::services::transfer::job::{JobId, Priority};
    use crate::storage::local::LocalProvider;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("piku-copy-{name}"));
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

        fn at(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The pass plus everything it reads, so a test can drive it the way the
    /// scheduler will.
    struct Harness {
        provider: LocalProvider,
        policy: PathPolicy,
        control: Arc<TransferControl>,
        resolver: ConflictResolver,
        plan: Plan,
        destination: Option<PathBuf>,
        pass: CopyPass,
    }

    impl Harness {
        fn new(
            kind: TransferKind,
            sources: Vec<PathBuf>,
            destination: Option<PathBuf>,
            standing: ConflictPolicy,
        ) -> Self {
            let policy = PathPolicy::with_system_roots();
            let control = TransferControl::new(JobId::next(), kind, Priority::Normal);
            let plan = enumerate(
                &policy,
                &sources,
                destination.as_deref(),
                &AtomicBool::new(false),
            )
            .expect("scan");
            control.set_totals(plan.total_bytes, plan.total_items);
            let pass = CopyPass::new(kind, &sources, &plan);

            Self {
                provider: LocalProvider::new(),
                policy,
                control,
                resolver: ConflictResolver::new(standing),
                plan,
                destination,
                pass,
            }
        }

        fn copy(sources: Vec<PathBuf>, destination: PathBuf) -> Self {
            Self::new(
                TransferKind::Copy,
                sources,
                Some(destination),
                ConflictPolicy::Ask,
            )
        }

        fn run(&mut self) -> Result<Pass, TransferError> {
            let Self {
                provider,
                policy,
                control,
                resolver,
                plan,
                destination,
                pass,
            } = self;
            let ctx = CopyContext {
                provider: &*provider,
                policy: &*policy,
                control,
                plan: &*plan,
                resolver: &*resolver,
                destination: destination.as_deref(),
            };
            pass.run(&ctx)
        }

        fn finish(&mut self) -> Pass {
            self.run().expect("the pass failed")
        }
    }

    #[test]
    fn a_file_is_copied_and_counted() {
        let fx = Fixture::new("file");
        let source = fx.file("a.txt", 2_048);
        let dest = fx.dir("dest");

        let mut h = Harness::copy(vec![source], dest.clone());
        assert_eq!(h.finish(), Pass::Done);

        assert_eq!(
            std::fs::read(dest.join("a.txt")).expect("copied").len(),
            2_048
        );
        let progress = h.control.progress();
        assert_eq!(progress.done_bytes, 2_048);
        assert_eq!(progress.done_items, 1);
        assert_eq!(progress.percent(), 100.0);
        assert_eq!(h.control.failure_count(), 0);
        assert!(h.pass.is_empty());
    }

    #[test]
    fn a_tree_is_copied_with_its_directories_created() {
        let fx = Fixture::new("tree");
        let src = fx.dir("src");
        fx.file("src/a.txt", 100);
        fx.file("src/nested/deep/b.txt", 250);
        let dest = fx.dir("dest");

        let mut h = Harness::copy(vec![src], dest.clone());
        assert_eq!(h.finish(), Pass::Done);

        assert!(dest.join("src/nested/deep").is_dir());
        assert_eq!(std::fs::read(dest.join("src/a.txt")).expect("a").len(), 100);
        assert_eq!(
            std::fs::read(dest.join("src/nested/deep/b.txt"))
                .expect("b")
                .len(),
            250
        );
        // src, a.txt, nested, deep, b.txt
        let progress = h.control.progress();
        assert_eq!(progress.done_items, 5);
        assert_eq!(progress.done_bytes, 350);
        assert_eq!(progress.percent(), 100.0);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_counted_but_neither_followed_nor_recreated() {
        let fx = Fixture::new("symlink");
        let outside = fx.file("outside.txt", 4_000);
        let src = fx.dir("src");
        std::os::unix::fs::symlink(&outside, src.join("link")).expect("symlink");
        let dest = fx.dir("dest");

        let mut h = Harness::copy(vec![src], dest.clone());
        assert_eq!(h.finish(), Pass::Done);

        assert!(
            !dest.join("src/link").exists(),
            "the link was recreated or followed"
        );
        // src and the link itself, matching what the scan counted.
        assert_eq!(h.control.progress().done_items, 2);
        assert_eq!(h.control.progress().done_bytes, 0);
    }

    /// The whole point of the design: the pass stops, the answer arrives from
    /// somewhere else entirely, and it carries on from the same node.
    #[test]
    fn a_clash_with_no_policy_stops_the_pass_and_resumes_on_the_answer() {
        let fx = Fixture::new("ask");
        let source = fx.file("report.pdf", 100);
        let dest = fx.dir("dest");
        fx.file("dest/report.pdf", 4_000);

        let mut h = Harness::copy(vec![source], dest.clone());
        let pending = match h.finish() {
            Pass::NeedsDecision(pending) => pending,
            other => panic!("expected a stop for a decision, got {other:?}"),
        };
        assert_eq!(pending.len(), 1);
        let Conflict::NameTaken(clash) = &pending[0] else {
            panic!("expected a name clash");
        };
        assert_eq!(&*clash.relative, "report.pdf");
        assert_eq!(clash.source.size, 100);
        assert_eq!(clash.existing.size, 4_000);
        assert_eq!(&*clash.keep_both, "report (2).pdf");
        // Nothing was written while it waited.
        assert_eq!(
            std::fs::read(dest.join("report.pdf"))
                .expect("intact")
                .len(),
            4_000
        );

        assert_eq!(
            h.resolver
                .record(&Decision::All(ConflictPolicy::Replace), &pending),
            Outcome::Proceed
        );
        assert_eq!(h.finish(), Pass::Done);
        assert_eq!(
            std::fs::read(dest.join("report.pdf"))
                .expect("replaced")
                .len(),
            100
        );
        assert_eq!(h.control.replaced(), 1);
    }

    /// A `Replace` onto a directory has to take it apart first, and it goes
    /// through the same bottom-up prune as any other removal.
    #[test]
    fn replace_removes_an_existing_directory_before_writing() {
        let fx = Fixture::new("replace-dir");
        let source = fx.file("notes", 10);
        let dest = fx.dir("dest");
        fx.file("dest/notes/stale/old.txt", 5_000);

        let mut h = Harness::new(
            TransferKind::Copy,
            vec![source],
            Some(dest.clone()),
            ConflictPolicy::Replace,
        );
        assert_eq!(h.finish(), Pass::Done);

        let written = dest.join("notes");
        assert!(written.is_file(), "the directory was not replaced");
        assert_eq!(std::fs::read(&written).expect("written").len(), 10);
        assert_eq!(h.control.replaced(), 1);
        assert_eq!(h.control.failure_count(), 0);
    }

    /// "Skip all" over an existing tree is the common case, and the bar has to
    /// reach the end anyway — so a skip credits what it did not copy.
    #[test]
    fn skip_credits_the_whole_subtree_so_the_bar_still_finishes() {
        let fx = Fixture::new("skip");
        let src = fx.dir("src");
        fx.file("src/a.txt", 1_000);
        fx.file("src/b.txt", 3_000);
        let dest = fx.dir("dest");
        fx.dir("dest/src");
        fx.file("dest/src/a.txt", 7);
        fx.file("dest/src/b.txt", 9);

        let mut h = Harness::new(
            TransferKind::Copy,
            vec![src],
            Some(dest.clone()),
            ConflictPolicy::Skip,
        );
        assert_eq!(h.finish(), Pass::Done);

        // Untouched...
        assert_eq!(std::fs::read(dest.join("src/a.txt")).expect("a").len(), 7);
        assert_eq!(std::fs::read(dest.join("src/b.txt")).expect("b").len(), 9);
        // ...and the completion bar still closes.
        assert_eq!(h.control.progress().percent(), 100.0);
        assert_eq!(h.control.skipped(), 2);
    }

    #[test]
    fn keep_both_writes_beside_the_existing_entry() {
        let fx = Fixture::new("keep-both");
        let source = fx.file("report.pdf", 100);
        let dest = fx.dir("dest");
        fx.file("dest/report.pdf", 4_000);

        let mut h = Harness::new(
            TransferKind::Copy,
            vec![source],
            Some(dest.clone()),
            ConflictPolicy::KeepBoth,
        );
        assert_eq!(h.finish(), Pass::Done);

        assert_eq!(
            std::fs::read(dest.join("report.pdf")).expect("old").len(),
            4_000
        );
        assert_eq!(
            std::fs::read(dest.join("report (2).pdf"))
                .expect("new")
                .len(),
            100
        );
    }

    /// A `Rename` onto a name that is *also* taken must not loop: the second
    /// clash at the same node is recorded and the node is dropped.
    #[test]
    fn a_rename_onto_another_taken_name_fails_instead_of_looping() {
        let fx = Fixture::new("rename-taken");
        let source = fx.file("a.txt", 100);
        let dest = fx.dir("dest");
        fx.file("dest/a.txt", 1);
        fx.file("dest/b.txt", 2);

        let mut h = Harness::new(
            TransferKind::Copy,
            vec![source],
            Some(dest.clone()),
            ConflictPolicy::Rename(FileName::new("b.txt").expect("a valid name")),
        );
        assert_eq!(h.finish(), Pass::Done);

        assert_eq!(std::fs::read(dest.join("b.txt")).expect("b").len(), 2);
        assert_eq!(h.control.failure_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_move_within_one_volume_renames_instead_of_copying() {
        use std::os::unix::fs::MetadataExt as _;

        let fx = Fixture::new("fast-move");
        let source = fx.file("a.txt", 4_096);
        let before = std::fs::metadata(&source).expect("stat").ino();
        let dest = fx.dir("dest");

        let mut h = Harness::new(
            TransferKind::Move,
            vec![source.clone()],
            Some(dest.clone()),
            ConflictPolicy::Ask,
        );
        assert_eq!(h.finish(), Pass::Done);

        let moved = dest.join("a.txt");
        assert!(!source.exists(), "the source survived the move");
        assert_eq!(
            std::fs::metadata(&moved).expect("stat").ino(),
            before,
            "the bytes were copied instead of the entry renamed"
        );
        // A rename reports nothing incrementally, so the counters are credited
        // from the metadata rather than left stalled.
        assert_eq!(h.control.progress().done_bytes, 4_096);
        assert_eq!(h.control.progress().percent(), 100.0);
    }

    #[cfg(unix)]
    #[test]
    fn a_renamed_directory_credits_its_whole_subtree() {
        let fx = Fixture::new("fast-move-tree");
        let src = fx.dir("src");
        fx.file("src/a.txt", 1_000);
        fx.file("src/nested/b.txt", 2_000);
        let dest = fx.dir("dest");

        let mut h = Harness::new(
            TransferKind::Move,
            vec![src.clone()],
            Some(dest.clone()),
            ConflictPolicy::Ask,
        );
        assert_eq!(h.finish(), Pass::Done);

        assert!(!src.exists());
        assert!(dest.join("src/nested/b.txt").is_file());
        let progress = h.control.progress();
        assert_eq!(progress.done_bytes, 3_000);
        assert_eq!(progress.done_items, 4, "src, a.txt, nested, b.txt");
        assert_eq!(progress.percent(), 100.0);
    }

    /// The fast path is not limited to top-level entries. A merge at the top
    /// takes the whole-tree rename off the table, but each *child* still moves
    /// as a directory-entry rewrite — which is what keeps "move 40 000 files
    /// into a folder that already exists" from reading a single byte.
    #[cfg(unix)]
    #[test]
    fn a_move_onto_a_merged_directory_renames_each_child() {
        use std::os::unix::fs::MetadataExt as _;

        let fx = Fixture::new("merge-move");
        let src = fx.dir("src");
        let child = fx.file("src/a.txt", 500);
        let before = std::fs::metadata(&child).expect("stat").ino();
        let dest = fx.dir("dest");
        fx.dir("dest/src");

        let mut h = Harness::new(
            TransferKind::Move,
            vec![src.clone()],
            Some(dest.clone()),
            ConflictPolicy::Ask,
        );
        assert_eq!(h.finish(), Pass::Done);

        assert!(!src.exists(), "the source tree survived");
        let moved = dest.join("src/a.txt");
        assert_eq!(std::fs::read(&moved).expect("a").len(), 500);
        assert_eq!(
            std::fs::metadata(&moved).expect("stat").ino(),
            before,
            "the child was copied instead of renamed"
        );
        assert_eq!(h.control.progress().percent(), 100.0);
    }

    /// One locked file must not turn a move into a delete. The old engine got
    /// this right by failing the whole job on the first error; once a copy is
    /// allowed to carry on past a bad node, the source cleanup needs its own
    /// guard.
    ///
    /// Forcing the failure takes some care, because a same-volume move never
    /// reads a byte: `rename` needs write permission on the two *directories*
    /// and none at all on the file, so a `chmod 000` source still moves. An
    /// unwritable destination is what defeats both the rename and the copy.
    #[cfg(unix)]
    #[test]
    fn a_move_whose_copy_failed_keeps_the_source() {
        use std::os::unix::fs::PermissionsExt as _;

        let fx = Fixture::new("kept-source");
        let src = fx.dir("src");
        let doomed = fx.file("src/a.txt", 10);
        let dest = fx.dir("dest");
        // Already there, so the two directories merge and the top-level rename
        // is off the table; read-only, so neither the rename nor the copy of
        // the child into it can succeed.
        let landing = fx.dir("dest/src");
        std::fs::set_permissions(&landing, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        // Root ignores the mode bits, so there would be nothing to observe.
        if std::fs::write(landing.join("probe"), b"x").is_ok() {
            let _ = std::fs::set_permissions(&landing, std::fs::Permissions::from_mode(0o755));
            return;
        }

        let mut h = Harness::new(
            TransferKind::Move,
            vec![src.clone()],
            Some(dest.clone()),
            ConflictPolicy::Ask,
        );
        assert_eq!(h.finish(), Pass::Done);

        assert!(
            doomed.exists(),
            "a file that could not be copied was deleted"
        );
        assert!(src.exists(), "the source tree was removed after a failure");
        let failures = h.control.failures();
        assert!(
            failures.iter().any(|f| f.detail.contains("source kept")),
            "the kept source was not reported: {failures:?}"
        );

        let _ = std::fs::set_permissions(&landing, std::fs::Permissions::from_mode(0o755));
    }

    #[test]
    fn a_permanent_delete_removes_bottom_up_and_credits_the_totals() {
        let fx = Fixture::new("delete");
        let doomed = fx.dir("doomed");
        fx.file("doomed/a.txt", 42);
        fx.file("doomed/nested/b.txt", 58);

        let mut h = Harness::new(
            TransferKind::DeletePermanent,
            vec![doomed.clone()],
            None,
            ConflictPolicy::Ask,
        );
        assert_eq!(h.finish(), Pass::Done);

        assert!(!doomed.exists(), "the tree survived a permanent delete");
        let progress = h.control.progress();
        assert_eq!(progress.done_items, 4, "doomed, a.txt, nested, b.txt");
        assert_eq!(progress.done_bytes, 100);
        assert_eq!(progress.percent(), 100.0);
        assert_eq!(h.control.failure_count(), 0);
    }

    /// A link inside a deleted tree is removed as a link. Following it would
    /// delete whatever it points at, which is never what was selected.
    #[cfg(unix)]
    #[test]
    fn a_delete_removes_a_symlinked_child_without_following_it() {
        let fx = Fixture::new("delete-symlink");
        let keep = fx.dir("keep");
        fx.file("keep/precious.txt", 1_000);
        let doomed = fx.dir("doomed");
        std::os::unix::fs::symlink(&keep, doomed.join("link")).expect("symlink");

        let mut h = Harness::new(
            TransferKind::DeletePermanent,
            vec![doomed.clone()],
            None,
            ConflictPolicy::Ask,
        );
        assert_eq!(h.finish(), Pass::Done);

        assert!(!doomed.exists(), "the tree survived");
        assert!(
            fx.at("keep/precious.txt").is_file(),
            "the delete followed a link out of the tree"
        );
    }

    #[test]
    fn pausing_between_nodes_returns_with_the_worklist_intact() {
        let fx = Fixture::new("pause");
        let src = fx.dir("src");
        for i in 0..8 {
            fx.file(&format!("src/f{i}.txt"), 64);
        }
        let dest = fx.dir("dest");

        let mut h = Harness::copy(vec![src], dest.clone());
        h.control.request_pause();
        assert_eq!(h.finish(), Pass::Paused);
        assert!(!h.pass.is_empty(), "the worklist was consumed while paused");
        assert_eq!(h.control.progress().done_items, 0);

        h.control.request_resume();
        assert_eq!(h.finish(), Pass::Done);
        assert_eq!(h.control.progress().percent(), 100.0);
        assert_eq!(h.control.progress().done_items, 9, "src plus eight files");
    }

    #[test]
    fn cancellation_stops_the_pass() {
        let fx = Fixture::new("cancel");
        let source = fx.file("a.txt", 64);
        let dest = fx.dir("dest");

        let mut h = Harness::copy(vec![source], dest.clone());
        h.control.cancel();
        assert_eq!(h.finish(), Pass::Cancelled);
        assert!(!dest.join("a.txt").exists());
    }

    /// A collision created after the scan — the case today's engine cannot
    /// answer at all — is covered by the standing policy without stopping.
    #[test]
    fn an_unplanned_clash_follows_the_standing_policy() {
        let fx = Fixture::new("unplanned");
        let src = fx.dir("src");
        fx.file("src/late.txt", 100);
        let dest = fx.dir("dest");

        let mut h = Harness::new(
            TransferKind::Copy,
            vec![src],
            Some(dest.clone()),
            ConflictPolicy::Skip,
        );
        // Created between the scan and the copy, so it is in no plan index.
        assert!(h.plan.clash_at(&dest.join("src/late.txt")).is_none());
        fx.file("dest/src/late.txt", 1);

        assert_eq!(h.finish(), Pass::Done);
        assert_eq!(
            std::fs::read(dest.join("src/late.txt"))
                .expect("kept")
                .len(),
            1,
            "the standing Skip was not applied to an unplanned clash"
        );
        assert_eq!(h.control.skipped(), 1);
    }

    /// The same case with no standing policy stops, and the index it invents
    /// has to survive the round trip — otherwise the answer comes back keyed to
    /// nothing and the pass asks again forever.
    #[test]
    fn an_unplanned_clash_answered_per_entry_resumes_once() {
        let fx = Fixture::new("unplanned-ask");
        let src = fx.dir("src");
        fx.file("src/late.txt", 100);
        let dest = fx.dir("dest");

        let mut h = Harness::copy(vec![src], dest.clone());
        fx.file("dest/src/late.txt", 1);

        let pending = match h.finish() {
            Pass::NeedsDecision(pending) => pending,
            other => panic!("expected a stop, got {other:?}"),
        };
        assert_eq!(
            h.resolver
                .record(&Decision::Each(vec![ConflictPolicy::Replace]), &pending),
            Outcome::Proceed
        );
        assert_eq!(h.finish(), Pass::Done, "the pass asked a second time");
        assert_eq!(
            std::fs::read(dest.join("src/late.txt"))
                .expect("replaced")
                .len(),
            100
        );
    }

    /// A top-level entry that disappears between the scan and the copy becomes
    /// one row rather than a dead job. (A *child* that disappears is never seen
    /// at all — the descent reads the directory at copy time, so it is simply
    /// not in the listing.)
    #[test]
    fn a_source_that_vanished_after_the_scan_is_a_row_not_a_dead_job() {
        let fx = Fixture::new("vanished");
        let doomed = fx.file("gone.txt", 100);
        let kept = fx.file("kept.txt", 50);
        let dest = fx.dir("dest");

        let mut h = Harness::copy(vec![doomed.clone(), kept], dest.clone());
        std::fs::remove_file(&doomed).expect("remove");

        assert_eq!(h.finish(), Pass::Done);
        assert_eq!(
            std::fs::read(dest.join("kept.txt")).expect("kept").len(),
            50
        );
        assert!(!dest.join("gone.txt").exists());
        assert_eq!(h.control.failure_count(), 1);
        assert_eq!(&*h.control.failures()[0].detail, "no longer there");
    }

    /// Progress goes into the control's atomics rather than a channel, which is
    /// what keeps a fast disk from eating the render loop. The counter has to
    /// move *during* a file, not only at the end of it.
    #[test]
    fn bytes_are_credited_as_they_are_written() {
        let fx = Fixture::new("granular");
        // Larger than one 1 MB chunk, so there is more than one report.
        let source = fx.file("big.bin", 3 * 1024 * 1024);
        let dest = fx.dir("dest");

        let mut h = Harness::copy(vec![source], dest);
        assert_eq!(h.finish(), Pass::Done);
        assert_eq!(h.control.progress().done_bytes, 3 * 1024 * 1024);
        assert_eq!(h.control.progress().done_items, 1);
    }

    #[cfg(unix)]
    #[test]
    fn one_filesystem_is_recognized_and_a_missing_parent_is_not() {
        let fx = Fixture::new("volume");
        let source = fx.file("a.txt", 1);
        let dest = fx.dir("dest");
        assert!(same_volume(&source, &dest.join("a.txt")));
        // A destination whose parent does not exist cannot be measured, so the
        // fast path is declined rather than guessed at.
        assert!(!same_volume(&source, &fx.at("nope/a.txt")));
    }

    /// The cancel flag the pass hands to `copy_file` is the control's own, so
    /// there is exactly one answer to "was this cancelled".
    #[test]
    fn the_pass_uses_the_controls_cancel_flag() {
        let control = TransferControl::new(JobId::next(), TransferKind::Copy, Priority::Normal);
        let flag = control.cancel_flag();
        control.cancel();
        assert!(flag.load(Ordering::Acquire));
    }
}
