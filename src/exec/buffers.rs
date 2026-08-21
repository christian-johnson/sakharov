//! Buffer-list management: special buffers (scratch/messages), buffer
//! switching/stashes, notebook open, new-file/new-notebook creation, and the
//! session-wide unsaved-changes sweep.

use ropey::Rope;

use std::path::Path;

use crate::{
    app::{App, MESSAGES_BUFFER, SCRATCH_BUFFER},
    mode::Mode,
    selection::Selection,
    source::SourceId,
};

use super::{lsp, notebook, rebuild_diag_cache, recompute_highlights};

pub(crate) const SCRATCH_INTRO: &str = "\
;; This buffer is for notes you don't want to save.\n\
;; Use it for scratch text.\n";

/// Returns true for virtual buffer names that don't correspond to real files:
/// `*scratch*`, `*Messages*`, and the table view's `*cell …*` buffers.
///
/// The `*…*` shape is the rule rather than a fixed list, so a new virtual
/// buffer is automatically excluded from saving, LSP sync, crash recovery and
/// the unsaved-changes sweep — the places a name that isn't a path must never
/// reach.
pub fn is_special_path(path: &std::path::Path) -> bool {
    SourceId::of(path).is_virtual()
}

/// Save a special buffer's rope when leaving it, so its text survives the
/// switch — `*scratch*`'s notes, and equally the query typed into `*sql*`.
///
/// Two special buffers are excluded, both because their text has a live source
/// that the stash would freeze: `*Messages*` is rebuilt from the message log
/// every time it is opened, and a `*cell …*` buffer is re-read from the grid by
/// whoever opens it (`table::open_cell_buffer` seeds and evicts that rope).
pub(super) fn save_current_special_buffer(app: &mut App) {
    let Some(SourceId::Virtual(name)) = app.current_source_id() else { return };
    if name == MESSAGES_BUFFER || app.in_cell_buffer() {
        return;
    }
    app.special_buffer_ropes.insert(name, app.buffer.rope.clone());
}

/// Stash or close whatever is currently open so it can be safely replaced.
///
/// Every "show something else" path goes through here.
pub(super) fn teardown_current_buffer(app: &mut App) {
    remember_cursor(app);
    // The one place a special buffer's text is kept — `*scratch*`'s notes, and
    // equally the query typed into `*sql*`, including on the way into the table
    // view, which is how `q` leaves `*sql*` for a grid.
    save_current_special_buffer(app);

    // One arm per view: whatever is on screen has to be put somewhere it can be
    // brought back from.  Exhaustive on purpose (see `crate::view`) — a view
    // that fell through to the text arm would have its state dropped on the
    // next buffer switch, silently losing whatever the user had done in it.
    match app.view() {
        // A table session holds no unsaved state (the view is read-only), but
        // it does hold a parsed copy of the file and the cursor cell.  Stash
        // both, so reading a cell in its own buffer and coming straight back
        // lands on the same cell without re-parsing.
        crate::view::View::Table => {
            if let Some(session) = app.table.take() {
                app.table_buffers.insert(session.id.clone(), session);
            }
        }

        // Stash the open notebook so edits are preserved if the user comes
        // back.  After this `app.buffer` holds stale cell text — do NOT stash
        // it, and do NOT `did_close` it: it was never opened with the LSP under
        // that virtual path.
        //
        // The second arm is the full-screen focused-cell overlay: it *draws*
        // as text and is edited like a file, so `view()` calls it `Text`, but
        // what is open is still a notebook and it must be stashed as one.
        // Treating it as a plain buffer would close a virtual cell path with
        // the LSP and drop every other cell's unsaved edits.
        crate::view::View::Notebook => notebook::stash_current_notebook(app),
        crate::view::View::Text if app.notebook.is_some() => {
            notebook::stash_current_notebook(app)
        }

        crate::view::View::Text => {
            if let (Some(ref lang), Some(ref old_path)) =
                (app.lsp_language.clone(), app.buffer.path.clone())
            {
                if !is_special_path(old_path) {
                    app.lsp.did_close(lang, old_path);
                }
            }
            stash_current_file_buffer(app);
        }
    }

    // An in-flight load has nothing worth keeping, whichever view it was for.
    app.table_pending = None;
    // Leaving a `*cell …*` buffer, whichever way: undo the settings it forced.
    super::table::leave_cell_buffer(app);
}

/// Switch the editor to a named special buffer (`*scratch*` or `*Messages*`).
pub fn switch_to_special_buffer(app: &mut App, name: &str) {
    teardown_current_buffer(app);

    let rope = match name {
        SCRATCH_BUFFER => app
            .special_buffer_ropes
            .get(SCRATCH_BUFFER)
            .cloned()
            .unwrap_or_else(|| Rope::from_str(SCRATCH_INTRO)),
        // *Messages* is the one special buffer with a live source: rebuild it
        // from the accumulated log rather than from the stash.
        MESSAGES_BUFFER => {
            let content = if app.messages.log.is_empty() {
                String::new()
            } else {
                let mut s = app.messages.log.join("\n");
                s.push('\n');
                s
            };
            Rope::from_str(&content)
        }
        // Anything else (a `*cell …*` buffer) is whatever its creator stashed.
        other => app
            .special_buffer_ropes
            .get(other)
            .cloned()
            .unwrap_or_default(),
    };

    let mut buf = crate::buffer::Buffer::new_empty();
    buf.rope = rope;
    buf.path = Some(std::path::PathBuf::from(name));

    app.buffer = buf;
    let (line, col) = remembered_cursor(app, std::path::Path::new(name));
    let line = line.min(app.buffer.rope.len_lines().saturating_sub(1));
    let head = app.buffer.rope.line_to_char(line)
        + col.min(app.buffer.rope.line(line).len_chars().saturating_sub(1));
    app.selection = Selection::point(head.min(app.buffer.rope.len_chars()));
    app.scroll_row = 0;
    app.scroll_col = 0;
    app.insert_session_active = false;
    app.lsp_language = None;
    // The name, not `None`: a special buffer has no file, but it can still have
    // a syntax — `*sql*` is SQL, and detection is the highlighter's business.
    app.highlighter = crate::highlight::Highlighter::new(Some(std::path::Path::new(name)));
    recompute_highlights(app);
    app.mode = Mode::Normal;
    app.git_diff.clear();
    rebuild_diag_cache(app);
}

/// Open `path` in whichever view its type calls for: a special buffer, the
/// notebook view (`.ipynb`), the table view (`.csv`/`.tsv`, when
/// `table.auto_open` is on), or the plain text editor.
///
/// Every "the user picked a file, show it" path goes through here — the buffer
/// picker, the file picker, buffer cycling, `:bd`'s fallback — so a new view
/// only has to be taught to one dispatcher instead of five.
pub fn open_path(app: &mut App, path: &std::path::Path) {
    // Opening a file is a commit: leave the dashboard.  The built-in pickers
    // clear this on popup-confirm, but the *external* picker (yazi/fzf) never
    // goes through a popup, so without this the dashboard stays painted over
    // the file that was just opened until the next keypress.
    app.show_splash = false;
    // Before anything is torn down: where the cursor is *now* is what the
    // buffer being left should reopen at (and `path` may be that buffer).
    remember_cursor(app);

    if app.table_buffers.contains_key(&SourceId::of(path)) {
        // A derived table (a frequency table, later a query result) is virtual,
        // so it would otherwise fall into the special-buffer branch below and
        // open as an empty text buffer named after itself.
        super::table::open_as_table(app, path);
    } else if is_special_path(path) {
        switch_to_special_buffer(app, path.to_str().unwrap_or(SCRATCH_BUFFER));
    } else if path.extension().and_then(|e| e.to_str()) == Some("ipynb") {
        open_as_notebook(app, path);
    } else if app.config.table.auto_open && super::table::is_table_path(path) {
        super::table::open_as_table(app, path);
    } else {
        // Reopen where it was left.  `open_file_at`'s explicit position is for
        // callers that mean one (a jump, a diagnostic); "the user picked this
        // file" means "put me back where I was".
        let (line, col) = remembered_cursor(app, path);
        lsp::open_file_at(app, path, line, col);
    }
}

/// Cycle through `open_buffers` by `delta` (+1 = next, -1 = prev).
pub(super) fn navigate_buffer(app: &mut App, delta: i32) {
    let n = app.open_buffers.len();
    if n <= 1 {
        return;
    }

    // A `*cell …*` buffer is a temporary read-out of a grid cell rather than an
    // entry of its own, so it sits "at" the table it came from: H/L step away
    // from that table instead of restarting at the head of the list.  Otherwise
    // it is simply whatever is on screen.
    let current = match app
        .table_cell_origin
        .as_ref()
        .map(|origin| origin.id.clone())
        .or_else(|| app.current_source_id())
    {
        Some(id) => id,
        None => return,
    };

    let current_idx = app.open_buffers.iter().position(|id| *id == current);

    let idx = match current_idx {
        Some(i) => ((i as i32 + delta).rem_euclid(n as i32)) as usize,
        None => 0,
    };

    let target = app.open_buffers[idx].to_path();
    open_path(app, &target);
}

/// Open a `.ipynb` file as a notebook, replacing whatever is currently open.
/// Called when the user selects a notebook from the buffer picker.
pub fn open_as_notebook(app: &mut App, path: &std::path::Path) {
    use crate::{notebook::Notebook, notebook_state::NotebookState};

    // Stash or close whatever is currently open.
    teardown_current_buffer(app);

    // Restore from stash if we've visited this notebook before (preserves unsaved edits).
    if notebook::restore_stashed_notebook(app, path) {
        register_buffer(&mut app.open_buffers, path);
        app.messages.show(format!(
            "Opened {}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        ));
        return;
    }

    let nb = match Notebook::from_path(path) {
        Ok(n) => n,
        Err(e) => {
            app.messages.show(format!("Failed to open notebook: {e}"));
            return;
        }
    };

    let lang = nb.metadata.kernel_language.clone();
    app.notebook = Some((nb, NotebookState::new()));
    notebook::focus_notebook_session(app);
    app.cell_focused_edit = false;
    app.mode = Mode::Normal;
    app.lsp_language = Some(lang);
    // Load cell 0 into the buffer — this sets the buffer/path/highlighter,
    // resets the selection + scroll, and opens the cell with the LSP.
    notebook::load_focused_cell(app);
    // Register the whole notebook with a notebook-aware server. When the server
    // is still initializing this is a no-op; the Initialized event re-runs it.
    notebook::notebook_lsp_open(app);

    register_buffer(&mut app.open_buffers, path);

    app.messages.show(format!(
        "Opened {}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    ));

    // Offer to restore unsaved cells from a previous crash, if any.
    crate::recovery::offer_on_open(app, path);
}

/// The directory a `:sql` query resolves a bare filename against.
///
/// The same answer as "where would a new file go": next to what you are working
/// on.  `FROM 'data.csv'` should mean the CSV beside the notebook, not one in
/// whatever directory the editor happened to be launched from.
pub(super) fn sql_working_dir(app: &App) -> Option<std::path::PathBuf> {
    let dir = current_buffer_dir(app);
    dir.is_dir().then_some(dir)
}

/// Resolve the directory new files should be created in: the directory of the
/// open notebook or current buffer, falling back to the working directory for
/// special buffers (scratch / messages / dashboard) or unnamed buffers.
fn current_buffer_dir(app: &App) -> std::path::PathBuf {
    if let Some(parent) = app
        .table
        .as_ref()
        .and_then(|s| s.path())
        .and_then(Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
    {
        return parent.to_path_buf();
    }
    if let Some((ref nb, _)) = app.notebook {
        return crate::notebook::notebook_dir(&nb.path);
    }
    app.buffer.path.as_deref()
        .filter(|p| !is_special_path(p))
        .and_then(|p| p.parent())
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
}

/// Resolve `name` against the current buffer's directory (absolute names are
/// used verbatim).
fn resolve_new_path(app: &App, name: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(name);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        current_buffer_dir(app).join(p)
    }
}

/// Create an empty file in the current buffer's directory and open it.
/// If it already exists, just open it instead of clobbering.
/// Called from the minibuffer `Prompt` handler once a name has been entered.
pub(crate) fn create_new_file(app: &mut App, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        app.messages.show("Usage: :new-file <name>");
        return;
    }
    let path = resolve_new_path(app, name);
    if path.exists() {
        app.messages.show(format!("{name} already exists — opening"));
        lsp::open_file_at(app, &path, 0, 0);
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            app.messages.show(format!("Could not create directory: {e}"));
            return;
        }
    }
    if let Err(e) = std::fs::write(&path, "") {
        app.messages.show(format!("Could not create file: {e}"));
        return;
    }
    lsp::open_file_at(app, &path, 0, 0);
    app.messages.show(format!("Created {name}"));
}

/// Create a valid empty `.ipynb` notebook in the current buffer's directory and
/// open it in the notebook interface.  If it already exists, just open it.
/// Called from the minibuffer `Prompt` handler once a name has been entered.
pub(crate) fn create_new_notebook(app: &mut App, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        app.messages.show("Usage: :new-notebook <name>");
        return;
    }
    // Ensure the file carries the .ipynb extension so it opens as a notebook.
    let mut name = name.to_string();
    if std::path::Path::new(&name).extension().and_then(|e| e.to_str()) != Some("ipynb") {
        name.push_str(".ipynb");
    }
    let path = resolve_new_path(app, &name);
    if path.exists() {
        app.messages.show(format!("{name} already exists — opening"));
        open_as_notebook(app, &path);
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            app.messages.show(format!("Could not create directory: {e}"));
            return;
        }
    }
    if let Err(e) = std::fs::write(&path, crate::notebook::empty_notebook_json()) {
        app.messages.show(format!("Could not create notebook: {e}"));
        return;
    }
    open_as_notebook(app, &path);
    app.messages.show(format!("Created {name}"));
}

/// Record where the cursor is in the text buffer being left, so switching back
/// to it lands where it was rather than at line 1.  Notebooks and tables keep
/// their own cursor in their own state, and neither is `app.buffer`, so this
/// only ever speaks for a plain (or special) text buffer.
pub(crate) fn remember_cursor(app: &mut App) {
    if app.notebook.is_some() || app.table.is_some() {
        return;
    }
    let Some(path) = app.buffer.path.as_deref() else { return };
    let head = app.selection.head.min(app.buffer.rope.len_chars());
    let line = app.buffer.rope.char_to_line(head);
    let col = head - app.buffer.rope.line_to_char(line);
    app.cursor_positions.insert(SourceId::of(path), (line, col));
}

/// The remembered `(line, column)` for `path`, or the top of the file.
pub(crate) fn remembered_cursor(app: &App, path: &std::path::Path) -> (usize, usize) {
    app.cursor_positions
        .get(&SourceId::of(path))
        .copied()
        .unwrap_or((0, 0))
}

/// Stash the current plain-file buffer so unsaved edits and undo history
/// survive switching away (the buffer is otherwise reloaded from disk when the
/// user comes back).  No-op for notebooks (stashed separately via
/// `notebook::stash_current_notebook`), special buffers, and path-less buffers.
/// Leaves `app.buffer` empty — every caller immediately replaces it.
pub(crate) fn stash_current_file_buffer(app: &mut App) {
    if app.notebook.is_some() {
        return;
    }
    let Some(path) = app.buffer.path.clone() else { return };
    if is_special_path(&path) {
        return;
    }
    let buf = std::mem::replace(&mut app.buffer, crate::buffer::Buffer::new_empty());
    app.file_buffers.insert(SourceId::of(&path), buf);
}

/// Remove and return the stashed buffer for `path`, if one exists.
pub(crate) fn take_stashed_file_buffer(
    app: &mut App,
    path: &std::path::Path,
) -> Option<crate::buffer::Buffer> {
    app.file_buffers.remove(&SourceId::of(path))
}

/// Names of every buffer holding unsaved changes, anywhere in the session:
/// the active buffer/notebook, stashed notebooks, and stashed plain files.
/// Special buffers (scratch/messages) are excluded — they are throwaway by
/// design and covered by crash recovery.
pub(crate) fn unsaved_buffer_names(app: &App) -> Vec<String> {
    fn short(p: &std::path::Path) -> String {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string_lossy().into_owned())
    }
    let mut names = Vec::new();
    if let Some((nb, _)) = &app.notebook {
        if nb.modified {
            names.push(short(&nb.path));
        }
    } else if app.buffer.modified {
        if let Some(p) = app.buffer.path.as_deref().filter(|p| !is_special_path(p)) {
            names.push(short(p));
        }
    }
    for (id, (nb, _)) in &app.notebook_buffers {
        if nb.modified {
            names.push(id.label().to_string());
        }
    }
    for (id, buf) in &app.file_buffers {
        if buf.modified {
            names.push(id.label().to_string());
        }
    }
    names
}

/// Register `path` in `open_buffers` if its identity is not already there.
pub(super) fn register_buffer(open_buffers: &mut Vec<SourceId>, path: &std::path::Path) {
    let id = SourceId::of(path);
    if !open_buffers.contains(&id) {
        open_buffers.push(id);
    }
}

// ---------------------------------------------------------------------------
// Saving and closing
// ---------------------------------------------------------------------------

/// `:w` / `:w!` — save whatever is open.
///
/// Format-on-save runs first when configured: a shell formatter saves the file
/// itself, and the LSP path defers the save until the `FormattingResult`
/// arrives, so both return early rather than saving twice.
pub(super) fn write_buffer(app: &mut App, force: bool) {
        if app.buffer.path.as_deref().map(is_special_path).unwrap_or(false) {
            app.messages.show("Special buffer — nothing to save");
            return;
        }
        // format_on_save: try shell formatter first, then LSP.
        if app.notebook.is_none() && app.config.editor.format_on_save {
            if super::run_shell_formatter(app) {
                // Shell formatter saved+formatted the file; show result and return.
                if app.messages.current().is_none() {
                    app.messages.show(format!("Saved {}", app.buffer.display_name()));
                }
                return;
            }
            // No shell formatter; try LSP-based format-then-save.
            let lang = app.current_language().map(|l| l.to_owned());
            let path = app.buffer.path.clone();
            if let (Some(lang), Some(path)) = (lang, path) {
                if !is_special_path(&path) && app.lsp.is_ready(&lang) {
                    let tab_size = app.config.editor.tab_width;
                    app.pending_format_save = true;
                    if app.lsp.format_document(&lang, &path, tab_size, true) {
                        return; // save happens when FormattingResult arrives
                    }
                    app.pending_format_save = false; // server doesn't support formatting
                }
            }
        }
        if app.notebook.is_some() {
            // Flushes any in-progress cell edits into nb.cells before serialising.
            let result = super::notebook::save_notebook(app);
            super::report_save(app, result, |app| {
                let name = app.notebook.as_ref()
                    .and_then(|(nb, _)| nb.path.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("notebook.ipynb")
                    .to_string();
                app.messages.show(format!("Saved {name}"));
            });
        } else {
            let result = app.buffer.save(None, force);
            super::report_save(app, result, |app| {
                app.messages.show(format!("Saved {}", app.buffer.display_name()));
                super::refresh_git(app);
            });
        }
}


/// `:wq` — save the active buffer, then quit only if nothing else in the
/// session still holds unsaved changes (stashed notebooks and files).
pub(super) fn write_and_quit(app: &mut App) {
        // Save the active buffer, then quit only if nothing else in the
        // session still holds unsaved changes (stashed notebooks/files).
        let saved = if app.buffer.path.as_deref().map(is_special_path).unwrap_or(false) {
            true
        } else if app.notebook.is_some() {
            let result = super::notebook::save_notebook(app);
            super::report_save(app, result, |_| {})
        } else {
            let result = app.buffer.save(None, false);
            super::report_save(app, result, |_| {})
        };
        if saved {
            let unsaved = super::unsaved_buffer_names(app);
            if unsaved.is_empty() {
                app.should_quit = true;
            } else {
                app.messages.show(format!(
                    "Saved — but unsaved changes remain in {} (:q! to discard)",
                    unsaved.join(", ")
                ));
            }
        }
}


/// `:bd` / `:bd!` — close what is open and land somewhere sensible.
///
/// "Somewhere sensible" is view-specific: a `*cell …*` buffer and a computed
/// table back out to where they came from, the SQL buffer to wherever `:sql`
/// was invoked, and a file to its neighbour in the buffer list.
pub(super) fn close_buffer(app: &mut App, force: bool) {

        // A `*cell …*` buffer, a computed table and the `*sql*` query buffer
        // are *backed out of* rather than closed: each was opened from
        // somewhere, and that somewhere is the only place it makes sense to
        // return to.  Without this they hit the refusal below and there is
        // no way out of them at all.
        if super::table::close_cell_buffer(app)
            || super::table::close_derived_table(app)
            || super::sql::close_buffer(app)
        {
            return;
        }

        // What is being closed.  In the table view `app.buffer` is a
        // detached, path-less buffer — the identity lives in the session,
        // and reading it from the buffer closed nothing at all.
        let key = app.current_source_id();

        // Special buffers cannot be closed.  A table over a real file is
        // not one of them, however virtual its detached buffer looks.
        if key.as_ref().is_some_and(crate::source::SourceId::is_virtual) {
            let name = key.as_ref().map_or("this buffer", |k| k.label());
            app.messages.show(format!("Cannot close special buffer {name}"));
            return;
        }

        // Check for unsaved changes.
        let is_modified = if let Some((ref nb, _)) = app.notebook {
            nb.modified
        } else {
            app.buffer.modified
        };
        if is_modified && !force {
            app.messages.show(
                "Buffer modified — save with :w or use :bd! to force close",
            );
            return;
        }

        // Tear down notebook/LSP for the current buffer.
        if app.notebook.is_some() {
            super::notebook::save_focused_cell(app);
            super::notebook::notebook_lsp_close(app);
            app.notebook = None;
            app.cell_focused_edit = false;
        } else if let (Some(ref lang), Some(ref old_path)) =
            (app.lsp_language.clone(), app.buffer.path.clone())
        {
            app.lsp.did_close(lang, old_path);
        }

        // Remove the closed buffer from the buffer list and every stash.
        let mut closed_idx = 0;
        if let Some(ref key) = key {
            closed_idx = app.open_buffers.iter().position(|s| s == key).unwrap_or(0);
            app.open_buffers.retain(|stored| stored != key);
            app.notebook_buffers.remove(key);
            app.file_buffers.remove(key);
            app.table_buffers.remove(key);
            app.cursor_positions.remove(key);
            // Closing a notebook shuts its kernel down — otherwise the
            // Python process outlives the buffer for the rest of the
            // session, holding whatever it had loaded.
            if app.compute.shutdown(key) {
                app.messages.show(format!("Kernel shut down ({})", key.label()));
            }
        }

        // Drop the closed buffer's contents now: the buffer-switch below
        // stashes whatever is in `app.buffer` (and in `app.table`), and the
        // buffer we just closed must not be resurrected into the stash.
        app.buffer = crate::buffer::Buffer::new_empty();
        app.table = None;

        // Land on the closed buffer's neighbour — the entry that slid into
        // its place, else the one before it — rather than restarting from
        // the head of the list.  *Messages* is skipped as a destination;
        // *scratch* is the fallback when nothing real is left.
        let next = app.open_buffers.iter()
            .cycle()
            .skip(closed_idx.min(app.open_buffers.len().saturating_sub(1)))
            .take(app.open_buffers.len())
            .find(|id| id.label() != crate::app::MESSAGES_BUFFER)
            .map(|id| id.to_path())
            .unwrap_or_else(|| std::path::PathBuf::from(crate::app::SCRATCH_BUFFER));

        open_path(app, &next);

        app.messages.show("Buffer closed");
}
