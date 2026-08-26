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
    popup::{Popup, StageEntry},
    source::SourceId,
    stash::Stash,
    vcs::{
        self,
        apply::{self as vcs_apply, Outcome},
        derive,
        layout::Dir,
        load::{self, git, RepoLoad},
        plan::Edit,
        run,
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

/// The replay running off the UI thread.
///
/// A replay can run a hook per commit, which may not block a frame, so it is
/// collected by [`poll`] in the run loop — the same shape as the table load
/// and the Quarto export.  Everything else long-running is an [`OutputJob`]
/// instead: the difference is whether the *result* is the point (a plan run to
/// completion, reported as an [`Outcome`]) or the *output* is.
pub struct VcsJob {
    pub label: String,
    rx: Receiver<Result<Box<Outcome>, String>>,
}

fn spawn<F>(app: &mut App, label: &str, work: F)
where
    F: FnOnce() -> Result<Box<Outcome>, String> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    app.vcs_job = Some(VcsJob { label: label.to_string(), rx });
    app.messages.show(format!("{label}…"));
}

/// A git command being watched line by line in `*git output*`.
///
/// The commands that go through here are the ones whose *middle* is worth
/// seeing: a commit runs the `pre-commit` hook, a push counts objects over a
/// network.  Run with `Command::output()` they froze the editor for however
/// long that took — no spinner, no output, nothing to distinguish a linter
/// suite from a hang.  Now the output buffer opens immediately and fills as
/// the hook prints, which is the same answer lazygit reaches by dropping back
/// to the terminal, without leaving the editor to get it.
pub struct OutputJob {
    /// What is happening, for the message line ("Committing").
    label: String,
    /// What to say when it works.  git's own last line is usually noise on
    /// success ("1 file changed…" is already in the buffer) and the useful
    /// report is the editor's own.
    ok: String,
    stream: run::Stream,
}

/// Run a git command in the output buffer, watching it as it goes.
///
/// Switches to `*git output*` up front rather than when something takes too
/// long: a rule of "only if it is slow" means the screen can jump under you
/// halfway through reading the graph, and a two-line transcript of a fast
/// commit is not a cost worth a timer to avoid.  `q` goes back.
fn stream_now(app: &mut App, args: &[&str], label: &str, ok: &str) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    let stream = match run::Stream::start(&root, args) {
        Ok(stream) => stream,
        Err(why) => {
            app.messages.show(why);
            return;
        }
    };

    // The command being run, as its own first line: the buffer is a transcript
    // and a transcript that does not say what was run is half of one.
    let header = format!("$ git {}\n", args.join(" "));
    app.special_buffer_ropes
        .insert(crate::app::GIT_OUTPUT_BUFFER.to_string(), ropey::Rope::from_str(&header));
    super::buffers::switch_to_special_buffer(app, crate::app::GIT_OUTPUT_BUFFER);

    app.messages.show(format!("{label}…  (q to go back)"));
    app.vcs_stream = Some(OutputJob { label: label.to_string(), ok: ok.to_string(), stream });
}

/// Append finished lines to the output buffer, wherever it currently lives.
///
/// The buffer is usually the one on screen, but `q` during a long hook leaves
/// the job running with nowhere visible to write — so the stash is the
/// fallback, and the transcript is whole either way when you come back.
fn append_output(app: &mut App, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }

    if app.in_git_output_buffer() {
        let end = app.buffer.rope.len_chars();
        app.buffer.rope.insert(end, &text);
        // Follow the tail.  Watching output that does not scroll is watching
        // the first screenful of it.
        let last = app.buffer.rope.len_lines().saturating_sub(1);
        let head = app.buffer.rope.line_to_char(last);
        app.selection = crate::selection::Selection::point(head);
        super::recompute_highlights(app);
    } else if let Some(rope) = app
        .special_buffer_ropes
        .get_mut(crate::app::GIT_OUTPUT_BUFFER)
    {
        let end = rope.len_chars();
        rope.insert(end, &text);
    }
}

/// Drain the streaming job.  Returns true when the screen changed.
fn poll_stream(app: &mut App) -> bool {
    let Some(mut job) = app.vcs_stream.take() else { return false };

    let mut lines = Vec::new();
    let mut verdict = None;
    for event in job.stream.drain() {
        match event {
            run::Event::Line(line) => lines.push(line),
            run::Event::Finished(result) => verdict = Some(result),
        }
    }
    let changed = !lines.is_empty() || verdict.is_some();
    append_output(app, &lines);

    match verdict {
        None => app.vcs_stream = Some(job),
        Some(result) => {
            let report = match result {
                Ok(()) => job.ok.clone(),
                Err(why) => format!("{}: {why}", job.label),
            };
            // Into the buffer as well as the message line: the verdict belongs
            // with the output it is a verdict on, and the message line is one
            // keystroke from being replaced.
            append_output(app, &[String::new(), format!("— {report}  (q to go back)")]);
            app.messages.show(report);
            // The repository moved under the graph, whether or not it is what
            // is on screen: the next `open` must not restore a stale snapshot.
            app.stashes.discard(&SourceId::virtual_named(VCS_BUFFER));
            refresh(app);
        }
    }
    changed
}

/// Collect a finished load or job.  Returns true when the screen changed.
pub fn poll(app: &mut App) -> bool {
    let mut changed = poll_stream(app);

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
            Ok(outcome) => {
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
    app.vcs_pending.is_some() || app.vcs_job.is_some() || app.vcs_stream.is_some()
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
    app.messages.show("Reading the repository…  (? for the keys)");
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
    // The session's own display options, not the config's: flipping the graph
    // or hiding a branch is a preference you state once, and a graph that came
    // back the other way up with every branch in it after you looked at a file
    // was asking you to state it again every time.
    let options = app.vcs_options.clone();
    match app.vcs.as_mut() {
        Some(state) => state.reload(dag, now),
        None => {
            let mut state = VcsState::new(root, dag, now);
            state.options = options;
            app.vcs = Some(state);
        }
    }
}

/// Turn the graph a quarter turn.
///
/// Session-only, like `:theme`: the message names the config key that makes it
/// stick, rather than the editor writing to the user's config behind them.
fn flip(app: &mut App) {
    let Some(state) = app.vcs.as_mut() else { return };
    let orient = state.flip();
    app.vcs_options = state.options.clone();
    update_scroll(app);
    app.messages.show(format!(
        "History runs {} — `[vcs] orientation = \"{}\"` to keep it",
        match orient {
            vcs::layout::Orientation::Horizontal => "left to right",
            vcs::layout::Orientation::Vertical => "down the screen, newest first",
        },
        orient.name()
    ));
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

/// Keep the cursor on screen, along time and across tracks.
///
/// Written in graph space like everything else in this view: which of the
/// screen's two dimensions is the time axis is the one thing it asks the
/// layout ([`viewport`]), so turning the graph does not need a second copy of
/// the same arithmetic.
pub fn update_scroll(app: &mut App) {
    let (height, width) = (app.viewport_height as u16, app.viewport_width as u16);
    let Some(state) = app.vcs.as_ref() else { return };
    let Some(focus) = state.focus.clone() else { return };
    let layout = state.layout(width);
    let Some(at) = layout.locate(&focus) else { return };
    let span = layout.metrics.block_along;
    // Where the focused thing sits on the *display's* time axis: vertical
    // draws the newest commit at the top, so a graph position and a screen
    // position run opposite ways there.
    let start = layout.display_along(at.along, span);
    let (along_extent, across_extent) = viewport(&layout, width, height);
    let total_along = layout.total_along;
    let (track_count, visible_tracks) = (layout.track_count, layout.visible_tracks(across_extent));
    let track = layout.track_at(at.across);
    let Some(state) = app.vcs.as_mut() else { return };

    // Time moves freely: it is the axis you travel along, so a block clipped
    // at the edge is the price of the cursor tracking smoothly.  What must
    // never happen is the focused block being *partly* off screen, so the
    // window is nudged by whole blocks' worth when it is.
    if along_extent > 0 {
        if start < state.scroll_along {
            state.scroll_along = start;
        } else if start + span > state.scroll_along + along_extent {
            state.scroll_along = (start + span).saturating_sub(along_extent);
        }
        state.scroll_along = state
            .scroll_along
            .min(total_along.saturating_sub(along_extent / 2));
    }

    // Tracks scroll a whole branch band at a time: half a block past the edge
    // is unreadable, so there is nothing to be gained by finer steps.
    if across_extent > 0 {
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

/// The viewport measured in graph space: how much of the time axis it shows,
/// and how much of the track axis.
fn viewport(layout: &vcs::layout::Layout, width: u16, height: u16) -> (u16, u16) {
    match layout.metrics.orient {
        vcs::layout::Orientation::Horizontal => (width, height),
        // One row is reserved at the top for the branch names, which are
        // pinned there rather than written per track.
        vcs::layout::Orientation::Vertical => (height.saturating_sub(1), width),
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
            // While something is held the cursor only stops where it could be
            // dropped, so a motion with no destination that way moves nothing.
            // Silence there is the same silence as a wedged editor: say which
            // it is.
            if !state.step(dir, width) && state.grabbed.is_some() {
                let held = state
                    .grabbed
                    .as_ref()
                    .map(|f| crate::vcs_ui::describe_focus(&state.dag, f))
                    .unwrap_or_default();
                app.messages.show(format!(
                    "Nothing that way could take {held} — Esc puts it back down"
                ));
            }
        }
        update_scroll(app);
        return true;
    }

    match cmd {
        // Paging and the file-end motions walk the same focus list along the
        // time axis; there is no separate "line" to address in a graph.  They
        // are stated as screen directions (`forward`/`back`), so `gg` reaches
        // the top of the graph and `J` pages down it whichever way round it is
        // drawn — which end of history that is depends on the picture.
        Command::PageDown | Command::GotoFileEnd => {
            repeat_step(app, orientation(app).forward(), page(app, cmd));
            return true;
        }
        Command::PageUp | Command::GotoFileStart => {
            repeat_step(app, orientation(app).back(), page(app, cmd));
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
        Command::VcsStatus => show_work_tree(app),
        Command::VcsGitStatus => show_git_status(app),
        Command::VcsHelp => app.popup = Some(Popup::reference("version control", HELP)),
        Command::VcsStage => run_now(app, &["add", "--all"], "Staged everything"),
        Command::VcsUnstage => run_now(app, &["reset"], "Unstaged everything"),
        Command::VcsFetch => stream_now(app, &["fetch", "--all"], "Fetching", "Fetched"),
        Command::VcsPull => stream_now(app, &["pull", "--ff-only"], "Pulling", "Pulled"),
        Command::VcsPush => push(app),
        Command::VcsOutput => show_output(app),
        Command::VcsFlip => flip(app),
        Command::VcsBranches => show_branches(app),
        Command::VcsHideBranch => hide_focused_branch(app),
        // Each of these needs a word from the user, and the palette can only
        // ever invoke a command bare — so a missing argument opens a minibuffer
        // prompt rather than being an error, the same way bare `:attach` does.
        Command::VcsNewBranch(name) => match name.trim() {
            "" => return ask(app, crate::mode::PromptKind::VcsBranch),
            name => new_branch(app, name),
        },
        Command::VcsCommit(message) => match message.trim() {
            // Asked before the prompt, not after: a message typed and then
            // refused is worse than being told there is nothing to commit.
            "" if staged(app) == 0 => nothing_staged(app),
            "" => return ask(app, crate::mode::PromptKind::VcsCommit),
            message => commit(app, message),
        },
        Command::VcsSetUpstream(target) => match target.trim() {
            "" => return ask(app, crate::mode::PromptKind::VcsUpstream),
            target => set_upstream(app, target),
        },

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

/// Ask for the missing half of a command in the minibuffer.
///
/// Returns true because the graph handled the command — it just is not finished
/// with it yet.
fn ask(app: &mut App, kind: crate::mode::PromptKind) -> bool {
    app.command_buf.clear();
    app.mode = crate::mode::Mode::Prompt { kind };
    true
}

fn staged(app: &App) -> usize {
    app.vcs.as_ref().map_or(0, |state| state.dag.work.staged())
}

fn nothing_staged(app: &mut App) {
    app.messages
        .show("Nothing staged — `w` stages a file, `+` stages everything");
}

/// Which way the graph is currently drawn.
fn orientation(app: &App) -> vcs::layout::Orientation {
    app.vcs
        .as_ref()
        .map_or(app.vcs_options.orientation, |s| s.options.orientation)
}

/// How many focus steps a paging command takes.
fn page(app: &App, cmd: &Command) -> usize {
    match cmd {
        // The ends of the graph: further than it can possibly be.
        Command::GotoFileStart | Command::GotoFileEnd => usize::MAX,
        // Half a screen, measured in blocks rather than cells, so paging lands
        // on a block rather than mid-border.  One step per block: arrows are
        // not somewhere the cursor stops.
        _ => {
            let Some(state) = app.vcs.as_ref() else { return 1 };
            let layout = state.layout(app.viewport_width as u16);
            let (along, _) = viewport(
                &layout,
                app.viewport_width as u16,
                app.viewport_height as u16,
            );
            ((along / 2) / layout.along_stride().max(1)).max(1) as usize
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
    let width = app.viewport_width as u16;
    match state.grab() {
        Ok(()) => {
            let held = state
                .grabbed
                .as_ref()
                .map(|f| crate::vcs_ui::describe_focus(&state.dag, f))
                .unwrap_or_default();
            // A grab with nowhere to go is not a grab: every motion key would
            // do nothing and say nothing, which reads as the editor having
            // stopped responding.  Refuse it, with the reason.
            if state.destinations(width) == 0 {
                state.grabbed = None;
                app.messages.show(format!(
                    "{held} has nowhere to go — everything else in the graph descends from it"
                ));
                return;
            }
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
            .map(Box::new)
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

/// Run a local git command now and report.
///
/// For the quick ones only — staging, a checkout, moving a ref: they finish in
/// milliseconds and have nothing to say while they do.  Anything that can run
/// a hook or talk to a network goes through [`stream_now`], or it blocks the
/// frame for as long as it takes with no sign that it is working.
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
    let args: Vec<&str> = if has_upstream {
        vec!["push"]
    } else {
        vec!["push", "--set-upstream", "origin", &branch]
    };
    stream_now(app, &args, "Pushing", "Pushed");
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

pub fn new_branch(app: &mut App, name: &str) {
    let Some(state) = app.vcs.as_ref() else { return };
    let Some(commit) = state.focused_commit() else {
        app.messages.show("No commit selected");
        return;
    };
    let args = ["checkout", "-b", name, commit.as_str()];
    run_now(app, &args, &format!("On new branch {name}"));
}

pub fn commit(app: &mut App, message: &str) {
    if staged(app) == 0 {
        nothing_staged(app);
        return;
    }
    // Streamed rather than run outright: this is the command that runs the
    // `pre-commit` hook, and a hook is the whole reason this path exists.
    stream_now(app, &["commit", "-m", message], "Committing", "Committed");
}

pub fn set_upstream(app: &mut App, target: &str) {
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

/// How much of one file's diff the pane will hold.
///
/// A generated file's diff can be a hundred thousand lines, and the pane is
/// something you scroll with `Ctrl+d` — reading past this was never going to
/// happen, and holding it costs the memory of the whole change.
const MAX_DIFF_LINES: usize = 4000;

/// Everything in the working tree that is not in a commit, worst first.
///
/// Ordered by how much it wants attention rather than alphabetically:
/// conflicts, then what is on its way into a commit, then the untracked
/// strays — which is also the order in which the list gets less urgent and
/// more interesting.
fn work_tree_entries(root: &std::path::Path) -> Vec<StageEntry> {
    let work = load::parse_status(&git(root, &["status", "--porcelain"]).unwrap_or_default());
    let mut entries = work.entries;
    entries.sort_by_key(|c| match () {
        () if c.is_conflicted() => 0,
        () if c.is_untracked() => 2,
        () => 1,
    });
    entries
        .into_iter()
        .map(|c| StageEntry {
            file: root.join(&c.path),
            detail: c.describe(),
            path: c.path,
            index: c.index,
            work: c.work,
        })
        .collect()
}

/// Open the work tree beside the selected file's diff — the staging view.
///
/// The HEAD block says *how many*; this says *which*, and shows *what*.  The
/// counts are the part you can see without being told; the list answers "what
/// is all this?", which is the question a repository full of scratch notebooks
/// raises; and the diff answers the one that actually stops someone
/// committing, which used to mean leaving the view and opening the file.
///
/// `Space` stages or unstages the selected file, so deciding and doing are the
/// same gesture in the same place.  It is an immediate action like the rest of
/// staging — nothing here can make a commit unreachable.
fn show_work_tree(app: &mut App) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    let entries = work_tree_entries(&root);
    if entries.is_empty() {
        app.messages
            .show("The working tree is clean — nothing uncommitted or untracked");
        return;
    }
    app.popup = Some(Popup::stage(entries));
    // Fill the first diff now rather than on the first keypress, or the pane
    // opens blank beside a file it is supposed to be describing.
    pump_stage_popup(app);
}

/// The branch picker: every ref, and whether the graph draws it.
///
/// A repository with thirty branches draws thirty tracks, and the two you are
/// working on are somewhere in the middle of them.  Hiding is a *display*
/// choice and nothing else — the branch is still there, still walked by
/// `derive`, still backed up by an apply — so the way back is always this list
/// rather than anything in git.
fn show_branches(app: &mut App) {
    let Some(state) = app.vcs.as_ref() else { return };
    let projection = state.plan.project(&state.dag);
    let items: Vec<crate::popup::ToggleItem> = state
        .dag
        .refs
        .iter()
        .map(|r| crate::popup::ToggleItem {
            label: r.name.clone(),
            detail: match projection.ref_target(&state.dag, &r.name) {
                Some(oid) => format!("{} {}", kind_word(r.kind), oid.short()),
                None => kind_word(r.kind).to_string(),
            },
            on: !state.options.hides(&r.name),
        })
        .collect();
    if items.is_empty() {
        app.messages.show("No branches yet — nothing to show or hide");
        return;
    }
    app.popup = Some(Popup::toggles("branches", items));
}

fn kind_word(kind: RefKind) -> &'static str {
    match kind {
        RefKind::Local => "branch",
        RefKind::Remote => "remote",
        RefKind::Tag => "tag",
    }
}

/// Run whatever the branch picker's last keypress asked for.
///
/// The same arrangement as [`pump_stage_popup`]: the key handler has no `App`
/// to reach the graph through, so it parks the request and this runs it.
pub fn pump_branch_popup(app: &mut App) {
    let width = app.viewport_width as u16;
    let Some(popup) = app.popup.as_mut() else { return };
    let crate::popup::PopupContent::Toggles(ref mut toggles) = popup.content else { return };
    let Some(index) = toggles.toggled.take() else { return };

    // `usize::MAX` is `a`: show everything.  A sentinel rather than a second
    // field, because the two are the same request — "these rows are on now".
    let names: Vec<String> = if index == usize::MAX {
        toggles
            .items
            .iter()
            .filter(|item| !item.on)
            .map(|item| item.label.clone())
            .collect()
    } else {
        toggles.items.get(index).map(|i| vec![i.label.clone()]).into_iter().flatten().collect()
    };
    for item in toggles.items.iter_mut() {
        if names.contains(&item.label) {
            item.on = !item.on;
        }
    }

    let Some(state) = app.vcs.as_mut() else { return };
    for name in &names {
        state.toggle_branch(name, width);
    }
    app.vcs_options = state.options.clone();
    let hidden = state.options.hidden.len();
    update_scroll(app);
    app.messages.show(match hidden {
        0 => "Every branch is in the graph".to_string(),
        1 => "1 branch hidden".to_string(),
        n => format!("{n} branches hidden"),
    });
}

/// `x` — take the branch under the cursor out of the picture.
///
/// The fast half of the gesture: hiding is nearly always something you decide
/// while looking straight at the branch you are tired of.  There is
/// deliberately no `x` to put one *back* — what is hidden is not on screen to
/// press a key on — so the message names the list that can.
fn hide_focused_branch(app: &mut App) {
    let width = app.viewport_width as u16;
    let Some(state) = app.vcs.as_mut() else { return };
    // A ref label, specifically — not `focused_branch`, which also answers for
    // the HEAD block.  HEAD is a big target sitting exactly where the cursor
    // starts, and hiding the branch you are standing on is not what pressing a
    // key there means.
    let Some(crate::vcs::layout::Focus::Ref(name)) = state.focus.clone() else {
        app.messages
            .show("Put the cursor on a branch label to hide it (b lists them all)");
        return;
    };
    let hidden = state.toggle_branch(&name, width);
    app.vcs_options = state.options.clone();
    update_scroll(app);
    app.messages.show(if hidden {
        format!("{name} hidden — b to bring it back")
    } else {
        format!("{name} is back in the graph")
    });
}

/// Run whatever the staging popup's last keypress asked for.
///
/// Called from the popup's `PopupAction::Continue` path, because that is the
/// only place with an `App` to reach git through — the same arrangement the
/// theme picker's live preview has.  Does nothing unless a staging popup is
/// open and something it describes has actually changed, so it is safe to call
/// on every key.
pub fn pump_stage_popup(app: &mut App) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    let Some(popup) = app.popup.as_mut() else { return };
    let crate::popup::PopupContent::Stage(ref mut stage) = popup.content else { return };

    let mut staged = None;
    if std::mem::take(&mut stage.toggle) {
        if let Some(entry) = stage.selected_entry() {
            let path = entry.path.clone();
            // A file with staged *and* unstaged parts is one `git add` away
            // from being ready, so Space finishes the job rather than undoing
            // the half already done.
            let args: Vec<&str> = if entry.fully_staged() {
                vec!["reset", "-q", "--", &path]
            } else {
                vec!["add", "--", &path]
            };
            let result = git(&root, &args);
            staged = Some((path, result));
        }
    }

    if let Some((path, result)) = staged {
        // Re-read rather than patch the two columns: `git add` on a file with
        // a conflict, or on one whose change was a rename, does not leave the
        // status this view guessed it would.
        let selected = stage.selected;
        stage.entries = work_tree_entries(&root);
        stage.select(
            stage
                .entries
                .iter()
                .position(|e| e.path == path)
                .unwrap_or(selected),
        );
        // The HEAD block's counts and the apply preflight both read this, and
        // both would be a keystroke out of date otherwise.
        if let Some(state) = app.vcs.as_mut() {
            state.dag.work = vcs::WorkTree::new(
                load::parse_status(&git(&root, &["status", "--porcelain"]).unwrap_or_default())
                    .entries,
            );
        }
        if let Err(why) = result {
            app.messages.show(why);
            return;
        }
        let Some(popup) = app.popup.as_mut() else { return };
        let crate::popup::PopupContent::Stage(ref mut again) = popup.content else { return };
        refresh_stage_diff(&root, again);
        return;
    }

    refresh_stage_diff(&root, stage);
}

/// Read the selected entry's diff, if the pane is not already showing it.
fn refresh_stage_diff(root: &std::path::Path, stage: &mut crate::popup::StageState) {
    let Some(entry) = stage.selected_entry() else {
        stage.diff = vec!["The working tree is clean.".to_string()];
        stage.loaded = None;
        return;
    };
    if stage.loaded.as_deref() == Some(entry.path.as_str()) {
        return;
    }
    let path = entry.path.clone();
    let untracked = entry.index == '?';
    let file = entry.file.clone();
    stage.diff = if untracked {
        untracked_preview(&file)
    } else {
        file_diff(root, &path)
    };
    stage.loaded = Some(path);
    stage.diff_scroll = 0;
}

/// What changed in `path`, against the last commit.
///
/// `HEAD` rather than the index, because the pane answers "what is not in a
/// commit yet" — which is the same thing the work tree itself means, and stays
/// the same picture as you stage, so `Space` never makes the diff you were
/// reading disappear.  The fallbacks cover a repository with no commits, where
/// there is no `HEAD` to diff against.
fn file_diff(root: &std::path::Path, path: &str) -> Vec<String> {
    for args in [
        vec!["diff", "HEAD", "--", path],
        vec!["diff", "--cached", "--", path],
        vec!["diff", "--", path],
    ] {
        let Ok(text) = git(root, &args) else { continue };
        if text.trim().is_empty() {
            continue;
        }
        return cap(crate::popup::sanitize_lines(&text));
    }
    vec![format!("No textual change in {path}.")]
}

/// An untracked file has no diff — it has contents, which is what you need to
/// see before deciding whether it belongs in the repository at all.
fn untracked_preview(file: &std::path::Path) -> Vec<String> {
    match std::fs::read_to_string(file) {
        Ok(text) => std::iter::once("(untracked — the whole file)".to_string())
            .chain(cap(crate::popup::sanitize_lines(&text)))
            .collect(),
        Err(why) => vec![format!("(untracked — cannot be shown as text: {why})")],
    }
}

fn cap(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > MAX_DIFF_LINES {
        lines.truncate(MAX_DIFF_LINES);
        lines.push(format!("… truncated at {MAX_DIFF_LINES} lines"));
    }
    lines
}

/// `git status`, verbatim, in a float.
///
/// The graph paraphrases the work tree — two summary rows on the HEAD block,
/// and `w` for the list of paths.  Both are this view's own wording, and
/// neither is what a git user checks when something looks wrong.  git's own
/// output is: it names the branch, how far it is from its upstream, what is
/// staged, what is not, and what it is in the middle of.  Shown rather than
/// re-worded, because the value of it is that it is the familiar text.
///
/// A focused float, like the SQL error and the cell peek: a passive one is
/// dismissed by the next keystroke, and this is several screens of text on a
/// busy tree.  `Esc` or `q` leaves.  Sized to its own widest line
/// ([`Popup::reference`]), because a text float clips rather than wraps and
/// git's output is laid out in columns.
fn show_git_status(app: &mut App) {
    let Some(root) = app.vcs.as_ref().map(|s| s.root.clone()) else { return };
    match git(&root, &["status"]) {
        Ok(text) => app.popup = Some(Popup::git_output("git status", text.trim_end())),
        Err(why) => app.messages.show(why),
    }
}

/// What `?` shows.
///
/// Written out rather than generated from the keymap because the keys are the
/// smaller half: this view has no git verb anywhere in it, so what a reader
/// needs is not "Space is grab" but *what grabbing a commit means* and what
/// does and does not touch the repository.  The walkthroughs are the point;
/// the tables are there so the walkthroughs can be short.
const HELP: &str = "\
The graph is the truth, and moving things in it is how history is changed.
You state a shape; the editor works out the git commands that produce it.

Nothing here touches the repository until you run :vc-apply — except the
everyday actions listed under `right now` below, none of which can make a
commit unreachable.

  MOVING AROUND
    h / l          back and forward through history
    j / k          between branches — each branch has a row of its own
                   (turned vertical with `o`, j / k walk history and h / l
                    step between branches: the keys follow the picture)
    J / K          half a screen
    gg / ge        the top / bottom of the graph
    Enter          on a branch: go there.  On a commit: read its diff
    y              copy the hash under the cursor

  REARRANGING HISTORY  (planned — nothing happens yet)
    Space          pick up what the cursor is on / put it down here
    d              remove the selected commit from the planned history
    m              merge the selection into the branch you are on
    u              take back the last planned change
    gx             discard the whole plan
    :vc-apply      show the git commands the plan becomes, and run them
    :vc-undo       put every branch back where it was before the last apply

  RIGHT NOW  (these run immediately)
    c              check out the branch or commit under the cursor
    s              git status, verbatim
    w              the work tree beside each file's diff — j/k pick a file,
                   Space stages or unstages it, Enter opens it
    +  /  -        stage / unstage everything

  A commit, fetch, pull or push opens *git output* and fills it as the
  command runs — a pre-commit hook prints there as it goes.  q comes back.

  Hiding a branch (b / x) only changes the picture: the branch is untouched,
  still walked when a plan is applied, still backed up.  A commit stays as
  long as any shown branch leads to it, so hiding a topic branch leaves the
  trunk it was cut from alone.  The modeline says how many are hidden.
    :vc-commit [message]       (asks for one if you leave it off)
    :vc-branch [name]          a new branch at the selected commit
    :vc-fetch  :vc-pull  :vc-push
    :vc-output                 the last command's output, as it ran
    b              which branches the graph draws (Space shows/hides, a all)
    x              take the branch under the cursor out of the picture
    o              turn the graph: history across, or down the screen
    r              re-read the repository
    ?              this sheet
    q              leave the graph

  WALKTHROUGH — move a commit onto a different parent
    1. Put the cursor on the commit you want to move (h / l / j / k).
    2. Space.  It turns amber: you are holding it.
    3. Move to the commit it should follow.  While you hold something, the
       cursor only stops where it could actually be dropped, and the graph
       rearranges under it so you can see the result before deciding.
    4. Space again.  The plan now has one change in it; the repository has
       none.  Esc instead of Space puts it back.
    5. :vc-apply lists what would run (cherry-pick, branch --force, …) and
       asks.  Everything above the commit you moved is recreated, because
       recreating a commit gives it a new hash and its children follow.

  WALKTHROUGH — move a branch to a different commit
    Put the cursor on the branch label itself (it sits on a block's top
    border), Space, move to the commit, Space.  Whether that becomes a
    fast-forward or a reset is not something you choose: it is whichever one
    the shape you drew means.

  WALKTHROUGH — merge one branch into another
    Merging is not a drag: nothing is being *moved*.  You say which commit is
    coming in, and it comes into whichever branch you are on.
    1. Be on the branch that should receive the merge.  Put the cursor on its
       label and press c if you are not — the modeline's ⎇ says where you are.
    2. Put the cursor on the tip of the branch coming in (its label, or the
       block it sits on).
    3. m.  A new block appears where the merge would be, amber, with two
       arrows: it does not exist yet.
    4. :vc-apply.  Nothing has touched the repository until then.
    Grabbing the last merge commit and moving it is a different request —
    it says that merge should have followed some other commit — which is a
    rewrite, and is refused: a merge cannot be replayed onto a new parent.

  WALKTHROUGH — undo an apply
    Every local branch is saved under refs/sakharov/undo/ before the first
    write, so :vc-undo restores all of them — including branches the plan
    never moved, whose commits a replay quietly changed underneath.

  WHAT IS REFUSED, AND WHY
    A drop that would make a commit its own ancestor is refused as you hover,
    not when you release — the preview never draws a graph git has no meaning
    for.  A plan whose replay list contains a merge is refused by name:
    cherry-pick cannot recreate one.  A dirty work tree blocks apply before
    anything is written.

  Arrows are not something you select.  An arrow is another name for the
  commit it leaves, so grab the commit and the arrow follows it.
";

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

/// `q` / `:bd` in a buffer the graph opened — a `*commit …*` diff, or the
/// `*git output*` transcript — back to the graph, which is the only place it
/// makes sense to go.  Returns false when neither is what is open, so the
/// caller carries on with its own close.
///
/// The same "back out of the temporary thing" gesture as `q` in a `*cell …*`
/// buffer or a derived table.  Without it `:bd` refused the `*…*` name
/// outright and `q` was unbound, which made the diff a buffer you could only
/// leave by naming somewhere else to go.
///
/// A transcript whose command is still running is *kept*: leaving to watch the
/// graph while a hook finishes must not throw away the output it is still
/// writing (`append_output` carries on into the stash), and `:vc-output`
/// brings it back.
pub(super) fn close_transient_buffer(app: &mut App) -> bool {
    if !app.in_commit_buffer() && !app.in_git_output_buffer() {
        return false;
    }
    if app.vcs_stream.is_none() {
        if let Some(id) = app.current_source_id() {
            app.special_buffer_ropes.remove(id.label());
        }
    }
    open(app);
    true
}

/// `:vc-output` — the last streamed command's transcript.
///
/// Cheap to reach again on purpose: the interesting output is a failing hook's,
/// and the natural thing to do on seeing it fail is to go and look at the code
/// — which means leaving the buffer, and then wanting it back.
pub fn show_output(app: &mut App) {
    if !app
        .special_buffer_ropes
        .contains_key(crate::app::GIT_OUTPUT_BUFFER)
    {
        app.messages.show("No git command has run yet");
        return;
    }
    super::buffers::switch_to_special_buffer(app, crate::app::GIT_OUTPUT_BUFFER);
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
    use crossterm::event::{KeyCode, KeyEvent};
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

    /// `h`/`l` walk from block to block.  Nothing in between: an arrow is not
    /// somewhere the cursor stops, so crossing the graph costs one press per
    /// commit rather than two.
    #[test]
    fn h_and_l_walk_the_blocks_with_nothing_in_between() {
        let mut app = app_in_graph();
        let start = focus(&app);
        super::handle(&mut app, &Command::MoveLeft);
        let next = focus(&app);
        assert_ne!(next, start);
        super::handle(&mut app, &Command::MoveRight);
        assert_eq!(focus(&app), start, "the walk is reversible");

        // Five commits, so five presses reach the oldest and a sixth has
        // nowhere left to go.  With arrows on the walk it took eleven.
        let mut seen = vec![focus(&app)];
        for _ in 0..12 {
            super::handle(&mut app, &Command::MoveLeft);
            let at = focus(&app);
            if seen.last() != Some(&at) {
                seen.push(at);
            }
        }
        assert!(seen.len() <= 8, "the walk stops on something extra: {seen:?}");
        for at in &seen {
            assert!(
                matches!(at, Focus::Commit(_) | Focus::Ref(_) | Focus::Head),
                "{at:?} is not a block or a label"
            );
        }
    }

    /// A repository with a `pre-commit` hook that prints and takes its time —
    /// the case this whole streaming path exists for.  Real git, because what
    /// is being tested is that a hook's output reaches the buffer while the
    /// hook is still running, and a mock hook is not a hook.
    fn repo_with_a_talkative_hook() -> Option<PathBuf> {
        let root = std::env::temp_dir().join(format!("sv-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).ok()?;
        let run = |args: &[&str]| git(&root, args).ok();
        run(&["init", "-b", "main"])?;
        run(&["config", "user.email", "test@example.com"])?;
        run(&["config", "user.name", "Test"])?;
        run(&["config", "commit.gpgsign", "false"])?;

        // An initial commit before the hook exists, so the snapshot the view
        // is built from has a history to show.
        std::fs::write(root.join("a.txt"), "hello\n").ok()?;
        run(&["add", "a.txt"])?;
        run(&["commit", "-m", "initial"])?;

        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(
            &hook,
            // Colour and a progress bar on purpose: both have to be gone by
            // the time the text is in a rope.
            "#!/bin/sh\nprintf '\\033[32mruff\\033[0m...Passed\\n'\nprintf '10%%\\r100%% done\\n'\nsleep 0.2\necho 'black...Passed'\n",
        )
        .ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).ok()?;
        }

        std::fs::write(root.join("a.txt"), "hello again\n").ok()?;
        run(&["add", "a.txt"])?;
        Some(root)
    }

    /// The end-to-end shape of a commit that runs a hook: the output buffer
    /// opens at once, fills as the hook prints, and reports the verdict — and
    /// the editor is *visibly* busy throughout, which is the whole complaint
    /// that started this (a synchronous commit froze the frame with no
    /// spinner, so a linter suite and a hang looked identical).
    #[test]
    fn a_commit_streams_its_hook_into_a_read_only_buffer() {
        let Some(root) = repo_with_a_talkative_hook() else { return };
        let mut app = app_in_graph();
        let load = load::start(root.clone(), 50);
        let dag = loop {
            if let Some(result) = load.poll() {
                break result.expect("read the repository");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        app.vcs = Some(VcsState::new(root.clone(), dag, 0));

        super::commit(&mut app, "second");
        assert!(app.in_git_output_buffer(), "the transcript is what is on screen");
        assert!(busy(&app), "the spinner has something to animate");

        // Drive the run loop until the job reports.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.vcs_stream.is_some() && std::time::Instant::now() < deadline {
            super::poll(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let text = app.buffer.rope.to_string();
        assert!(text.starts_with("$ git commit"), "no transcript header: {text}");
        assert!(text.contains("ruff...Passed"), "hook output missing: {text}");
        assert!(text.contains("black...Passed"), "later hook output missing: {text}");
        assert!(!text.contains('\u{1b}'), "an escape sequence reached the rope");
        assert!(!text.contains("10%"), "a progress bar's earlier frames were kept");
        assert!(text.contains("Committed"), "no verdict: {text}");
        assert!(!busy(&app), "the job is done");

        // Read-only: the transcript is evidence, not a document.
        super::super::execute(&mut app, &Command::EnterInsert);
        assert_eq!(app.mode, crate::mode::Mode::Normal, "Insert opened in a transcript");
        assert_eq!(app.buffer.rope.to_string(), text, "the transcript changed");

        // And `q` backs out of it, like every other buffer the graph opens.
        assert!(close_transient_buffer(&mut app), "q had nothing to back out of");
        assert!(!app.in_git_output_buffer(), "still in the transcript");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The cursor has to stay on screen in either picture — and the two
    /// pictures scroll different screen dimensions, which is exactly the sort
    /// of thing that is right in the one that was written first and silently
    /// wrong in the other.
    #[test]
    fn the_cursor_stays_in_view_whichever_way_the_graph_runs() {
        for orient in [
            vcs::layout::Orientation::Horizontal,
            vcs::layout::Orientation::Vertical,
        ] {
            let mut app = app_in_graph();
            // Small enough that the fixture cannot possibly fit.
            app.viewport_width = 60;
            app.viewport_height = 12;
            app.vcs.as_mut().unwrap().options.orientation = orient;

            // Walk to the oldest end and back, checking every step.
            for dir in [orient.back(), orient.forward()] {
                for _ in 0..12 {
                    super::handle(&mut app, &Command::MoveLeft);
                    app.vcs.as_mut().unwrap().step(dir, 60);
                    update_scroll(&mut app);

                    let state = app.vcs.as_ref().unwrap();
                    let layout = state.layout(60);
                    let Some(focus) = state.focus.clone() else { continue };
                    let Some(at) = layout.locate(&focus) else { continue };
                    let start = layout.display_along(at.along, layout.metrics.block_along);
                    let (along_extent, across_extent) = viewport(&layout, 60, 12);
                    assert!(
                        start + 1 > state.scroll_along
                            && start < state.scroll_along + along_extent,
                        "{orient:?}: {focus:?} is off screen along time \
                         (at {start}, window {}..{})",
                        state.scroll_along,
                        state.scroll_along + along_extent
                    );
                    let track = layout.track_at(at.across);
                    assert!(
                        track >= state.scroll_track
                            && track < state.scroll_track + layout.visible_tracks(across_extent),
                        "{orient:?}: {focus:?} is off screen across tracks"
                    );
                }
            }
        }
    }

    /// A grab that cannot be completed is refused rather than entered.
    ///
    /// This is what "the whole thing froze" was: holding something narrows the
    /// walk to places it could be dropped, and grabbing the oldest commit in a
    /// graph leaves none — every other commit descends from it, and a commit
    /// cannot follow its own descendant.  Every motion key then did nothing
    /// and said nothing, which is exactly what a wedged editor looks like.
    #[test]
    fn a_grab_with_nowhere_to_go_says_so_instead_of_going_quiet() {
        let mut app = app_in_graph();
        // `a` is the root: everything else in the fixture descends from it.
        app.vcs.as_mut().unwrap().focus = Some(Focus::Commit(Oid::new("a")));
        super::handle(&mut app, &Command::VcsGrab);

        assert!(app.vcs.as_ref().unwrap().grabbed.is_none(), "the grab was entered anyway");
        let said = app.messages.current().unwrap_or_default();
        assert!(said.contains("nowhere to go"), "it went quiet instead: {said:?}");
    }

    /// …and a motion that finds no destination *that way* says so too, rather
    /// than looking like a key that was not received.
    #[test]
    fn a_motion_with_no_destination_that_way_says_so() {
        let mut app = app_in_graph();
        // `c` can be dropped on `a` (older) but not on anything that descends
        // from it, so travelling toward the newest end has nowhere to stop.
        app.vcs.as_mut().unwrap().focus = Some(Focus::Commit(Oid::new("c")));
        super::handle(&mut app, &Command::VcsGrab);
        assert!(app.vcs.as_ref().unwrap().grabbed.is_some(), "the fixture must allow the grab");

        app.messages.clear();
        for _ in 0..8 {
            super::handle(&mut app, &Command::MoveRight);
        }
        let said = app.messages.current().unwrap_or_default();
        assert!(
            said.contains("Nothing that way"),
            "a motion that moved nothing said {said:?}"
        );
    }

    /// The two halves of the gesture: `x` takes the branch under the cursor
    /// out of the picture, and the picker is the only way back — since what is
    /// hidden is not on screen to press a key on.
    #[test]
    fn x_hides_the_branch_under_the_cursor_and_the_picker_brings_it_back() {
        let mut app = app_in_graph();
        app.vcs.as_mut().unwrap().focus = Some(Focus::Ref("feature".into()));
        super::handle(&mut app, &Command::VcsHideBranch);

        let state = app.vcs.as_ref().unwrap();
        assert!(state.options.hides("feature"), "the branch is still drawn");
        assert!(app.vcs_options.hides("feature"), "the session did not remember");
        // The cursor was *on* what just disappeared, so it has to have moved
        // to something that still exists.
        let layout = state.layout(120);
        let focus = state.focus.clone().expect("a cursor");
        assert!(layout.locate(&focus).is_some(), "the cursor is on {focus:?}, which is gone");

        // The picker lists it as off; Space turns it back on.
        super::handle(&mut app, &Command::VcsBranches);
        let popup = app.popup.as_mut().expect("the picker opened");
        let crate::popup::PopupContent::Toggles(ref mut toggles) = popup.content else {
            panic!("the picker is not a list of switches");
        };
        let at = toggles
            .items
            .iter()
            .position(|item| item.label == "feature")
            .expect("feature is listed");
        assert!(!toggles.items[at].on, "a hidden branch is listed as shown");
        toggles.toggled = Some(at);
        pump_branch_popup(&mut app);

        assert!(!app.vcs.as_ref().unwrap().options.hides("feature"));
        assert!(!app.vcs_options.hides("feature"));
        assert!(
            app.vcs.as_ref().unwrap().layout(120).block(&Oid::new("d")).is_some(),
            "the branch's commits did not come back"
        );
    }

    /// `a` in the picker is the way out of having hidden one thing too many.
    #[test]
    fn a_in_the_picker_shows_everything_again() {
        let mut app = app_in_graph();
        for name in ["feature", "main"] {
            app.vcs.as_mut().unwrap().focus = Some(Focus::Ref(name.into()));
            super::handle(&mut app, &Command::VcsHideBranch);
        }
        assert_eq!(app.vcs_options.hidden.len(), 2);

        super::handle(&mut app, &Command::VcsBranches);
        let popup = app.popup.as_mut().expect("the picker opened");
        let crate::popup::PopupContent::Toggles(ref mut toggles) = popup.content else {
            panic!("the picker is not a list of switches");
        };
        toggles.toggled = Some(usize::MAX);
        pump_branch_popup(&mut app);

        assert!(app.vcs_options.hidden.is_empty(), "something stayed hidden");
        assert!(app.vcs.as_ref().unwrap().options.hidden.is_empty());
    }

    /// `gg` goes to the top of the graph and `ge` to the bottom, in a graph
    /// exactly as in a buffer.  Which end of *history* that is depends on the
    /// picture — the newest commit is at the right in one and at the top in
    /// the other — and saying it the other way round sent `gg` walking away
    /// from the top of the screen.
    #[test]
    fn gg_and_ge_reach_the_ends_of_the_screen_not_the_ends_of_history() {
        for orient in [
            vcs::layout::Orientation::Horizontal,
            vcs::layout::Orientation::Vertical,
        ] {
            let mut app = app_in_graph();
            app.vcs.as_mut().unwrap().options.orientation = orient;

            let screen_pos = |app: &App| {
                let state = app.vcs.as_ref().unwrap();
                let layout = state.layout(120);
                let at = layout.locate(state.focus.as_ref().unwrap()).expect("a cursor");
                layout.screen(at.along, at.across)
            };
            let forward_axis = |(x, y): (u16, u16)| match orient {
                vcs::layout::Orientation::Horizontal => x,
                vcs::layout::Orientation::Vertical => y,
            };

            super::handle(&mut app, &Command::GotoFileStart);
            let top = forward_axis(screen_pos(&app));
            super::handle(&mut app, &Command::GotoFileEnd);
            let bottom = forward_axis(screen_pos(&app));
            assert!(
                top < bottom,
                "{orient:?}: gg landed at {top} and ge at {bottom} — the wrong way round"
            );

            // …and paging goes the same way as `ge`, not the opposite one.
            super::handle(&mut app, &Command::GotoFileStart);
            super::handle(&mut app, &Command::PageDown);
            assert!(
                forward_axis(screen_pos(&app)) > top,
                "{orient:?}: J paged backwards"
            );
        }
    }

    /// A preference stated once has to survive the view going away: the graph
    /// is closed and reopened constantly (`q`, a file, `H`/`L`), and `close`
    /// throws the session's state away.
    #[test]
    fn the_orientation_survives_the_view_being_closed_and_reopened() {
        let mut app = app_in_graph();
        let start = app.vcs.as_ref().unwrap().options.orientation;
        super::handle(&mut app, &Command::VcsFlip);
        let flipped = app.vcs.as_ref().unwrap().options.orientation;
        assert_ne!(flipped, start, "the flip did nothing");
        assert_eq!(app.vcs_options.orientation, flipped, "the session did not remember");

        // The view is closed — its state, and the orientation with it, is gone.
        app.vcs = None;
        // …and reopened, which is a fresh read of the repository.
        let fresh = app_in_graph().vcs.take().expect("a fixture graph");
        install(&mut app, fresh.root.clone(), fresh.dag);
        assert_eq!(
            app.vcs.as_ref().unwrap().options.orientation,
            flipped,
            "the graph came back the way the config says, not the way it was left"
        );
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

    /// `?` is the whole view explained, since none of it is a git verb and
    /// there is no command line to read the answer off.
    #[test]
    fn question_mark_opens_the_help_float() {
        let mut app = app_in_graph();
        assert!(
            app.keymap
                .lookup(crate::keymap::Layer::Vcs, &crate::keymap::KeyBinding::char('?'))
                .is_some_and(|c| matches!(c, [Command::VcsHelp])),
            "? has to be bound in the graph"
        );
        super::handle(&mut app, &Command::VcsHelp);
        let popup = app.popup.as_ref().expect("the help opened");
        let crate::popup::PopupContent::Text(ref text) = popup.content else {
            panic!("the help is a text float");
        };
        assert!(text.focused, "it is read, not glanced at");
        let body = text.lines.join("\n");
        for topic in ["WALKTHROUGH", ":vc-apply", "Space"] {
            assert!(body.contains(topic), "the help never mentions {topic}");
        }
    }

    /// Every single-character key the help lists has to actually be bound in
    /// the graph, or the sheet teaches presses that do nothing.  Listed here
    /// rather than parsed out of the prose: the help is written for a reader,
    /// and a parser for it would be pinning the formatting, not the keys.
    #[test]
    fn the_help_only_advertises_keys_that_are_bound() {
        let app = app_in_graph();
        for key in [
            'h', 'l', 'j', 'k', 'J', 'K', 'c', 's', 'w', '+', '-', 'r', 'q', 'y', 'd', 'm',
            'u', ' ', '?',
        ] {
            assert!(
                super::HELP.contains(key),
                "the help stopped mentioning `{key}`"
            );
            assert!(
                app.keymap
                    .lookup_layered(crate::keymap::Layer::Vcs, &crate::keymap::KeyBinding::char(key))
                    .is_some(),
                "the help advertises `{key}`, which is not bound"
            );
        }
    }

    /// `s` is `git status`, verbatim.  The graph paraphrases the work tree
    /// everywhere else; this is the one place it shows git's own words, which
    /// is what a git user checks when something looks wrong.
    #[test]
    fn s_shows_git_status_verbatim_in_a_float() {
        let here = std::env::current_dir().expect("cwd");
        let Some(root) = load::discover_root(&here) else {
            return; // not a checkout (a source tarball); nothing to ask git
        };
        let mut app = app_in_graph();
        app.vcs.as_mut().expect("the graph").root = root;
        super::handle(&mut app, &Command::VcsGitStatus);

        let popup = app.popup.as_ref().expect("the float opened");
        let crate::popup::PopupContent::Text(ref text) = popup.content else {
            panic!("git status is shown as text, not a list");
        };
        assert!(text.focused, "a passive float would be dismissed by the next key");
        let body = text.lines.join("\n");
        assert!(
            body.contains("On branch") || body.contains("HEAD detached"),
            "this is not git's own output: {body}"
        );
    }

    /// A command that wants a word asks for it, rather than reporting that it
    /// does not exist.
    ///
    /// `:version-control-commit` refusing to *parse* bare meant the command
    /// line answered "Unknown command", which says the wrong thing entirely —
    /// and the palette, which can only ever invoke a command bare, could never
    /// reach any of these three at all.
    #[test]
    fn a_command_missing_its_argument_asks_for_it_in_the_minibuffer() {
        for (name, kind) in [
            ("version-control-branch", crate::mode::PromptKind::VcsBranch),
            ("version-control-upstream", crate::mode::PromptKind::VcsUpstream),
        ] {
            let parsed = Command::parse(name)
                .unwrap_or_else(|| panic!("`:{name}` does not parse bare"));
            let mut app = app_in_graph();
            super::handle(&mut app, &parsed);
            assert_eq!(
                app.mode,
                crate::mode::Mode::Prompt { kind },
                "`:{name}` did not ask for its argument"
            );
        }
    }

    /// …except when the answer could not be used anyway: asking for a commit
    /// message and *then* refusing it would waste the typing.
    #[test]
    fn a_commit_with_nothing_staged_says_so_instead_of_asking_for_a_message() {
        let parsed = Command::parse("version-control-commit").expect("it parses bare");
        let mut app = app_in_graph();
        super::handle(&mut app, &parsed);
        assert_eq!(app.mode, crate::mode::Mode::Normal);
        assert!(app.messages.current().unwrap_or_default().contains("Nothing staged"));

        // With something staged it asks.
        app.vcs.as_mut().unwrap().dag.work = crate::vcs::WorkTree::new(vec![
            crate::vcs::Change { path: "a.rs".into(), index: 'M', work: ' ' },
        ]);
        super::handle(&mut app, &parsed);
        assert_eq!(
            app.mode,
            crate::mode::Mode::Prompt { kind: crate::mode::PromptKind::VcsCommit }
        );
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
        app.vcs.as_mut().unwrap().focus = Some(Focus::Commit(Oid::new("c")));
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
        app.vcs.as_mut().unwrap().focus = Some(Focus::Commit(Oid::new("c")));
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

    /// A throwaway repository with something in every state the staging view
    /// distinguishes.  Real git, because what is under test is the reading of
    /// `git status` and the effect of `git add` — a mock would test the mock.
    fn repo_with_changes(name: &str) -> Option<PathBuf> {
        // Named per test: they run concurrently, and two of them sharing a
        // work tree is one staging the other's files out from under it.
        let root = std::env::temp_dir().join(format!("sv-stage-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let run = |args: &[&str]| git(&root, args).ok();
        run(&["init", "-b", "main"])?;
        run(&["config", "user.email", "test@example.com"])?;
        run(&["config", "user.name", "Test"])?;
        run(&["config", "commit.gpgsign", "false"])?;
        std::fs::write(root.join("tracked.rs"), "fn main() {}\n").ok()?;
        run(&["add", "tracked.rs"])?;
        run(&["commit", "-m", "first"])?;
        // One edited-but-unstaged file, and one nobody has told git about.
        std::fs::write(root.join("tracked.rs"), "fn main() {\n    let x = 1;\n}\n").ok()?;
        std::fs::write(root.join("scratch.ipynb"), "{}\n").ok()?;
        Some(root)
    }

    fn stage_state(app: &App) -> &crate::popup::StageState {
        let popup = app.popup.as_ref().expect("the staging view is open");
        let crate::popup::PopupContent::Stage(ref state) = popup.content else {
            panic!("the work tree is shown as a staging view");
        };
        state
    }

    /// The staging view is the answer to "what is all this?" — the HEAD block
    /// says how many, this says which, and the pane says *what*, which is the
    /// question that actually stops someone committing.  An untracked scratch
    /// file has to be in it or the feature misses its whole point.
    #[test]
    fn the_staging_view_lists_the_work_tree_and_shows_the_selection_s_diff() {
        let Some(root) = repo_with_changes("list") else { return };
        let mut app = app_in_graph();
        app.vcs.as_mut().expect("the graph").root = root.clone();
        super::handle(&mut app, &Command::VcsStatus);

        let state = stage_state(&app);
        let paths: Vec<&str> = state.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["tracked.rs", "scratch.ipynb"],
            "what is on its way into a commit first, then the strays"
        );
        // The pane is filled when the view opens, not on the first keypress.
        assert_eq!(state.loaded.as_deref(), Some("tracked.rs"));
        assert!(
            state.diff.iter().any(|l| l.contains("+    let x = 1;")),
            "the diff pane is not showing the change: {:?}",
            state.diff
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Space stages the selected file and the view immediately shows it as
    /// staged — deciding and doing in one place is the whole point of the
    /// pane being there.
    #[test]
    fn space_stages_the_selected_file_and_the_view_follows() {
        let Some(root) = repo_with_changes("space") else { return };
        let mut app = app_in_graph();
        app.vcs.as_mut().expect("the graph").root = root.clone();
        super::handle(&mut app, &Command::VcsStatus);
        assert!(!stage_state(&app).entries[0].staged(), "it starts unstaged");

        crate::input::handle_key(&mut app, KeyEvent::from(KeyCode::Char(' ')));

        let state = stage_state(&app);
        assert_eq!(state.entries[0].path, "tracked.rs");
        assert!(state.entries[0].fully_staged(), "Space did not stage it");
        assert_eq!(state.selected, 0, "the cursor stayed on the file it acted on");
        // The graph behind it agrees: the HEAD block's counts and the apply
        // preflight both read this, and both would be a keystroke stale.
        assert_eq!(app.vcs.as_ref().unwrap().dag.work.staged(), 1);

        // …and again puts it back.
        crate::input::handle_key(&mut app, KeyEvent::from(KeyCode::Char(' ')));
        assert!(!stage_state(&app).entries[0].staged(), "Space did not unstage it");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Moving the selection re-reads the pane, so the diff on screen is always
    /// the diff of the file under the cursor.
    #[test]
    fn moving_the_selection_reloads_the_diff_pane() {
        let Some(root) = repo_with_changes("move") else { return };
        let mut app = app_in_graph();
        app.vcs.as_mut().expect("the graph").root = root.clone();
        super::handle(&mut app, &Command::VcsStatus);

        crate::input::handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        let state = stage_state(&app);
        assert_eq!(state.selected, 1);
        assert_eq!(state.loaded.as_deref(), Some("scratch.ipynb"));
        // An untracked file has no diff — it has contents, which is what you
        // need before deciding whether it belongs in the repository at all.
        assert!(
            state.diff.first().is_some_and(|l| l.contains("untracked")),
            "{:?}",
            state.diff
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A clean tree says so rather than opening an empty list.    /// A clean tree says so rather than opening an empty list.
    #[test]
    fn a_clean_work_tree_says_so_instead_of_listing_nothing() {
        let root = std::env::temp_dir().join(format!("sv-stage-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a temp directory");
        if git(&root, &["init", "-b", "main"]).is_err() {
            return;
        }
        let mut app = app_in_graph();
        app.vcs.as_mut().expect("the graph").root = root.clone();
        super::handle(&mut app, &Command::VcsStatus);
        assert!(app.popup.is_none());
        assert!(app.messages.current().unwrap_or_default().contains("clean"));
        let _ = std::fs::remove_dir_all(&root);
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
