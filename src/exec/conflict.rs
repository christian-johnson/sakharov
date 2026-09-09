//! The merge-conflict resolver: opening it, moving around it, and the one
//! keystroke in it that writes.
//!
//! ## What this view is for
//!
//! Git's answer to a conflict is to put both versions in your file between
//! markers labelled `HEAD` and a hash, and leave.  This view replaces that
//! with three facts git has but does not show: **who each side is** (a branch,
//! an author, a date — resolved from whatever operation is in progress),
//! **what the common ancestor said** (so you can tell a real disagreement from
//! a one-sided change), and **which way round ours and theirs are**, which
//! silently inverts during a rebase or a cherry-pick.
//!
//! ## Two kinds of key, and one that writes
//!
//! Everything except `Enter` works on the in-memory choices: the file on disk
//! is untouched, and closing the view throws the choices away without having
//! changed anything.  `Enter` writes the merged file and stages it — and even
//! that is one command from being undone (`:conflict-revert`), because git
//! holds all three stages until the operation finishes.

use std::path::PathBuf;

use crate::{
    app::{App, CONFLICT_BUFFER},
    command::Command,
    conflict::{
        self,
        load::{self, ConflictLoad},
        state::ConflictState,
        write, Choice, Side,
    },
    popup::{KeyHint, Popup},
    source::SourceId,
    stash::Stash,
    view::View,
    vcs::load::{discover_root, git},
};

/// What the user types to get somewhere a text command would work.
const ESCAPE_HATCH: &str = ":conflict-close";

// ---------------------------------------------------------------------------
// Opening and closing
// ---------------------------------------------------------------------------

/// `:conflicts` — read the conflicted index and hand the resolver the screen.
pub fn open(app: &mut App) {
    // The repository already on screen wins over the working directory.  The
    // resolver is most often reached *from* the graph — an apply that
    // conflicted, or `Enter` in the staging view — and the graph may well be
    // showing a repository other than the one the editor was launched in.
    // Rediscovering from the cwd there reads a different checkout's conflicts,
    // which is wrong in the quiet way: it reports "nothing is conflicted"
    // about a repository nobody asked about.
    let root = match app.vcs.as_ref().map(|s| s.root.clone()) {
        Some(root) => Some(root),
        None => {
            let hint = app
                .current_source_id()
                .and_then(|id| id.as_path().map(PathBuf::from))
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            discover_root(&hint)
        }
    };
    let Some(root) = root else {
        app.messages
            .show("Not inside a git repository (`git init` to start one)");
        return;
    };

    enter(app);
    // A session stashed on the way out comes back whole — same file, same
    // hunk, same choices.  Losing a half-finished resolution to a buffer
    // switch would make the view unusable for the exact case it exists for:
    // going to look at the code before deciding.
    let id = SourceId::virtual_named(CONFLICT_BUFFER);
    if app.stashes.view_of(&id) == Some(View::Conflict) {
        if let Some(Stash::Conflict(state)) = app.stashes.take(&id) {
            app.conflict = Some(*state);
            return;
        }
    }
    app.conflict_pending = Some(load::start(root));
    app.messages.show("Reading the conflicts…  (? for the keys)");
}

/// Hand the screen to the resolver, detaching the buffer behind it.
///
/// Path-less on purpose, like the grid and the graph: while the resolver is
/// what is on screen, no save path in the editor may hold a writable handle on
/// a file — least of all on a conflicted one, whose only complete copy is in
/// git's index.
fn enter(app: &mut App) {
    super::teardown_current_buffer(app);
    app.conflict = None;
    app.buffer = crate::buffer::Buffer::new_empty();
    app.buffer.path = None;
    app.selection = crate::selection::Selection::point(0);
    let id = SourceId::virtual_named(CONFLICT_BUFFER);
    app.open_buffers.retain(|stored| *stored != id);
    app.open_buffers.push(id);
    app.show_splash = false;
}

pub fn close(app: &mut App) {
    app.conflict = None;
    app.conflict_pending = None;
    app.stashes.discard(&SourceId::virtual_named(CONFLICT_BUFFER));
    app.open_buffers
        .retain(|id| *id != SourceId::virtual_named(CONFLICT_BUFFER));
    match app.open_buffers.last().cloned() {
        Some(id) => super::open_path(app, &id.to_path()),
        None => super::buffers::switch_to_special_buffer(app, crate::app::SCRATCH_BUFFER),
    }
}

/// Open the resolver instead of the file, when `path` is a conflicted one.
///
/// The staging view (`w`) lists conflicts first and marks them red, and
/// `Enter` there means "show me this file" — which for a conflicted file used
/// to mean the raw text, markers and all.  That is precisely the thing this
/// view replaces, so the one place it is most likely to be reached from is
/// routed here.  Returns false for everything else, so the ordinary open
/// carries on.
///
/// Read from the snapshot's work tree rather than by asking git: the staging
/// view already keeps that current (every toggle writes a fresh read back into
/// `Dag.work`), and a second query per Enter would be a second answer that
/// could disagree with the list on screen.
pub fn open_if_conflicted(app: &mut App, path: &std::path::Path) -> bool {
    let Some(state) = app.vcs.as_ref() else { return false };
    let root = state.root.clone();
    let conflicted = state
        .dag
        .work
        .entries
        .iter()
        .filter(|c| c.is_conflicted())
        .any(|c| root.join(&c.path) == path);
    if !conflicted {
        return false;
    }
    // Recorded rather than applied: the read is asynchronous, so there is
    // usually no snapshot yet to find the file in.  `reinstall` honours it.
    app.conflict_want = path
        .strip_prefix(&root)
        .ok()
        .map(|rel| rel.to_string_lossy().into_owned());
    open(app);
    land_on_wanted_file(app);
    true
}

/// Move to the file named by `conflict_want`, once there is a snapshot.
fn land_on_wanted_file(app: &mut App) {
    let Some(wanted) = app.conflict_want.clone() else { return };
    let Some(state) = app.conflict.as_mut() else { return };
    app.conflict_want = None;
    if let Some(idx) = state.set.files.iter().position(|f| f.path == wanted) {
        state.file = idx;
        state.nth = 0;
        state.scroll = 0;
    }
    announce_file(app);
}

/// Re-read the conflicted index, **keeping** the answers already given.
///
/// Answers are kept because refreshing is what you do after resolving a file
/// in another window or editing one by hand, and throwing away the twenty
/// hunks you had already settled would make the key unusable.  A file whose
/// conflict count changed underneath loses its answers, since they no longer
/// address the same regions — that is checked in [`reinstall`].
fn refresh(app: &mut App) {
    let Some(root) = app.conflict.as_ref().map(|s| s.set.root.clone()) else { return };
    app.conflict_pending = Some(load::start(root));
}

/// Collect a finished read.  Returns true when the screen changed.
pub fn poll(app: &mut App) -> bool {
    let Some(result) = app.conflict_pending.as_ref().and_then(ConflictLoad::poll) else {
        return false;
    };
    app.conflict_pending = None;
    match result {
        Ok(set) => reinstall(app, set),
        Err(why) => {
            app.messages.show(format!("Cannot read the conflicts: {why}"));
            // Nothing to show and nothing to go back to but the file that was
            // open: stranding the user in the detached empty buffer would be
            // the same dead end the table load avoids.
            if app.conflict.is_none() {
                close(app);
            }
        }
    }
    true
}

/// Install a freshly-read snapshot, carrying over what has already been
/// answered.
fn reinstall(app: &mut App, mut set: conflict::ConflictSet) {
    if let Some(old) = app.conflict.as_ref() {
        for file in &mut set.files {
            let Some(previous) = old.set.files.iter().find(|f| f.path == file.path) else {
                continue;
            };
            // Only when the shape still matches: a file whose conflict count
            // changed is not the file those answers were about, and applying
            // them by position would write text the user never chose.
            if previous.choices.len() == file.choices.len() {
                file.choices = previous.choices.clone();
                file.history = previous.history.clone();
            }
        }
    }

    if set.is_empty() {
        let done = app.conflict.is_some();
        app.conflict = None;
        app.messages.show(match done {
            true => "Every conflict is resolved — `g C` continues the operation".to_string(),
            false => "Nothing is conflicted".to_string(),
        });
        close(app);
        return;
    }

    let files = set.files.len();
    let hunks: usize = set.files.iter().map(conflict::ConflictFile::conflict_count).sum();
    let operation = set.operation.describe();
    let keep = app.conflict.as_ref().map(|s| (s.file, s.nth, s.show_base));
    let mut state = ConflictState::new(set);
    if let Some((file, nth, show_base)) = keep {
        state.file = file.min(state.set.files.len() - 1);
        state.focus(nth);
        state.show_base = show_base;
    }
    app.conflict = Some(state);
    // The one warning worth putting in front of somebody before they start.
    // In a replay git has checked out the base and is applying your commits to
    // it, so the pane labelled with `HEAD` is somebody else's work — the
    // inversion behind most conflicts resolved the wrong way round.  The panes
    // say it too, but a line on the way in is what stops the first `a` being
    // pressed out of habit.
    let inverted = match app.conflict.as_ref().map(|s| s.set.operation) {
        Some(op) if !op.left_is_yours() => "  ·  your work is on the RIGHT in a replay",
        _ => "",
    };
    app.messages.show(format!(
        "{hunks} conflict{} in {files} file{} from the {operation}{inverted}  (? for the keys)",
        if hunks == 1 { "" } else { "s" },
        if files == 1 { "" } else { "s" },
    ));
    // A file asked for before the read finished — the staging view's `Enter`.
    land_on_wanted_file(app);
}

/// Follow the cursor.  Called once per frame from the run loop.
pub fn update_scroll(app: &mut App) {
    let Some(chrome) = crate::view::Chrome::split(ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: app.viewport_width as u16,
        height: app.viewport_height as u16 + crate::view::CHROME_ROWS,
    }) else {
        return;
    };
    if let Some(state) = app.conflict.as_mut() {
        state.update_scroll(chrome.content);
    }
}

// ---------------------------------------------------------------------------
// Command routing
// ---------------------------------------------------------------------------

/// Handle `cmd` in the resolver.  Returns true when it was ours.
pub fn handle(app: &mut App, cmd: &Command) -> bool {
    debug_assert_eq!(app.view(), View::Conflict);

    match cmd {
        // Motion.  `h`/`l` choose a pane and `j`/`k` walk the hunks, so the
        // keys mean in the panes' geometry what they mean everywhere else —
        // except when the panes are stacked, where the two axes swap with
        // them (see `travel`).
        Command::MoveLeft | Command::MoveRight | Command::MoveUp | Command::MoveDown => {
            motion(app, cmd);
        }
        Command::ConflictNextHunk | Command::SearchNext => step(app, true),
        Command::ConflictPrevHunk | Command::SearchPrev => step(app, false),
        Command::ConflictNextFile => step_file(app, true),
        Command::ConflictPrevFile => step_file(app, false),
        Command::PageDown | Command::GotoFileEnd => step(app, true),
        Command::PageUp | Command::GotoFileStart => step(app, false),

        // The choices.  None of these touch the disk.
        Command::ConflictTakeSide => toggle_focused_side(app),
        Command::ConflictTakeLeft => take_only(app, Side::Left, false),
        Command::ConflictTakeRight => take_only(app, Side::Right, false),
        Command::ConflictTakeLeftAll => take_only(app, Side::Left, true),
        Command::ConflictTakeRightAll => take_only(app, Side::Right, true),
        Command::ConflictEditHunk => edit_hunk(app),
        Command::Undo | Command::VcsUndoEdit => undo(app),

        // The picture.
        Command::ConflictToggleBase => toggle_base(app),
        Command::ConflictDiff => show_diff(app),
        Command::ConflictHelp => {
            app.popup = Some(Popup::key_sheet(
                "merge conflicts",
                key_sheet(),
                "nothing is written until Enter · Esc closes",
            ));
        }
        Command::ConflictRefresh => {
            refresh(app);
            app.messages.show("Re-reading the conflicted files…");
        }
        Command::ConflictOpen => app.messages.show("Already resolving conflicts"),
        Command::ConflictClose | Command::BufferClose | Command::VcsClose => close(app),
        Command::EnterNormal => app.messages.show("Nothing to cancel — q leaves the resolver"),

        // The one key that writes.
        Command::ConflictWriteFile | Command::VcsEnter | Command::TableOpenCell => {
            write_file(app);
        }
        Command::ConflictRevertFile => revert_file(app),

        // Yanking the resolved text is what a `y` here plausibly means, and
        // there is no selection to yank instead.
        Command::YankSelection => yank_merged(app),

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

/// Which screen axis walks the hunks, and which chooses a pane.
///
/// Side by side, the panes are left and right, so `h`/`l` choose one and
/// `j`/`k` walk the file.  Stacked, the panes are one above the other and the
/// two swap over — the keys follow the picture, exactly as they do when the
/// commit graph is flipped, so there is nothing extra to remember.
fn motion(app: &mut App, cmd: &Command) {
    let with_base = app.conflict.as_ref().is_some_and(|s| s.show_base);
    let stacked = conflict::layout::arrangement(app.viewport_width as u16, with_base)
        == conflict::layout::Arrangement::Stacked;
    let (across, forward) = match cmd {
        Command::MoveLeft => (!stacked, false),
        Command::MoveRight => (!stacked, true),
        Command::MoveUp => (stacked, false),
        Command::MoveDown => (stacked, true),
        _ => return,
    };
    match across {
        true => choose_side(app, forward),
        false => step_any(app, forward),
    }
}

/// Move the focus between panes.
///
/// The base pane is never focusable: it is the *ancestor*, which is not one of
/// the things that can be taken — offering it as a place the cursor stops
/// would be offering a choice that is not a choice, the same reason arrows are
/// not focusable in the commit graph.
fn choose_side(app: &mut App, forward: bool) {
    let Some(state) = app.conflict.as_mut() else { return };
    let next = match forward {
        true => Side::Right,
        false => Side::Left,
    };
    if state.side == next {
        return;
    }
    state.side = next;
}

/// Walk to the next hunk in either direction, answered or not.
///
/// `j`/`k` read the file; `n`/`N` tour what is left.  Two different questions,
/// so two different keys — a `j` that skipped settled hunks would make it
/// impossible to go back and look at one.
fn step_any(app: &mut App, forward: bool) {
    let Some(state) = app.conflict.as_mut() else { return };
    let n = state.current().map_or(0, conflict::ConflictFile::conflict_count);
    if n == 0 {
        return;
    }
    state.nth = match forward {
        true => (state.nth + 1) % n,
        false => (state.nth + n - 1) % n,
    };
}

fn step(app: &mut App, forward: bool) {
    let Some(state) = app.conflict.as_mut() else { return };
    if state.step(forward) {
        return;
    }
    // Nothing to step to is a *fact about the file*, and saying nothing here
    // is indistinguishable from a wedged editor — the one report the commit
    // graph's navigation ever had.
    let decided = state.current().is_some_and(conflict::ConflictFile::is_decided);
    let message = match decided {
        true => "Every conflict in this file is answered — Enter writes it",
        false => "No other conflict to go to",
    };
    app.messages.show(message);
}

fn step_file(app: &mut App, forward: bool) {
    let Some(state) = app.conflict.as_mut() else { return };
    if !state.step_file(forward) {
        app.messages.show("This is the only conflicted file");
        return;
    }
    announce_file(app);
}

fn announce_file(app: &mut App) {
    let Some(state) = app.conflict.as_ref() else { return };
    let (at, total) = (state.file + 1, state.set.files.len());
    let Some(file) = state.current() else { return };
    let left = file.undecided_count();
    app.messages.show(format!(
        "{} — file {at} of {total}, {left} of {} conflict{} left",
        file.path,
        file.conflict_count(),
        if file.conflict_count() == 1 { "" } else { "s" }
    ));
}

// ---------------------------------------------------------------------------
// Choosing
// ---------------------------------------------------------------------------

fn toggle_focused_side(app: &mut App) {
    let Some(state) = app.conflict.as_mut() else { return };
    let (nth, side) = (state.nth, state.side);
    let next = state.choice().toggle(side);
    let Some(file) = state.current_mut() else { return };
    file.choose(nth, next.clone());
    report_choice(app, &next);
}

fn take_only(app: &mut App, side: Side, every: bool) {
    let Some(state) = app.conflict.as_mut() else { return };
    let nth = state.nth;
    let Some(file) = state.current_mut() else { return };
    if !every {
        file.choose(nth, Choice::only(side));
        let choice = Choice::only(side);
        report_choice(app, &choice);
        return;
    }
    // Only the *unanswered* ones: `A` is "settle the rest the same way", and
    // overwriting deliberate answers already given would make it a key nobody
    // could risk pressing.
    let targets: Vec<usize> = (0..file.conflict_count())
        .filter(|i| file.choices.get(*i).is_some_and(|c| !c.is_decided()))
        .collect();
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
    let left = side_name(app, Side::Left);
    let right = side_name(app, Side::Right);
    let message = match choice {
        Choice::Edited(_) => "Using your own text for this conflict".to_string(),
        Choice::Sides { left: true, right: true } => {
            format!("Keeping both — {left}, then {right}")
        }
        Choice::Sides { left: true, right: false } => format!("Taking {left}"),
        Choice::Sides { left: false, right: true } => format!("Taking {right}"),
        Choice::Sides { left: false, right: false } => {
            "Taking neither — this section is deleted".to_string()
        }
    };
    let remaining = app
        .conflict
        .as_ref()
        .and_then(|s| s.current())
        .map_or(0, conflict::ConflictFile::undecided_count);
    let tail = match remaining {
        0 => "  ·  all answered, Enter writes the file".to_string(),
        n => format!("  ·  {n} left"),
    };
    app.messages.show(format!("{message}{tail}"));
}

/// A short name for a side, for the message line.
///
/// Taken from the resolved label rather than from "ours"/"theirs" — the
/// message line is one of the places the inversion would otherwise creep back
/// in through the wording.
fn side_name(app: &App, side: Side) -> String {
    let Some(state) = app.conflict.as_ref() else { return String::new() };
    let label = state.set.label(side);
    match label.role.split(" — ").next().unwrap_or_default() {
        "" => format!("the {} side", if side == Side::Left { "left" } else { "right" }),
        role => role.to_string(),
    }
}

fn undo(app: &mut App) {
    let Some(state) = app.conflict.as_mut() else { return };
    let Some(file) = state.current_mut() else { return };
    let Some(nth) = file.undo() else {
        app.messages.show("No choices to take back");
        return;
    };
    state.focus(nth);
    let choice = state.choice();
    report_choice(app, &choice);
}

fn toggle_base(app: &mut App) {
    let Some(state) = app.conflict.as_mut() else { return };
    state.show_base = !state.show_base;
    let on = state.show_base;
    let has_base = state
        .current()
        .and_then(|f| f.base.as_ref())
        .is_some_and(|b| !b.is_empty());
    app.messages.show(match (on, has_base) {
        // Worth saying: an add/add conflict has no ancestor at all, and an
        // empty third pane otherwise reads as a bug.
        (true, false) => "This file has no common ancestor — it was added on both sides",
        (true, true) => "Showing the common ancestor",
        (false, _) => "Hiding the common ancestor",
    });
}

// ---------------------------------------------------------------------------
// Reading and writing
// ---------------------------------------------------------------------------

/// `d` — the two versions against their common ancestor, in a focused float.
fn show_diff(app: &mut App) {
    let Some(state) = app.conflict.as_ref() else { return };
    let Some(file) = state.current() else { return };
    let root = state.set.root.clone();
    let path = file.path.clone();
    // git's own diff between the stages, which is the one rendering of "who
    // changed what" that a git user already reads fluently.
    match git(&root, &["diff", "--", &path]) {
        Ok(text) if !text.trim().is_empty() => {
            app.popup = Some(Popup::git_output(&format!("diff — {path}"), text.trim_end()));
        }
        _ => app.messages.show("Nothing to diff for this file"),
    }
}

/// `e` — hand this hunk's merged text to the ordinary editor.
///
/// The escape hatch for a conflict that genuinely needs merging rather than
/// choosing.  A `*conflict …*` buffer, backed out of with `q` like every other
/// temporary buffer in the editor; what is in it on the way out becomes that
/// hunk's answer.
fn edit_hunk(app: &mut App) {
    let Some(state) = app.conflict.as_ref() else { return };
    let Some(file) = state.current() else { return };
    let Some(conflict::hunk::Region::Conflict(versions)) = file
        .regions
        .iter()
        .filter(|r| r.is_conflict())
        .nth(state.nth)
    else {
        app.messages.show("No conflict selected");
        return;
    };

    // Seeded with whatever is currently chosen, so `a` then `e` is "take this
    // side and adjust it" — which is the common shape of a hand merge.
    let seed = match state.choice() {
        Choice::Edited(text) => text,
        Choice::Sides { left: false, right: false } => {
            format!("{}{}", versions.left, versions.right)
        }
        choice => {
            let mut text = String::new();
            if choice.takes(Side::Left) {
                text.push_str(&versions.left);
            }
            if choice.takes(Side::Right) {
                text.push_str(&versions.right);
            }
            text
        }
    };

    let name = format!("{}{} {}*", crate::app::CONFLICT_EDIT_PREFIX, state.nth + 1, file.path);
    app.conflict_edit = Some((state.file, state.nth));
    app.special_buffer_ropes
        .insert(name.clone(), ropey::Rope::from_str(&seed));
    super::buffers::switch_to_special_buffer(app, &name);
    app.messages
        .show("Edit this section, then q to take it back to the resolver");
}

/// `q` in a `*conflict …*` buffer: the edited text becomes that hunk's answer.
///
/// Returns false when that is not what is open, so the caller carries on with
/// its own close — the same shape as `vcs::close_transient_buffer`.
pub(super) fn close_edit_buffer(app: &mut App) -> bool {
    if !app.in_conflict_edit_buffer() {
        return false;
    }
    let text = app.buffer.rope.to_string();
    let target = app.conflict_edit.take();
    if let Some(id) = app.current_source_id() {
        app.special_buffer_ropes.remove(id.label());
    }
    open(app);

    let Some((file_idx, nth)) = target else { return true };
    let Some(state) = app.conflict.as_mut() else { return true };
    state.file = file_idx.min(state.set.files.len().saturating_sub(1));
    state.focus(nth);
    if let Some(file) = state.current_mut() {
        file.choose(nth, Choice::Edited(text));
    }
    let choice = state.choice();
    report_choice(app, &choice);
    true
}

/// `Enter` — write this file's resolution and stage it.
///
/// No confirmation, deliberately: nothing here is destructive in the way a
/// history rewrite is, git still holds all three stages until the operation
/// finishes, and `:conflict-revert` puts the file back markers and all.  A
/// dialog on the key you press once per file would be ceremony.
fn write_file(app: &mut App) {
    let Some(state) = app.conflict.as_ref() else { return };
    let Some(file) = state.current() else { return };
    if !file.is_decided() {
        let n = file.undecided_count();
        app.messages.show(format!(
            "{n} conflict{} still unanswered — n goes to the next one",
            if n == 1 { "" } else { "s" }
        ));
        return;
    }
    let root = state.set.root.clone();
    let path = file.path.clone();
    if let Err(why) = write::resolve(&root, file) {
        app.messages.show(why);
        return;
    }

    if let Some(file) = app.conflict.as_mut().and_then(ConflictState::current_mut) {
        file.resolved = true;
    }
    let left = app.conflict.as_ref().map_or(0, |s| s.set.unresolved());
    if left == 0 {
        app.messages
            .show(format!("Resolved {path} — that was the last one"));
        // Re-reading is what turns "the last one" into leaving the view, since
        // a resolved index has nothing left to show.
        refresh(app);
        return;
    }
    app.messages.show(format!(
        "Resolved {path} — {left} file{} left  (] for the next)",
        if left == 1 { "" } else { "s" }
    ));
    if let Some(state) = app.conflict.as_mut() {
        state.step_file(true);
    }
    announce_file(app);
}

/// Put a file back the way git left it, markers and all.
fn revert_file(app: &mut App) {
    let Some(state) = app.conflict.as_ref() else { return };
    let Some(file) = state.current() else { return };
    let (root, path) = (state.set.root.clone(), file.path.clone());
    match write::revert(&root, &path) {
        Ok(()) => {
            app.messages
                .show(format!("Put {path} back the way git left it"));
            refresh(app);
        }
        Err(why) => app.messages.show(why),
    }
}

fn yank_merged(app: &mut App) {
    let Some(state) = app.conflict.as_ref() else { return };
    let Some(file) = state.current() else { return };
    crate::clipboard::write(&file.merged());
    app.messages
        .show(format!("Yanked the resolved text of {}", file.path));
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The `g` sub-mode's meanings here.
pub fn goto_command(c: char) -> Option<Command> {
    Some(match c {
        'c' => Command::VcsContinue,
        'a' => Command::VcsAbort,
        'x' => Command::ConflictRevertFile,
        'r' => Command::ConflictRefresh,
        'w' => Command::ConflictWriteFile,
        's' => Command::VcsGitStatus,
        'V' => Command::VcsOpen,
        '?' => Command::ConflictHelp,
        _ => return None,
    })
}

/// What the `g` which-key popup advertises here.  Pinned to [`goto_command`]
/// by a test.
pub fn goto_hints() -> Vec<(String, String)> {
    [
        ("c", "carry on with the operation"),
        ("a", "abort the whole operation"),
        ("w", "write this file's resolution"),
        ("x", "put this file back the way git left it"),
        ("s", "git status, verbatim"),
        ("r", "re-read the conflicted files"),
        ("V", "the commit graph"),
        ("?", "the keys"),
        ("b", "buffer picker"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Every key the resolver binds.
///
/// Same surface as the commit graph's `?`, for the same reason: the question a
/// help key is pressed to answer is "which keys does this have", and the
/// answer has to be complete.  Pinned to the keymap by
/// `the_key_sheet_only_lists_keys_that_are_bound`.
pub fn key_sheet() -> Vec<KeyHint> {
    let key = KeyHint::binding;
    vec![
        KeyHint::heading("MOVING AROUND"),
        key("h l", "focus the left / right version"),
        key("j k", "through this file's conflicts"),
        key("n N", "to the next / previous *unanswered* one"),
        key("] [", "next / previous conflicted file"),
        key("3", "show the common ancestor"),
        key("d", "this file's diff"),
        KeyHint::heading("CHOOSING  (nothing is written yet)"),
        key("Space", "take, or drop, the focused version"),
        key("a", "take only the left version"),
        key("b", "take only the right version"),
        key("A", "take the left for every unanswered conflict"),
        key("B", "take the right for every unanswered conflict"),
        key("e", "edit this section by hand"),
        key("u", "take back the last choice"),
        KeyHint::heading("FINISHING"),
        key("Enter", "write this file and stage it"),
        key("g x", "put this file back, markers and all"),
        key("g c", "carry on with the merge or rebase"),
        key("g a", "abort the whole operation"),
        key("g s", "git status, verbatim"),
        key("y", "copy this file's resolved text"),
        key("r", "re-read the conflicted files"),
        key("?", "this sheet"),
        key("q", "leave the resolver"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conflict::testrepo::{diverged, Repo};
    use crate::keymap::{KeyBinding, Layer};

    /// An editor sitting in the resolver over a real conflicted repository.
    fn app_resolving(repo: &Repo) -> Option<App> {
        let mut app = App::new(None, crate::config::Config::load()).ok()?;
        app.viewport_height = 40;
        app.viewport_width = 140;
        app.buffer = crate::buffer::Buffer::new_empty();
        let set = load::read(&repo.root).ok()?;
        app.conflict = Some(ConflictState::new(set));
        Some(app)
    }

    /// A conflicted merge, opened.
    fn merging() -> Option<(Repo, App)> {
        let repo = diverged()?;
        let _ = git(&repo.root, &["merge", "feature"]);
        let app = app_resolving(&repo)?;
        Some((repo, app))
    }

    /// The resolver owns the screen, so it is what `View` reports — which is
    /// what the keymap layer, the scroll and the status line all key off.
    #[test]
    fn the_resolver_owns_the_view_and_has_no_text_buffer_behind_it() {
        let Some((_repo, app)) = merging() else { return };
        assert_eq!(app.view(), View::Conflict);
        assert!(!app.view().has_text_buffer());
        assert!(app.buffer.path.is_none(), "no writable handle on any file");
        assert_eq!(crate::input::keymap_layer(&app), Layer::Conflict);
    }

    /// The headline gesture: Space takes the focused side, and the merged text
    /// follows immediately — with nothing written.
    #[test]
    fn space_takes_the_focused_side_and_writes_nothing() {
        let Some((repo, mut app)) = merging() else { return };
        let before = std::fs::read_to_string(repo.root.join("f.txt")).unwrap();

        handle(&mut app, &Command::ConflictTakeSide);
        let file = app.conflict.as_ref().unwrap().current().unwrap();
        assert_eq!(file.merged(), "a\ntimeout = 45\nz\n", "the left side went in");
        assert!(file.is_decided());

        assert_eq!(
            std::fs::read_to_string(repo.root.join("f.txt")).unwrap(),
            before,
            "nothing may reach the disk before Enter"
        );
    }

    /// Two switches, four answers — including *both*, which is the one a
    /// take-ours/take-theirs menu cannot express, and *neither*, which no
    /// marker-editing workflow makes easy.
    #[test]
    fn the_two_switches_reach_every_answer_from_the_keyboard() {
        let Some((_repo, mut app)) = merging() else { return };
        let merged = |app: &App| {
            app.conflict.as_ref().unwrap().current().unwrap().merged()
        };

        handle(&mut app, &Command::ConflictTakeSide); // left on
        assert_eq!(merged(&app), "a\ntimeout = 45\nz\n");

        handle(&mut app, &Command::MoveRight); // focus the right pane
        handle(&mut app, &Command::ConflictTakeSide); // right on too
        assert_eq!(merged(&app), "a\ntimeout = 45\ntimeout = 60\nz\n", "both, in file order");

        handle(&mut app, &Command::MoveLeft);
        handle(&mut app, &Command::ConflictTakeSide); // left back off
        assert_eq!(merged(&app), "a\ntimeout = 60\nz\n");

        handle(&mut app, &Command::MoveRight);
        handle(&mut app, &Command::ConflictTakeSide); // right off as well
        assert_eq!(merged(&app), "a\nz\n", "neither — the section is deleted");
    }

    /// Enter is the only key that reaches the disk, and after it git considers
    /// the path resolved.
    #[test]
    fn enter_writes_the_file_and_stages_it() {
        let Some((repo, mut app)) = merging() else { return };
        handle(&mut app, &Command::ConflictTakeRight);
        handle(&mut app, &Command::ConflictWriteFile);

        assert_eq!(
            std::fs::read_to_string(repo.root.join("f.txt")).unwrap(),
            "a\ntimeout = 60\nz\n"
        );
        let unmerged = git(&repo.root, &["ls-files", "-u", "--", "f.txt"]).unwrap();
        assert!(unmerged.trim().is_empty(), "git still calls it conflicted");
    }

    /// A file with an unanswered section is refused, and the refusal says how
    /// many are left and which key goes to one.  Writing it would silently
    /// delete that section — and take the markers that would have shown the
    /// mistake with it.
    #[test]
    fn enter_on_an_unanswered_file_refuses_and_says_what_is_left() {
        let Some((repo, mut app)) = merging() else { return };
        let before = std::fs::read_to_string(repo.root.join("f.txt")).unwrap();

        handle(&mut app, &Command::ConflictWriteFile);
        let message = app.messages.current().unwrap_or_default();
        assert!(message.contains("unanswered"), "{message}");
        assert!(message.contains('n'), "the refusal has to say how to get there");
        assert_eq!(
            std::fs::read_to_string(repo.root.join("f.txt")).unwrap(),
            before,
            "nothing may be written"
        );
    }

    /// The labels are the whole point.  In a merge the left pane is your
    /// branch and the right is what is coming in, and both are named — never
    /// "ours" and "theirs", and never a bare hash.
    #[test]
    fn both_panes_are_named_after_real_branches_and_commits() {
        let Some((_repo, app)) = merging() else { return };
        let set = &app.conflict.as_ref().unwrap().set;

        assert!(set.left.role.contains("main"), "{}", set.left.role);
        assert!(set.right.role.contains("feature"), "{}", set.right.role);
        for label in [&set.left, &set.right] {
            assert!(!label.commit.is_empty(), "a pane with no hash");
            assert!(!label.author.is_empty(), "a pane with no author");
            assert!(label.when > 0, "a pane with no date");
            assert!(!label.summary.is_empty(), "a pane with no message");
        }
        for word in ["ours", "theirs", "Ours", "Theirs"] {
            assert!(
                !set.left.role.contains(word) && !set.right.role.contains(word),
                "the two words this view exists to avoid are back: {word}"
            );
        }
    }

    /// …and in a rebase they swap over, which is the single most confusing
    /// thing about resolving one.  The message line has to say it too, since
    /// that is where a choice is reported back.
    #[test]
    fn a_rebase_names_your_own_commit_on_the_right() {
        let Some(repo) = diverged() else { return };
        let _ = git(&repo.root, &["checkout", "feature"]);
        let _ = git(&repo.root, &["rebase", "main"]);
        let Some(mut app) = app_resolving(&repo) else { return };

        assert!(!app.conflict.as_ref().unwrap().set.operation.left_is_yours());
        handle(&mut app, &Command::MoveRight);
        handle(&mut app, &Command::ConflictTakeSide);
        let message = app.messages.current().unwrap_or_default();
        assert!(
            message.contains("Your commit"),
            "the message line lost the inversion: {message}"
        );
    }

    /// `A` settles every *unanswered* section and leaves deliberate answers
    /// alone — a key that overwrote them is one nobody could risk pressing.
    #[test]
    fn take_all_leaves_answers_already_given_alone() {
        let Some(repo) = diverged() else { return };
        // Three conflicting sections in one file.
        let three = |a: &str, b: &str, c: &str| format!("x\n{a}\ny\n{b}\nz\n{c}\nw\n");
        let _ = git(&repo.root, &["checkout", "-B", "base3", "main"]);
        repo.commit("g.txt", &three("1", "2", "3"), "base3");
        let _ = git(&repo.root, &["checkout", "-b", "side3"]);
        repo.commit("g.txt", &three("1s", "2s", "3s"), "side3");
        let _ = git(&repo.root, &["checkout", "base3"]);
        repo.commit("g.txt", &three("1m", "2m", "3m"), "main3");
        let _ = git(&repo.root, &["merge", "side3"]);

        let Some(mut app) = app_resolving(&repo) else { return };
        let state = app.conflict.as_mut().unwrap();
        state.file = state
            .set
            .files
            .iter()
            .position(|f| f.path == "g.txt")
            .expect("g.txt is conflicted");
        state.nth = 0;
        let total = state.current().unwrap().conflict_count();
        if total < 2 {
            return; // git merged them as one region; nothing to prove here
        }

        // Answer the first deliberately, then settle the rest the other way.
        handle(&mut app, &Command::ConflictTakeLeft);
        handle(&mut app, &Command::ConflictTakeRightAll);

        let file = app.conflict.as_ref().unwrap().current().unwrap();
        assert!(file.is_decided(), "everything is answered");
        assert_eq!(
            file.choices[0],
            crate::conflict::Choice::only(Side::Left),
            "the deliberate answer was overwritten"
        );
        assert!(file.choices[1..].iter().all(|c| *c == crate::conflict::Choice::only(Side::Right)));
    }

    /// `u` takes back the last choice *and puts the cursor on it* — undoing
    /// something off screen without saying where is the same as doing nothing.
    #[test]
    fn undo_takes_back_the_last_choice_and_goes_to_it() {
        let Some((_repo, mut app)) = merging() else { return };
        handle(&mut app, &Command::ConflictTakeLeft);
        assert!(app.conflict.as_ref().unwrap().current().unwrap().is_decided());

        handle(&mut app, &Command::Undo);
        let state = app.conflict.as_ref().unwrap();
        assert!(!state.current().unwrap().is_decided(), "the choice came back off");
        assert_eq!(state.nth, 0, "the cursor is on what was undone");

        handle(&mut app, &Command::Undo);
        assert!(app.messages.current().unwrap_or_default().contains("No choices"));
    }

    /// The answers survive a buffer switch.  The natural thing to do halfway
    /// through a conflict is leave to read the code, and coming back to an
    /// empty resolver would punish exactly that habit.
    #[test]
    fn the_answers_survive_leaving_the_view_and_coming_back() {
        let Some((_repo, mut app)) = merging() else { return };
        handle(&mut app, &Command::ConflictTakeRight);

        crate::exec::buffers::teardown_current_buffer(&mut app);
        assert!(app.conflict.is_none(), "the view was torn down");
        open(&mut app);

        let file = app.conflict.as_ref().expect("it came back").current().unwrap();
        assert_eq!(file.merged(), "a\ntimeout = 60\nz\n", "the answer was lost");
    }

    /// Every key the sheet lists has to be bound, or it teaches presses that
    /// do nothing — the same pin the graph's sheet has.
    #[test]
    fn the_key_sheet_only_lists_keys_that_are_bound() {
        let app = App::new(None, crate::config::Config::load()).expect("app");
        for hint in key_sheet() {
            if hint.is_heading() {
                continue;
            }
            for token in hint.key.split_whitespace() {
                if matches!(token, "Enter" | "Esc" | "Space") {
                    continue;
                }
                let bound = |c: char| {
                    app.keymap
                        .lookup_layered(Layer::Conflict, &KeyBinding::char(c))
                        .is_some()
                };
                let mut chars = token.chars();
                let first = chars.next().expect("a non-empty token");
                match chars.next() {
                    Some(second) => assert!(
                        bound(first) && bound(second),
                        "`{token}` is not bound"
                    ),
                    None => assert!(
                        bound(first)
                            || goto_command(first).is_some()
                            || crate::input::goto_command(View::Conflict, first).is_some(),
                        "the sheet lists `{}` ({}), which is not bound",
                        hint.key,
                        hint.description
                    ),
                }
            }
        }
    }

    /// …and the `g` popup likewise.
    #[test]
    fn goto_hints_only_advertise_real_bindings() {
        for (key, description) in goto_hints() {
            let c = key.chars().next().expect("a key");
            assert!(
                goto_command(c).is_some()
                    || crate::input::goto_command(View::Conflict, c).is_some(),
                "g{key} ({description}) is advertised but does nothing"
            );
        }
    }

    /// A text command that reached the resolver is refused by name rather than
    /// running against the empty buffer behind it.
    #[test]
    fn text_commands_are_refused_and_universal_ones_fall_through() {
        let Some((_repo, mut app)) = merging() else { return };
        for cmd in [Command::Write, Command::EnterInsert, Command::OpenLineBelow] {
            assert!(handle(&mut app, &cmd), "{} should be ours to refuse", cmd.name());
            let message = app.messages.current().unwrap_or_default();
            assert!(
                message.contains("read-only") || message.contains(cmd.name()),
                "{}: {message}",
                cmd.name()
            );
        }
        for cmd in [Command::Quit, Command::OpenCommandPalette, Command::ToggleWordWrap] {
            assert!(!handle(&mut app, &cmd), "{} should fall through", cmd.name());
        }
    }

    /// The whole thing, end to end, over two conflicted files: open, answer
    /// each one from the keyboard, write it, move on, and finish with a
    /// repository git considers ready to commit.
    ///
    /// The one test that would catch the view being individually correct and
    /// collectively unusable.
    #[test]
    fn two_conflicted_files_are_resolved_one_after_the_other() {
        let Some(repo) = diverged() else { return };
        // A second conflicting file, so `]` and the finishing path are real.
        let _ = git(&repo.root, &["checkout", "feature"]);
        repo.commit("g.txt", "left\nshared\n", "g on feature");
        let _ = git(&repo.root, &["checkout", "main"]);
        repo.commit("g.txt", "right\nshared\n", "g on main");
        let _ = git(&repo.root, &["merge", "feature"]);

        let Some(mut app) = app_resolving(&repo) else { return };
        assert_eq!(app.conflict.as_ref().unwrap().set.files.len(), 2);

        // First file: take the incoming side, write it.
        handle(&mut app, &Command::ConflictTakeRight);
        handle(&mut app, &Command::ConflictWriteFile);
        // Writing the last-but-one moves to the next file by itself.
        let state = app.conflict.as_ref().expect("still resolving");
        assert_eq!(state.set.unresolved(), 1, "one file left");

        // Second file: keep both sides.
        handle(&mut app, &Command::ConflictTakeSide);
        handle(&mut app, &Command::MoveRight);
        handle(&mut app, &Command::ConflictTakeSide);
        handle(&mut app, &Command::ConflictWriteFile);

        // Nothing is conflicted any more, by git's own account.
        let unmerged = git(&repo.root, &["ls-files", "-u"]).unwrap();
        assert!(unmerged.trim().is_empty(), "still conflicted: {unmerged}");
        // …and the merge can actually be completed, which is the only verdict
        // that matters.
        git(&repo.root, &["commit", "--no-edit"]).expect("the merge commits");
        let log = git(&repo.root, &["log", "-1", "--format=%P"]).unwrap();
        assert_eq!(log.split_whitespace().count(), 2, "not a merge commit: {log}");
    }

    /// Put the editor in the graph with a real work tree behind it.  The
    /// staging-view routing reads nothing else from the snapshot.
    fn install_work_tree(app: &mut App, root: &std::path::Path) {
        let work = crate::vcs::load::parse_status(
            &git(root, &["status", "--porcelain"]).unwrap_or_default(),
        );
        let dag = crate::vcs::Dag::new(
            Vec::new(),
            Vec::new(),
            crate::vcs::Head::default(),
            work,
            false,
        );
        app.vcs = Some(crate::vcs::state::VcsState::new(root.to_path_buf(), dag, 0));
    }

    /// Picking a conflicted file out of the staging view has to land on *that*
    /// file, not on whichever one happened to be first.
    ///
    /// The read is asynchronous, so the request outlives the frame that made
    /// it — which is precisely the case that would otherwise be dropped
    /// silently, and only ever in the real (unstashed) path.
    #[test]
    fn picking_a_conflicted_file_lands_on_it_even_though_the_read_is_async() {
        let Some(repo) = diverged() else { return };
        let _ = git(&repo.root, &["checkout", "feature"]);
        repo.commit("g.txt", "left\n", "g on feature");
        let _ = git(&repo.root, &["checkout", "main"]);
        repo.commit("g.txt", "right\n", "g on main");
        let _ = git(&repo.root, &["merge", "feature"]);

        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.viewport_height = 40;
        app.viewport_width = 140;
        app.buffer = crate::buffer::Buffer::new_empty();
        // Standing in the graph, as the staging view's Enter would be.  Only
        // the work tree matters here — that is all the routing reads.
        install_work_tree(&mut app, &repo.root);

        // The second file, deliberately not the first.
        let wanted = repo.root.join("g.txt");
        assert!(open_if_conflicted(&mut app, &wanted), "it has to route here");
        assert!(app.conflict.is_none(), "the read is asynchronous");

        // Drain the load the way the run loop does.  Waited on with a real
        // sleep rather than a spin: the read is several git subprocesses, and
        // a busy loop finishes thousands of iterations before the first one
        // has started.
        for _ in 0..600 {
            if poll(&mut app) && app.conflict.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let state = app.conflict.as_ref().expect("the read finished");
        assert_eq!(
            state.current().expect("a file").path,
            "g.txt",
            "landed on the wrong file"
        );
    }

    /// …and a file that is not conflicted falls through, so the ordinary open
    /// carries on.
    #[test]
    fn picking_an_ordinary_file_is_not_routed_to_the_resolver() {
        let Some(repo) = diverged() else { return };
        let _ = git(&repo.root, &["merge", "feature"]);
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.buffer = crate::buffer::Buffer::new_empty();
        install_work_tree(&mut app, &repo.root);
        assert!(!open_if_conflicted(&mut app, &repo.root.join("nowhere.txt")));
        assert!(app.conflict.is_none() && app.conflict_pending.is_none());
    }
}
