use crate::{
    app::App,
    buffer::Buffer,
    highlight::Highlighter,
    lsp::{path_to_uri, NotebookCell},
    notebook::CellType,
    selection::Selection,
};

/// Keep the focused cell on-screen, reading the viewport/image settings off
/// `app`. Wraps the long [`NotebookState::ensure_focused_visible`] argument list
/// so call sites don't repeat it.
pub(super) fn ensure_focused_visible(app: &mut App) {
    let image_rows = app.config.notebook.image_rows;
    let cell_px = app.graphics.cell_pixel_size;
    let viewport_height = app.viewport_height;
    let available_cols = app.viewport_width.saturating_sub(2) as u16;
    if let Some((nb, state)) = app.notebook.as_mut() {
        state.ensure_focused_visible(
            &nb.cells,
            viewport_height,
            &app.buffer.rope,
            image_rows,
            cell_px,
            available_cols,
        );
    }
}

/// The fix-up ritual every structural cell change (add / delete / convert /
/// structural undo-redo) must run: keep the focused cell visible, reload it into
/// `app.buffer`, resync the notebook with the LSP (cell URIs shift on add/delete),
/// and return to Normal mode.
pub(super) fn after_structural_edit(app: &mut App) {
    ensure_focused_visible(app);
    load_focused_cell(app);
    notebook_lsp_reopen(app);
    app.mode = crate::mode::Mode::Normal;
}

/// Insert a fresh empty code cell above or below the focused cell, focus it, and
/// run the structural-edit fix-up. Shared by the new-cell-above/below commands.
pub(super) fn insert_new_cell(app: &mut App, above: bool) {
    save_focused_cell(app);
    push_cell_snapshot(app);
    if let Some((nb, state)) = app.notebook.as_mut() {
        let new_idx = if above {
            state.focused_cell
        } else {
            (state.focused_cell + 1).min(nb.cells.len())
        };
        nb.cells.insert(new_idx, crate::notebook::Cell {
            id: crate::notebook::new_cell_id(),
            cell_type: CellType::Code,
            source: ropey::Rope::new(),
            outputs: vec![],
            execution_count: None,
            rendered: false,
        });
        state.focused_cell = new_idx;
        nb.modified = true;
    }
    after_structural_edit(app);
}

/// Apply one structural undo (or redo) step: pop the snapshot, restore the
/// cell list + focus, and run the structural-edit fix-up ritual.
pub(super) fn structural_history_step(app: &mut App, redo: bool) {
    let snap = {
        let current = app.notebook.as_ref()
            .map(|(nb, state)| (state.focused_cell, nb.cells.clone()));
        if let Some((focused, cells)) = current {
            if let Some((_, ref mut state)) = app.notebook {
                if redo {
                    state.pop_snapshot_redo(focused, &cells)
                } else {
                    state.pop_snapshot_undo(focused, &cells)
                }
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some((focused, cells)) = snap {
        if let Some((ref mut nb, ref mut state)) = app.notebook {
            nb.cells = cells;
            nb.modified = true;
            state.focused_cell = focused.min(nb.cells.len().saturating_sub(1));
        }
        after_structural_edit(app);
    } else {
        app.messages.show(if redo { "Nothing to redo" } else { "Nothing to undo" });
    }
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

        let virtual_path = crate::notebook::cell_virtual_path(&notebook_path, &language, idx);

        app.buffer = Buffer::new_empty();
        app.buffer.rope = source;
        app.buffer.path = Some(virtual_path.clone());
        app.selection = Selection::point(0);
        app.scroll_row = 0;
        app.scroll_col = 0;
        app.insert_session_active = false;
        // Loading a cell never starts in the full-screen overlay.
        app.cell_focused_edit = false;

        app.highlighter = Highlighter::new(Some(&virtual_path));
        super::recompute_highlights(app);

        // Ensure the LSP server is running. Cell documents themselves are synced
        // by notebook_lsp_open / lsp_did_change, which handle both notebook-sync
        // and plain-doc servers per server.
        if let Some(server_config) = app.config.language_servers.get(&language).cloned() {
            let nb_dir = app.notebook.as_ref()
                .and_then(|(nb, _)| nb.path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.to_path_buf()));
            app.lsp.ensure_server(&language, &server_config, nb_dir.as_deref());
        }
    }
}

/// Stash the current notebook into `app.notebook_buffers` so it can be restored
/// when the user navigates back.  Syncs the focused cell, closes LSP documents,
/// and clears `app.notebook` / the focused-edit flag.
pub fn stash_current_notebook(app: &mut App) {
    save_focused_cell(app);
    notebook_lsp_close(app);
    let _ = crate::kitty::clear_images();
    app.graphics.image_ids.clear();
    if let Some((nb, state)) = app.notebook.take() {
        let key = nb.path.canonicalize().unwrap_or_else(|_| nb.path.clone());
        app.notebook_buffers.insert(key, (nb, state));
    }
    app.cell_focused_edit = false;
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
    app.mode = crate::mode::Mode::Normal;
    load_focused_cell(app);
    super::recompute_highlights(app);
    // The stash closed the notebook on the LSP; re-register all cells so
    // cross-cell completion/diagnostics/definition work again.
    notebook_lsp_open(app);
    true
}

/// Execute the focused cell. Markdown cells "execute" by rendering (no kernel);
/// code cells start a kernel if needed and stream output asynchronously.
pub(super) fn execute_focused_cell(app: &mut App) {
    save_focused_cell(app);

    // "Executing" a Markdown cell just renders it (no kernel involvement).
    let is_markdown = app.notebook.as_ref().and_then(|(nb, state)| {
        nb.cells.get(state.focused_cell).map(|c| c.cell_type == CellType::Markdown)
    }).unwrap_or(false);
    if is_markdown {
        if let Some((nb, state)) = app.notebook.as_mut() {
            let idx = state.focused_cell;
            if idx < nb.cells.len() {
                nb.cells[idx].rendered = true;
            }
        }
        app.mode = crate::mode::Mode::Normal;
        app.messages.show("Rendered markdown");
        return;
    }

    // One cell at a time: the persistent kernel is a single REPL.
    let busy = app.notebook.as_ref()
        .map(|(_, state)| state.executing_cell.is_some())
        .unwrap_or(false);
    if busy {
        app.messages.show("Kernel busy — wait or :interrupt-kernel");
        return;
    }

    if let Some((nb, state)) = app.notebook.as_mut() {
        let nb_dir = crate::notebook::notebook_dir(&nb.path);
        if nb.kernel.is_none() || !nb.kernel.as_mut().map(|k| k.is_alive()).unwrap_or(false) {
            match nb.start_kernel(&nb_dir) {
                Ok(found_venv) => {
                    if !found_venv {
                        app.messages.show(
                            "Kernel started (no venv found — using system python3)",
                        );
                    }
                }
                Err(e) => {
                    app.messages.show(format!("Kernel start failed: {e}"));
                    return;
                }
            }
        }
        let idx = state.focused_cell;
        if idx < nb.cells.len() {
            let code = nb.cells[idx].source.to_string();
            nb.cells[idx].outputs.clear();
            if let Some(ref mut session) = nb.kernel {
                // Fire-and-forget: output streams back via process_kernel_events.
                match session.start_execution(&code) {
                    Ok(()) => {
                        state.executing_cell = Some(idx);
                        nb.modified = true;
                        app.messages.show(format!("Running cell [{}]…", idx + 1));
                    }
                    Err(e) => {
                        app.messages.show(format!("Kernel error: {e}"));
                        nb.kernel = None;
                    }
                }
            }
        }
    }
    // Old output image Arcs were just freed; drop their Kitty cache entries so
    // freshly-streamed images upload cleanly.
    app.graphics.image_ids.clear();
}

/// Kill and restart the kernel, clearing all in-memory execution state.
pub(super) fn restart_kernel(app: &mut App) {
    if let Some((nb, state)) = app.notebook.as_mut() {
        nb.kernel = None;
        state.executing_cell = None;
        let nb_dir = crate::notebook::notebook_dir(&nb.path);
        match nb.start_kernel(&nb_dir) {
            Ok(found_venv) => {
                app.messages.show(if found_venv {
                    "Kernel restarted"
                } else {
                    "Kernel restarted (no venv found — using system python3)"
                });
            }
            Err(e) => app.messages.show(format!("Kernel restart failed: {e}")),
        }
    }
}

/// Send SIGINT to the running kernel.
pub(super) fn interrupt_kernel(app: &mut App) {
    if let Some((nb, _)) = app.notebook.as_ref() {
        if let Some(ref session) = nb.kernel {
            session.interrupt();
            app.messages.show("Kernel interrupted");
        } else {
            app.messages.show("No kernel running");
        }
    }
}

/// Clear the focused cell's outputs, deleting any Kitty image placements first.
pub(super) fn clear_outputs(app: &mut App) {
    if let Some((nb, state)) = app.notebook.as_mut() {
        let idx = state.focused_cell;
        if idx < nb.cells.len() {
            if app.graphics.terminal.supports_graphics() {
                use crate::notebook::Output;
                // Per-ID deletion (a=d,i=N) is more reliable than catch-all a=d.
                let ids: Vec<u32> = nb.cells[idx].outputs.iter()
                    .filter_map(|o| {
                        let png = match o {
                            Output::DisplayData { data } => data.image_png.as_ref(),
                            Output::ExecuteResult { data, .. } => data.image_png.as_ref(),
                            _ => None,
                        }?;
                        let ptr_key = std::sync::Arc::as_ptr(png) as usize;
                        app.graphics.image_ids.remove(&ptr_key)
                    })
                    .collect();
                let _ = crate::kitty::delete_images(&ids);
            }
            nb.cells[idx].outputs.clear();
            nb.modified = true;
        }
    }
}

/// Convert the focused cell between code and markdown, clearing code-only state
/// and resyncing the LSP under the new language id.
pub(super) fn convert_cell(app: &mut App, to_markdown: bool) {
    save_focused_cell(app);
    push_cell_snapshot(app);
    if let Some((nb, state)) = app.notebook.as_mut() {
        let idx = state.focused_cell;
        if idx < nb.cells.len() {
            let cell = &mut nb.cells[idx];
            cell.cell_type = if to_markdown { CellType::Markdown } else { CellType::Code };
            // Outputs / execution counts only belong to code cells.
            cell.outputs.clear();
            cell.execution_count = None;
            // Show the source for editing; the user re-runs to render.
            cell.rendered = false;
            nb.modified = true;
        }
    }
    // The cell's LSP language id changed (python ↔ markdown) and its virtual
    // document must be reopened under the new language.
    after_structural_edit(app);
    app.messages.show(if to_markdown { "Cell → markdown" } else { "Cell → code" });
}

/// Delete the focused cell (a no-op on an empty notebook).
pub(super) fn delete_cell(app: &mut App) {
    save_focused_cell(app);
    push_cell_snapshot(app);
    if let Some((nb, state)) = app.notebook.as_mut() {
        if !nb.cells.is_empty() {
            nb.cells.remove(state.focused_cell);
            nb.modified = true;
            state.focused_cell = state.focused_cell.min(nb.cells.len().saturating_sub(1));
        }
    }
    let _ = crate::kitty::clear_images();
    app.graphics.image_ids.clear();
    after_structural_edit(app);
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

/// Register the currently-loaded notebook with every initialized server
/// (`notebookDocument/didOpen` or per-cell `didOpen`, chosen per server).
pub fn notebook_lsp_open(app: &mut App) {
    if let Some((ref nb, _)) = app.notebook {
        let lang = nb.metadata.kernel_language.clone();
        if !app.lsp.is_ready(&lang) {
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
        // Also drop the shadow concatenated document used for hover/signature/
        // references requests, wherever it was lazily opened.
        let shadow = crate::notebook::concat_virtual_path(&nb.path, &lang);
        app.lsp.did_close(&lang, &shadow);
    }
}

/// Close and immediately reopen the notebook in LSP after a structural change.
pub(super) fn notebook_lsp_reopen(app: &mut App) {
    notebook_lsp_close(app);
    notebook_lsp_open(app);
}

