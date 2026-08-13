//! Buffer-list management: special buffers (scratch/messages), buffer
//! switching/stashes, notebook open, new-file/new-notebook creation, and the
//! session-wide unsaved-changes sweep.

use ropey::Rope;

use std::path::Path;

use crate::{app::App, mode::Mode, selection::Selection, source::SourceId};

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

/// Save the scratch buffer rope when leaving it (so edits survive switches).
pub(super) fn save_current_special_buffer(app: &mut App) {
    if let Some(ref path) = app.buffer.path.clone() {
        if path.to_str() == Some("*scratch*") {
            app.special_buffer_ropes
                .insert("*scratch*".to_string(), app.buffer.rope.clone());
        }
    }
}

/// Stash or close whatever is currently open (notebook or plain file) so it
/// can be safely replaced. A notebook is stashed whole; a plain buffer is
/// closed with the LSP (skipping virtual/special paths, which were never
/// opened with it) and its unsaved edits are kept in memory.
pub(super) fn teardown_current_buffer(app: &mut App) {
    // A table session holds no unsaved state (the view is read-only), but it
    // does hold a parsed copy of the file and the cursor cell — stash both, so
    // reading a cell in its own buffer and coming straight back lands on the
    // same cell without re-parsing.  An in-flight load has nothing worth
    // keeping and is simply dropped.
    if let Some(session) = app.table.take() {
        app.table_buffers.insert(session.id.clone(), session);
    }
    app.table_pending = None;
    // Leaving a `*cell …*` buffer, whichever way: undo the settings it forced.
    super::table::leave_cell_buffer(app);
    if app.notebook.is_some() {
        // Stash the open notebook so edits are preserved if the user comes back.
        // (After this, `app.buffer` holds stale cell text — do NOT stash it,
        // and do NOT did_close it: it was never opened with the LSP under
        // that virtual path.)
        notebook::stash_current_notebook(app);
    } else {
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

/// Switch the editor to a named special buffer (`*scratch*` or `*Messages*`).
pub fn switch_to_special_buffer(app: &mut App, name: &str) {
    save_current_special_buffer(app);
    teardown_current_buffer(app);

    let rope = match name {
        "*scratch*" => app
            .special_buffer_ropes
            .get("*scratch*")
            .cloned()
            .unwrap_or_else(|| Rope::from_str(SCRATCH_INTRO)),
        // *Messages* is the one special buffer with a live source: rebuild it
        // from the accumulated log rather than from the stash.
        "*Messages*" => {
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
    app.selection = Selection::point(0);
    app.scroll_row = 0;
    app.scroll_col = 0;
    app.insert_session_active = false;
    app.lsp_language = None;
    app.highlighter = crate::highlight::Highlighter::new(None);
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

    if app.table_buffers.contains_key(&SourceId::of(path)) {
        // A derived table (a frequency table, later a query result) is virtual,
        // so it would otherwise fall into the special-buffer branch below and
        // open as an empty text buffer named after itself.
        super::table::open_as_table(app, path);
    } else if is_special_path(path) {
        switch_to_special_buffer(app, path.to_str().unwrap_or("*scratch*"));
    } else if path.extension().and_then(|e| e.to_str()) == Some("ipynb") {
        open_as_notebook(app, path);
    } else if app.config.table.auto_open && super::table::is_table_path(path) {
        super::table::open_as_table(app, path);
    } else {
        lsp::open_file_at(app, path, 0, 0);
    }
}

/// Cycle through `open_buffers` by `delta` (+1 = next, -1 = prev).
pub(super) fn navigate_buffer(app: &mut App, delta: i32) {
    let n = app.open_buffers.len();
    if n <= 1 {
        return;
    }

    let current = if let Some(ref session) = app.table {
        // The table view's buffer is detached, so the session holds the identity.
        session.id.clone()
    } else if let Some(ref origin) = app.table_cell_origin {
        // A cell buffer sits "at" the table it was read out of, so H/L step
        // away from that table rather than restarting from the list head.
        origin.id.clone()
    } else if let Some((ref nb, _)) = app.notebook {
        SourceId::of(&nb.path)
    } else if let Some(ref p) = app.buffer.path {
        SourceId::of(p)
    } else {
        return;
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

    // Save scratch content when leaving it.
    save_current_special_buffer(app);

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