use crate::{
    app::{App, CellEditSession},
    buffer::Buffer,
    highlight::Highlighter,
    lang::lang_to_ext,
    lsp::{path_to_uri, NotebookCell},
    notebook::CellType,
    selection::Selection,
};

/// Resolve the notebook's parent directory, falling back to cwd.
pub(super) fn notebook_dir(path: &std::path::Path) -> std::path::PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
}

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

        let ext = lang_to_ext(&language);
        let stem = notebook_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "notebook".into());
        let dir = notebook_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let virtual_path = dir.join(format!("{stem}__cell{idx}.{ext}"));

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
    let ext = lang_to_ext(lang);
    let stem = nb.path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "notebook".into());
    let dir = nb.path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    nb.cells.iter().enumerate().map(|(idx, cell)| {
        let kind = match cell.cell_type { CellType::Code => 2, _ => 1 };
        let cell_path = dir.join(format!("{stem}__cell{idx}.{ext}"));
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

