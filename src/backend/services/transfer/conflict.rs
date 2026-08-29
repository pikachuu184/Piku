//! Collisions, and the one decision surface that settles them.
//!
//! The engine's rule: **a collision is discovered before bytes move, and
//! answered once.** `enumerate` already stats every node for its byte totals,
//! so the clashes are known by the time the first write would happen. That is
//! what makes "apply to all" mean something for 40 000 files instead of 40 000
//! dialogs.
//!
//! Today's engine renames silently — `report.pdf` becomes `report (2).pdf` and
//! nobody is told. [`unique_destination`] is that same function, moved here
//! unchanged: the behavior stays available as [`ConflictPolicy::KeepBoth`], it
//! just stops being the default nobody chose.

use std::collections::HashMap;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::backend::path::{FileName, PathPolicy};

/// What to do when a destination name is already taken.
///
/// `Ask` is the default, and it is the only variant that stops a job.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ConflictPolicy {
    #[default]
    Ask,
    /// Overwrite what is there. Destroys data, so it is audited per item.
    Replace,
    /// Leave the existing entry alone; the source is counted as skipped.
    Skip,
    /// Write beside it under the first free `name (n)` spelling.
    KeepBoth,
    /// Write under this exact name. Only ever applies to one entry — see
    /// [`ConflictResolver::record`].
    Rename(FileName),
}

impl ConflictPolicy {
    /// Whether a job carrying this policy stops to ask.
    pub fn asks(&self) -> bool {
        matches!(self, Self::Ask)
    }

    /// The instruction this policy yields, or `None` if it needs an answer.
    pub fn resolution(&self) -> Option<Resolution> {
        match self {
            Self::Ask => None,
            Self::Replace => Some(Resolution::Replace),
            Self::Skip => Some(Resolution::Skip),
            Self::KeepBoth => Some(Resolution::KeepBoth),
            Self::Rename(name) => Some(Resolution::Rename(name.clone())),
        }
    }
}

/// The instruction the copy loop follows for one colliding entry.
///
/// Distinct from [`ConflictPolicy`] because a policy may be `Ask` and an
/// instruction never can. Making that unrepresentable is cheaper than
/// remembering to check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Replace,
    Skip,
    KeepBoth,
    Rename(FileName),
}

impl Resolution {
    /// The audit op recorded when this resolution is carried out.
    ///
    /// All four are recorded, not just `Replace`. `Replace` destroys data, which
    /// is the obvious case; the other three change *where the user's data ended
    /// up*, which is the question an audit log gets asked afterwards.
    pub fn audit_op(&self) -> &'static str {
        match self {
            Self::Replace => "transfer.conflict.replace",
            Self::Skip => "transfer.conflict.skip",
            Self::KeepBoth => "transfer.conflict.keep_both",
            Self::Rename(_) => "transfer.conflict.rename",
        }
    }

    /// Whether carrying this out overwrites something that already exists.
    pub fn destroys_existing(&self) -> bool {
        matches!(self, Self::Replace)
    }
}

/// One half of a clash, as the row renders it.
///
/// Plain numbers rather than formatted strings: `core::format::format_size` and
/// `format_time` live on the UI side and are used by every other row in the
/// app, so formatting here would be a second implementation that drifts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Side {
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
}

impl Side {
    pub fn of(metadata: &Metadata) -> Self {
        Self {
            size: if metadata.is_dir() { 0 } else { metadata.len() },
            modified: metadata.modified().ok(),
            is_dir: metadata.is_dir(),
        }
    }
}

/// A destination name that is already taken.
///
/// Carries **no raw paths.** `relative` and `existing_path` are already through
/// `security::text`, because a filename is attacker-controlled and this struct
/// exists to be rendered. `index` is how the UI names an entry back to the
/// engine without ever holding the path it refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameClash {
    pub index: usize,
    /// Destination-relative name, sanitized. What titles the row.
    pub relative: Arc<str>,
    /// Full path of what is already there, sanitized.
    pub existing_path: Arc<str>,
    pub source: Side,
    pub existing: Side,
    /// The name [`Resolution::KeepBoth`] would produce, so the modal can show
    /// the final destination instead of promising a surprise.
    pub keep_both: Arc<str>,
}

/// Everything that can stop a transfer for an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conflict {
    NameTaken(NameClash),
    /// The destination volume is short. Overridable, because free space is a
    /// snapshot and the user may know something the engine does not — another
    /// job is about to finish, or the figure came from a network mount that
    /// reports badly.
    InsufficientSpace {
        needed: u64,
        available: u64,
    },
    /// A permission failure the user may be able to fix and retry past.
    Denied {
        path: Arc<str>,
        detail: Arc<str>,
    },
}

impl Conflict {
    /// The plan index, for the variants that name a single entry.
    pub fn index(&self) -> Option<usize> {
        match self {
            Self::NameTaken(clash) => Some(clash.index),
            Self::InsufficientSpace { .. } | Self::Denied { .. } => None,
        }
    }

    /// Whether this is answered per entry (`Replace`/`Skip`/…) or for the whole
    /// job (`Proceed`/`Cancel`).
    pub fn is_per_entry(&self) -> bool {
        matches!(self, Self::NameTaken(_))
    }
}

/// The answer coming back from the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// One policy for every pending clash — the "Apply to all conflicts" path.
    All(ConflictPolicy),
    /// One policy per pending clash, positionally against the slice the user was
    /// shown.
    Each(Vec<ConflictPolicy>),
    /// Go ahead despite a whole-job block (a space shortfall).
    Proceed,
    Cancel,
}

/// What [`ConflictResolver::record`] concluded.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The job may run. Every pending clash now has an instruction.
    Proceed,
    /// The job is over.
    Cancel,
    /// The decision did not settle everything — a policy list that did not line
    /// up, or an `All(Rename(_))`. The job asks again rather than guessing.
    Incomplete,
}

/// Holds the standing policy and the per-entry answers for one job.
///
/// Pure bookkeeping: no filesystem access, so the policy semantics are testable
/// without a temp directory.
#[derive(Debug)]
pub struct ConflictResolver {
    standing: ConflictPolicy,
    per_index: HashMap<usize, ConflictPolicy>,
    /// Set once a whole-job block has been waved through, so the same shortfall
    /// does not stop the job on every subsequent check.
    overridden: bool,
}

impl ConflictResolver {
    /// `standing` is the policy the request carried. `Ask` means every clash
    /// without an individual answer stops the job.
    pub fn new(standing: ConflictPolicy) -> Self {
        Self {
            standing,
            per_index: HashMap::new(),
            overridden: false,
        }
    }

    pub fn standing(&self) -> &ConflictPolicy {
        &self.standing
    }

    /// Whether a whole-job block has been waved through.
    pub fn is_overridden(&self) -> bool {
        self.overridden
    }

    /// What to do about the entry at `index`, or `None` meaning "stop and ask".
    ///
    /// An individual answer wins over the standing policy, which is what lets a
    /// user say `Replace` for one file and `Skip` for the rest.
    pub fn resolution_for(&self, index: usize) -> Option<Resolution> {
        match self.per_index.get(&index) {
            Some(policy) => policy.resolution(),
            None => self.standing.resolution(),
        }
    }

    /// Which of `pending` still has no instruction — what a second ask shows.
    pub fn unresolved(&self, pending: &[Conflict]) -> Vec<Conflict> {
        pending
            .iter()
            .filter(|conflict| match conflict {
                Conflict::NameTaken(clash) => self.resolution_for(clash.index).is_none(),
                _ => !self.overridden,
            })
            .cloned()
            .collect()
    }

    /// Fold a decision in.
    ///
    /// `pending` must be the exact slice, in order, that produced the decision —
    /// [`Decision::Each`] is positional against it.
    pub fn record(&mut self, decision: &Decision, pending: &[Conflict]) -> Outcome {
        match decision {
            Decision::Cancel => Outcome::Cancel,

            Decision::Proceed => {
                // Only clears whole-job blocks. A name clash is not something
                // "proceed" can answer: there is no such thing as copying over
                // a file *and* not copying over it.
                self.overridden = true;
                if self.unresolved(pending).is_empty() {
                    Outcome::Proceed
                } else {
                    Outcome::Incomplete
                }
            }

            // Renaming every clash to one name would collide 8 files into 1.
            // Refused rather than silently applied to the first.
            Decision::All(ConflictPolicy::Rename(_)) => Outcome::Incomplete,

            Decision::All(policy) => {
                self.standing = policy.clone();
                // A blanket answer supersedes earlier per-entry ones; otherwise
                // "apply to all" would quietly not apply to all.
                self.per_index.clear();
                self.overridden = true;
                if policy.asks() {
                    Outcome::Incomplete
                } else {
                    Outcome::Proceed
                }
            }

            Decision::Each(policies) => {
                let entries: Vec<usize> = pending.iter().filter_map(Conflict::index).collect();
                if policies.len() != entries.len() {
                    // A mismatch means the UI and the engine disagree about what
                    // was shown. Applying the overlap would answer the wrong
                    // files, so nothing is applied.
                    return Outcome::Incomplete;
                }
                for (index, policy) in entries.into_iter().zip(policies) {
                    self.per_index.insert(index, policy.clone());
                }
                // `Each` answers the entries, not a space shortfall, so
                // `overridden` deliberately stays put.
                if self.unresolved(pending).is_empty() {
                    Outcome::Proceed
                } else {
                    Outcome::Incomplete
                }
            }
        }
    }
}

/// Explorer-style collision handling: `name.txt` → `name (2).txt`.
///
/// Moved verbatim from `services::jobs::queue`, now the implementation of
/// [`Resolution::KeepBoth`] rather than an unconditional default. The wanted
/// path is authorized before any filesystem probe; on failure it is returned
/// untouched so the caller's guarded operations reject it with a real error.
pub fn unique_destination(policy: &PathPolicy, wanted: &Path) -> PathBuf {
    if policy.validate(wanted).is_err() {
        return wanted.to_path_buf();
    }
    if !wanted.exists() {
        return wanted.to_path_buf();
    }
    let parent = wanted.parent().unwrap_or_else(|| Path::new(""));
    let stem = wanted
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = wanted
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    for n in 2..10_000 {
        let candidate = if ext.is_empty() {
            parent.join(format!("{stem} ({n})"))
        } else {
            parent.join(format!("{stem} ({n}).{ext}"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    wanted.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clash(index: usize) -> Conflict {
        Conflict::NameTaken(NameClash {
            index,
            relative: format!("file-{index}.txt").into(),
            existing_path: format!("/dest/file-{index}.txt").into(),
            source: Side {
                size: 100,
                modified: None,
                is_dir: false,
            },
            existing: Side {
                size: 200,
                modified: None,
                is_dir: false,
            },
            keep_both: format!("file-{index} (2).txt").into(),
        })
    }

    fn shortfall() -> Conflict {
        Conflict::InsufficientSpace {
            needed: 1_000,
            available: 10,
        }
    }

    #[test]
    fn ask_is_the_default_and_the_only_policy_that_stops() {
        assert_eq!(ConflictPolicy::default(), ConflictPolicy::Ask);
        assert!(ConflictPolicy::Ask.asks());
        assert!(ConflictPolicy::Ask.resolution().is_none());
        for policy in [
            ConflictPolicy::Replace,
            ConflictPolicy::Skip,
            ConflictPolicy::KeepBoth,
            ConflictPolicy::Rename(FileName::new("x.txt").unwrap()),
        ] {
            assert!(!policy.asks(), "{policy:?} should not stop the job");
            assert!(policy.resolution().is_some(), "{policy:?} has no answer");
        }
    }

    #[test]
    fn a_job_with_no_policy_stops_on_every_clash() {
        let resolver = ConflictResolver::new(ConflictPolicy::Ask);
        let pending = vec![clash(0), clash(1)];
        assert!(resolver.resolution_for(0).is_none());
        assert_eq!(resolver.unresolved(&pending).len(), 2);
    }

    /// The point of resolving up front: one answer covers the whole job, and
    /// later clashes — including ones the scan never saw — stop nothing.
    #[test]
    fn apply_to_all_suppresses_every_later_stop() {
        let mut resolver = ConflictResolver::new(ConflictPolicy::Ask);
        let pending = vec![clash(0), clash(1), clash(2)];
        let outcome = resolver.record(&Decision::All(ConflictPolicy::Replace), &pending);

        assert_eq!(outcome, Outcome::Proceed);
        assert!(resolver.unresolved(&pending).is_empty());
        for index in 0..3 {
            assert_eq!(resolver.resolution_for(index), Some(Resolution::Replace));
        }
        // A file created mid-transfer, never in `pending`, is covered too.
        assert_eq!(resolver.resolution_for(9_999), Some(Resolution::Replace));
    }

    #[test]
    fn per_entry_answers_beat_the_standing_policy() {
        let mut resolver = ConflictResolver::new(ConflictPolicy::Skip);
        let pending = vec![clash(0), clash(1)];
        let outcome = resolver.record(
            &Decision::Each(vec![ConflictPolicy::Replace, ConflictPolicy::KeepBoth]),
            &pending,
        );

        assert_eq!(outcome, Outcome::Proceed);
        assert_eq!(resolver.resolution_for(0), Some(Resolution::Replace));
        assert_eq!(resolver.resolution_for(1), Some(Resolution::KeepBoth));
        // Anything not named falls back to the standing policy.
        assert_eq!(resolver.resolution_for(2), Some(Resolution::Skip));
    }

    #[test]
    fn a_blanket_answer_supersedes_earlier_per_entry_ones() {
        let mut resolver = ConflictResolver::new(ConflictPolicy::Ask);
        let pending = vec![clash(0), clash(1)];
        resolver.record(
            &Decision::Each(vec![ConflictPolicy::Skip, ConflictPolicy::Skip]),
            &pending,
        );
        resolver.record(&Decision::All(ConflictPolicy::Replace), &pending);
        assert_eq!(
            resolver.resolution_for(0),
            Some(Resolution::Replace),
            "apply-to-all did not apply to all"
        );
    }

    #[test]
    fn a_positional_list_that_does_not_line_up_is_refused_whole() {
        let mut resolver = ConflictResolver::new(ConflictPolicy::Ask);
        let pending = vec![clash(0), clash(1), clash(2)];
        let outcome = resolver.record(&Decision::Each(vec![ConflictPolicy::Replace]), &pending);

        assert_eq!(outcome, Outcome::Incomplete);
        assert!(
            resolver.resolution_for(0).is_none(),
            "a mismatched list answered the wrong entry"
        );
    }

    /// Renaming eight files to one name is not an answer.
    #[test]
    fn rename_cannot_be_applied_to_all() {
        let mut resolver = ConflictResolver::new(ConflictPolicy::Ask);
        let pending = vec![clash(0), clash(1)];
        let outcome = resolver.record(
            &Decision::All(ConflictPolicy::Rename(FileName::new("one.txt").unwrap())),
            &pending,
        );
        assert_eq!(outcome, Outcome::Incomplete);
        assert!(resolver.resolution_for(0).is_none());
    }

    #[test]
    fn proceed_clears_a_shortfall_but_not_a_name_clash() {
        let mut resolver = ConflictResolver::new(ConflictPolicy::Ask);

        let space_only = vec![shortfall()];
        assert_eq!(
            resolver.record(&Decision::Proceed, &space_only),
            Outcome::Proceed
        );
        assert!(resolver.is_overridden());
        assert!(resolver.unresolved(&space_only).is_empty());

        // The same override does nothing for a clash, which still has no answer.
        let mixed = vec![shortfall(), clash(0)];
        assert_eq!(
            resolver.record(&Decision::Proceed, &mixed),
            Outcome::Incomplete
        );
        assert_eq!(resolver.unresolved(&mixed), vec![clash(0)]);
    }

    #[test]
    fn cancel_ends_the_job_whatever_is_pending() {
        let mut resolver = ConflictResolver::new(ConflictPolicy::Ask);
        assert_eq!(
            resolver.record(&Decision::Cancel, &[clash(0)]),
            Outcome::Cancel
        );
        assert_eq!(resolver.record(&Decision::Cancel, &[]), Outcome::Cancel);
    }

    #[test]
    fn only_replace_destroys_what_is_already_there() {
        assert!(Resolution::Replace.destroys_existing());
        for resolution in [
            Resolution::Skip,
            Resolution::KeepBoth,
            Resolution::Rename(FileName::new("x.txt").unwrap()),
        ] {
            assert!(!resolution.destroys_existing());
        }
    }

    /// Audit op names are capped at 64 characters by `security::audit`, and
    /// every resolution records one — including the non-destructive ones, since
    /// "where did my file go" is the question the log gets asked.
    #[test]
    fn every_resolution_has_a_distinct_audit_op() {
        let ops: Vec<&str> = [
            Resolution::Replace,
            Resolution::Skip,
            Resolution::KeepBoth,
            Resolution::Rename(FileName::new("x.txt").unwrap()),
        ]
        .iter()
        .map(Resolution::audit_op)
        .collect();

        for op in &ops {
            assert!(op.len() <= 64, "{op} exceeds MAX_OP_CHARS");
            assert!(op.starts_with("transfer.conflict."));
        }
        let mut unique = ops.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ops.len(), "two resolutions share an op name");
    }

    #[test]
    fn only_a_name_clash_names_a_single_entry() {
        assert_eq!(clash(7).index(), Some(7));
        assert!(clash(7).is_per_entry());
        assert_eq!(shortfall().index(), None);
        assert!(!shortfall().is_per_entry());
    }

    #[test]
    fn keep_both_numbers_from_two_and_preserves_the_extension() {
        let policy = PathPolicy::with_system_roots();
        let dir = std::env::temp_dir().join("piku-keep-both");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        // A free name comes back untouched.
        let free = dir.join("report.pdf");
        assert_eq!(unique_destination(&policy, &free), free);

        std::fs::write(&free, b"a").expect("write");
        assert_eq!(
            unique_destination(&policy, &free),
            dir.join("report (2).pdf")
        );

        std::fs::write(dir.join("report (2).pdf"), b"b").expect("write");
        assert_eq!(
            unique_destination(&policy, &free),
            dir.join("report (3).pdf")
        );

        // No extension: the suffix goes on the end, not before a phantom dot.
        let bare = dir.join("notes");
        std::fs::write(&bare, b"c").expect("write");
        assert_eq!(unique_destination(&policy, &bare), dir.join("notes (2)"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unauthorizable path comes back unchanged rather than being probed, so
    /// the caller's guarded operation is what reports the real error.
    #[test]
    fn keep_both_does_not_probe_an_unauthorized_path() {
        let policy = PathPolicy::with_system_roots();
        let relative = Path::new("not/absolute.txt");
        assert_eq!(unique_destination(&policy, relative), relative);
    }
}
