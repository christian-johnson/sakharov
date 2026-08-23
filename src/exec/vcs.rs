//! The version-control view: opening it, moving around it, and the two very
//! different kinds of action it offers.
//!
//! ## Two kinds of action
//!
//! **Planned.** Anything that rewrites history — moving an arrow, repointing a
//! branch, dropping a commit, planning a merge — only changes the picture.  It
//! accumulates on `VcsState::plan` and reaches the repository exactly once, in
//! [`apply`], behind a confirmation and a backup.
//!
//! **Immediate.** Checkout, stage, unstage, commit, fetch, pull, push.  These
//! add to the repository or move the work tree; none of them can lose a commit,
//! and pretending they need staging through a plan would make the everyday
//! cases ceremonious for no safety gained.
//!
//! The line between them is "could this make a commit unreachable".  That is
//! also exactly the line the backup refs exist to cover.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use crate::{
    app::{App, VCS_BUFFER},
    command::Command,
    popup::Popup,
    source::SourceId,
    stash::Stash,
    vcs::{
        self,
        apply::{self as vcs_apply, Outcome},
        derive,
        layout::Dir,
        load::{self, git, RepoLoad},
        plan::Edit,
        state::VcsState,
        Oid, RefKind,
    },
    view::View,
};

/// How many commits the graph walks back.  Generous enough that a normal
/// week's work is all on screen, bounded so opening the view on the kernel
/// tree is not a minute of `git log`.
const MAX_COMMITS: usize = 400;

/// What the user types to get somewhere a text command would work.
const ESCAPE_HATCH: &str = ":vc-close";

// ---------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------

/// What a finished background job produced.
pub enum JobDone {
    /// A plain report for the message line.
    Message(String),
    /// A plan run to completion (or to its first conflict).
    Applied(Box<Outcome>),
}

/// A git invocation running off the UI thread.
///
/// Fetch, pull and push talk to a network; a replay can run a hook per commit.
/// None of that may block a frame, so all of it goes through here and is
/// collected by [`poll`] in the run loop, the same shape as the table load and
/// the Quarto export.
pub struct VcsJob {
    pub label: String,
    rx: Receiver<Result<JobDone, String>>,
}

fn spawn<F>(app: &mut App, label: &str, work: F)
where
    F: FnOnce() -> Result<JobDone, String> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    app.vcs_job = Some(VcsJob { label: label.to_string(), rx });
    app.messages.show(format!("{label}…"));
}

/// Collect a finished load or job.  Returns true when the screen changed.
pub fn poll(app: &mut App) -> bool {
    let mut changed = false;

    if let Some(result) = app.vcs_pending.as_ref().and_then(RepoLoad::poll) {
        let load = app.vcs_pending.take().expect("just polled it");
        changed = true;
        match result {
            Ok(dag) => install(app, load.root, dag),
            Err(why) => {
                app.messages.show(format!("Could not read the repository: {why}"));
                super::buffers::switch_to_special_buffer(app, crate::app::SCRATCH_BUFFER);
            }
        }
    }

    if let Some(result) = app.vcs_job.as_ref().and_then(|j| j.rx.try_recv().ok()) {
        let job = app.vcs_job.take().expect("just polled it");
        changed = true;
        match result {
            Ok(JobDone::Message(text)) => {
                app.messages.show(text);
                refresh(app);
            }
            Ok(JobDone::Applied(outcome)) => {
                report_outcome(app, &outcome);
                refresh(app);
            }
            Err(why) => app.messages.show(format!("{}: {why}", job.label)),
        }
    }
    changed
}

/// Anything in flight, for the status-line spinner.
pub fn busy(app: &App) -> bool {
    app.vcs_pending.is_some() || app.vcs_job.is_some()
}

// ---------------------------------------------------------------------------
// Opening and closing
// ---------------------------------------------------------------------------

/// Open the graph for the repository containing whatever is on screen.
pub fn open(app: &mut App) {
    let hint = app
        .current_source_id()
        .and_then(|id| id.as_path().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let Some(root) = load::discover_root(&hint) else {
        app.messages
            .show("Not inside a git repository (`git init` to start one)");
        return;
    };

    enter(app);
    // A session stashed on the way out comes back whole — same cursor, same
    // plan.  Losing a half-built plan to a buffer switch would make the view
    // unusable for anything that takes more than one gesture.
    let id = SourceId::virtual_named(VCS_BUFFER);
    if app.stashes.view_of(&id) == Some(View::Vcs) {
        if let Some(Stash::Vcs(state)) = app.stashes.take(&id) {
            app.vcs = Some(*state);
            return;
        }
    }
    app.vcs_pending = Some(load::start(root, MAX_COMMITS));
    app.messages.show("Reading the repository…");
}

/// Hand the screen to the graph, detaching the buffer behind it.
fn enter(app: &mut App) {
    super::teardown_current_buffer(app);
    app.vcs = None;
    // Detached, path-less: nothing in the editor may hold a writable handle on
    // a file while the graph is what is on screen (`view::refusal` classifies
    // the write commands on top of this).
    app.buffer = crate::buffer::Buffer::new_empty();
    app.buffer.path = None;
    app.selection = crate::selection::Selection::point(0);
    let id = SourceId::virtual_named(VCS_BUFFER);
    app.open_buffers.retain(|stored| *stored != id);
    app.open_buffers.push(id);
    app.show_splash = false;
}

fn install(app: &mut App, root: PathBuf, dag: vcs::Dag) {
    let now = vcs::now_secs();
    match app.vcs.as_mut() {
        Some(state) => state.reload(dag, now),
        None => app.vcs = Some(VcsState::new(root, dag, now)),
    }
}

/// Leave the graph for whatever was open before it.
pub fn close(app: &mut App) {
    app.vcs = None;
    app.vcs_pending = None;
    app.stashes.discard(&SourceId::virtual_named(VCS_BUFFER));
    app.open_buffers
        .retain(|id| *id != SourceId::virtual_named(VCS_BUFFER));
    match app.open_buffers.last().cloned() {
        Some(id) => super::open_path(app, &id.to_path()),
        None => super::buffers::switch_to_special_buffer(app, crate::app::SCRATCH_BUFFER),
    }
}

/// Re-read the repository, discarding the plan.
fn refresh(app: &mut App) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else {
        return;
    };
    app.vcs_pending = Some(load::start(root, MAX_COMMITS));
}

// ---------------------------------------------------------------------------
// Scroll
// ---------------------------------------------------------------------------

/// Keep the cursor on screen, in columns and in tracks.
pub fn update_scroll(app: &mut App) {
    let (height, width) = (app.viewport_height as u16, app.viewport_width as u16);
    let Some(state) = app.vcs.as_ref() else { return };
    let Some(focus) = state.focus.clone() else { return };
    let layout = state.layout(width);
    let Some(at) = layout.locate(&focus) else { return };
    let (row, col) = (at.row, at.col);
    let (block_width, total_cols) = (layout.block_width, layout.total_cols);
    let (track_count, visible_tracks) = (layout.track_count, layout.visible_tracks(height));
    let track = (row / (vcs::layout::BLOCK_H + vcs::layout::TRACK_GAP)) as usize;
    let Some(state) = app.vcs.as_mut() else { return };

    // Columns move freely: the time axis is the one you travel along, so a
    // block clipped at the edge is the price of the cursor tracking smoothly.
    // What must never happen is the focused block being *partly* off screen,
    // so the window is nudged by whole blocks' worth when it is.
    if width > 0 {
        if col < state.scroll_col {
            state.scroll_col = col;
        } else if col + block_width > state.scroll_col + width {
            state.scroll_col = (col + block_width).saturating_sub(width);
        }
        state.scroll_col = state.scroll_col.min(total_cols.saturating_sub(width / 2));
    }

    // Tracks scroll a whole branch row at a time: half a block above the top
    // edge is unreadable, so there is nothing to be gained by finer steps.
    if height > 0 {
        if track < state.scroll_track {
            state.scroll_track = track;
        } else if track >= state.scroll_track + visible_tracks {
            state.scroll_track = track + 1 - visible_tracks;
        }
        state.scroll_track = state
            .scroll_track
            .min(track_count.saturating_sub(visible_tracks));
    }
}

// ---------------------------------------------------------------------------
// Command routing
// ---------------------------------------------------------------------------

/// Handle `cmd` in the graph view.  Returns true when it was ours.
pub fn handle(app: &mut App, cmd: &Command) -> bool {
    debug_assert_eq!(app.view(), View::Vcs);
    let width = app.viewport_width as u16;

    // Motion first: the ordinary motions are reinterpreted against the graph,
    // exactly as the grid reinterprets them against cells.
    let dir = match cmd {
        Command::MoveLeft | Command::MoveWordBackward | Command::MoveBigWordBackward => Some(Dir::Left),
        Command::MoveRight | Command::MoveWordForward | Command::MoveBigWordForward => Some(Dir::Right),
        Command::MoveUp => Some(Dir::Up),
        Command::MoveDown => Some(Dir::Down),
        _ => None,
    };
    if let Some(dir) = dir {
        if let Some(state) = app.vcs.as_mut() {
            state.step(dir, width);
        }
        update_scroll(app);
        return true;
    }

    match cmd {
        // Paging and the file-end motions walk the same focus list along the
        // time axis; there is no separate "line" to address in a graph, and
        // "the start of the graph" is its oldest commit.
        Command::PageDown | Command::GotoFileEnd => {
            repeat_step(app, Dir::Right, page(app, cmd));
            return true;
        }
        Command::PageUp | Command::GotoFileStart => {
            repeat_step(app, Dir::Left, page(app, cmd));
            return true;
        }

        Command::VcsGrab => grab_or_release(app),
        Command::EnterNormal => cancel(app),
        Command::VcsClose | Command::BufferClose => close(app),
        Command::VcsRefresh => {
            refresh(app);
            app.messages.show("Re-reading the repository…");
        }
        Command::VcsOpen => app.messages.show("Already in the version-control view"),

        // --- planned edits ---
        Command::VcsDrop => plan_drop(app),
        Command::VcsMerge => plan_merge(app),
        Command::VcsUndoEdit | Command::Undo => undo_edit(app),
        Command::VcsReset => reset_plan(app),
        Command::VcsApply => confirm_apply(app),
        Command::VcsUndo => undo_apply(app),
        Command::VcsAbort => in_progress(app, vcs_apply::abort, "Aborted"),
        Command::VcsContinue => in_progress(app, vcs_apply::resume, "Resumed"),

        // --- immediate actions ---
        Command::VcsCheckout => checkout(app),
        Command::VcsShow => show_commit(app),
        // Enter: a branch label is a place to *go*, so it is checked out;
        // anything else is a commit to read.  `TableOpenCell` is what the
        // grid binds Enter to, and it arrives here from a user rebinding.
        Command::VcsEnter | Command::TableOpenCell => enter_action(app),
        Command::VcsStage => run_now(app, &["add", "--all"], "Staged everything"),
        Command::VcsUnstage => run_now(app, &["reset"], "Unstaged everything"),
        Command::VcsFetch => remote_job(app, vec!["fetch".into(), "--all".into()], "Fetching"),
        Command::VcsPull => remote_job(app, vec!["pull".into(), "--ff-only".into()], "Pulling"),
        Command::VcsPush => push(app),
        Command::VcsNewBranch(name) => new_branch(app, name),
        Command::VcsCommit(message) => commit(app, message),
        Command::VcsSetUpstream(target) => set_upstream(app, target),

        // `:42` has no meaning in a graph, and neither does yanking a
        // selection — but the hash under the cursor is what a user reaching
        // for `y` in a commit viewer actually wants.
        Command::YankSelection => yank_hash(app),

        // Anything the graph does not reinterpret is classified by the shared
        // rule in `crate::view`: which commands need a rope is a property of
        // the commands, not of this view.  `None` falls through unchanged.
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

/// How many focus steps a paging command takes.
fn page(app: &App, cmd: &Command) -> usize {
    match cmd {
        // The ends of the graph: further than it can possibly be.
        Command::GotoFileStart | Command::GotoFileEnd => usize::MAX,
        // Half a screen, measured in blocks rather than columns, so paging
        // lands on a block rather than mid-border.  Each block costs two steps
        // (the block, then the arrow beside it).
        _ => {
            let stride = app
                .vcs
                .as_ref()
                .map_or(vcs::layout::MIN_BLOCK + vcs::layout::GAP, |state| {
                    state.layout(app.viewport_width as u16).col_stride()
                });
            let cols = app.viewport_width as u16 / 2;
            (2 * (cols / stride)).max(1) as usize
        }
    }
}

fn repeat_step(app: &mut App, dir: Dir, times: usize) {
    let width = app.viewport_width as u16;
    if let Some(state) = app.vcs.as_mut() {
        for _ in 0..times {
            if !state.step(dir, width) {
                break;
            }
        }
    }
    update_scroll(app);
}

// ---------------------------------------------------------------------------
// The grab gesture
// ---------------------------------------------------------------------------

fn grab_or_release(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    if state.grabbed.is_some() {
        match state.release() {
            Ok(Some(edit)) => {
                let count = state.plan.edits().len();
                app.messages.show(format!(
                    "{} — {count} planned change{} (:vc-apply to make them real)",
                    edit.label(),
                    if count == 1 { "" } else { "s" }
                ));
            }
            Ok(None) => app.messages.show("Put back where it was"),
            Err(why) => app.messages.show(why),
        }
        return;
    }
    match state.grab() {
        Ok(()) => {
            let held = state
                .grabbed
                .as_ref()
                .map(|f| crate::vcs_ui::describe_focus(&state.dag, f))
                .unwrap_or_default();
            app.messages
                .show(format!("Holding {held} — move to a commit and press Space again"));
        }
        Err(why) => app.messages.show(why),
    }
}

fn cancel(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    if state.cancel_drag() {
        app.messages.show("Put down; nothing changed");
    }
}

// ---------------------------------------------------------------------------
// Planned edits
// ---------------------------------------------------------------------------

fn plan_drop(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    let Some(commit) = state.focused_commit() else {
        app.messages.show("No commit selected");
        return;
    };
    match state.push_edit(Edit::Drop { commit: commit.clone() }) {
        Ok(()) => app
            .messages
            .show(format!("{} would be removed (:vc-apply to make it real)", commit.short())),
        Err(why) => app.messages.show(why),
    }
}

fn plan_merge(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    let Some(into) = state.dag.head.branch.clone() else {
        app.messages
            .show("HEAD is detached — check a branch out to merge into it");
        return;
    };
    let Some(from) = state.focused_commit() else {
        app.messages.show("No commit selected");
        return;
    };
    match state.push_edit(Edit::Merge { into: into.clone(), from: from.clone() }) {
        Ok(()) => app.messages.show(format!(
            "{} would be merged into {into} (:vc-apply to make it real)",
            from.short()
        )),
        Err(why) => app.messages.show(why),
    }
}

fn undo_edit(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    match state.plan.pop() {
        Some(edit) => app.messages.show(format!("Took back: {}", edit.label())),
        None => app.messages.show("No planned changes to take back"),
    }
}

fn reset_plan(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    if state.plan.is_empty() {
        app.messages.show("Nothing planned");
        return;
    }
    let n = state.plan.edits().len();
    state.plan.clear();
    state.grabbed = None;
    app.messages
        .show(format!("Discarded {n} planned change{}", if n == 1 { "" } else { "s" }));
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// A backup namespace name.  Sortable as a timestamp, which is what
/// `vcs::apply::backups` relies on to order them.
fn stamp() -> String {
    let secs = vcs::now_secs();
    format!("{secs:010}")
}

/// Show what would run, and ask.
///
/// The confirmation is not decoration.  The graph shows the *shape* the user
/// asked for; this shows the operations that shape turns into, which is where
/// a surprise would show up — a replay of nine commits when they expected a
/// branch to move, say.
fn confirm_apply(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    if state.plan.is_empty() {
        app.messages.show("Nothing planned — move something first");
        return;
    }
    if let Err(why) = vcs_apply::preflight(&state.dag) {
        app.messages.show(format!("Cannot apply: {why}"));
        return;
    }
    let ops = match derive::derive(&state.dag, &state.plan) {
        Ok(ops) => ops,
        Err(why) => {
            app.messages.show(format!("Cannot apply: {why}"));
            return;
        }
    };
    if ops.is_empty() {
        app.messages.show("The planned changes cancel out — nothing to do");
        return;
    }
    let lines: Vec<String> = ops.iter().map(derive::Op::describe).collect();
    app.popup = Some(Popup::vcs_plan(lines));
}

/// Run the plan.  Called from the popup's confirmation.
pub fn apply_confirmed(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    let (root, stamp) = (state.root.clone(), stamp());
    // Derived here, on the UI thread, where the plan and the snapshot live;
    // only the *running* goes to the background thread, which needs neither.
    let dag = &state.dag;
    let plan = &state.plan;
    if let Err(why) = vcs_apply::preflight(dag) {
        app.messages.show(format!("Cannot apply: {why}"));
        return;
    }
    let ops = match derive::derive(dag, plan) {
        Ok(ops) => ops,
        Err(why) => {
            app.messages.show(format!("Cannot apply: {why}"));
            return;
        }
    };
    let branches: Vec<(String, Oid)> = dag
        .local_branches()
        .map(|r| (r.name.clone(), r.target.clone()))
        .collect();

    spawn(app, "Applying", move || {
        vcs_apply::run(&root, &branches, &ops, &stamp)
            .map(|outcome| JobDone::Applied(Box::new(outcome)))
    });
}

fn report_outcome(app: &mut App, outcome: &Outcome) {
    if outcome.succeeded() {
        // The plan is spent: it described a change the repository has now
        // made, and keeping it would re-derive the same operations against a
        // graph that already contains them.
        if let Some(state) = app.vcs.as_mut() {
            state.plan.clear();
        }
        let undo = outcome
            .backup
            .as_deref()
            .map_or_else(String::new, |_| "  (:vc-undo to put it back)".to_string());
        app.messages.show(format!(
            "Applied {} operation{}{undo}",
            outcome.total,
            if outcome.total == 1 { "" } else { "s" }
        ));
        return;
    }
    match &outcome.failure {
        None => unreachable!("succeeded() is the negation of failure.is_some()"),
        Some(failure) if !failure.conflicts.is_empty() => {
            let names: Vec<String> = failure
                .conflicts
                .iter()
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect();
            app.messages.show(format!(
                "Stopped at step {} of {} — conflicts in {}. \
                 Fix them in the editor then :vc-continue, or :vc-abort to back out",
                outcome.ran + 1,
                outcome.total,
                names.join(", ")
            ));
        }
        Some(failure) => app.messages.show(format!(
            "Stopped at step {} of {}: {} — {}",
            outcome.ran + 1,
            outcome.total,
            failure.op.describe(),
            failure.message
        )),
    }
}

fn undo_apply(app: &mut App) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    let Some(stamp) = vcs_apply::backups(&root).first().cloned() else {
        app.messages.show("Nothing to undo — no backup was taken");
        return;
    };
    match vcs_apply::undo(&root, &stamp) {
        Ok(branches) => {
            app.messages
                .show(format!("Restored {}", branches.join(", ")));
            refresh(app);
        }
        Err(why) => app.messages.show(format!("Could not undo: {why}")),
    }
}

fn in_progress(
    app: &mut App,
    action: fn(&std::path::Path) -> Result<&'static str, String>,
    verb: &str,
) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    match action(&root) {
        Ok(what) => {
            app.messages.show(format!("{verb} the {what}"));
            refresh(app);
        }
        Err(why) => app.messages.show(why),
    }
}

// ---------------------------------------------------------------------------
// Immediate actions
// ---------------------------------------------------------------------------

/// Run a local git command now and report.  Local operations only: anything
/// that can touch a network goes through [`remote_job`].
fn run_now(app: &mut App, args: &[&str], ok: &str) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    match git(&root, args) {
        Ok(_) => {
            app.messages.show(ok.to_string());
            refresh(app);
        }
        Err(why) => app.messages.show(why),
    }
}

fn remote_job(app: &mut App, args: Vec<String>, label: &str) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    let label_owned = label.to_string();
    spawn(app, label, move || {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        git(&root, &borrowed).map(|out| {
            let tail = out.lines().last().unwrap_or("").trim().to_owned();
            JobDone::Message(if tail.is_empty() {
                format!("{label_owned} finished")
            } else {
                format!("{label_owned}: {tail}")
            })
        })
    });
}

/// Push, setting the upstream when the branch has none.
///
/// `--set-upstream` on a branch that has never been pushed is what the user
/// means every time; git's own suggestion to re-run with the flag is a step
/// that exists only because the command line cannot ask.
fn push(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    let Some(branch) = state.dag.head.branch.clone() else {
        app.messages
            .show("HEAD is detached — there is no branch to push");
        return;
    };
    let has_upstream = state
        .dag
        .find_ref(&branch)
        .is_some_and(|r| r.upstream.is_some());
    let args = if has_upstream {
        vec!["push".to_string()]
    } else {
        vec![
            "push".to_string(),
            "--set-upstream".to_string(),
            "origin".to_string(),
            branch,
        ]
    };
    remote_job(app, args, "Pushing");
}

fn checkout(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    match checkout_plan(state) {
        Some((args, report)) => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            run_now(app, &args, &report);
        }
        None => app.messages.show("Nothing selected"),
    }
}

/// Which checkout the cursor's position asks for, and how to report it.
///
/// Pulled out of [`checkout`] because the rule — and specifically *when this
/// view is willing to detach HEAD* — is the interesting part, and the rest is
/// shelling out.
fn checkout_plan(state: &VcsState) -> Option<(Vec<String>, String)> {
    let owned = |parts: [&str; 2]| parts.iter().map(|s| (*s).to_string()).collect();

    // A branch under the cursor is what you meant.
    if let Some(branch) = state.focused_branch() {
        let local = state
            .dag
            .find_ref(&branch)
            .is_some_and(|r| r.kind == RefKind::Local);
        if local {
            return Some((owned(["checkout", &branch]), format!("On {branch}")));
        }
        // Checking out a remote-tracking branch detaches HEAD, which is almost
        // never what someone selecting `origin/main` wants.
        let local_name = branch.split_once('/').map_or(branch.as_str(), |(_, n)| n);
        return Some((
            vec!["checkout".into(), "-B".into(), local_name.into(), branch.clone()],
            format!("On {local_name}, tracking {branch}"),
        ));
    }

    let commit = state.focused_commit()?;

    // A commit that a local branch already points at *is* that branch, as far
    // as anyone using this view is concerned.  Detaching there is technically
    // what was asked for and almost never what was meant: you end up off every
    // branch, and the next commit you make is unreachable the moment you leave
    // it.  Detached HEAD should be somewhere you arrive deliberately — from a
    // commit in the middle of history — not somewhere pressing `c` on the tip
    // of `main` puts you.
    if let Some(branch) = state
        .dag
        .local_branches()
        .find(|r| r.target == commit)
        .map(|r| r.name.clone())
    {
        return Some((owned(["checkout", &branch]), format!("On {branch}")));
    }

    Some((
        owned(["checkout", "--detach"])
            .into_iter()
            .chain(std::iter::once(commit.to_string()))
            .collect(),
        format!("HEAD detached at {} — make a branch here with :vc-branch <name>", commit.short()),
    ))
}

fn new_branch(app: &mut App, name: &str) {
    let Some(state) = app.vcs.as_ref() else { return };
    let Some(commit) = state.focused_commit() else {
        app.messages.show("No commit selected");
        return;
    };
    let args = ["checkout", "-b", name, commit.as_str()];
    run_now(app, &args, &format!("On new branch {name}"));
}

fn commit(app: &mut App, message: &str) {
    let Some(state) = app.vcs.as_ref() else { return };
    if state.dag.work.staged() == 0 {
        app.messages
            .show("Nothing staged — `s` stages every change in the work tree");
        return;
    }
    run_now(app, &["commit", "-m", message], "Committed");
}

fn set_upstream(app: &mut App, target: &str) {
    let args = ["branch", "--set-upstream-to", target];
    run_now(app, &args, &format!("Tracking {target}"));
}

/// Show the selected commit's diff in a buffer.
///
/// A buffer rather than a float: a diff is long, and it is read with the
/// ordinary motions and search, which is exactly what a buffer already gives.
/// Enter: check out a branch, or read a commit.
///
/// A branch label under the cursor is somewhere to go — the graph is the map,
/// and pressing Enter on a place on a map means go there.  Everything else the
/// cursor can sit on names a commit, and what you want from a commit is to
/// read it.
fn enter_action(app: &mut App) {
    let on_local_branch = app.vcs.as_ref().is_some_and(|state| {
        state
            .focused_branch()
            .and_then(|name| state.dag.find_ref(&name).map(|r| r.kind))
            .is_some_and(|kind| kind == RefKind::Local)
    });
    if on_local_branch {
        checkout(app);
        return;
    }
    show_commit(app);
}

fn show_commit(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    let Some(commit) = state.focused_commit() else {
        app.messages.show("No commit selected");
        return;
    };
    if commit.is_pending() {
        app.messages
            .show("That commit does not exist yet — :vc-apply would create it");
        return;
    }
    let root = state.root.clone();
    let text = match git(&root, &["show", "--stat", "--patch", commit.as_str()]) {
        Ok(text) => text,
        Err(why) => {
            app.messages.show(why);
            return;
        }
    };
    let name = format!("{}{}*", crate::app::COMMIT_BUFFER_PREFIX, commit.short());
    app.special_buffer_ropes
        .insert(name.clone(), ropey::Rope::from_str(&text));
    super::buffers::switch_to_special_buffer(app, &name);
}

/// `q` / `:bd` in a `*commit …*` buffer — back to the graph it was opened
/// from, which is the only place it makes sense to go.  Returns false when
/// that buffer is not what is open, so the caller carries on with its own
/// close.
///
/// The same "back out of the temporary thing" gesture as `q` in a `*cell …*`
/// buffer or a derived table.  Without it `:bd` refused the `*…*` name
/// outright and `q` was unbound, which made the diff a buffer you could only
/// leave by naming somewhere else to go.
pub(super) fn close_commit_buffer(app: &mut App) -> bool {
    if !app.in_commit_buffer() {
        return false;
    }
    if let Some(id) = app.current_source_id() {
        app.special_buffer_ropes.remove(id.label());
    }
    open(app);
    true
}

fn yank_hash(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    let Some(commit) = state.focused_commit() else {
        app.messages.show("Nothing selected");
        return;
    };
    crate::clipboard::write(commit.as_str());
    app.messages.show(format!("Yanked {}", commit.as_str()));
}

/// The `g` sub-mode's meanings here.
pub fn goto_command(c: char) -> Option<Command> {
    Some(match c {
        'x' => Command::VcsReset,
        'r' => Command::VcsRefresh,
        'a' => Command::VcsApply,
        'm' => Command::VcsMerge,
        'd' => Command::VcsDrop,
        'c' => Command::VcsCheckout,
        _ => return None,
    })
}

/// What the `g` which-key popup advertises here.  Pinned to
/// [`goto_command`] by a test, so the popup can never advertise a key that
/// does nothing.
pub fn goto_hints() -> Vec<(String, String)> {
    [
        ("g", "first commit"),
        ("e", "last commit"),
        ("c", "check out the selection"),
        ("m", "plan a merge into the current branch"),
        ("d", "remove the selected commit"),
        ("a", "apply the planned changes"),
        ("x", "discard the planned changes"),
        ("r", "re-read the repository"),
        ("b", "buffer picker"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{layout::Focus, plan::Edit, Commit, Dag, Head, Ref, WorkTree};

    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: Oid::new(id),
            parents: parents.iter().map(|p| Oid::new(*p)).collect(),
            summary: format!("commit {id}"),
            author: "T".into(),
            when: 0,
            insertions: 1,
            deletions: 0,
        }
    }

    /// An app showing a fixed two-branch graph, without going near a real
    /// repository: the command routing is what is under test here, and the
    /// git side is covered end-to-end in `vcs::apply`.
    fn app_in_graph() -> App {
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.viewport_height = 40;
        app.viewport_width = 120;
        let dag = Dag::new(
            vec![
                commit("d", &["c"]),
                commit("f", &["e"]),
                commit("c", &["a"]),
                commit("e", &["a"]),
                commit("a", &[]),
            ],
            vec![
                Ref { name: "feature".into(), kind: RefKind::Local, target: Oid::new("d"), upstream: None },
                Ref { name: "main".into(), kind: RefKind::Local, target: Oid::new("f"), upstream: None },
            ],
            Head { branch: Some("main".into()), target: Some(Oid::new("f")) },
            WorkTree::default(),
            false,
        );
        app.buffer = crate::buffer::Buffer::new_empty();
        app.vcs = Some(VcsState::new(PathBuf::from("/tmp/repo"), dag, 0));
        app
    }

    fn focus(app: &App) -> Focus {
        app.vcs.as_ref().unwrap().focus.clone().expect("a cursor")
    }

    /// The graph owns the screen, so it is what `View` reports and what the
    /// keymap layer and the status line key off.
    #[test]
    fn the_graph_owns_the_view_and_has_no_text_buffer_behind_it() {
        let app = app_in_graph();
        assert_eq!(app.view(), View::Vcs);
        assert!(!app.view().has_text_buffer());
        assert!(app.buffer.path.is_none(), "nothing may write through the buffer");
        assert_eq!(
            app.current_source_id(),
            Some(SourceId::virtual_named(crate::app::VCS_BUFFER))
        );
    }

    /// `h` walks back through blocks and arrows alternately, which is what
    /// makes an arrow selectable at all.
    #[test]
    fn h_and_l_walk_the_blocks_and_the_arrows_between_them() {
        let mut app = app_in_graph();
        let start = focus(&app);
        super::handle(&mut app, &Command::MoveLeft);
        let next = focus(&app);
        assert_ne!(next, start);
        super::handle(&mut app, &Command::MoveRight);
        assert_eq!(focus(&app), start, "the walk is reversible");

        // Somewhere back through history there is an arrow, or nothing can be
        // dragged.
        let mut seen_edge = false;
        for _ in 0..12 {
            super::handle(&mut app, &Command::MoveLeft);
            seen_edge |= matches!(focus(&app), Focus::Edge { .. });
        }
        assert!(seen_edge, "arrows must be reachable with h");
    }

    /// The headline gesture, through the command layer this time: grab, move,
    /// drop — and the plan has one edit that has not touched git.
    #[test]
    fn space_grabs_moves_and_drops_leaving_a_plan_and_not_a_commit() {
        let mut app = app_in_graph();
        app.vcs.as_mut().unwrap().focus = Some(Focus::Commit(Oid::new("c")));
        super::handle(&mut app, &Command::VcsGrab);
        assert!(app.vcs.as_ref().unwrap().grabbed.is_some());

        app.vcs.as_mut().unwrap().focus = Some(Focus::Commit(Oid::new("f")));
        app.vcs.as_mut().unwrap().step(Dir::Down, 120);
        app.vcs.as_mut().unwrap().focus = Some(Focus::Commit(Oid::new("f")));
        super::handle(&mut app, &Command::VcsGrab);

        let state = app.vcs.as_ref().unwrap();
        assert_eq!(state.plan.edits().len(), 1, "one planned change");
        assert!(state.grabbed.is_none(), "and the drag is over");
        assert!(
            app.messages.current().unwrap_or_default().contains("planned"),
            "the message says it is only planned: {:?}",
            app.messages.current()
        );
    }

    /// Escape puts the held thing down without deciding anything.
    #[test]
    fn escape_abandons_a_drag() {
        let mut app = app_in_graph();
        app.vcs.as_mut().unwrap().focus = Some(Focus::Commit(Oid::new("c")));
        super::handle(&mut app, &Command::VcsGrab);
        super::handle(&mut app, &Command::EnterNormal);
        let state = app.vcs.as_ref().unwrap();
        assert!(state.grabbed.is_none());
        assert!(state.plan.is_empty());
    }

    /// `u` is undo everywhere in the editor; here the only thing that has been
    /// changed is the plan, so that is what it takes back.
    #[test]
    fn u_takes_back_the_last_planned_change() {
        let mut app = app_in_graph();
        app.vcs
            .as_mut()
            .unwrap()
            .push_edit(Edit::Drop { commit: Oid::new("c") })
            .unwrap();
        super::handle(&mut app, &Command::Undo);
        assert!(app.vcs.as_ref().unwrap().plan.is_empty());
        // And again, with nothing left, says so rather than doing nothing.
        super::handle(&mut app, &Command::Undo);
        assert!(app
            .messages
            .current()
            .unwrap_or_default()
            .contains("No planned changes"));
    }

    /// Applying an empty plan must not open a confirmation for no operations.
    #[test]
    fn applying_nothing_says_so_instead_of_opening_a_dialog() {
        let mut app = app_in_graph();
        super::handle(&mut app, &Command::VcsApply);
        assert!(app.popup.is_none());
        assert!(app
            .messages
            .current()
            .unwrap_or_default()
            .contains("Nothing planned"));
    }

    /// Every destructive apply is confirmed, and the confirmation lists the
    /// actual git commands — the one place a surprise would surface.
    #[test]
    fn applying_a_plan_asks_first_and_shows_the_commands() {
        let mut app = app_in_graph();
        app.vcs
            .as_mut()
            .unwrap()
            .push_edit(Edit::Reparent {
                child: Oid::new("c"),
                slot: 0,
                new_parent: Some(Oid::new("f")),
            })
            .unwrap();
        super::handle(&mut app, &Command::VcsApply);

        let popup = app.popup.as_ref().expect("a confirmation opened");
        let crate::popup::PopupContent::List(ref list) = popup.content else {
            panic!("the confirmation is a list of operations");
        };
        let labels: Vec<String> = list.items.iter().map(|i| i.label.clone()).collect();
        assert!(labels.iter().any(|l| l.contains("cherry-pick")), "{labels:?}");
        assert!(labels.iter().any(|l| l.contains("branch --force")), "{labels:?}");
        // Nothing has run: the plan is untouched until the popup is confirmed.
        assert_eq!(app.vcs.as_ref().unwrap().plan.edits().len(), 1);
    }

    /// A dirty work tree is refused before the confirmation, not after it:
    /// asking a question whose answer cannot be acted on is worse than not
    /// asking.
    #[test]
    fn a_dirty_work_tree_is_refused_before_the_question_is_asked() {
        let mut app = app_in_graph();
        app.vcs.as_mut().unwrap().dag.work = crate::vcs::WorkTree::new(vec![
            crate::vcs::Change { path: "a.rs".into(), index: ' ', work: 'M' },
        ]);
        app.vcs
            .as_mut()
            .unwrap()
            .push_edit(Edit::Drop { commit: Oid::new("c") })
            .unwrap();
        super::handle(&mut app, &Command::VcsApply);
        assert!(app.popup.is_none());
        assert!(app
            .messages
            .current()
            .unwrap_or_default()
            .contains("uncommitted"));
    }

    /// The text commands reach the graph on the same keys they always do, and
    /// have to be refused with a reason rather than run against the empty
    /// buffer behind it.
    #[test]
    fn text_commands_are_refused_with_a_reason() {
        let mut app = app_in_graph();
        for cmd in [Command::Write, Command::LspGotoDefinition, Command::EnterInsert] {
            app.messages.clear();
            assert!(super::handle(&mut app, &cmd), "{} should be ours", cmd.name());
            let message = app.messages.current().unwrap_or_default();
            assert!(!message.is_empty(), "{} was refused silently", cmd.name());
        }
    }

    /// …while anything that means the same thing in every view falls through
    /// unchanged.
    #[test]
    fn universal_commands_fall_through() {
        let mut app = app_in_graph();
        for cmd in [Command::Quit, Command::OpenCommandPalette, Command::ToggleWordWrap] {
            assert!(!super::handle(&mut app, &cmd), "{} should fall through", cmd.name());
        }
    }

    /// The `g` which-key popup must never advertise a key that does nothing —
    /// the same pairing `goto_hints` has with `input::goto_command` elsewhere.
    #[test]
    fn goto_hints_only_advertise_real_bindings() {
        for (key, description) in goto_hints() {
            let c = key.chars().next().expect("a key");
            let handled = goto_command(c).is_some()
                || crate::input::goto_command(View::Vcs, c).is_some();
            assert!(handled, "g{key} ({description}) is advertised but does nothing");
        }
    }

    /// `:vc` from an ordinary text buffer has to actually open the view.
    /// Nothing else in this module is reachable if the door does not work.
    #[test]
    fn the_open_command_reaches_the_view_from_a_text_buffer() {
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        assert_eq!(app.view(), View::Text);
        crate::exec::execute(&mut app, &Command::VcsOpen);
        assert!(
            !app.messages.log.is_empty(),
            "opening says something either way — it found a repository or it did not"
        );
    }

    /// Opening from a file inside a real repository must actually reach the
    /// graph — the door, exercised the way the editor uses it.
    #[test]
    fn opening_from_a_file_in_a_repository_reaches_the_graph() {
        let here = std::env::current_dir().expect("cwd");
        if load::discover_root(&here).is_none() {
            return; // not a checkout (a source tarball); nothing to open
        }
        let mut app =
            App::new(Some("Cargo.toml"), crate::config::Config::load()).expect("app");
        crate::exec::execute(&mut app, &Command::VcsOpen);
        assert_eq!(app.view(), View::Vcs, "messages: {:?}", app.messages.log);
    }

    /// A branch label under the cursor is what `c` checks out; a bare commit
    /// is a detached checkout, and the two must not be confused.
    #[test]
    fn the_cursor_knows_whether_it_is_on_a_branch_or_a_commit() {
        let mut app = app_in_graph();
        let state = app.vcs.as_mut().unwrap();
        state.focus = Some(Focus::Ref("main".into()));
        assert_eq!(state.focused_branch().as_deref(), Some("main"));
        state.focus = Some(Focus::Commit(Oid::new("c")));
        assert_eq!(state.focused_branch(), None);
        assert_eq!(state.focused_commit(), Some(Oid::new("c")));
    }
    /// While something is held, the walk is over *destinations* — and the only
    /// destination is a commit.  Stopping the cursor on a branch label or an
    /// arrow offered a choice that was never a choice (both are other names
    /// for a commit already on the walk) and doubled the presses to cross the
    /// graph.  Anything the drop would refuse is skipped too.
    #[test]
    fn a_drag_walks_only_the_commits_it_could_actually_be_dropped_on() {
        let mut app = app_in_graph();
        app.vcs.as_mut().unwrap().focus = Some(Focus::Edge { child: Oid::new("c"), slot: 0 });
        super::handle(&mut app, &Command::VcsGrab);

        let mut seen = Vec::new();
        for _ in 0..8 {
            super::handle(&mut app, &Command::MoveRight);
            let focus = app.vcs.as_ref().unwrap().focus.clone().expect("a cursor");
            if !seen.last().is_some_and(|last| *last == focus) {
                seen.push(focus);
            }
        }

        assert!(!seen.is_empty(), "the cursor never moved");
        for focus in &seen {
            assert!(
                matches!(focus, Focus::Commit(_)),
                "a drag stopped on {focus:?}, which is not somewhere to drop"
            );
        }
        // `c` cannot follow itself, and `d` is `c`'s own child — both are
        // ahead of the cursor and both are skipped rather than stopped on.
        for unreachable in [Oid::new("c"), Oid::new("d")] {
            assert!(
                !seen.contains(&Focus::Commit(unreachable.clone())),
                "stopped on {unreachable}, which the drop would refuse"
            );
        }
        assert!(seen.contains(&Focus::Commit(Oid::new("f"))), "{seen:?}");
    }

    /// …and the preview really does follow the cursor, press by press, rather
    /// than only appearing once the drag is released.
    #[test]
    fn the_preview_follows_the_cursor_during_a_drag() {
        let mut app = app_in_graph();
        app.vcs.as_mut().unwrap().focus = Some(Focus::Edge { child: Oid::new("c"), slot: 0 });
        super::handle(&mut app, &Command::VcsGrab);

        let mut previewed = Vec::new();
        for _ in 0..8 {
            super::handle(&mut app, &Command::MoveRight);
            let state = app.vcs.as_ref().unwrap();
            let projection = state.plan.project(&state.dag);
            previewed.push(projection.parents(&state.dag, &Oid::new("c")).to_vec());
        }
        assert!(
            previewed.iter().any(|p| p == &[Oid::new("f")]),
            "walking on to `f` never previewed `c` following it: {previewed:?}"
        );
        // And none of it was decided: the stack is still empty.
        assert!(app.vcs.as_ref().unwrap().plan.edits().is_empty());
    }

    /// Pressing `c` on the tip of a branch must check that branch out, not
    /// detach at the commit it happens to point to.  A detached HEAD is
    /// somewhere you should arrive deliberately, from the middle of history.
    #[test]
    fn checking_out_a_commit_a_branch_points_at_checks_out_the_branch() {
        let mut app = app_in_graph();
        let state = app.vcs.as_mut().unwrap();

        // `f` is `main`'s tip, selected as a *commit* rather than as the label.
        state.focus = Some(Focus::Commit(Oid::new("f")));
        let (args, report) = super::checkout_plan(state).expect("a checkout");
        assert_eq!(args, ["checkout", "main"]);
        assert_eq!(report, "On main");

        // A commit in the middle of history still detaches — that is a real
        // thing to want, and there is no branch to name instead.
        state.focus = Some(Focus::Commit(Oid::new("e")));
        let (args, report) = super::checkout_plan(state).expect("a checkout");
        assert_eq!(args, ["checkout", "--detach", "e"]);
        assert!(report.contains("detached"), "{report}");
    }

    /// Enter on a branch label goes there.  The graph is a map, and pressing
    /// Enter on a place on a map means go to it — not "tell me about the
    /// commit underneath it".
    #[test]
    fn enter_on_a_branch_checks_it_out_and_on_a_commit_reads_it() {
        let mut app = app_in_graph();
        app.vcs.as_mut().unwrap().focus = Some(Focus::Ref("feature".into()));
        let state = app.vcs.as_ref().unwrap();
        assert_eq!(
            super::checkout_plan(state).expect("a checkout").0,
            ["checkout", "feature"]
        );

        // A commit is something to read, so Enter opens its diff instead —
        // which needs a real repository, and is covered by the buffer test
        // below.  Here it is enough that the two are told apart.
        app.vcs.as_mut().unwrap().focus = Some(Focus::Commit(Oid::new("c")));
        assert!(app.vcs.as_ref().unwrap().focused_branch().is_none());
    }

    /// A `*commit …*` diff is backed out of with `q`, like every other
    /// temporary buffer in the editor.  Without it `:bd` refused the `*…*`
    /// name and `q` was unbound, so the diff was a buffer with no way out.
    #[test]
    fn q_backs_out_of_a_commit_diff_to_the_graph() {
        let mut app = app_in_graph();
        let name = format!("{}abc1234*", crate::app::COMMIT_BUFFER_PREFIX);
        app.special_buffer_ropes
            .insert(name.clone(), ropey::Rope::from_str("diff --git a/x b/x\n"));
        super::super::buffers::switch_to_special_buffer(&mut app, &name);
        assert!(app.in_commit_buffer());
        assert_eq!(
            crate::input::keymap_layer(&app),
            crate::keymap::Layer::Commit,
            "the q-goes-back layer has to be selected"
        );
        assert!(app
            .keymap
            .lookup(crate::keymap::Layer::Commit, &crate::keymap::KeyBinding::char('q'))
            .is_some_and(|c| matches!(c, [Command::BufferClose])));

        super::super::execute(&mut app, &Command::BufferClose);
        assert!(!app.in_commit_buffer(), "must leave the diff");
    }

}
