//! Backend tests against throwaway repositories created with gix itself —
//! no `git.exe`, no hooks, everything under the OS temp dir.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::super::backend::GitBackend;
use super::super::types::{DiffLineKind, DiffTarget, GitStatusCode};
use super::GixBackend;

/// A throwaway repository under the OS temp dir, removed on drop.
struct TestRepo {
    root: PathBuf,
    repo: gix::Repository,
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn init_repo(tag: &str) -> TestRepo {
    let root = std::env::temp_dir().join(format!("piku-git-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let repo = gix::init(&root).unwrap();
    let config_path = root.join(".git").join("config");
    let mut config = std::fs::read_to_string(&config_path).unwrap_or_default();
    config.push_str("\n[user]\n\tname = Piku Test\n\temail = test@example.com\n");
    std::fs::write(config_path, config).unwrap();
    TestRepo { root, repo }
}

/// Write `files` into the worktree, commit them as a flat tree on HEAD, and
/// sync the index so status reports a clean tree afterwards.
fn commit_files(
    t: &TestRepo,
    files: &[(&str, &str)],
    msg: &str,
    parent: Option<gix::ObjectId>,
) -> gix::ObjectId {
    let mut entries = Vec::new();
    for (name, content) in files {
        assert!(!name.contains('/'), "flat trees only in tests");
        std::fs::write(t.root.join(name), content).unwrap();
        let blob = t.repo.write_blob(content.as_bytes()).unwrap().detach();
        entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: (*name).into(),
            oid: blob,
        });
    }
    entries.sort();
    let tree = gix::objs::Tree { entries };
    let tree_id = t.repo.write_object(&tree).unwrap().detach();

    let sig = gix::actor::Signature {
        name: "Piku Test".into(),
        email: "test@example.com".into(),
        time: gix::date::Time::now_utc(),
    };
    let (mut b1, mut b2) = Default::default();
    let id = t
        .repo
        .commit_as(
            sig.to_ref(&mut b1),
            sig.to_ref(&mut b2),
            "HEAD",
            msg,
            tree_id,
            parent.into_iter(),
        )
        .unwrap()
        .detach();
    let mut index = t.repo.index_from_tree(&tree_id).unwrap();
    index.write(Default::default()).unwrap();
    id
}

/// Create or move a ref with an inline reflog identity. `Repository::reference`
/// sources the reflog committer from git config, which CI runners don't have —
/// this keeps the tests hermetic the same way `commit_files` does.
fn set_ref(
    t: &TestRepo,
    name: &str,
    target: gix::ObjectId,
    expected: gix::refs::transaction::PreviousValue,
) {
    use gix::refs::transaction::{Change, LogChange, RefEdit, RefLog};

    let sig = gix::actor::Signature {
        name: "Piku Test".into(),
        email: "test@example.com".into(),
        time: gix::date::Time::now_utc(),
    };
    let mut buf = Default::default();
    t.repo
        .edit_references_as(
            Some(RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: "test".into(),
                    },
                    expected,
                    new: gix::refs::Target::Object(target),
                },
                name: name.try_into().unwrap(),
                deref: false,
            }),
            Some(sig.to_ref(&mut buf)),
        )
        .unwrap();
}

fn no_interrupt() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn discovers_repo_from_nested_dir_and_rejects_non_repo() {
    let t = init_repo("discover");
    commit_files(&t, &[("a.txt", "hello\n")], "init", None);
    let nested = t.root.join("sub").join("deeper");
    std::fs::create_dir_all(&nested).unwrap();

    let backend = GixBackend::default();
    let found = backend.discover(&nested).expect("repo should be found");
    assert_eq!(
        found.to_string_lossy().to_lowercase(),
        backend
            .guard
            .sanitize(&t.root)
            .unwrap()
            .to_string_lossy()
            .to_lowercase()
    );

    let non_repo = std::env::temp_dir();
    assert!(backend.discover(&non_repo).is_none());
}

#[test]
fn status_reports_untracked_and_modified() {
    let t = init_repo("status");
    commit_files(&t, &[("tracked.txt", "one\n")], "init", None);
    std::fs::write(t.root.join("tracked.txt"), "one\ntwo\n").unwrap();
    std::fs::write(t.root.join("new.txt"), "fresh\n").unwrap();

    let backend = GixBackend::default();
    let status = backend.status(&t.root, &no_interrupt()).unwrap();
    let get = |name: &str| {
        let abs = backend.guard.sanitize(&t.root.join(name)).unwrap();
        status.by_path.get(&abs).copied()
    };
    assert_eq!(
        get("tracked.txt").and_then(|s| s.worktree),
        Some(GitStatusCode::Modified)
    );
    assert_eq!(
        get("new.txt").and_then(|s| s.worktree),
        Some(GitStatusCode::Untracked)
    );
}

#[test]
fn commits_and_paging_and_sanitization() {
    let t = init_repo("commits");
    let c1 = commit_files(&t, &[("a.txt", "1\n")], "first", None);
    let _c2 = commit_files(
        &t,
        &[("a.txt", "2\n")],
        "evil \u{1b}[31mred\u{202E}\u{0007} summary",
        Some(c1),
    );

    let backend = GixBackend::default();
    let page = backend.commits(&t.root, None, 10, &no_interrupt()).unwrap();
    assert_eq!(page.len(), 2);
    // Newest first; hostile bytes stripped (ESC removed, bidi/bell removed).
    assert_eq!(page[0].summary, "evil [31mred summary");
    assert_eq!(page[1].summary, "first");

    // Paging continues after the given id without repeating it.
    let next = backend
        .commits(&t.root, Some(&page[0].id), 10, &no_interrupt())
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].id, page[1].id);
}

#[test]
fn commit_detail_lists_changed_files() {
    let t = init_repo("detail");
    let c1 = commit_files(&t, &[("a.txt", "1\n")], "first", None);
    let c2 = commit_files(
        &t,
        &[("a.txt", "2\n"), ("b.txt", "new\n")],
        "second",
        Some(c1),
    );

    let backend = GixBackend::default();
    let detail = backend
        .commit_detail(&t.root, &c2.to_string(), &no_interrupt())
        .unwrap();
    assert_eq!(detail.info.summary, "second");
    let mut rels: Vec<_> = detail.changes.iter().map(|c| c.rel_path.as_str()).collect();
    rels.sort();
    assert_eq!(rels, vec!["a.txt", "b.txt"]);
    // Every change resolved to a guarded absolute path.
    assert!(detail.changes.iter().all(|c| c.abs_path.is_some()));
}

#[test]
fn file_history_only_lists_touching_commits() {
    let t = init_repo("history");
    let c1 = commit_files(&t, &[("a.txt", "1\n"), ("b.txt", "x\n")], "one", None);
    let c2 = commit_files(&t, &[("a.txt", "1\n"), ("b.txt", "y\n")], "two", Some(c1));
    let _c3 = commit_files(&t, &[("a.txt", "3\n"), ("b.txt", "y\n")], "three", Some(c2));

    let backend = GixBackend::default();
    let history = backend
        .file_history(&t.root, Path::new("a.txt"), &no_interrupt())
        .unwrap();
    let summaries: Vec<_> = history.iter().map(|c| c.summary.as_str()).collect();
    assert_eq!(summaries, vec!["three", "one"]);
}

#[test]
fn diff_commit_vs_parent_produces_hunks() {
    let t = init_repo("diff");
    let c1 = commit_files(&t, &[("a.txt", "line1\nline2\nline3\n")], "first", None);
    let c2 = commit_files(
        &t,
        &[("a.txt", "line1\nchanged\nline3\n")],
        "second",
        Some(c1),
    );

    let backend = GixBackend::default();
    let diff = backend
        .diff_file(
            &t.root,
            &DiffTarget::CommitVsParent {
                commit: c2.to_string(),
                rel_path: PathBuf::from("a.txt"),
            },
            &no_interrupt(),
        )
        .unwrap();
    assert_eq!(diff.hunks.len(), 1);
    let kinds: Vec<DiffLineKind> = diff.hunks[0].lines.iter().map(|(k, _)| *k).collect();
    assert!(kinds.contains(&DiffLineKind::Del));
    assert!(kinds.contains(&DiffLineKind::Add));
    let del = diff.hunks[0]
        .lines
        .iter()
        .find(|(k, _)| *k == DiffLineKind::Del)
        .unwrap();
    assert_eq!(del.1, "line2");
}

#[test]
fn diff_rejects_traversal_paths() {
    let t = init_repo("traversal");
    commit_files(&t, &[("a.txt", "1\n")], "init", None);
    let backend = GixBackend::default();
    let err = backend.diff_file(
        &t.root,
        &DiffTarget::WorktreeVsIndex {
            rel_path: PathBuf::from("..\\..\\outside.txt"),
        },
        &no_interrupt(),
    );
    assert!(err.is_err());
}

#[test]
fn branches_mark_head() {
    let t = init_repo("branches");
    let c1 = commit_files(&t, &[("a.txt", "1\n")], "init", None);
    // A second branch pointing at the same commit.
    set_ref(
        &t,
        "refs/heads/feature/extra",
        c1,
        gix::refs::transaction::PreviousValue::MustNotExist,
    );

    let backend = GixBackend::default();
    let branches = backend.branches(&t.root).unwrap();
    let locals: Vec<_> = branches.iter().filter(|b| !b.is_remote).collect();
    assert_eq!(locals.len(), 2);
    assert_eq!(locals.iter().filter(|b| b.is_head).count(), 1);
    assert!(locals.iter().any(|b| b.name == "feature/extra"));
}

#[test]
fn blob_at_caps_and_validates() {
    let t = init_repo("blob");
    let c1 = commit_files(&t, &[("a.txt", "0123456789\n")], "init", None);
    let backend = GixBackend::default();

    let (bytes, truncated) = backend
        .blob_at(&t.root, &c1.to_string(), Path::new("a.txt"), 4)
        .unwrap();
    assert_eq!(bytes, b"0123");
    assert!(truncated);

    // Non-hex "commit id" (a revspec) must be rejected outright.
    assert!(
        backend
            .blob_at(&t.root, "HEAD@{1}", Path::new("a.txt"), 100)
            .is_err()
    );
}

#[test]
fn stage_commit_unstage_cycle() {
    let t = init_repo("mutate");
    let c1 = commit_files(&t, &[("a.txt", "one\n")], "init", None);
    let backend = GixBackend::default();

    // Modify tracked + add new file, stage both.
    std::fs::write(t.root.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(t.root.join("b.txt"), "fresh\n").unwrap();
    backend
        .stage(&t.root, &[PathBuf::from("a.txt"), PathBuf::from("b.txt")])
        .unwrap();

    let status = backend.status(&t.root, &no_interrupt()).unwrap();
    let get = |name: &str| {
        let abs = backend.guard.sanitize(&t.root.join(name)).unwrap();
        status.by_path.get(&abs).copied().unwrap_or_default()
    };
    assert!(get("a.txt").index.is_some(), "a.txt should be staged");
    assert!(get("b.txt").index.is_some(), "b.txt should be staged");

    // Commit — identity comes from repo-local config we set here.
    {
        let config_path = t.root.join(".git").join("config");
        let mut config = std::fs::read_to_string(&config_path).unwrap_or_default();
        config.push_str("\n[user]\n\tname = Piku Test\n\temail = test@example.com\n");
        std::fs::write(&config_path, config).unwrap();
    }
    let new_id = backend.commit_create(&t.root, "second commit").unwrap();
    assert_ne!(new_id, c1.to_string());

    // Clean after commit.
    let status = backend.status(&t.root, &no_interrupt()).unwrap();
    assert!(
        status.by_path.is_empty(),
        "worktree should be clean, got {:?}",
        status.by_path
    );

    // Commit content is correct.
    let (bytes, _) = backend
        .blob_at(&t.root, &new_id, Path::new("b.txt"), 100)
        .unwrap();
    assert_eq!(bytes, b"fresh\n");

    // Modify + stage + unstage returns the index to HEAD's version.
    std::fs::write(t.root.join("a.txt"), "three\n").unwrap();
    backend.stage(&t.root, &[PathBuf::from("a.txt")]).unwrap();
    backend.unstage(&t.root, &[PathBuf::from("a.txt")]).unwrap();
    let status = backend.status(&t.root, &no_interrupt()).unwrap();
    let a = {
        let abs = backend.guard.sanitize(&t.root.join("a.txt")).unwrap();
        status.by_path.get(&abs).copied().unwrap_or_default()
    };
    assert!(a.index.is_none(), "a.txt should be unstaged");
    assert!(
        a.worktree.is_some(),
        "a.txt should still be modified in worktree"
    );
}

#[test]
fn commit_refuses_empty_message_and_missing_identity() {
    let t = init_repo("commitguards");
    commit_files(&t, &[("a.txt", "1\n")], "init", None);
    let backend = GixBackend::default();
    assert!(backend.commit_create(&t.root, "   ").is_err());
}

#[test]
fn branch_create_delete_and_guards() {
    let t = init_repo("branchops");
    commit_files(&t, &[("a.txt", "1\n")], "init", None);
    let backend = GixBackend::default();

    backend.create_branch(&t.root, "feature/x").unwrap();
    assert!(
        backend
            .branches(&t.root)
            .unwrap()
            .iter()
            .any(|b| b.name == "feature/x")
    );

    // Duplicate creation fails; hostile names rejected before gix runs.
    assert!(backend.create_branch(&t.root, "feature/x").is_err());
    assert!(backend.create_branch(&t.root, "evil..name").is_err());
    assert!(backend.create_branch(&t.root, "-flag").is_err());

    // Cannot delete the checked-out branch; can delete the other.
    let head = backend
        .branches(&t.root)
        .unwrap()
        .into_iter()
        .find(|b| b.is_head)
        .unwrap();
    assert!(backend.delete_branch(&t.root, &head.name).is_err());
    backend.delete_branch(&t.root, "feature/x").unwrap();
    assert!(
        !backend
            .branches(&t.root)
            .unwrap()
            .iter()
            .any(|b| b.name == "feature/x")
    );
}

#[test]
fn checkout_switches_branches_and_refuses_dirty() {
    let t = init_repo("checkout");
    let c1 = commit_files(&t, &[("a.txt", "base\n")], "init", None);
    let backend = GixBackend::default();

    // Second branch with different content for a.txt plus an extra file.
    let _c2 = commit_files(
        &t,
        &[("a.txt", "branched\n"), ("extra.txt", "only here\n")],
        "on-branch",
        Some(c1),
    );
    // That commit advanced HEAD's branch; create `other` there, then move
    // the default branch back to c1 to diverge them.
    backend.create_branch(&t.root, "other").unwrap();
    let head = backend
        .branches(&t.root)
        .unwrap()
        .into_iter()
        .find(|b| b.is_head)
        .unwrap();
    set_ref(
        &t,
        format!("refs/heads/{}", head.name).as_str(),
        c1,
        gix::refs::transaction::PreviousValue::Any,
    );
    // Reset worktree + index to c1 state manually (branch rewind above
    // moved the ref only).
    std::fs::write(t.root.join("a.txt"), "base\n").unwrap();
    let _ = std::fs::remove_file(t.root.join("extra.txt"));
    let c1_tree = t.repo.find_commit(c1).unwrap().tree_id().unwrap().detach();
    let mut ix = t.repo.index_from_tree(&c1_tree).unwrap();
    ix.write(Default::default()).unwrap();

    // Dirty worktree refuses checkout.
    std::fs::write(t.root.join("a.txt"), "dirty\n").unwrap();
    assert!(matches!(
        backend.checkout(&t.root, "other", &no_interrupt()),
        Err(super::super::backend::GitError::DirtyWorktree)
    ));
    std::fs::write(t.root.join("a.txt"), "base\n").unwrap();

    // Clean checkout lands the branch's files.
    backend.checkout(&t.root, "other", &no_interrupt()).unwrap();
    assert_eq!(
        std::fs::read_to_string(t.root.join("a.txt")).unwrap(),
        "branched\n"
    );
    assert_eq!(
        std::fs::read_to_string(t.root.join("extra.txt")).unwrap(),
        "only here\n"
    );
    let head_now = backend
        .branches(&t.root)
        .unwrap()
        .into_iter()
        .find(|b| b.is_head)
        .unwrap();
    assert_eq!(head_now.name, "other");

    // Switching back removes the branch-only file.
    backend
        .checkout(&t.root, &head.name, &no_interrupt())
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(t.root.join("a.txt")).unwrap(),
        "base\n"
    );
    assert!(!t.root.join("extra.txt").exists());
}

#[test]
fn fetch_rejects_non_https_remotes() {
    let t = init_repo("fetchscheme");
    commit_files(&t, &[("a.txt", "1\n")], "init", None);
    // A file:// remote (the classic local-exfiltration vector) must be
    // refused before any connection is attempted.
    {
        let config_path = t.root.join(".git").join("config");
        let mut config = std::fs::read_to_string(&config_path).unwrap_or_default();
        config.push_str(
            "\n[remote \"origin\"]\n\turl = file:///C:/somewhere/else\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        );
        std::fs::write(&config_path, config).unwrap();
    }
    let backend = GixBackend::default();
    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let err = backend
        .fetch(&t.root, "origin", &tx, &no_interrupt())
        .unwrap_err();
    assert!(
        err.to_string().contains("https"),
        "expected scheme rejection, got: {err}"
    );

    // Unknown remotes are refused too.
    assert!(
        backend
            .fetch(&t.root, "nonexistent", &tx, &no_interrupt())
            .is_err()
    );
}

#[test]
fn ignored_entries_are_collapsed_and_not_dirty() {
    let t = init_repo("ignored");
    commit_files(&t, &[(".gitignore", "build/\nsecret.key\n")], "init", None);
    // An ignored directory with several files, plus one ignored file.
    std::fs::create_dir_all(t.root.join("build")).unwrap();
    std::fs::write(t.root.join("build").join("x.o"), "obj").unwrap();
    std::fs::write(t.root.join("build").join("y.o"), "obj").unwrap();
    std::fs::write(t.root.join("secret.key"), "shh").unwrap();

    let backend = GixBackend::default();
    let status = backend.status(&t.root, &no_interrupt()).unwrap();
    let abs = |name: &str| backend.guard.sanitize(&t.root.join(name)).unwrap();

    // Collapsed: the directory is one ignored entry; its files are not.
    assert!(status.ignored.contains(&abs("build")));
    assert!(status.ignored.contains(&abs("secret.key")));
    // Ignored entries never appear in the dirty map.
    assert!(!status.by_path.contains_key(&abs("build")));
    assert!(!status.by_path.contains_key(&abs("secret.key")));
}

#[test]
fn checkout_and_delete_work_through_full_ref_names() {
    let t = init_repo("refname");
    let c1 = commit_files(&t, &[("a.txt", "1\n")], "init", None);
    // A branch with a non-ASCII name, created directly at the ref level —
    // the UI's create path would reject it, but external tools make these.
    set_ref(
        &t,
        "refs/heads/feature/naïve",
        c1,
        gix::refs::transaction::PreviousValue::MustNotExist,
    );

    let backend = GixBackend::default();
    let branches = backend.branches(&t.root).unwrap();
    let exotic = branches
        .iter()
        .find(|b| b.ref_name == "refs/heads/feature/naïve")
        .expect("exotic branch listed");
    assert!(!exotic.is_head);

    // Checkout + delete through the full ref name.
    backend
        .checkout(&t.root, &exotic.ref_name, &no_interrupt())
        .unwrap();
    let head = backend
        .branches(&t.root)
        .unwrap()
        .into_iter()
        .find(|b| b.is_head)
        .unwrap();
    assert_eq!(head.ref_name, "refs/heads/feature/naïve");

    // Traversal-shaped "ref names" are rejected outright.
    assert!(
        backend
            .checkout(&t.root, "refs/heads/../escape", &no_interrupt())
            .is_err()
    );
    assert!(
        backend
            .delete_branch(&t.root, "refs/heads/x\u{7}y")
            .is_err()
    );
}

#[test]
fn merge_in_progress_is_reported() {
    let t = init_repo("state");
    let c1 = commit_files(&t, &[("a.txt", "1\n")], "init", None);
    let backend = GixBackend::default();

    let snap = backend.snapshot(&t.root, &no_interrupt()).unwrap();
    assert_eq!(snap.in_progress, None);

    // Simulate a merge in progress the way git does: MERGE_HEAD appears.
    std::fs::write(t.root.join(".git").join("MERGE_HEAD"), format!("{c1}\n")).unwrap();
    let snap = backend.snapshot(&t.root, &no_interrupt()).unwrap();
    assert_eq!(snap.in_progress, Some("merging"));
}
