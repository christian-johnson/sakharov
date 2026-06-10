//! Buffer-list management: special buffers (scratch/messages), buffer
//! switching/stashes, notebook open, new-file/new-notebook creation, and the
//! session-wide unsaved-changes sweep.

use ropey::Rope;

use crate::{app::App, mode::Mode, selection::Selection};

use super::{lsp, notebook, rebuild_diag_cache, recompute_highlights};

pub(crate) const SCRATCH_INTRO: &str = "\
;; This buffer is for notes you don't want to save.\n\
;; Use it for scratch text.\n";

/// Returns true for virtual buffer names that don't correspond to real files.
pub fn is_special_path(path: &std::path::Path) -> bool {
    matches!(path.to_str(), Some("*scratch*") | Some("*Messages*"))
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

/// Switch the editor to a named special buffer (`*scratch*` or `*Messages*`).
pub fn switch_to_special_buffer(app: &mut App, name: &str) {
    save_current_special_buffer(app);

    if app.notebook.is_some() {
        // Stash the open notebook so edits are preserved if the user comes back.
        // (After this, `app.buffer` holds stale cell text — do NOT stash it.)
        notebook::stash_current_notebook(app);
    } else {
        // Plain buffer: close it with the LSP and keep its unsaved edits in memory.
        if let (Some(ref lang), Some(ref old_path)) =
            (app.lsp_language.clone(), app.buffer.path.clone())
        {
            if !is_special_path(old_path) {
                app.lsp.did_close(lang, old_path);
            }
        }
        stash_current_file_buffer(app);
    }

    let rope = if name == "*scratch*" {
        app.special_buffer_ropes
            .get("*scratch*")
            .cloned()
            .unwrap_or_else(|| Rope::from_str(SCRATCH_INTRO))
    } else {
        // *Messages*: rebuild from the accumulated log.
        let content = if app.messages.log.is_empty() {
            String::new()
        } else {
            let mut s = app.messages.log.join("\n");
            s.push('\n');
            s
        };
        Rope::from_str(&content)
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

/// Cycle through `open_buffers` by `delta` (+1 = next, -1 = prev).
pub(super) fn navigate_buffer(app: &mut App, delta: i32) {
    let n = app.open_buffers.len();
    if n <= 1 {
        return;
    }

    let current_canon = if let Some((ref nb, _)) = app.notebook {
        nb.path.canonicalize().unwrap_or_else(|_| nb.path.clone())
    } else if let Some(ref p) = app.buffer.path {
        p.canonicalize().unwrap_or_else(|_| p.clone())
    } else {
        return;
    };

    let current_idx = app.open_buffers.iter().position(|p| {
        p.canonicalize().unwrap_or_else(|_| p.clone()) == current_canon
    });

    let idx = match current_idx {
        Some(i) => ((i as i32 + delta).rem_euclid(n as i32)) as usize,
        None => 0,
    };

    let target = app.open_buffers[idx].clone();
    if is_special_path(&target) {
        switch_to_special_buffer(app, target.to_str().unwrap_or("*scratch*"));
    } else if target.extension().and_then(|e| e.to_str()) == Some("ipynb") {
        open_as_notebook(app, &target);
    } else {
        lsp::open_file_at(app, &target, 0, 0);
    }
}

/// Open a `.ipynb` file as a notebook, replacing whatever is currently open.
/// Called when the user selects a notebook from the buffer picker.
pub fn open_as_notebook(app: &mut App, path: &std::path::Path) {
    use crate::{notebook::Notebook, notebook_state::NotebookState};

    // Save scratch content when leaving it.
    save_current_special_buffer(app);

    // Stash or close whatever is currently open.
    if app.notebook.is_some() {
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
    let key = path.canonicalize().unwrap_or(path);
    let buf = std::mem::replace(&mut app.buffer, crate::buffer::Buffer::new_empty());
    app.file_buffers.insert(key, buf);
}

/// Remove and return the stashed buffer for `path`, if one exists.
pub(crate) fn take_stashed_file_buffer(
    app: &mut App,
    path: &std::path::Path,
) -> Option<crate::buffer::Buffer> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    app.file_buffers.remove(&key)
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
    for (path, (nb, _)) in &app.notebook_buffers {
        if nb.modified {
            names.push(short(path));
        }
    }
    for (path, buf) in &app.file_buffers {
        if buf.modified {
            names.push(short(path));
        }
    }
    names
}

/// Append `path` to `open_buffers` if it is not already present (by canonical path).
pub(super) fn register_buffer(open_buffers: &mut Vec<std::path::PathBuf>, path: &std::path::Path) {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !open_buffers.iter().any(|stored| {
        stored.canonicalize().unwrap_or_else(|_| stored.clone()) == canon
    }) {
        open_buffers.push(canon);
    }
}