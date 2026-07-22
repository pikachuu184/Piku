//! Git mode for the inspector: repository dashboard (branch, sync state,
//! cleanliness), interactive commit timeline with expandable changed files,
//! and per-file history when a single file is selected. Read paths only —
//! every string shown here was sanitized at the backend boundary.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use gpui::AppContext as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::app::assets::PikuIcon;

use crate::core::entry::EntryKind;
use crate::services::git::COMMIT_PAGE;
use crate::services::git::types::{CommitDetail, CommitInfo, GitStatusCode};
use crate::state::PikuState;

use super::inspector_panel::InspectorPanel;

/// Everything the Git mode remembers between renders. Keyed to one repo
/// root; switching repositories resets the whole state.
#[derive(Default)]
pub(super) struct GitViewState {
    root: Option<PathBuf>,
    commits: Vec<CommitInfo>,
    commits_loading: bool,
    /// True once a page came back shorter than requested (end of history).
    commits_done: bool,
    /// Currently expanded commit and its lazily loaded detail.
    expanded: Option<String>,
    detail: Option<Arc<CommitDetail>>,
    detail_loading: bool,
    /// Per-file history for the selected file.
    file_hist_for: Option<PathBuf>,
    file_hist: Vec<CommitInfo>,
    file_hist_loading: bool,
    /// The one loaded diff (key describes what it compares, for both the
    /// section title and click-toggling).
    diff_key: Option<String>,
    diff: Option<Arc<crate::services::git::types::DiffPayload>>,
    diff_loading: bool,
    /// Staleness guard for all background git loads of this view.
    generation: u64,
    /// Derived staged/unstaged lists, rebuilt only when the status snapshot
    /// Arc changes (keyed on its pointer) instead of on every render.
    changes_key: Option<usize>,
    staged_rows: Vec<(PathBuf, GitStatusCode)>,
    unstaged_rows: Vec<(PathBuf, GitStatusCode)>,
    /// Lazily created multi-line commit message editor.
    commit_input: Option<gpui::Entity<gpui_component::input::InputState>>,
    /// True while a commit kicked from this panel is in flight — clears the
    /// message box once the mutation epoch advances without an error.
    committing: bool,
    /// Store epoch at the time caches were filled; a newer epoch (a
    /// successful mutation) invalidates the commit timeline.
    seen_epoch: u64,
    /// Last mutation error surfaced inline in this panel.
    error: Option<String>,
}

impl GitViewState {
    /// Reset when pointed at a different repository or after a mutation
    /// (commit/checkout/…) made the cached timeline stale. The commit-input
    /// entity survives so typed text is not lost on refreshes.
    fn retarget(&mut self, root: &PathBuf, epoch: u64) {
        if self.root.as_ref() != Some(root) || self.seen_epoch != epoch {
            *self = Self {
                root: Some(root.clone()),
                generation: self.generation + 1,
                seen_epoch: epoch,
                commit_input: self.commit_input.take(),
                committing: self.committing,
                error: self.error.take(),
                ..Self::default()
            };
        }
    }
}

fn relative_time(time: SystemTime, now: SystemTime) -> String {
    let Ok(elapsed) = now.duration_since(time) else {
        return "future".into();
    };
    let secs = elapsed.as_secs();
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=2_591_999 => format!("{}d ago", secs / 86_400),
        _ => format!("{}mo ago", secs / 2_592_000),
    }
}

impl InspectorPanel {
    /// Kick a commit-page load when none is resident and none is running.
    fn ensure_commits(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.git.commits_loading || self.git.commits_done || !self.git.commits.is_empty() {
            return;
        }
        self.load_commits(root, None, cx);
    }

    fn load_commits(&mut self, root: PathBuf, before: Option<String>, cx: &mut Context<Self>) {
        self.git.commits_loading = true;
        self.git.generation += 1;
        let generation = self.git.generation;
        let backend = PikuState::global(cx).git.read(cx).backend();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = cx.background_executor().spawn(async move {
            use crate::services::git::backend::GitBackend as _;
            backend.commits(&root, before.as_deref(), COMMIT_PAGE, &interrupt)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.git.generation != generation {
                    return;
                }
                this.git.commits_loading = false;
                if let Ok(mut page) = result {
                    if page.len() < COMMIT_PAGE {
                        this.git.commits_done = true;
                    }
                    this.git.commits.append(&mut page);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_commit(&mut self, root: PathBuf, id: String, cx: &mut Context<Self>) {
        if self.git.expanded.as_deref() == Some(id.as_str()) {
            self.git.expanded = None;
            self.git.detail = None;
            cx.notify();
            return;
        }
        self.git.expanded = Some(id.clone());
        self.git.detail = None;
        self.git.detail_loading = true;
        self.git.generation += 1;
        let generation = self.git.generation;
        let backend = PikuState::global(cx).git.read(cx).backend();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = cx.background_executor().spawn(async move {
            use crate::services::git::backend::GitBackend as _;
            backend.commit_detail(&root, &id, &interrupt)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.git.generation != generation {
                    return;
                }
                this.git.detail_loading = false;
                if let Ok(detail) = result {
                    this.git.detail = Some(Arc::new(detail));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Load (or toggle off) the diff described by `key`.
    fn load_diff(
        &mut self,
        root: PathBuf,
        target: crate::services::git::types::DiffTarget,
        key: String,
        cx: &mut Context<Self>,
    ) {
        if self.git.diff_key.as_deref() == Some(key.as_str()) {
            self.git.diff_key = None;
            self.git.diff = None;
            cx.notify();
            return;
        }
        self.git.diff_key = Some(key);
        self.git.diff = None;
        self.git.diff_loading = true;
        self.git.generation += 1;
        let generation = self.git.generation;
        let backend = PikuState::global(cx).git.read(cx).backend();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = cx.background_executor().spawn(async move {
            use crate::services::git::backend::GitBackend as _;
            backend.diff_file(&root, &target, &interrupt)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.git.generation != generation {
                    return;
                }
                this.git.diff_loading = false;
                match result {
                    Ok(payload) => this.git.diff = Some(Arc::new(payload)),
                    Err(err) => {
                        // Display-boundary re-sanitization (defense in depth).
                        let note = crate::security::git_text::sanitize_git_text(
                            &err.to_string(),
                            300,
                            false,
                        );
                        this.git.diff = Some(Arc::new(crate::services::git::types::DiffPayload {
                            old_label: String::new(),
                            new_label: String::new(),
                            hunks: Vec::new(),
                            truncated: false,
                            note: Some(note),
                        }));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Load the selected file's history once per (repo, file).
    fn ensure_file_history(&mut self, root: PathBuf, file: PathBuf, cx: &mut Context<Self>) {
        if self.git.file_hist_for.as_ref() == Some(&file) {
            return;
        }
        let Ok(rel) = file.strip_prefix(&root).map(|p| p.to_path_buf()) else {
            return;
        };
        self.git.file_hist_for = Some(file);
        self.git.file_hist = Vec::new();
        self.git.file_hist_loading = true;
        self.git.generation += 1;
        let generation = self.git.generation;
        let backend = PikuState::global(cx).git.read(cx).backend();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = cx.background_executor().spawn(async move {
            use crate::services::git::backend::GitBackend as _;
            backend.file_history(&root, &rel, &interrupt)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.git.generation != generation {
                    return;
                }
                this.git.file_hist_loading = false;
                if let Ok(history) = result {
                    this.git.file_hist = history;
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn status_letter(code: GitStatusCode, cx: &gpui::App) -> AnyElement {
    div()
        .w(px(14.))
        .flex_none()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .text_center()
        .child(code.glyph())
        .into_any_element()
}

fn section_label(text: &'static str, cx: &gpui::App) -> AnyElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

/// Section header with a small leading icon (same muted styling).
fn section_label_iconed(
    icon: impl Into<gpui_component::Icon>,
    text: &'static str,
    cx: &gpui::App,
) -> AnyElement {
    h_flex()
        .gap_1()
        .items_center()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(icon.into().size(px(12.)))
        .child(text)
        .into_any_element()
}

pub(super) fn render_git(
    this: &mut InspectorPanel,
    window: &mut Window,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let Some(root) = this.repo_root(cx) else {
        return div().into_any_element();
    };
    // One clock read per frame; every commit row's relative time shares it.
    let now = SystemTime::now();
    let epoch = PikuState::global(cx).git.read(cx).epoch();
    this.git.retarget(&root, epoch);
    // Surface any pending mutation error in this panel (the store clears it
    // on the next successful operation).
    if let Some(err) = PikuState::global(cx).git.read(cx).error() {
        this.git.error = Some(err.to_string());
    } else if this.git.committing {
        // Errors belonging to an in-flight commit were consumed; local
        // validation errors stay until the user acts again.
        this.git.error = None;
    }

    let state = PikuState::global(cx);
    let selection = state.selection.read(cx);
    let single_file = (selection.entries.len() == 1
        && selection.entries[0].kind == EntryKind::File)
        .then(|| selection.entries[0].clone());
    let git = state.git.read(cx);
    let Some(snap) = git.snapshot(&root).cloned() else {
        return div()
            .p_3()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("Reading repository…")
            .into_any_element();
    };
    let file_status = single_file
        .as_ref()
        .and_then(|f| git.status_of(selection.dir.as_deref()?, &f.path));

    // ---- Header: branch / detached, ahead/behind, remotes ------------------
    let branch_line = snap
        .branch
        .clone()
        .or_else(|| {
            snap.detached_short
                .clone()
                .map(|s| format!("detached @ {s}"))
        })
        .unwrap_or_else(|| "no commits yet".into());

    let mut header = v_flex()
        .gap_1()
        .p_3()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    Icon::new(PikuIcon::GitBranch)
                        .size(px(14.))
                        .text_color(cx.theme().foreground),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .truncate()
                        .child(branch_line),
                )
                .when_some(snap.in_progress, |row, state| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("({state})")),
                    )
                }),
        );
    if let (Some(ahead), Some(behind)) = (snap.ahead, snap.behind) {
        header = header.child(
            h_flex()
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(IconName::ArrowUp).size(px(11.)))
                .child(format!("{ahead} ahead"))
                .child(Icon::new(IconName::ArrowDown).size(px(11.)))
                .child(format!("{behind} behind")),
        );
    }
    let dirty = snap.dirty_total();
    let clean_line = if dirty == 0 {
        "working tree clean".to_string()
    } else {
        format!(
            "{} staged · {} changed · {} untracked{}{}",
            snap.staged,
            snap.unstaged,
            snap.untracked,
            if snap.conflicted > 0 {
                format!(" · {} conflicted", snap.conflicted)
            } else {
                String::new()
            },
            if snap.truncated { " (truncated)" } else { "" },
        )
    };
    header = header.child(
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(clean_line),
    );
    for remote in &snap.remotes {
        header = header.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .truncate()
                .child(format!(
                    "{} → {}{}",
                    remote.name,
                    remote.url,
                    if remote.fetchable {
                        ""
                    } else {
                        " (fetch disabled)"
                    }
                )),
        );
    }
    // Sync actions: Fetch runs as a background job in the status bar; Push
    // is shown disabled — the pure-Rust backend does not support it yet.
    let fetchable_remote = snap
        .remotes
        .iter()
        .find(|remote| remote.fetchable)
        .map(|remote| remote.name.clone());
    let fetching = PikuState::global(cx)
        .jobs
        .read(cx)
        .active_job()
        .is_some_and(|job| job.is_active() && job.kind == crate::services::jobs::JobKind::GitFetch);
    header = header.child(
        h_flex()
            .gap_1()
            .pt_1()
            .items_center()
            .when_some(fetchable_remote, |row, remote| {
                row.child(
                    div()
                        .id("git-fetch-btn")
                        .px_2()
                        .py_0p5()
                        .text_xs()
                        .rounded(cx.theme().radius)
                        .when(!fetching, |s| {
                            s.cursor_pointer()
                                .bg(cx.theme().list_active)
                                .text_color(cx.theme().foreground)
                                .hover(|s| s.bg(cx.theme().list_hover))
                        })
                        .when(fetching, |s| s.text_color(cx.theme().muted_foreground))
                        .child(if fetching { "Fetching…" } else { "Fetch" })
                        .on_click(cx.listener({
                            let root = root.clone();
                            move |_, _, window, cx| {
                                let root = root.clone();
                                let remote = remote.clone();
                                PikuState::global(cx).jobs.clone().update(cx, |jobs, cx| {
                                    jobs.submit_git_fetch(root, remote, window, cx);
                                });
                            }
                        })),
                )
            })
            .child({
                // Honest capability display: keyed off the backend so a
                // future push-capable backend lights this up automatically.
                use crate::services::git::backend::GitBackend as _;
                let push_ok = PikuState::global(cx)
                    .git
                    .read(cx)
                    .backend()
                    .push_supported();
                div()
                    .px_2()
                    .py_0p5()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if push_ok {
                        "Push"
                    } else {
                        "Push — not yet supported"
                    })
            }),
    );

    let mut body = v_flex().w_full().gap_3().p_3().child(header);

    // ---- Mutation error (inline, dismissed on the next successful op) ------
    if let Some(error) = this.git.error.clone() {
        body = body.child(
            h_flex()
                .p_2()
                .gap_1()
                .items_start()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size(px(13.))
                        .text_color(cx.theme().foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(cx.theme().foreground)
                        .child(error),
                ),
        );
    }

    // ---- Changes: staged / unstaged lists with stage & unstage actions -----
    let status_map = git.status_snapshot(&root);
    if let Some(status) = &status_map {
        /// Rows rendered per list before "… and N more" takes over.
        const CHANGE_ROWS_SHOWN: usize = 100;
        // Rebuild the derived lists only when the status snapshot itself was
        // replaced — the Arc pointer doubles as a cheap version key.
        let status_key = Arc::as_ptr(&status.by_path) as usize;
        if this.git.changes_key != Some(status_key) {
            this.git.changes_key = Some(status_key);
            this.git.staged_rows.clear();
            this.git.unstaged_rows.clear();
            for (abs, file_status) in status.by_path.iter() {
                let Ok(rel) = abs.strip_prefix(&root) else {
                    continue;
                };
                if let Some(code) = file_status.index {
                    this.git.staged_rows.push((rel.to_path_buf(), code));
                }
                if let Some(code) = file_status.worktree {
                    this.git.unstaged_rows.push((rel.to_path_buf(), code));
                }
            }
            this.git.staged_rows.sort();
            this.git.unstaged_rows.sort();
        }
        // Taken out for the duration of row building (listeners capture
        // clones; the vecs go back untouched afterwards).
        let staged = std::mem::take(&mut this.git.staged_rows);
        let unstaged = std::mem::take(&mut this.git.unstaged_rows);

        let change_list = |this: &mut InspectorPanel,
                           label: &'static str,
                           entries: &[(PathBuf, GitStatusCode)],
                           stage_action: bool,
                           cx: &mut Context<InspectorPanel>|
         -> Option<AnyElement> {
            let _ = this;
            if entries.is_empty() {
                return None;
            }
            let mut section = v_flex().gap_0p5().child(section_label(label, cx));
            for (ix, (rel, code)) in entries.iter().take(CHANGE_ROWS_SHOWN).enumerate() {
                let rel_for_click = rel.clone();
                let root_for_click = root.clone();
                let button_id = gpui::SharedString::from(format!("chg-{label}-{ix}"));
                // Clicking the row shows the change itself: staged rows
                // compare index ↔ HEAD, unstaged rows worktree ↔ index.
                let diff_rel = rel.clone();
                let diff_root = root.clone();
                let diff_key = format!(
                    "{}:{}",
                    if stage_action { "worktree" } else { "staged" },
                    rel.display()
                );
                section = section.child(
                    h_flex()
                        .id(gpui::SharedString::from(format!("chgrow-{label}-{ix}")))
                        .gap_1()
                        .items_center()
                        .rounded(cx.theme().radius)
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().list_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let target = if stage_action {
                                crate::services::git::types::DiffTarget::WorktreeVsIndex {
                                    rel_path: diff_rel.clone(),
                                }
                            } else {
                                crate::services::git::types::DiffTarget::IndexVsHead {
                                    rel_path: diff_rel.clone(),
                                }
                            };
                            this.load_diff(diff_root.clone(), target, diff_key.clone(), cx);
                        }))
                        .child(status_letter(*code, cx))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(cx.theme().foreground)
                                .truncate()
                                .child(rel.display().to_string()),
                        )
                        .child(
                            div()
                                .id(button_id)
                                .px_1p5()
                                .text_xs()
                                .flex_none()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .text_color(cx.theme().muted_foreground)
                                .hover(|s| {
                                    s.bg(cx.theme().list_hover)
                                        .text_color(cx.theme().foreground)
                                })
                                .child(
                                    Icon::new(if stage_action {
                                        IconName::Plus
                                    } else {
                                        IconName::Minus
                                    })
                                    .size(px(11.)),
                                )
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    let root = root_for_click.clone();
                                    let rels = vec![rel_for_click.clone()];
                                    PikuState::global(cx).git.clone().update(cx, |git, cx| {
                                        if stage_action {
                                            git.stage(root, rels, cx);
                                        } else {
                                            git.unstage(root, rels, cx);
                                        }
                                    });
                                })),
                        ),
                );
            }
            if entries.len() > CHANGE_ROWS_SHOWN {
                section = section.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("… and {} more", entries.len() - CHANGE_ROWS_SHOWN)),
                );
            }
            Some(section.into_any_element())
        };

        if let Some(section) = change_list(this, "Staged", &staged, false, cx) {
            body = body.child(section);
        }
        if let Some(section) = change_list(this, "Changes", &unstaged, true, cx) {
            body = body.child(section);
        }

        // ---- Commit box ----------------------------------------------------
        if !staged.is_empty() || this.git.committing {
            let input = this
                .git
                .commit_input
                .get_or_insert_with(|| {
                    cx.new(|cx| {
                        gpui_component::input::InputState::new(window, cx)
                            .multi_line(true)
                            .auto_grow(2, 6)
                            .placeholder("Commit message…")
                    })
                })
                .clone();
            // A finished successful commit clears the box.
            if this.git.committing && this.git.error.is_none() && staged.is_empty() {
                this.git.committing = false;
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            let can_commit = !staged.is_empty() && !this.git.committing;
            body = body.child(
                v_flex()
                    .gap_1()
                    .child(section_label_iconed(PikuIcon::GitCommit, "Commit", cx))
                    .child(gpui_component::input::Input::new(&input).small())
                    .child(
                        div()
                            .id("git-commit-btn")
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_center()
                            .rounded(cx.theme().radius)
                            .when(can_commit, |s| {
                                s.cursor_pointer()
                                    .bg(cx.theme().list_active)
                                    .text_color(cx.theme().foreground)
                                    .hover(|s| s.bg(cx.theme().list_hover))
                            })
                            .when(!can_commit, |s| s.text_color(cx.theme().muted_foreground))
                            .child(if this.git.committing {
                                "Committing…"
                            } else {
                                "Commit staged changes"
                            })
                            .on_click(cx.listener({
                                let root = root.clone();
                                let input = input.clone();
                                move |this, _, _, cx| {
                                    if this.git.committing {
                                        return;
                                    }
                                    let message: String = input
                                        .read(cx)
                                        .value()
                                        .chars()
                                        .take(crate::services::git::MAX_BODY_CHARS)
                                        .collect();
                                    if message.trim().is_empty() {
                                        this.git.error = Some("commit message is empty".into());
                                        cx.notify();
                                        return;
                                    }
                                    this.git.committing = true;
                                    this.git.error = None;
                                    PikuState::global(cx).git.clone().update(cx, |git, cx| {
                                        git.commit(root.clone(), message, cx)
                                    });
                                    cx.notify();
                                }
                            })),
                    ),
            );
        }
        // Put the derived lists back for the next frame's cache hit.
        this.git.staged_rows = staged;
        this.git.unstaged_rows = unstaged;
    }

    // ---- Selected file: status + history -----------------------------------
    if let Some(file) = single_file {
        let name = file.name.clone();
        let file_rel = file.path.strip_prefix(&root).ok().map(|p| p.to_path_buf());
        this.ensure_file_history(root.clone(), file.path.clone(), cx);

        let mut section = v_flex()
            .gap_1()
            .child(section_label_iconed(PikuIcon::Clock, "File history", cx))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .truncate()
                            .child(name),
                    )
                    .when_some(file_status.and_then(|s| s.primary()), |row, code| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("({})", code.label())),
                        )
                    }),
            );
        // Uncommitted changes: one click compares worktree ↔ index.
        if let (Some(rel), Some(status)) = (file_rel.clone(), file_status)
            && status.worktree.is_some()
        {
            let key = format!("worktree:{}", rel.display());
            let active = this.git.diff_key.as_deref() == Some(key.as_str());
            section = section.child(
                div()
                    .id("git-diff-worktree")
                    .px_2()
                    .py_0p5()
                    .text_xs()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .when(active, |s| s.bg(cx.theme().list_active))
                    .text_color(cx.theme().muted_foreground)
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .child("View uncommitted changes")
                    .on_click(cx.listener({
                        let root = root.clone();
                        move |this, _, _, cx| {
                            this.load_diff(
                                root.clone(),
                                crate::services::git::types::DiffTarget::WorktreeVsIndex {
                                    rel_path: rel.clone(),
                                },
                                key.clone(),
                                cx,
                            );
                        }
                    })),
            );
        }
        if this.git.file_hist_loading {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Loading history…"),
            );
        } else if this.git.file_hist.is_empty() {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("No committed history for this file"),
            );
        } else {
            // Taken out to avoid cloning the whole history per frame; the
            // rows only borrow each entry.
            let history = std::mem::take(&mut this.git.file_hist);
            for commit in &history {
                section = section.child(commit_row(this, &root, commit, file_rel.clone(), now, cx));
            }
            this.git.file_hist = history;
        }
        body = body.child(section);
    }

    // ---- Commit timeline ----------------------------------------------------
    this.ensure_commits(root.clone(), cx);
    let mut timeline = v_flex().gap_1().child(section_label_iconed(
        PikuIcon::GitCommit,
        "Recent commits",
        cx,
    ));
    if this.git.commits.is_empty() && this.git.commits_loading {
        timeline = timeline.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Loading commits…"),
        );
    } else if this.git.commits.is_empty() {
        timeline = timeline.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("No commits yet"),
        );
    }
    let commits = std::mem::take(&mut this.git.commits);
    for commit in &commits {
        timeline = timeline.child(commit_row(this, &root, commit, None, now, cx));
    }
    this.git.commits = commits;
    if !this.git.commits.is_empty() && !this.git.commits_done {
        let last_id = this.git.commits.last().map(|c| c.id.clone());
        timeline = timeline.child(
            div()
                .id("git-load-more")
                .px_2()
                .py_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .rounded(cx.theme().radius)
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().list_hover))
                .child(if this.git.commits_loading {
                    "Loading…"
                } else {
                    "Load more"
                })
                .on_click(cx.listener({
                    let root = root.clone();
                    move |this, _, _, cx| {
                        if !this.git.commits_loading
                            && let Some(before) = last_id.clone()
                        {
                            this.load_commits(root.clone(), Some(before), cx);
                        }
                    }
                })),
        );
    }
    body = body.child(timeline);

    // ---- Loaded diff --------------------------------------------------------
    if this.git.diff_key.is_some() {
        let mut section =
            v_flex()
                .gap_1()
                .child(section_label_iconed(PikuIcon::FileCode, "Diff", cx));
        if this.git.diff_loading {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Computing diff…"),
            );
        } else if let Some(payload) = this.git.diff.clone() {
            section = section.child(super::preview_view::diff_block(&payload, cx));
        }
        body = body.child(section);
    }

    body.into_any_element()
}

/// One commit row. In timeline mode (`file_rel` is `None`) it expands to the
/// commit's changed files; in file-history mode a click loads that file's
/// diff against the commit's parent.
fn commit_row(
    this: &mut InspectorPanel,
    root: &PathBuf,
    commit: &CommitInfo,
    file_rel: Option<PathBuf>,
    now: SystemTime,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let compact = file_rel.is_some();
    let expanded = !compact && this.git.expanded.as_deref() == Some(commit.id.as_str());
    let id_for_click = commit.id.clone();
    let root_for_click = root.clone();

    let mut meta = format!(
        "{} · {} · {}",
        commit.short,
        commit.author,
        relative_time(commit.time, now)
    );
    if !commit.refs.is_empty() {
        meta.push_str(&format!(" · [{}]", commit.refs.join(", ")));
    }

    let mut row = v_flex()
        .id(gpui::SharedString::from(format!("commit-{}", commit.id)))
        .px_2()
        .py_1()
        .gap_0p5()
        .rounded(cx.theme().radius)
        .cursor_pointer()
        .when(expanded, |s| s.bg(cx.theme().list_active))
        .when(!expanded, |s| s.hover(|s| s.bg(cx.theme().list_hover)))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().foreground)
                .truncate()
                .child(commit.summary.clone()),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .truncate()
                .child(meta),
        );

    if let Some(rel) = file_rel {
        // File-history mode: show what this commit did to the file.
        let key = format!("commit:{}:{}", commit.id, rel.display());
        row = row.on_click(cx.listener(move |this, _, _, cx| {
            this.load_diff(
                root_for_click.clone(),
                crate::services::git::types::DiffTarget::CommitVsParent {
                    commit: id_for_click.clone(),
                    rel_path: rel.clone(),
                },
                key.clone(),
                cx,
            );
        }));
    } else {
        row = row.on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_commit(root_for_click.clone(), id_for_click.clone(), cx);
        }));
    }

    if expanded {
        if this.git.detail_loading {
            row = row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Loading changes…"),
            );
        } else if let Some(detail) = this.git.detail.clone() {
            /// Changed-file rows rendered per expanded commit — the element
            /// tree stays bounded even for huge commits (detail itself is
            /// already capped harder at the backend).
            const DETAIL_ROWS_SHOWN: usize = 200;
            if !detail.body.is_empty() {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail.body.clone()),
                );
            }
            for (change_ix, change) in detail.changes.iter().take(DETAIL_ROWS_SHOWN).enumerate() {
                // Clicking a changed file shows its diff against the parent
                // commit; stop propagation so the commit row doesn't collapse.
                let commit_for_diff = commit.id.clone();
                let root_for_diff = root.clone();
                let rel_for_diff = PathBuf::from(&change.rel_path);
                let key = format!("commit:{}:{}", commit.id, change.rel_path);
                row = row.child(
                    h_flex()
                        .id(gpui::SharedString::from(format!(
                            "cmt-chg-{}-{change_ix}",
                            commit.short
                        )))
                        .gap_1()
                        .items_center()
                        .rounded(cx.theme().radius)
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().list_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.load_diff(
                                root_for_diff.clone(),
                                crate::services::git::types::DiffTarget::CommitVsParent {
                                    commit: commit_for_diff.clone(),
                                    rel_path: rel_for_diff.clone(),
                                },
                                key.clone(),
                                cx,
                            );
                        }))
                        .child(status_letter(change.code, cx))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().foreground)
                                .truncate()
                                .child(change.rel_path.clone()),
                        ),
                );
            }
            let hidden = detail.changes.len().saturating_sub(DETAIL_ROWS_SHOWN);
            if detail.truncated || hidden > 0 {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(if hidden > 0 {
                            format!("… and {hidden} more changed files")
                        } else {
                            "… change list truncated".to_string()
                        }),
                );
            }
        }
    }
    row.into_any_element()
}
