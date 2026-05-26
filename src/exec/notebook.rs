use crate::{
    app::{App, CellEditSession},
    buffer::Buffer,
    highlight::Highlighter,
    lsp::{path_to_uri, NotebookCell},
    notebook::CellType,
    selection::Selection,
};

/// Snapshot the full cell list before a structural mutation (undo support).
pub(super) fn push_cell_snapshot(app: &mut App) {
    let snapshot = app.notebook.as_ref()
        .map(|(nb, state)| (state.focused_cell, nb.cells.clone()));
    if let Some((focused, cells)) = snapshot {
        if let Some((_, ref mut state)) = app.notebook {
            state.push_snapshot(focused, &cells);
        }
    }
}

/// Write `app.buffer.rope` back to the currently focused notebook cell.
pub(super) fn save_focused_cell(app: &mut App) {
    if let Some((ref mut nb, ref state)) = app.notebook {
        let idx = state.focused_cell;
        if idx < nb.cells.len() {
            nb.cells[idx].source = app.buffer.rope.clone();
        }
    }
}

/// Load the focused notebook cell into app.buffer, updating all dependent state.
pub fn load_focused_cell(app: &mut App) {
    if let Some((ref nb, ref state)) = app.notebook {
        let idx = state.focused_cell;
        if idx >= nb.cells.len() {
            return;
        }
        let cell = &nb.cells[idx];
        let language = nb.metadata.kernel_language.clone();
        let notebook_path = nb.path.clone();
        let source = cell.source.clone();

        let virtual_path = crate::notebook::cell_virtual_path(&notebook_path, &language, idx);

        app.buffer = Buffer::new_empty();
        app.buffer.rope = source;
        app.buffer.path = Some(virtual_path.clone());
        app.selection = Selection::point(0);
        app.scroll_row = 0;
        app.scroll_col = 0;
        app.insert_session_active = false;

        app.notebook_cell_edit = Some(CellEditSession {
            cell_index: idx,
            language: language.clone(),
            notebook_path,
            focused_edit: false,
        });

        app.highlighter = Highlighter::new(Some(&virtual_path));
        super::recompute_highlights(app);

        // Ensure the LSP server is running.
        if let Some(server_config) = app.config.language_servers.get(&language).cloned() {
            let nb_dir = app.notebook.as_ref()
                .and_then(|(nb, _)| nb.path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.to_path_buf()));
            app.lsp.ensure_server(&language, &server_config, nb_dir.as_deref());
        }

        if let Some(ref session) = app.notebook_cell_edit {
            if !app.lsp.notebook_sync_supported(&session.language) {
                let text = app.buffer.rope.to_string();
                if app.lsp.is_doc_open(&session.language, &virtual_path) {
                    app.lsp.did_change(&session.language, &virtual_path, &text);
                } else {
                    app.lsp.did_open(&session.language, &virtual_path, &text);
                }
            }
        }
    }
}

/// Stash the current notebook into `app.notebook_buffers` so it can be restored
/// when the user navigates back.  Syncs the focused cell, closes LSP documents,
/// and clears `app.notebook` / `app.notebook_cell_edit`.
pub fn stash_current_notebook(app: &mut App) {
    save_focused_cell(app);
    notebook_lsp_close(app);
    // Remove visible Kitty images and invalidate ID cache before leaving.
    let _ = crate::kitty::clear_images();
    app.kitty_image_ids.clear();
    if let Some((nb, state)) = app.notebook.take() {
        let key = nb.path.canonicalize().unwrap_or_else(|_| nb.path.clone());
        app.notebook_buffers.insert(key, (nb, state));
    }
    app.notebook_cell_edit = None;
}

/// Restore a previously stashed notebook.  Returns `true` and updates all app
/// state when a stash is found; returns `false` when no stash exists for `path`
/// (caller should load from disk instead).
pub fn restore_stashed_notebook(app: &mut App, path: &std::path::Path) -> bool {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Some((nb, state)) = app.notebook_buffers.remove(&key) else {
        return false;
    };
    let lang = nb.metadata.kernel_language.clone();
    app.lsp_language = Some(lang);
    app.notebook = Some((nb, state));
    app.mode = crate::mode::Mode::Notebook;
    load_focused_cell(app);
    super::recompute_highlights(app);
    true
}

/// Generate a simple unique cell ID.
pub(super) fn new_cell_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{t:016x}{n:016x}")
}

/// Build the full cell list for `notebookDocument/didOpen` or a reopen.
fn build_notebook_cells(nb: &crate::notebook::Notebook) -> Vec<NotebookCell> {
    let lang = &nb.metadata.kernel_language;
    nb.cells.iter().enumerate().map(|(idx, cell)| {
        let kind = match cell.cell_type { CellType::Code => 2, _ => 1 };
        let cell_path = crate::notebook::cell_virtual_path(&nb.path, lang, idx);
        let language_id = match cell.cell_type {
            CellType::Code => lang.clone(),
            CellType::Markdown => "markdown".into(),
            _ => "plaintext".into(),
        };
        NotebookCell {
            kind,
            uri: path_to_uri(&cell_path),
            language_id,
            text: cell.source.to_string(),
        }
    }).collect()
}

/// Send `notebookDocument/didOpen` for the currently-loaded notebook.
pub fn notebook_lsp_open(app: &mut App) {
    if let Some((ref nb, _)) = app.notebook {
        let lang = nb.metadata.kernel_language.clone();
        if !app.lsp.is_ready(&lang) || !app.lsp.notebook_sync_supported(&lang) {
            return;
        }
        let notebook_uri = path_to_uri(&nb.path);
        let cells = build_notebook_cells(nb);
        app.lsp.notebook_did_open(&lang, &notebook_uri, &cells);
    }
}

pub(super) fn notebook_lsp_close(app: &mut App) {
    if let Some((ref nb, _)) = app.notebook {
        let lang = nb.metadata.kernel_language.clone();
        let notebook_uri = path_to_uri(&nb.path);
        app.lsp.notebook_did_close(&lang, &notebook_uri);
    }
}

/// Close and immediately reopen the notebook in LSP after a structural change.
pub(super) fn notebook_lsp_reopen(app: &mut App) {
    notebook_lsp_close(app);
    notebook_lsp_open(app);
}

