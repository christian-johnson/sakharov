//! The version-control view: opening it, moving around it, and building the
//! plan.  Every key here changes only the plan (and the picture); the
//! repository is written once, by `:vc-apply`, behind a confirmation and a
//! backup.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use crate::{
    app::{App, VCS_BUFFER},
    command::Command,
    conflict::{self, Choice, Side},
    mode::{Mode, PromptKind},
    popup::{KeyHint, ListItem, Popup},
    source::SourceId,
    stash::Stash,
    vcs::{
        self,
        apply::{self as vcs_apply, Outcome},
        derive,
        load::{self, git},
        plan::{ResetMode, Scope, Step},
        state::{Focus, VcsState},
        Entry, Mark, Target,
    },
    view::View,
};

/// What the user types to get somewhere a text command would work.
const ESCAPE_HATCH: &str = ":vc-close";

/// The buffer a commit message is written in, and its amend twin.
pub const COMMIT_MESSAGE: &str = "*commit message*";
pub const AMEND_MESSAGE: &str = "*commit message (amend)*";

/// Separates an action from its value in a picker's payload.
const SEP: char = '\x1f';

// ---------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------

/// An apply running off the UI thread: hooks may take their time.
pub struct VcsJob {
    rx: Receiver<Result<Box<Outcome>, String>>,
}

/// Collect a finished load or apply.  Returns true when the screen changed.
pub fn poll(app: &mut App) -> bool {
    let mut changed = false;
    if let Some(result) = app.jobs.vcs_load.as_ref().and_then(load::RepoLoad::poll) {
        let job = app.jobs.vcs_load.take().expect("just polled it");
        changed = true;
        match result {
            Ok(repo) => install(app, job.root, repo),
            Err(why) => {
                app.messages.show(format!("Could not read the repository: {why}"));
                if app.vcs.is_none() {
                    close(app);
                }
            }
        }
    }
    if let Some(result) = app.jobs.vcs.as_ref().and_then(|j| j.rx.try_recv().ok()) {
        app.jobs.vcs = None;
        changed = true;
        match result {
            Ok(outcome) => report(app, &outcome),
            Err(why) => app.messages.show(format!("Cannot apply: {why}")),
        }
    }
    changed
}

/// Anything in flight, for the status-line spinner.
pub fn busy(app: &App) -> bool {
    app.jobs.vcs_load.is_some() || app.jobs.vcs.is_some()
}

// ---------------------------------------------------------------------------
// Opening and closing
// ---------------------------------------------------------------------------

/// Open the view for the repository containing whatever is on screen.
pub fn open(app: &mut App) {
    let id = SourceId::virtual_named(VCS_BUFFER);
    if app.stashes.view_of(&id) == Some(View::Vcs) {
        enter(app);
        if let Some(Stash::Vcs(state)) = app.stashes.take(&id) {
            app.vcs = Some(*state);
        }
        return;
    }
    let hint = app
        .current_source_id()
        .and_then(|id| id.as_path().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let Some(root) = load::discover_root(&hint) else {
        app.messages.show("Not inside a git repository (`git init` to start one)");
        return;
    };
    enter(app);
    app.jobs.vcs_load = Some(load::start(root));
    app.messages.show("Reading the repository…  (? for the keys)");
}

/// `:conflicts` — the view, on the first conflicted file.
pub fn open_conflicts(app: &mut App) {
    open(app);
    app.vcs_want_conflict = true;
    land_on_conflict(app);
}

fn land_on_conflict(app: &mut App) {
    if !app.vcs_want_conflict {
        return;
    }
    let Some(state) = app.vcs.as_mut() else { return };
    app.vcs_want_conflict = false;
    let first = state.repo().files.iter().find(|(_, f)| f.index.is_conflict()).map(|(p, _)| p.clone());
    match first {
        Some(path) => {
            state.select(path);
            state.focus = Focus::Diff;
        }
        None => app.messages.show("Nothing is conflicted"),
    }
}

/// Hand the screen to the view, detaching the buffer behind it.
fn enter(app: &mut App) {
    super::teardown_current_buffer(app);
    app.vcs = None;
    app.buffer = crate::buffer::Buffer::new_empty();
    app.buffer.path = None;
    app.selection = crate::selection::Selection::point(0);
    let id = SourceId::virtual_named(VCS_BUFFER);
    app.open_buffers.retain(|stored| *stored != id);
    app.open_buffers.push(id);
    app.show_splash = false;
}

fn install(app: &mut App, root: PathBuf, repo: vcs::Repo) {
    match app.vcs.as_mut() {
        Some(state) => {
            let dropped = state.reload(repo);
            match dropped.as_slice() {
                [] => {}
                list => {
                    for (step, why) in list {
                        app.messages.show(format!("Dropped from the plan: {} — {why}", step.label()));
                    }
                    app.messages.show(format!(
                        "Re-read the repository; {} planned step{} no longer applied (see *Messages*)",
                        list.len(),
                        if list.len() == 1 { "" } else { "s" }
                    ));
                }
            }
        }
        None => {
            let mut state = VcsState::new(root, repo);
            if let Some(c) = state.conflicts.as_mut() {
                c.show_base = app.config.conflict.show_base;
            }
            app.vcs = Some(state);
        }
    }
    land_on_conflict(app);
}

/// Leave for whatever was open before.
pub fn close(app: &mut App) {
    if app.vcs.as_ref().is_some_and(|s| !s.plan.is_empty()) {
        app.messages.show("Left the view; the plan is kept until you come back (gV)");
    }
    let id = SourceId::virtual_named(VCS_BUFFER);
    if let Some(state) = app.vcs.take() {
        if !state.plan.is_empty() {
            app.stashes.put(id.clone(), Stash::Vcs(Box::new(state)));
        }
    }
    app.jobs.vcs_load = None;
    app.open_buffers.retain(|b| *b != id);
    match app.open_buffers.last().cloned() {
        Some(id) => super::open_path(app, &id.to_path()),
        None => super::buffers::switch_to_special_buffer(app, crate::app::SCRATCH_BUFFER),
    }
}

/// Re-read the repository, keeping the plan.
fn refresh(app: &mut App) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    app.jobs.vcs_load = Some(load::start(root));
}

/// Keep every cursor on screen.  Called once per frame.
pub fn update_scroll(app: &mut App) {
    let area = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: app.viewport_width as u16,
        height: app.viewport_height as u16,
    };
    if let Some(state) = app.vcs.as_mut() {
        state.update_scroll(area);
    }
}

/// True when the keys should go to the resolver.
pub fn in_conflict_panel(app: &App) -> bool {
    app.vcs.as_ref().is_some_and(|s| s.focus == Focus::Diff && s.showing_conflict())
}

// ---------------------------------------------------------------------------
// Command routing
// ---------------------------------------------------------------------------

/// Handle `cmd` in the view.  Returns true when it was ours.
pub fn handle(app: &mut App, cmd: &Command) -> bool {
    debug_assert_eq!(app.view(), View::Vcs);
    if app.vcs.is_none() {
        // Still loading: only leaving makes sense.
        if matches!(cmd, Command::VcsClose | Command::BufferClose) {
            close(app);
        }
        return true;
    }
    if in_conflict_panel(app) && conflict_command(app, cmd) {
        update_scroll(app);
        return true;
    }
    match cmd {
        Command::MoveDown => vertical(app, 1),
        Command::MoveUp => vertical(app, -1),
        Command::PageDown => page(app, true),
        Command::PageUp => page(app, false),
        Command::GotoFileStart => vertical(app, isize::MIN / 2),
        Command::GotoFileEnd => vertical(app, isize::MAX / 2),
        Command::MoveLeft => left(app),
        Command::MoveRight | Command::VcsEnter | Command::TableOpenCell => right(app),
        // Consumed and ignored: popups over the view (the key sheet) pass the
        // `Esc` that closes them through, and `h` already goes back.
        Command::EnterNormal => {}
        Command::VcsFocusNext => cycle_focus(app),
        Command::VcsNextFile => step_file(app, true),
        Command::VcsPrevFile => step_file(app, false),

        Command::VcsToggleStage => toggle_stage(app),
        Command::VcsDiscard => discard(app),
        Command::VcsStageAll => stage_all(app, true),
        Command::VcsUnstageAll => stage_all(app, false),
        Command::VcsCommit(message) => commit(app, message, false),
        Command::VcsAmend(message) => commit(app, message, true),

        Command::VcsBranchMenu => menu(app, 'b'),
        Command::VcsStashMenu => menu(app, 's'),
        Command::VcsLogMenu => menu(app, 'L'),
        Command::VcsSwitch => pick_branch(app, "switch", "switch to"),
        Command::VcsMerge => pick_branch(app, "merge", "merge into the current branch"),
        Command::VcsRebase => pick_branch(app, "rebase", "rebase the current branch onto"),
        Command::VcsDeleteBranch => pick_branch(app, "delete", "delete branch"),
        Command::VcsNewBranch(name) => match name.trim() {
            "" => ask(app, PromptKind::VcsBranch),
            name => plan(app, Step::CreateBranch { name: name.to_string() }),
        },
        Command::VcsRenameBranch(name) => match (name.trim(), current_branch(app)) {
            (_, None) => app.messages.show("HEAD is detached — there is no branch to rename"),
            ("", Some(_)) => ask(app, PromptKind::VcsRenameBranch),
            (to, Some(from)) => plan(app, Step::RenameBranch { from, to: to.to_string() }),
        },
        Command::VcsStashPush(message) => match message.trim() {
            "" => ask(app, PromptKind::VcsStash),
            m => plan(app, Step::StashPush { message: m.to_string() }),
        },
        Command::VcsStashPop => pick_stash(app, "pop"),
        Command::VcsStashApply => pick_stash(app, "apply"),
        Command::VcsStashDrop => pick_stash(app, "drop"),
        Command::VcsCherryPick => pick_commit(app, "pick", "cherry-pick"),
        Command::VcsRevert => pick_commit(app, "revert", "revert"),
        Command::VcsResetSoft => pick_commit(app, "soft", "reset (soft) to"),
        Command::VcsResetMixed => pick_commit(app, "mixed", "reset (mixed) to"),
        Command::VcsTag(name) => match (name.trim(), app.vcs.as_ref().and_then(|s| s.tag_target.clone())) {
            ("", _) => pick_commit(app, "tag", "tag"),
            (name, target) => {
                let at = target.unwrap_or_else(|| Target::Branch(current_branch(app).unwrap_or_default()));
                if let Some(s) = app.vcs.as_mut() {
                    s.tag_target = None;
                }
                plan(app, Step::Tag { name: name.to_string(), at });
            }
        },
        Command::VcsContinue => plan(app, Step::Continue),
        Command::VcsAbort => plan(app, Step::Abort),

        Command::VcsUndoEdit | Command::Undo => undo_step(app),
        Command::VcsRemoveStep => remove_step(app),
        Command::VcsReset => clear_plan(app),
        Command::VcsApply => confirm_apply(app),
        Command::VcsUndo => undo_apply(app),
        Command::VcsRefresh => {
            refresh(app);
            app.messages.show("Re-reading the repository…");
        }
        Command::VcsGitStatus => git_status(app),
        Command::VcsHelp | Command::ConflictHelp => {
            app.popup = Some(Popup::key_sheet("version control", key_sheet(&app.keymap), "nothing is written until g a · Esc closes"));
        }
        Command::VcsOpen => app.messages.show("Already in the version-control view"),
        Command::ConflictOpen => {
            app.vcs_want_conflict = true;
            land_on_conflict(app);
        }
        Command::VcsClose | Command::BufferClose => close(app),
        _ => {
            return match crate::view::refusal(cmd) {
                Some(why) => {
                    app.messages.show(why.message(cmd, ESCAPE_HATCH));
                    true
                }
                None => false,
            }
        }
    }
    update_scroll(app);
    true
}

fn ask(app: &mut App, kind: PromptKind) {
    app.command_buf.clear();
    app.mode = Mode::Prompt { kind };
}

/// Open a which-key strip for a prefix key.
fn menu(app: &mut App, prefix: char) {
    app.mode = Mode::Prefix(prefix);
    app.popup = Some(Popup::which_key(&prefix.to_string(), super::prefix_hints(app, prefix)));
}

/// Push a step and say what happened.
fn plan(app: &mut App, step: Step) {
    let Some(state) = app.vcs.as_mut() else { return };
    let label = step.label();
    match state.push(step) {
        Ok(()) => {
            let n = state.plan.len();
            app.messages.show(format!("Planned: {label}  ({n} step{} — g a applies)", if n == 1 { "" } else { "s" }));
        }
        Err(why) => app.messages.show(format!("Cannot {label}: {why}")),
    }
}

fn current_branch(app: &App) -> Option<String> {
    app.vcs.as_ref().and_then(|s| s.repo().head.branch.clone())
}

// ---------------------------------------------------------------------------
// Moving around
// ---------------------------------------------------------------------------

fn vertical(app: &mut App, delta: isize) {
    let Some(state) = app.vcs.as_mut() else { return };
    match state.focus {
        Focus::Tree => state.step_tree(delta),
        Focus::Diff => {
            state.step_change(delta);
        }
        Focus::Plan => state.step_plan(delta),
    }
}

fn page(app: &mut App, down: bool) {
    let half = (app.viewport_height / 2).max(1);
    let Some(state) = app.vcs.as_mut() else { return };
    match state.focus {
        Focus::Diff => {
            state.diff_scroll = if down { state.diff_scroll + half } else { state.diff_scroll.saturating_sub(half) };
            state.follow = false;
        }
        _ => {
            let delta = if down { half as isize } else { -(half as isize) };
            vertical(app, delta);
        }
    }
}

/// `h`: fold a directory, go up to the parent, or leave the diff.
fn left(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    match state.focus {
        Focus::Diff | Focus::Plan => state.focus = Focus::Tree,
        Focus::Tree => {
            let Some(row) = state.selected_row() else { return };
            if row.dir && !row.collapsed {
                state.toggle_fold(Some(false));
            } else if let Some((parent, _)) = row.path.rsplit_once('/') {
                state.select(parent.to_string());
            }
        }
    }
}

/// `l` / Enter: unfold a directory, or move into the diff.
fn right(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    match state.focus {
        Focus::Tree => match state.selected_row() {
            Some(row) if row.dir => {
                state.toggle_fold(Some(true));
            }
            Some(_) if state.showing_conflict() => {
                state.focus = Focus::Diff;
                // The inversion behind most conflicts resolved the wrong way
                // round: in a replay the side labelled with HEAD is not yours.
                let replay = state.conflicts.as_ref().is_some_and(|c| !c.set.operation.left_is_yours());
                if replay {
                    app.messages.show("In a replay your own work is on the RIGHT  (? for the keys)");
                }
            }
            Some(_) if state.diff.is_some() => state.focus = Focus::Diff,
            Some(_) => {}
            None => app.messages.show("Nothing changed — nothing to look at"),
        },
        Focus::Diff | Focus::Plan => {}
    }
}

fn cycle_focus(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    let has_main = state.diff.as_ref().is_some_and(|d| !d.changes.is_empty()) || state.showing_conflict();
    state.focus = match state.focus {
        Focus::Tree if has_main => Focus::Diff,
        Focus::Tree | Focus::Diff if !state.plan.is_empty() => Focus::Plan,
        _ => Focus::Tree,
    };
}

fn step_file(app: &mut App, forward: bool) {
    let Some(state) = app.vcs.as_mut() else { return };
    if !state.step_file(forward) {
        app.messages.show(if forward { "This is the last changed file" } else { "This is the first changed file" });
    }
}

// ---------------------------------------------------------------------------
// Working-tree steps
// ---------------------------------------------------------------------------

/// What the cursor is on, as `(path, scope)`.
fn target(state: &VcsState) -> Option<(String, Scope)> {
    let row = state.selected_row()?;
    if row.dir {
        return Some((row.path, Scope::Dir));
    }
    if state.focus == Focus::Diff {
        if let Some(info) = state.diff.as_ref().and_then(|d| d.changes.get(state.change)) {
            return Some((row.path, Scope::Change(info.change.key())));
        }
    }
    Some((row.path, Scope::File))
}

/// `Space`: stage what is under the cursor, or unstage it when it is
/// already wholly staged.
fn toggle_stage(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    let Some((path, scope)) = target(state) else {
        app.messages.show("Nothing changed — nothing to stage");
        return;
    };
    let staged = match &scope {
        Scope::Change(_) => state
            .diff
            .as_ref()
            .and_then(|d| d.changes.get(state.change))
            .is_some_and(|c| c.staged == vcs::diff::Staged::Yes),
        _ => state.selected_mark() == Some(Mark::Staged),
    };
    if state.selected_mark() == Some(Mark::Conflicted) && !matches!(scope, Scope::Dir) {
        app.messages.show("Conflicted — Enter on the file to resolve it");
        return;
    }
    let step = if staged { Step::Unstage { path, scope } } else { Step::Stage { path, scope } };
    plan(app, step);
}

fn discard(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    match target(state) {
        Some((path, scope)) => plan(app, Step::Discard { path, scope }),
        None => app.messages.show("Nothing changed — nothing to discard"),
    }
}

/// `a` / `A`: stage or unstage every changed path.
fn stage_all(app: &mut App, stage: bool) {
    let Some(state) = app.vcs.as_mut() else { return };
    let paths: Vec<String> = state
        .repo()
        .files
        .iter()
        .filter(|(_, f)| match state.store.mark(f) {
            Mark::Conflicted => false,
            Mark::Staged => !stage,
            Mark::Untracked | Mark::Unstaged => stage,
            Mark::Partly => true,
        })
        .map(|(p, _)| p.clone())
        .collect();
    if paths.is_empty() {
        app.messages.show(if stage { "Everything is already staged" } else { "Nothing is staged" });
        return;
    }
    let mut done = 0;
    for path in paths {
        let step = match stage {
            true => Step::Stage { path, scope: Scope::File },
            false => Step::Unstage { path, scope: Scope::File },
        };
        if state.push(step).is_ok() {
            done += 1;
        }
    }
    let verb = if stage { "stage" } else { "unstage" };
    app.messages.show(format!("Planned: {verb} {done} file{}  (g a applies)", if done == 1 { "" } else { "s" }));
}

// ---------------------------------------------------------------------------
// Committing
// ---------------------------------------------------------------------------

/// `c` / `C`: a message buffer, or straight to the plan when the message was
/// given on the command line.
fn commit(app: &mut App, message: &str, amend: bool) {
    if !message.trim().is_empty() {
        plan(app, Step::Commit { message: message.trim().to_string(), amend });
        return;
    }
    let Some(state) = app.vcs.as_ref() else { return };
    let (staged, _, _) = state.counts();
    if staged == 0 && !amend {
        app.messages.show("Nothing staged — Space stages the selection, a stages everything");
        return;
    }
    let repo = state.repo();
    let seed = match (amend, &repo.head.commit) {
        (true, Some(head)) => match head.pending_index().and_then(|i| repo.pending.get(i)) {
            Some(p) => p.message.clone(),
            None => git(&state.root, &["log", "-1", "--format=%B", head.as_str()]).unwrap_or_default().trim_end().to_string(),
        },
        _ => String::new(),
    };
    let staged_paths: Vec<String> = repo
        .files
        .iter()
        .filter(|(_, f)| matches!(state.store.mark(f), Mark::Staged | Mark::Partly))
        .map(|(p, _)| format!("#   {p}"))
        .collect();
    let text = format!(
        "{seed}\n\n# Write the message above; lines starting with # are left out.\n\
         # q adds the commit to the plan.  An empty message cancels.\n#\n# Staged:\n{}\n",
        staged_paths.join("\n")
    );
    let name = if amend { AMEND_MESSAGE } else { COMMIT_MESSAGE };
    app.special_buffer_ropes.insert(name.to_string(), ropey::Rope::from_str(&text));
    super::buffers::switch_to_special_buffer(app, name);
    app.messages.show("Write the message, then q to add the commit to the plan");
}

/// `q` in a commit-message buffer: back to the view, with the commit planned.
/// Returns false when that is not what is open.
pub(super) fn close_transient_buffer(app: &mut App) -> bool {
    let amend = match app.current_source_id() {
        Some(id) if id.label() == COMMIT_MESSAGE => false,
        Some(id) if id.label() == AMEND_MESSAGE => true,
        _ => return false,
    };
    let message: String = app
        .buffer
        .rope
        .to_string()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if let Some(id) = app.current_source_id() {
        app.special_buffer_ropes.remove(id.label());
    }
    open(app);
    if message.is_empty() {
        app.messages.show("Empty message — nothing was planned");
    } else {
        plan(app, Step::Commit { message, amend });
    }
    true
}

// ---------------------------------------------------------------------------
// Pickers
// ---------------------------------------------------------------------------

fn picker(app: &mut App, title: &str, items: Vec<ListItem>) {
    if items.is_empty() {
        app.messages.show(format!("Nothing to {title}"));
        return;
    }
    app.popup = Some(Popup::vcs_pick(title, items));
}

fn choice(action: &str, value: &str, label: String, detail: String) -> ListItem {
    ListItem::choice(label, detail, format!("{action}{SEP}{value}"))
}

fn pick_branch(app: &mut App, action: &str, title: &str) {
    let Some(state) = app.vcs.as_ref() else { return };
    let repo = state.repo();
    let current = repo.head.branch.clone();
    let items = repo
        .branches
        .iter()
        .filter(|(name, _)| Some(*name) != current.as_ref())
        .map(|(name, oid)| choice(action, name, name.clone(), oid.short().to_string()))
        .collect();
    picker(app, title, items);
}

fn pick_stash(app: &mut App, action: &str) {
    let Some(state) = app.vcs.as_ref() else { return };
    let items = state
        .repo()
        .stashes
        .iter()
        .enumerate()
        .map(|(i, s)| choice(action, &i.to_string(), format!("stash@{{{i}}}"), s.message.clone()))
        .collect();
    picker(app, &format!("{action} a stash"), items);
}

fn pick_commit(app: &mut App, action: &str, title: &str) {
    let Some(state) = app.vcs.as_ref() else { return };
    let Some(head) = state.snapshot.head.commit.clone() else {
        app.messages.show("There are no commits yet");
        return;
    };
    let log = git(&state.root, &["log", "-n", "300", "--format=%H\x1f%h\x1f%s\x1f%an\x1f%ct", head.as_str()]).unwrap_or_default();
    let now = vcs::now_secs();
    let items = log
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            let when = f.get(4)?.parse().unwrap_or(0);
            let detail = format!("{} · {}", f.get(3)?, vcs::relative_time(when, now));
            Some(choice(action, f[0], format!("{} {}", f.get(1)?, f.get(2)?), detail))
        })
        .collect();
    picker(app, title, items);
}

/// A picker row was confirmed.
pub fn picked(app: &mut App, payload: &str) {
    let Some((action, value)) = payload.split_once(SEP) else { return };
    let branch = || Target::Branch(value.to_string());
    let commit = || vcs::Oid::new(value);
    let index = value.parse().unwrap_or(0);
    let step = match action {
        "switch" => Step::Switch { branch: value.to_string() },
        "merge" => Step::Merge { from: branch() },
        "rebase" => Step::Rebase { onto: branch() },
        "delete" => Step::DeleteBranch { name: value.to_string() },
        "pop" => Step::StashPop { index },
        "apply" => Step::StashApply { index },
        "drop" => Step::StashDrop { index },
        "pick" => Step::CherryPick { commit: commit() },
        "revert" => Step::Revert { commit: commit() },
        "soft" => Step::Reset { to: Target::Commit(commit()), mode: ResetMode::Soft },
        "mixed" => Step::Reset { to: Target::Commit(commit()), mode: ResetMode::Mixed },
        "tag" => {
            if let Some(state) = app.vcs.as_mut() {
                state.tag_target = Some(Target::Commit(commit()));
            }
            ask(app, PromptKind::VcsTag);
            return;
        }
        _ => return,
    };
    plan(app, step);
}

/// A minibuffer prompt this view opened was answered.
pub fn prompted(app: &mut App, kind: PromptKind, text: &str) {
    let text = text.trim().to_string();
    let cmd = match kind {
        PromptKind::VcsBranch if !text.is_empty() => Command::VcsNewBranch(text),
        PromptKind::VcsRenameBranch if !text.is_empty() => Command::VcsRenameBranch(text),
        PromptKind::VcsTag if !text.is_empty() => Command::VcsTag(text),
        PromptKind::VcsStash => {
            plan(app, Step::StashPush { message: text });
            return;
        }
        _ => {
            if let Some(state) = app.vcs.as_mut() {
                state.tag_target = None;
            }
            return;
        }
    };
    handle(app, &cmd);
}

// ---------------------------------------------------------------------------
// The plan itself
// ---------------------------------------------------------------------------

fn undo_step(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    match state.pop() {
        Some(step) => app.messages.show(format!("Took back: {}", step.label())),
        None => app.messages.show("Nothing planned to take back"),
    }
}

fn remove_step(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    if state.focus != Focus::Plan {
        app.messages.show("Tab to the plan, then x removes the step under the cursor");
        return;
    }
    match state.remove_step() {
        Ok(step) => app.messages.show(format!("Removed: {}", step.label())),
        Err(why) => app.messages.show(format!("Cannot remove it: {why}")),
    }
}

fn clear_plan(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    let n = state.plan.len();
    if n == 0 {
        app.messages.show("Nothing planned");
        return;
    }
    state.clear_plan();
    state.focus = Focus::Tree;
    app.messages.show(format!("Discarded {n} planned step{}", if n == 1 { "" } else { "s" }));
}

/// The derived commands, or why there are none.
fn derive_ops(state: &VcsState) -> Result<Vec<derive::Op>, String> {
    if state.plan.is_empty() {
        return Err("nothing planned".to_string());
    }
    let ops = derive::derive(&state.store, &state.snapshot, state.plan.steps(), state.plan.states())?;
    if ops.is_empty() {
        return Err("the planned steps cancel out — nothing to do".to_string());
    }
    Ok(ops)
}

/// Show what would run, and ask.
fn confirm_apply(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    let checked = vcs_apply::preflight(&state.root, &state.snapshot).and_then(|()| derive_ops(state));
    match checked {
        Ok(ops) => app.popup = Some(Popup::vcs_plan(ops.iter().map(derive::Op::describe).collect())),
        Err(why) => app.messages.show(format!("Cannot apply: {why}")),
    }
}

/// Run the plan.  Called from the confirmation.
pub fn apply_confirmed(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    let ops = match derive_ops(state) {
        Ok(ops) => ops,
        Err(why) => {
            app.messages.show(format!("Cannot apply: {why}"));
            return;
        }
    };
    let (root, snapshot) = (state.root.clone(), state.snapshot.clone());
    let stamp = format!("{:010}", vcs::now_secs());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(vcs_apply::run(&root, &snapshot, &ops, &stamp).map(Box::new));
    });
    app.jobs.vcs = Some(VcsJob { rx });
    app.messages.show("Applying…");
}

fn report(app: &mut App, outcome: &Outcome) {
    // Spent either way: on success the repository now contains it, and on a
    // failure the rest of it was planned against a state that never happened.
    if let Some(state) = app.vcs.as_mut() {
        state.clear_plan();
    }
    match &outcome.failure {
        None => app.messages.show(format!(
            "Applied {} operation{}  (:vc-undo puts everything back)",
            outcome.total,
            if outcome.total == 1 { "" } else { "s" }
        )),
        Some(failure) => {
            let mut text = format!(
                "Stopped at step {} of {}:\n  {}\n\n{}",
                outcome.ran + 1,
                outcome.total,
                failure.op,
                failure.message
            );
            if !failure.conflicts.is_empty() {
                text.push_str(&format!("\n\nConflicted now: {}", failure.conflicts.join(", ")));
                app.vcs_want_conflict = true;
            }
            text.push_str("\n\n:vc-undo puts everything back the way it was before the apply.");
            app.popup = Some(Popup::git_output("apply stopped", &text));
            app.messages.show(format!("Apply stopped at step {} of {}", outcome.ran + 1, outcome.total));
        }
    }
    refresh(app);
}

fn undo_apply(app: &mut App) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    let Some(stamp) = vcs_apply::backups(&root).first().cloned() else {
        app.messages.show("Nothing to undo — no apply has been backed up");
        return;
    };
    match vcs_apply::undo(&root, &stamp) {
        Ok(()) => {
            app.messages.show("Put the repository back the way it was before the last apply");
            refresh(app);
        }
        Err(why) => app.messages.show(format!("Could not undo: {why}")),
    }
}

fn git_status(app: &mut App) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    match git(&root, &["status"]) {
        Ok(text) => app.popup = Some(Popup::git_output("git status", text.trim_end())),
        Err(why) => app.messages.show(why),
    }
}

// ---------------------------------------------------------------------------
// The conflict panel
// ---------------------------------------------------------------------------

/// Commands the resolver reinterprets.  Returns false for anything else,
/// which then falls through to the view's own handling.
fn conflict_command(app: &mut App, cmd: &Command) -> bool {
    match cmd {
        Command::MoveLeft | Command::MoveRight => {
            let stacked = app.vcs.as_ref().and_then(|s| s.conflicts.as_ref()).is_some_and(|c| {
                conflict::layout::arrangement(app.viewport_width as u16, c.show_base) == conflict::layout::Arrangement::Stacked
            });
            if stacked {
                return false;
            }
            choose_side(app, matches!(cmd, Command::MoveRight));
        }
        Command::MoveDown | Command::MoveUp => step_any(app, matches!(cmd, Command::MoveDown)),
        Command::ConflictNextHunk | Command::SearchNext => step_unanswered(app, true),
        Command::ConflictPrevHunk | Command::SearchPrev => step_unanswered(app, false),
        Command::ConflictTakeSide => {
            let Some(c) = conflicts(app) else { return true };
            let (nth, side) = (c.nth, c.side);
            let next = c.choice().toggle(side);
            if let Some(file) = c.current_mut() {
                file.choose(nth, next.clone());
            }
            report_choice(app, &next);
        }
        Command::ConflictTakeLeft => take_only(app, Side::Left, false),
        Command::ConflictTakeRight => take_only(app, Side::Right, false),
        Command::ConflictTakeLeftAll => take_only(app, Side::Left, true),
        Command::ConflictTakeRightAll => take_only(app, Side::Right, true),
        Command::ConflictEditHunk => edit_hunk(app),
        Command::ConflictToggleBase => toggle_base(app),
        Command::ConflictWriteFile | Command::VcsEnter | Command::TableOpenCell => resolve_file(app),
        Command::Undo | Command::VcsUndoEdit => {
            let Some(c) = conflicts(app) else { return true };
            let undone = c.current_mut().and_then(conflict::ConflictFile::undo);
            match undone {
                Some(nth) => {
                    c.focus(nth);
                    let choice = c.choice();
                    report_choice(app, &choice);
                }
                // Nothing chosen to take back: take back a planned step.
                None => undo_step(app),
            }
        }
        Command::YankSelection => {
            let yanked = conflicts(app).and_then(|c| c.current()).map(|f| (f.merged(), f.path.clone()));
            if let Some((text, path)) = yanked {
                crate::clipboard::write(&text);
                app.messages.show(format!("Yanked the resolved text of {path}"));
            }
        }
        _ => return false,
    }
    true
}

fn conflicts(app: &mut App) -> Option<&mut conflict::state::ConflictState> {
    app.vcs.as_mut()?.conflicts.as_mut()
}

fn choose_side(app: &mut App, right: bool) {
    if let Some(c) = conflicts(app) {
        c.side = if right { Side::Right } else { Side::Left };
    }
}

fn step_any(app: &mut App, forward: bool) {
    let Some(c) = conflicts(app) else { return };
    let n = c.current().map_or(0, conflict::ConflictFile::conflict_count);
    if n > 0 {
        c.nth = if forward { (c.nth + 1) % n } else { (c.nth + n - 1) % n };
    }
}

fn step_unanswered(app: &mut App, forward: bool) {
    let Some(c) = conflicts(app) else { return };
    if c.step(forward) {
        return;
    }
    let decided = c.current().is_some_and(conflict::ConflictFile::is_decided);
    app.messages.show(if decided {
        "Every conflict in this file is answered — Enter plans the resolution"
    } else {
        "No other conflict to go to"
    });
}

fn take_only(app: &mut App, side: Side, every: bool) {
    let Some(c) = conflicts(app) else { return };
    let nth = c.nth;
    let Some(file) = c.current_mut() else { return };
    if !every {
        file.choose(nth, Choice::only(side));
        report_choice(app, &Choice::only(side));
        return;
    }
    // Only the unanswered ones: overwriting deliberate answers would make this
    // a key nobody could risk pressing.
    let targets: Vec<usize> = (0..file.conflict_count()).filter(|i| !file.choices[*i].is_decided()).collect();
    let n = targets.len();
    for i in targets {
        file.choose(i, Choice::only(side));
    }
    let name = side_name(app, side);
    app.messages.show(format!(
        "Took {name} for {n} remaining conflict{}  (u takes them back one at a time)",
        if n == 1 { "" } else { "s" }
    ));
}

fn report_choice(app: &mut App, choice: &Choice) {
    let (left, right) = (side_name(app, Side::Left), side_name(app, Side::Right));
    let message = match choice {
        Choice::Edited(_) => "Using your own text for this conflict".to_string(),
        Choice::Sides { left: true, right: true } => format!("Keeping both — {left}, then {right}"),
        Choice::Sides { left: true, right: false } => format!("Taking {left}"),
        Choice::Sides { left: false, right: true } => format!("Taking {right}"),
        Choice::Sides { left: false, right: false } => "Taking neither — this section is deleted".to_string(),
    };
    let remaining = conflicts(app).and_then(|c| c.current()).map_or(0, conflict::ConflictFile::undecided_count);
    let tail = match remaining {
        0 => "  ·  all answered, Enter plans the resolution".to_string(),
        n => format!("  ·  {n} left"),
    };
    app.messages.show(format!("{message}{tail}"));
}

/// A side's name for the message line, from its label — never "ours".
fn side_name(app: &mut App, side: Side) -> String {
    let Some(c) = conflicts(app) else { return String::new() };
    match c.set.label(side).role.split(" — ").next().unwrap_or_default() {
        "" => format!("the {} side", if side == Side::Left { "left" } else { "right" }),
        role => role.to_string(),
    }
}

fn toggle_base(app: &mut App) {
    let mut config = app.config.clone();
    config.conflict.show_base = !config.conflict.show_base;
    let on = config.conflict.show_base;
    super::settings::replace_config(app, config);
    let has_base = conflicts(app).and_then(|c| c.current()).and_then(|f| f.base.as_ref()).is_some_and(|b| !b.is_empty());
    app.messages.show(match (on, has_base) {
        (true, false) => "This file has no common ancestor — it was added on both sides",
        (true, true) => "Showing the common ancestor",
        (false, _) => "Hiding the common ancestor",
    });
}

/// `e`: hand this section's text to the ordinary editor.
fn edit_hunk(app: &mut App) {
    let Some(c) = conflicts(app) else { return };
    let (file_idx, nth) = (c.file, c.nth);
    let choice = c.choice();
    let Some(file) = c.current() else { return };
    let Some(conflict::hunk::Region::Conflict(v)) = file.regions.iter().filter(|r| r.is_conflict()).nth(nth) else {
        app.messages.show("No conflict selected");
        return;
    };
    // Seeded with what is chosen, so `a` then `e` is "take this side and
    // adjust it".
    let seed = match &choice {
        Choice::Edited(text) => text.clone(),
        Choice::Sides { left: false, right: false } => format!("{}{}", v.left, v.right),
        c => format!(
            "{}{}",
            if c.takes(Side::Left) { v.left.as_str() } else { "" },
            if c.takes(Side::Right) { v.right.as_str() } else { "" }
        ),
    };
    let name = format!("{}{} {}*", crate::app::CONFLICT_EDIT_PREFIX, nth + 1, file.path);
    app.conflict_edit = Some((file_idx, nth));
    app.special_buffer_ropes.insert(name.clone(), ropey::Rope::from_str(&seed));
    super::buffers::switch_to_special_buffer(app, &name);
    app.messages.show("Edit this section, then q to take it back to the resolver");
}

/// `q` in a `*conflict …*` buffer: the text becomes that section's answer.
pub(super) fn close_edit_buffer(app: &mut App) -> bool {
    if !app.in_conflict_edit_buffer() {
        return false;
    }
    let mut text = app.buffer.rope.to_string();
    // Sections are whole lines; without its newline the edit would be glued
    // to the line after it.
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let target = app.conflict_edit.take();
    if let Some(id) = app.current_source_id() {
        app.special_buffer_ropes.remove(id.label());
    }
    open(app);
    let Some((file_idx, nth)) = target else { return true };
    let Some(c) = conflicts(app) else { return true };
    c.file = file_idx.min(c.set.files.len().saturating_sub(1));
    c.focus(nth);
    if let Some(file) = c.current_mut() {
        file.choose(nth, Choice::Edited(text));
    }
    let choice = c.choice();
    report_choice(app, &choice);
    true
}

/// `Enter`: plan this file's resolution.
fn resolve_file(app: &mut App) {
    let Some(c) = conflicts(app) else { return };
    let Some(file) = c.current() else { return };
    if !file.is_decided() {
        let n = file.undecided_count();
        app.messages.show(format!("{n} conflict{} still unanswered — n goes to the next one", if n == 1 { "" } else { "s" }));
        return;
    }
    let merged = file.merged();
    let path = file.path.clone();
    // Both sides switched off everywhere, with nothing agreed: the answer is
    // that the file should not exist.
    let deleted = merged.is_empty() && file.regions.iter().all(|r| r.is_conflict());
    let mode = app
        .vcs
        .as_ref()
        .and_then(|s| s.repo().files.get(&path))
        .and_then(|f| match &f.index {
            vcs::Index::Conflict(c) => c.left.as_ref().or(c.right.as_ref()).map(|e| e.mode),
            vcs::Index::Entry(_) => None,
        })
        .unwrap_or(vcs::MODE_FILE);
    let result = (!deleted).then(|| Entry::text(mode, merged.into_bytes()));
    plan(app, Step::Resolve { path, result });
    let Some(state) = app.vcs.as_mut() else { return };
    let next = state.repo().files.iter().find(|(_, f)| f.index.is_conflict()).map(|(p, _)| p.clone());
    match next {
        Some(path) => state.select(path),
        None => {
            state.focus = Focus::Tree;
            if state.repo().operation.is_some() {
                app.messages.show("That was the last conflict — g C plans continuing the operation");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The key sheet
// ---------------------------------------------------------------------------

/// Every key the view binds, grouped.  Generated from the keymap, so a
/// rebinding shows up here and nothing unbound is ever advertised.
pub fn key_sheet(keymap: &crate::keymap::Keymap) -> Vec<KeyHint> {
    use crate::keymap::Layer;
    use Command as C;
    let key = |cmds: &[Command], d: &str| KeyHint::for_commands(keymap, Layer::Vcs, cmds, d);
    let conflict = |cmds: &[Command], d: &str| KeyHint::for_commands(keymap, Layer::Conflict, cmds, d);
    vec![
        KeyHint::heading("MOVING AROUND"),
        key(&[C::MoveDown, C::MoveUp], "next / previous file, change or step"),
        key(&[C::MoveLeft, C::MoveRight], "fold / unfold a directory · into and out of the diff"),
        key(&[C::VcsFocusNext], "cycle the tree, the diff and the plan"),
        key(&[C::VcsNextFile, C::VcsPrevFile], "next / previous changed file"),
        key(&[C::PageDown, C::PageUp], "scroll half a screen"),
        KeyHint::heading("STAGING  (planned)"),
        key(&[C::VcsToggleStage], "stage / unstage the file, directory or change"),
        key(&[C::VcsDiscard], "discard it (backed up when applied)"),
        key(&[C::VcsStageAll, C::VcsUnstageAll], "stage / unstage everything"),
        key(&[C::VcsCommit(String::new())], "commit what is staged"),
        key(&[C::VcsAmend(String::new())], "amend the last commit"),
        KeyHint::heading("BRANCHES, STASH, HISTORY  (planned)"),
        key(&[C::VcsSwitch], "switch to a branch"),
        key(&[C::VcsNewBranch(String::new())], "new branch here, and switch to it"),
        key(&[C::VcsRenameBranch(String::new())], "rename the current branch"),
        key(&[C::VcsDeleteBranch], "delete a branch"),
        key(&[C::VcsMerge], "merge a branch into this one"),
        key(&[C::VcsRebase], "rebase this branch onto another"),
        key(&[C::VcsStashPush(String::new())], "stash the changes"),
        key(&[C::VcsStashPop, C::VcsStashApply, C::VcsStashDrop], "pop / apply / drop a stash"),
        key(&[C::VcsCherryPick, C::VcsRevert], "cherry-pick / revert a commit"),
        key(&[C::VcsResetSoft, C::VcsResetMixed], "reset soft / mixed to a commit"),
        key(&[C::VcsTag(String::new())], "tag a commit"),
        key(&[C::VcsContinue, C::VcsAbort], "continue / abort the operation in progress"),
        KeyHint::heading("CONFLICTS  (when the diff shows one)"),
        conflict(&[C::ConflictTakeSide], "take or drop the focused side"),
        conflict(&[C::ConflictTakeLeft, C::ConflictTakeRight], "take only the left / right side"),
        conflict(&[C::ConflictTakeLeftAll, C::ConflictTakeRightAll], "the same for every unanswered conflict in the file"),
        conflict(&[C::ConflictNextHunk, C::ConflictPrevHunk], "next / previous unanswered conflict"),
        conflict(&[C::ConflictEditHunk], "edit this section by hand"),
        conflict(&[C::ConflictToggleBase], "show the common ancestor"),
        conflict(&[C::ConflictWriteFile], "plan this file's resolution"),
        KeyHint::heading("THE PLAN"),
        key(&[C::VcsUndoEdit], "take back the last step"),
        key(&[C::VcsRemoveStep], "remove the step under the cursor (in the plan)"),
        key(&[C::VcsReset], "discard the whole plan"),
        key(&[C::VcsApply], "apply the plan — the only key that writes"),
        key(&[C::VcsUndo], "undo the last apply"),
        key(&[C::VcsRefresh], "re-read the repository, keeping the plan"),
        key(&[C::VcsGitStatus], "git status, verbatim"),
        key(&[C::VcsClose], "leave"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::testrepo::TestRepo;

    /// An app showing the view over a real repository.
    fn app_in(repo: &TestRepo) -> App {
        let mut app = App::new(None, crate::config::Config::load());
        app.viewport_height = 30;
        app.viewport_width = 140;
        let snap = load::read(repo.path()).unwrap();
        app.buffer = crate::buffer::Buffer::new_empty();
        app.vcs = Some(VcsState::new(repo.root.clone(), snap));
        app
    }

    fn repo() -> Option<TestRepo> {
        let repo = TestRepo::new()?;
        repo.commit("src/a.rs", "one\ntwo\nthree\nfour\nfive\n", "base");
        repo.write("src/a.rs", "one\nTWO\nthree\nFOUR\nfive\n");
        repo.write("notes.txt", "scratch\n");
        Some(repo)
    }

    fn run(app: &mut App, cmd: Command) {
        super::super::execute(app, &cmd);
    }

    fn state(app: &App) -> &VcsState {
        app.vcs.as_ref().unwrap()
    }

    #[test]
    fn the_view_owns_the_screen_and_has_no_buffer_behind_it() {
        let Some(repo) = repo() else { return };
        let app = app_in(&repo);
        assert_eq!(app.view(), View::Vcs);
        assert!(app.buffer.path.is_none());
    }

    /// The whole loop the view exists for: pick a change in the diff, stage it
    /// alone, commit, apply — and the repository ends up exactly so.
    #[test]
    fn staging_one_change_from_the_diff_and_committing_reaches_the_repository() {
        let Some(repo) = repo() else { return };
        let mut app = app_in(&repo);
        app.vcs.as_mut().unwrap().select("src/a.rs".into());
        run(&mut app, Command::MoveRight);
        assert_eq!(state(&app).focus, Focus::Diff);
        run(&mut app, Command::MoveDown);
        assert_eq!(state(&app).change, 1, "j walks to the second change");
        run(&mut app, Command::VcsToggleStage);
        assert_eq!(state(&app).plan.len(), 1);
        assert_eq!(state(&app).selected_mark(), Some(Mark::Partly));
        assert!(repo.git(&["diff", "--cached"]).unwrap().is_empty(), "nothing has touched the index yet");

        run(&mut app, Command::VcsCommit("Only the fourth line".into()));
        run(&mut app, Command::VcsApply);
        assert!(app.popup.is_some(), "apply asks first");
        app.popup = None;
        apply_confirmed(&mut app);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.jobs.vcs.is_some() && std::time::Instant::now() < deadline {
            poll(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(repo.git(&["show", "HEAD:src/a.rs"]).unwrap(), "one\ntwo\nthree\nFOUR\nfive\n");
        assert_eq!(repo.read("src/a.rs").unwrap(), "one\nTWO\nthree\nFOUR\nfive\n");
        assert_eq!(repo.git(&["log", "-1", "--format=%s"]).unwrap(), "Only the fourth line\n");
    }

    #[test]
    fn space_on_a_staged_file_unstages_it_and_u_takes_steps_back() {
        let Some(repo) = repo() else { return };
        let mut app = app_in(&repo);
        app.vcs.as_mut().unwrap().select("notes.txt".into());
        run(&mut app, Command::VcsToggleStage);
        assert_eq!(state(&app).selected_mark(), Some(Mark::Staged));
        run(&mut app, Command::VcsToggleStage);
        assert_eq!(state(&app).selected_mark(), Some(Mark::Untracked));
        assert_eq!(state(&app).plan.len(), 2);
        run(&mut app, Command::VcsUndoEdit);
        run(&mut app, Command::VcsUndoEdit);
        assert!(state(&app).plan.is_empty());
    }

    #[test]
    fn a_commit_with_nothing_staged_says_so_instead_of_opening_a_buffer() {
        let Some(repo) = repo() else { return };
        let mut app = app_in(&repo);
        run(&mut app, Command::VcsCommit(String::new()));
        assert_eq!(app.view(), View::Vcs);
        assert!(app.messages.current().unwrap_or_default().contains("Nothing staged"));
    }

    #[test]
    fn the_commit_buffer_plans_the_commit_when_it_is_closed() {
        let Some(repo) = repo() else { return };
        let mut app = app_in(&repo);
        run(&mut app, Command::VcsStageAll);
        run(&mut app, Command::VcsCommit(String::new()));
        assert!(app.in_commit_buffer(), "the message is written in a buffer");
        let text = format!("Write things\n{}", app.buffer.rope);
        app.buffer.rope = ropey::Rope::from_str(&text);
        run(&mut app, Command::BufferClose);
        assert_eq!(app.view(), View::Vcs);
        let steps = state(&app).plan.steps();
        assert!(matches!(steps.last(), Some(Step::Commit { message, .. }) if message == "Write things"), "{steps:?}");
    }

    #[test]
    fn a_hand_edited_section_keeps_its_line_ending() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("f.txt", "a\nb\nc\n", "base");
        repo.git(&["checkout", "-q", "-b", "feature"]).unwrap();
        repo.commit("f.txt", "a\nX\nc\n", "feature");
        repo.git(&["checkout", "-q", "main"]).unwrap();
        repo.commit("f.txt", "a\nY\nc\n", "main");
        let _ = repo.git(&["-c", "rerere.enabled=false", "merge", "feature"]);
        let mut app = app_in(&repo);
        app.vcs_want_conflict = true;
        land_on_conflict(&mut app);
        assert!(in_conflict_panel(&app));

        run(&mut app, Command::ConflictEditHunk);
        assert!(app.in_conflict_edit_buffer());
        app.buffer.rope = ropey::Rope::from_str("Z");
        run(&mut app, Command::BufferClose);
        let merged = state(&app).conflicts.as_ref().and_then(|c| c.current()).map(|f| f.merged());
        assert_eq!(merged.as_deref(), Some("a\nZ\nc\n"));
    }

    #[test]
    fn the_key_sheet_only_lists_keys_that_are_bound() {
        let keymap = crate::keymap::Keymap::default_bindings();
        for hint in key_sheet(&keymap) {
            assert!(!hint.key.contains(':'), "{} is not bound to a key", hint.description);
        }
    }

    #[test]
    fn text_commands_are_refused_with_a_reason() {
        let Some(repo) = repo() else { return };
        let mut app = app_in(&repo);
        run(&mut app, Command::EnterInsert);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.messages.current().is_some());
    }
}
