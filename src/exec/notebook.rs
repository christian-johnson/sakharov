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
    let mut added: Option<usize> = None;
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
        added = Some(new_idx);
    }
    after_structural_edit(app);
    if let Some(idx) = added {
        app.messages.show(format!("New cell [{}]", idx + 1));
    }
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

/// Write `app.buffer.rope` back to the currently focused notebook cell,
/// propagating the buffer's modified flag to the notebook (same discipline as
/// `input::sync_buffer_to_notebook`).
pub(super) fn save_focused_cell(app: &mut App) {
    if let Some((ref mut nb, ref state)) = app.notebook {
        let idx = state.focused_cell;
        if idx < nb.cells.len() {
            nb.cells[idx].source = app.buffer.rope.clone();
            if app.buffer.modified {
                nb.modified = true;
            }
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
/// code cells are queued and run as soon as the kernel is free — a second
/// `:run` while a cell is executing enqueues instead of refusing.
pub(super) fn execute_focused_cell(app: &mut App) {
    save_focused_cell(app);
    let Some(idx) = app.notebook.as_ref().map(|(_, s)| s.focused_cell) else { return };
    queue_cells(app, idx..idx + 1);
}

/// Execute every cell in order (`below_only` starts from the focused cell).
/// Markdown cells render; code cells are queued and run sequentially.
pub(super) fn execute_all_cells(app: &mut App, below_only: bool) {
    save_focused_cell(app);
    let Some((nb, state)) = app.notebook.as_ref() else { return };
    let start = if below_only { state.focused_cell } else { 0 };
    let end = nb.cells.len();
    queue_cells(app, start..end);
}

/// Render any markdown cells in `range` and queue the code cells for
/// execution, starting the kernel if needed. Shared by run / run-all.
fn queue_cells(app: &mut App, range: std::ops::Range<usize>) {
    let mut queued: Vec<String> = Vec::new();
    let mut rendered = 0usize;
    if let Some((nb, _)) = app.notebook.as_mut() {
        for idx in range {
            let Some(cell) = nb.cells.get_mut(idx) else { continue };
            match cell.cell_type {
                CellType::Markdown => {
                    cell.rendered = true;
                    rendered += 1;
                }
                CellType::Code => queued.push(cell.id.clone()),
                CellType::Raw => {}
            }
        }
    }
    if rendered > 0 {
        app.mode = crate::mode::Mode::Normal;
    }
    if queued.is_empty() {
        app.messages.show(if rendered > 0 { "Rendered markdown" } else { "No code cells to run" });
        return;
    }
    if !ensure_kernel(app) {
        return;
    }
    let n = queued.len();
    if let Some((_, state)) = app.notebook.as_mut() {
        state.exec_queue.extend(queued);
    }
    // Try to start immediately; otherwise report what's waiting and why.
    if !pump_execution_queue(app) {
        let starting = app.notebook.as_ref().and_then(|(nb, _)| nb.kernel.as_ref())
            .map(|k| k.status == crate::notebook::KernelStatus::Starting)
            .unwrap_or(false);
        let plural = if n == 1 { "cell" } else { "cells" };
        app.messages.show(if starting {
            format!("Queued {n} {plural} — waiting for kernel to start")
        } else {
            format!("Queued {n} {plural} — kernel busy")
        });
    }
}

/// Make sure a kernel process exists and is alive (booting counts), spawning
/// one asynchronously if needed. Returns false when the spawn itself failed.
fn ensure_kernel(app: &mut App) -> bool {
    let Some((nb, _)) = app.notebook.as_mut() else { return false };
    if nb.kernel.as_mut().map(|k| k.is_alive()).unwrap_or(false) {
        return true;
    }
    let nb_dir = crate::notebook::notebook_dir(&nb.path);
    match nb.start_kernel(&nb_dir) {
        Ok(found_venv) => {
            let python = nb.kernel.as_ref().map(|k| k.python.clone()).unwrap_or_default();
            app.messages.show(if found_venv {
                format!("Kernel starting ({python})…")
            } else {
                "Kernel starting (no venv found — using system python3)…".to_string()
            });
            true
        }
        Err(e) => {
            app.messages.show(format!("Kernel start failed: {e}"));
            false
        }
    }
}

/// Start the next queued cell if the kernel is idle and nothing is executing.
/// Returns true when state changed (a cell started, or the queue drained
/// stale entries). Called after every kernel event and after queueing.
pub(super) fn pump_execution_queue(app: &mut App) -> bool {
    use crate::notebook::KernelStatus;

    let mut started: Option<(usize, usize)> = None; // (cell idx, cells still queued)
    let mut failed: Option<String> = None;
    if let Some((nb, state)) = app.notebook.as_mut() {
        if state.executing_cell.is_some() || state.exec_queue.is_empty() {
            return false;
        }
        if nb.kernel.as_ref().map(|k| k.status != KernelStatus::Idle).unwrap_or(true) {
            return false;
        }
        while let Some(id) = state.exec_queue.pop_front() {
            // Resolve by ID at start time — the cell may have been moved,
            // deleted, or converted since it was queued.
            let Some(idx) = nb.cells.iter().position(|c| c.id == id) else { continue };
            if nb.cells[idx].cell_type != CellType::Code {
                continue;
            }
            let code = nb.cells[idx].source.to_string();
            nb.cells[idx].outputs.clear();
            let Some(session) = nb.kernel.as_mut() else { break };
            // Fire-and-forget: output streams back via process_kernel_events.
            match session.start_execution(&code) {
                Ok(()) => {
                    state.executing_cell = Some(idx);
                    state.executing_since = Some(std::time::Instant::now());
                    nb.modified = true;
                    started = Some((idx, state.exec_queue.len()));
                }
                Err(e) => {
                    failed = Some(format!("Kernel error: {e}"));
                    nb.kernel = None;
                    state.exec_queue.clear();
                }
            }
            break;
        }
    }
    if let Some(msg) = failed {
        app.messages.show(msg);
        return true;
    }
    let Some((idx, remaining)) = started else { return false };
    app.messages.show(if remaining > 0 {
        format!("Running cell [{}]… ({remaining} queued)", idx + 1)
    } else {
        format!("Running cell [{}]…", idx + 1)
    });
    // Old output image Arcs were just freed; drop their Kitty cache entries so
    // freshly-streamed images upload cleanly.
    app.graphics.image_ids.clear();
    true
}

/// Kill and restart the kernel, clearing all in-memory execution state
/// (including any queued cells).
pub(super) fn restart_kernel(app: &mut App) {
    if let Some((nb, state)) = app.notebook.as_mut() {
        nb.kernel = None;
        state.executing_cell = None;
        state.executing_since = None;
        state.exec_queue.clear();
        let nb_dir = crate::notebook::notebook_dir(&nb.path);
        match nb.start_kernel(&nb_dir) {
            Ok(found_venv) => {
                app.messages.show(if found_venv {
                    "Kernel restarting…"
                } else {
                    "Kernel restarting (no venv found — using system python3)…"
                });
            }
            Err(e) => app.messages.show(format!("Kernel restart failed: {e}")),
        }
    }
}

/// Send SIGINT to the running kernel and drop any queued cells.
pub(super) fn interrupt_kernel(app: &mut App) {
    if let Some((nb, state)) = app.notebook.as_mut() {
        if let Some(ref session) = nb.kernel {
            session.interrupt();
            let dropped = state.exec_queue.len();
            state.exec_queue.clear();
            app.messages.show(if dropped > 0 {
                format!("Kernel interrupted — {dropped} queued cell(s) dropped")
            } else {
                "Kernel interrupted".to_string()
            });
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
            app.messages.show(format!("Cleared outputs of cell [{}]", idx + 1));
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
    let mut deleted: Option<usize> = None;
    if let Some((nb, state)) = app.notebook.as_mut() {
        if !nb.cells.is_empty() {
            nb.cells.remove(state.focused_cell);
            nb.modified = true;
            deleted = Some(state.focused_cell);
            state.focused_cell = state.focused_cell.min(nb.cells.len().saturating_sub(1));
        }
    }
    let _ = crate::kitty::clear_images();
    app.graphics.image_ids.clear();
    after_structural_edit(app);
    if let Some(idx) = deleted {
        app.messages.show(format!("Deleted cell [{}] — :notebook-undo-structural to restore", idx + 1));
    }
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

