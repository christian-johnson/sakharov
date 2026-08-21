use crate::{
    app::App,
    motion,
    buffer::Buffer,
    highlight::Highlighter,
    lsp::{path_to_uri, NotebookCell},
    notebook::CellType,
    selection::Selection,
    source::SourceId,
};

/// The geometry every notebook height question is measured against, taken from
/// the terminal the frame will be drawn into.
///
/// The one place `exec` derives it.  Five call sites used to build the same
/// `(cell_pixel_size, viewport_width - 2, word_wrap)` triple by hand, and the
/// renderer built a sixth from `area.width` — six chances for the scroll math
/// and the renderer to disagree about how tall a cell is.
pub(crate) fn geometry(app: &App) -> crate::notebook_ui::Geometry {
    crate::notebook_ui::Geometry::new(
        app.viewport_width as u16,
        app.graphics.cell_pixel_size,
        app.config.editor.word_wrap,
    )
}

/// The output caps for cell `idx`, honouring whether the user expanded it.
pub(crate) fn output_limits(app: &App, idx: usize) -> crate::notebook_ui::OutputLimits {
    let expanded = app
        .notebook
        .as_ref()
        .is_some_and(|(_, state)| state.is_output_expanded(idx));
    crate::notebook_ui::OutputLimits::new(&app.config.notebook, expanded)
}

/// The fix-up ritual every structural cell change (add / delete / convert /
/// structural undo-redo) must run: reload the focused cell into `app.buffer`,
/// resync the notebook with the LSP (cell URIs shift on add/delete), and
/// return to Normal mode.  Scroll follows the focused cell automatically —
/// `exec::update_scroll` re-anchors it every frame.
pub(super) fn after_structural_edit(app: &mut App) {
    // Output expansion is keyed by cell index, which every structural edit
    // shifts — a stale entry would silently expand the wrong cell and throw
    // the height model (and so the scroll anchor) off.
    if let Some((_, state)) = app.notebook.as_mut() {
        state.expanded_outputs.clear();
    }
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

/// Flush the focused cell, then serialise the notebook to disk.
///
/// Clearing `app.buffer.modified` is part of the save: the focused cell *is*
/// `app.buffer`, and every keystroke re-propagates that flag onto the notebook
/// (`save_focused_cell` / `input::sync_buffer_to_notebook`). Leaving it set
/// makes `[+]` reappear on the first cursor move after a clean save.
pub(super) fn save_notebook(app: &mut App) -> anyhow::Result<()> {
    save_focused_cell(app);
    let result = match app.notebook.as_mut() {
        Some((nb, _)) => nb.save(),
        None => return Ok(()),
    };
    if result.is_ok() {
        app.buffer.modified = false;
    }
    result
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
        let key = crate::source::SourceId::of(&nb.path);
        app.notebook_buffers.insert(key, (nb, state));
    }
    app.cell_focused_edit = false;
}

/// Restore a previously stashed notebook.  Returns `true` and updates all app
/// state when a stash is found; returns `false` when no stash exists for `path`
/// (caller should load from disk instead).
pub fn restore_stashed_notebook(app: &mut App, path: &std::path::Path) -> bool {
    let key = crate::source::SourceId::of(path);
    let Some((nb, state)) = app.notebook_buffers.remove(&key) else {
        return false;
    };
    let lang = nb.metadata.kernel_language.clone();
    app.lsp_language = Some(lang);
    app.notebook = Some((nb, state));
    focus_notebook_session(app);
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
        let starting = notebook_session(app)
            .is_some_and(|c| *c.status() == crate::compute::KernelStatus::Starting);
        let plural = if n == 1 { "cell" } else { "cells" };
        app.messages.show(if starting {
            format!("Queued {n} {plural} — waiting for kernel to start")
        } else {
            format!("Queued {n} {plural} — kernel busy")
        });
    }
}

/// The key of the open notebook's own kernel.  One kernel per notebook, so this
/// is the notebook's own identity (see [`crate::compute`] for why sharing one
/// across notebooks is the wrong default).
pub(crate) fn notebook_key(app: &App) -> Option<SourceId> {
    app.notebook.as_ref().map(|(nb, _)| SourceId::of(&nb.path))
}

/// The open notebook's kernel, if it has one yet.
pub fn notebook_session(app: &App) -> Option<&crate::compute::ComputeSession> {
    app.compute.get(&notebook_key(app)?)
}

/// Point the *active* session at the notebook now on screen.
///
/// "Active" is which namespace a view with no kernel of its own talks to, so it
/// has to follow what the user is looking at — not the last notebook a cell
/// happened to run in.  Called on every path that makes a notebook current.
pub(super) fn focus_notebook_session(app: &mut App) {
    if let Some(key) = notebook_key(app) {
        app.compute.focus(&key);
    }
}

/// Make sure the open notebook has a live kernel of its own (booting counts),
/// spawning one asynchronously if not, and focus it so a view with no kernel of
/// its own talks to this notebook's namespace.  False when the spawn failed.
pub(super) fn ensure_kernel(app: &mut App) -> bool {
    let Some((nb, _)) = app.notebook.as_ref() else { return false };
    let key = SourceId::of(&nb.path);
    let root = crate::notebook::notebook_dir(&nb.path);
    match app.compute.ensure(&key, &root) {
        Ok(None) => true, // reused
        Ok(Some(found_venv)) => {
            let python = app
                .compute
                .get(&key)
                .map(|c| c.kernel.python.clone())
                .unwrap_or_default();
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
    use crate::compute::{Consumer, RequestKind};

    match app.notebook.as_ref() {
        Some((_, state)) if state.executing_cell.is_none() && !state.exec_queue.is_empty() => {}
        _ => return false,
    }
    let Some(key) = notebook_key(app) else { return false };
    if !app.compute.get(&key).is_some_and(|c| c.is_idle()) {
        return false;
    }

    let mut started: Option<(usize, usize)> = None; // (cell idx, cells still queued)
    let mut failed: Option<String> = None;
    while let Some((nb, state)) = app.notebook.as_mut() {
        // Resolve by ID at start time — the cell may have been moved, deleted,
        // or converted since it was queued.
        let Some(id) = state.exec_queue.pop_front() else { break };
        let Some(idx) = nb.cells.iter().position(|c| c.id == id) else { continue };
        if nb.cells[idx].cell_type != CellType::Code {
            continue;
        }
        let code = nb.cells[idx].source.to_string();
        nb.cells[idx].outputs.clear();
        let remaining = state.exec_queue.len();

        let Some(compute) = app.compute.get_mut(&key) else { break };
        // Fire-and-forget: output streams back via process_kernel_events, routed
        // by the request id to this cell's *stable* id (an index would go stale
        // if the cell moved while it was running).  That id is also the compile
        // filename, which is what makes a traceback frame a jump target.
        let kind = RequestKind::Exec { tag: id.clone() };
        match compute.request(kind, &code, Consumer::NotebookCell(id)) {
            Ok(_) => {
                if let Some((nb, state)) = app.notebook.as_mut() {
                    state.executing_cell = Some(idx);
                    state.executing_since = Some(std::time::Instant::now());
                    nb.modified = true;
                }
                started = Some((idx, remaining));
            }
            Err(e) => {
                // The pipe is broken; this notebook's kernel is gone.
                failed = Some(format!("Kernel error: {e}"));
                app.compute.shutdown(&key);
                if let Some((_, state)) = app.notebook.as_mut() {
                    state.exec_queue.clear();
                }
            }
        }
        break;
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
/// Restarts the kernel of the notebook in focus (or, with no notebook open, the
/// active session).  Other notebooks' kernels are untouched — restarting one
/// namespace must not destroy another's.
pub(super) fn restart_kernel(app: &mut App) {
    // Dropping the session invalidates *every* consumer waiting on it, not just
    // the notebook's, so the notebook's own execution state is reset too.
    let key = notebook_key(app).or_else(|| app.compute.active_key().cloned());
    let Some(key) = key else {
        app.messages.show("No kernel to restart");
        return;
    };
    // Restart against the root the old session served; failing that (no kernel
    // yet) the notebook's own directory.
    let root = app
        .compute
        .get(&key)
        .map(|c| c.root().to_path_buf())
        .or_else(|| app.notebook.as_ref().map(|(nb, _)| crate::notebook::notebook_dir(&nb.path)));
    let Some(root) = root else {
        app.messages.show("No kernel to restart");
        return;
    };
    app.compute.shutdown(&key);
    if let Some((_, state)) = app.notebook.as_mut() {
        state.executing_cell = None;
        state.executing_since = None;
        state.exec_queue.clear();
    }
    match app.compute.ensure(&key, &root) {
        Ok(started) => {
            app.messages.show(if started == Some(false) {
                "Kernel restarting (no venv found — using system python3)…"
            } else {
                "Kernel restarting…"
            });
        }
        Err(e) => app.messages.show(format!("Kernel restart failed: {e}")),
    }
}

/// Send SIGINT to the focused notebook's kernel and drop any queued cells.
pub(super) fn interrupt_kernel(app: &mut App) {
    let session = notebook_key(app)
        .and_then(|k| app.compute.get(&k))
        .or_else(|| app.compute.active());
    let Some(session) = session else {
        app.messages.show("No kernel running");
        return;
    };
    session.kernel.interrupt();
    let dropped = app
        .notebook
        .as_mut()
        .map(|(_, state)| {
            let n = state.exec_queue.len();
            state.exec_queue.clear();
            n
        })
        .unwrap_or(0);
    app.messages.show(if dropped > 0 {
        format!("Kernel interrupted — {dropped} queued cell(s) dropped")
    } else {
        "Kernel interrupted".to_string()
    });
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

// ---------------------------------------------------------------------------
// Cell-stack motion
//
// Vertical motion in a notebook flows continuously: through the focused cell's
// source, into its output block, and on into the next cell.  These live here
// rather than in `exec::mod` because they are notebook mechanics, not command
// dispatch — `super::execute()` calls them and nothing else does.
// ---------------------------------------------------------------------------

/// True when vertical motion should flow through the notebook cell stack
/// rather than staying inside the buffer (i.e. a notebook is open and we're
/// not in the full-screen single-cell overlay).
pub(super) fn notebook_vertical(app: &App) -> bool {
    app.in_notebook_nav()
}

/// Visual rows in cell `cell_idx`'s output block (0 for none), sized exactly
/// as the renderer draws it — including the cell's expand/collapse state, so
/// `j`/`k` reach every row of an expanded block.  Used by the output-block
/// navigation below.
pub(super) fn nb_output_rows(app: &App, cell_idx: usize) -> usize {
    let geo = geometry(app);
    let limits = output_limits(app, cell_idx);
    let Some((nb, _)) = app.notebook.as_ref() else { return 0 };
    nb.cells
        .get(cell_idx)
        .map(|cell| crate::notebook_ui::cell_output_rows(cell, limits, geo))
        .unwrap_or(0)
}

/// `j` inside a notebook: continue past the last source line into the cell's
/// output block, then into the next cell.  Returns true when it handled the
/// motion (the caller must not run the ordinary source `move_down`).
///
/// `extend` (Select mode) is only ever passed `true` when already browsing
/// output (see the `MoveDown` handler) — it extends the row-selection one
/// output row further but never crosses into the next cell, so a selection
/// stays within a single cell's output block.
pub(super) fn notebook_move_down(app: &mut App, extend: bool) -> bool {
    let (focused, count) = match app.notebook.as_ref() {
        Some((nb, s)) => (s.focused_cell, nb.cells.len()),
        None => return false,
    };
    let output_row = app.notebook.as_ref().and_then(|(_, s)| s.output_row);

    if let Some(r) = output_row {
        let out_rows = nb_output_rows(app, focused);
        if r + 1 < out_rows {
            if let Some((_, s)) = app.notebook.as_mut() {
                s.output_row = Some(r + 1);
                s.output_col = 0;
                if !extend {
                    s.output_anchor = None;
                }
            }
            super::update_scroll(app);
            return true;
        }
        if extend {
            return true; // pinned at the last output row while selecting
        }
        // Past the last output row → next cell's first source line.
        if focused + 1 < count {
            if let Some((_, s)) = app.notebook.as_mut() {
                s.clear_output_browsing();
            }
            switch_focused_cell(app, focused + 1);
            place_cursor_at_line(app, 0, 0);
            super::update_scroll(app);
        }
        return true; // consumed even when pinned at the very bottom
    }

    // On the source: only the last row descends into the output block.  In a
    // wrapped cell the last *line* can still have rows below the cursor, and
    // `j` must walk those before leaving the cell.
    let rope = &app.buffer.rope;
    let pos = app.selection.head.min(rope.len_chars());
    let on_last_line = rope.len_chars() == 0 || rope.char_to_line(pos) + 1 >= rope.len_lines();
    if !on_last_line || !super::at_last_visual_row(app) {
        return false;
    }
    if nb_output_rows(app, focused) > 0 {
        if let Some((_, s)) = app.notebook.as_mut() {
            s.output_row = Some(0);
            s.output_col = 0;
            s.output_anchor = None;
        }
        super::update_scroll(app);
        return true;
    }
    // No outputs: cross straight into the next cell (column preserved).
    if focused + 1 < count {
        let col = motion::col_of(rope, pos);
        switch_focused_cell(app, focused + 1);
        place_cursor_at_line(app, 0, col);
        super::update_scroll(app);
        return true;
    }
    false
}

/// `k` inside a notebook: the inverse of [`notebook_move_down`] — climb the
/// output block back to the source, then up into the previous cell (landing on
/// its last output row when it has outputs, else its last source line). See
/// [`notebook_move_down`] for the `extend` (Select mode) semantics.
pub(super) fn notebook_move_up(app: &mut App, extend: bool) -> bool {
    let focused = match app.notebook.as_ref() {
        Some((_, s)) => s.focused_cell,
        None => return false,
    };
    let output_row = app.notebook.as_ref().and_then(|(_, s)| s.output_row);

    if let Some(r) = output_row {
        if r > 0 {
            if let Some((_, s)) = app.notebook.as_mut() {
                s.output_row = Some(r - 1);
                s.output_col = 0;
                if !extend {
                    s.output_anchor = None;
                }
            }
            super::update_scroll(app);
            return true;
        }
        if extend {
            return true; // pinned at the first output row while selecting
        }
        // r == 0 climbs back onto the source (cursor already sits on the
        // last source line it descended from).
        if let Some((_, s)) = app.notebook.as_mut() {
            s.clear_output_browsing();
        }
        super::update_scroll(app);
        return true;
    }

    // On the source: only the first row crosses into the previous cell (see
    // `notebook_move_down` — a wrapped first line has rows above the cursor).
    let rope = &app.buffer.rope;
    let pos = app.selection.head.min(rope.len_chars());
    let on_first_line = rope.len_chars() == 0 || rope.char_to_line(pos) == 0;
    if !on_first_line || !super::at_first_visual_row(app) || focused == 0 {
        return false;
    }
    let col = motion::col_of(rope, pos);
    switch_focused_cell(app, focused - 1);
    let last_line = app.buffer.rope.len_lines().saturating_sub(1);
    place_cursor_at_line(app, last_line, col);
    // Land in the previous cell's output block when it has one.
    let prev_out = nb_output_rows(app, focused - 1);
    if prev_out > 0 {
        if let Some((_, s)) = app.notebook.as_mut() {
            s.output_row = Some(prev_out - 1);
            s.output_col = 0;
            s.output_anchor = None;
        }
    }
    super::update_scroll(app);
    true
}

/// Switch the focused notebook cell to `new_idx` (clamped to the valid range),
/// flushing the current cell to the LSP and notebook model first and loading the
/// target cell into `app.buffer`. The cursor lands at the start of the new cell;
/// callers wanting a specific position set the selection afterwards. No-op when
/// no notebook is open.
pub(super) fn switch_focused_cell(app: &mut App, new_idx: usize) {
    if app.notebook.is_none() {
        return;
    }
    super::lsp_did_change(app);
    save_focused_cell(app);
    if let Some((ref nb, ref mut state)) = app.notebook {
        let last = nb.cells.len().saturating_sub(1);
        state.focused_cell = new_idx.min(last);
    }
    load_focused_cell(app);
}

/// Place a point selection on `line_idx` (clamped) at column `col` (clamped to
/// the line's content), using the same column discipline as vertical motion.
pub(super) fn place_cursor_at_line(app: &mut App, line_idx: usize, col: usize) {
    let rope = &app.buffer.rope;
    if rope.len_chars() == 0 {
        app.selection = Selection::point(0);
        return;
    }
    let line_idx = line_idx.min(rope.len_lines().saturating_sub(1));
    let line_start = rope.line_to_char(line_idx);
    let line = rope.line(line_idx);
    let nl = line.len_chars();
    let content_len = if nl > 0 && (line.char(nl - 1) == '\n' || line.char(nl - 1) == '\r') {
        nl - 1
    } else {
        nl
    };
    let head = if content_len == 0 {
        line_start
    } else {
        line_start + col.min(content_len - 1)
    };
    app.selection = Selection::point(head);
}

// ---------------------------------------------------------------------------
// Output-text navigation, selection, and yank — a read-only cursor over a
// cell's rendered output block (streams, results, tracebacks), addressed
// exactly like the plain buffer but backed by a virtual rope over the
// output text instead of `app.buffer.rope` (see `notebook_ui::output_rows_content`
// / `output_virtual_rope`). Reusing `motion::*` against that rope means
// h/l/w/b/e/0/^/$ and Select-mode extension all behave identically to the
// source buffer without re-implementing char/word boundaries a second time.
// ---------------------------------------------------------------------------

/// A line's content length excluding a trailing `\n`/`\r` — the same
/// trimming `motion.rs`'s internal `line_end_char` applies, so a column
/// derived here never spills past this row into the next one when added to
/// `line_to_char`.
pub(super) fn rope_line_content_len(rope: &ropey::Rope, line_idx: usize) -> usize {
    let line = rope.line(line_idx);
    let n = line.len_chars();
    if n > 0 && matches!(line.char(n - 1), '\n' | '\r') { n - 1 } else { n }
}

/// Build the focused cell's output virtual rope (see
/// `notebook_ui::output_virtual_rope`) using exactly the geometry the
/// renderer used to lay it out, so char addresses agree with what's on
/// screen. `None` when no notebook is open or there's no focused cell.
pub(super) fn focused_output_rope(app: &App) -> Option<ropey::Rope> {
    let geo = geometry(app);
    let (nb, state) = app.notebook.as_ref()?;
    let limits = output_limits(app, state.focused_cell);
    let cell = nb.cells.get(state.focused_cell)?;
    Some(crate::notebook_ui::output_virtual_rope(cell, limits, geo))
}

/// Apply a `motion::*` function to the read-only output-text cursor exactly
/// as it would apply to `app.selection`: map `(output_row, output_col)` to a
/// char index into the focused cell's output virtual rope, run the motion,
/// map the result back. `extend` (Select mode) moves only the head,
/// establishing `output_anchor` as the fixed point (or keeping it already
/// set) — the same anchor/head contract `Selection` has, just addressed in
/// `(row, col)` instead of a flat char index. Returns whether it moved
/// anything (false only if output browsing ended between the caller's check
/// and here, which shouldn't happen in practice).
pub(super) fn output_motion(
    app: &mut App,
    extend: bool,
    f: fn(&ropey::Rope, Selection, bool) -> Selection,
) -> bool {
    let Some(rope) = focused_output_rope(app) else { return false };
    let Some(row) = app.notebook.as_ref().and_then(|(_, s)| s.output_row) else { return false };
    let output_col = app.notebook.as_ref().map(|(_, s)| s.output_col).unwrap_or(0);
    let output_anchor = app.notebook.as_ref().and_then(|(_, s)| s.output_anchor);

    let last_line = rope.len_lines().saturating_sub(1);
    let to_char = |r: usize, c: usize| {
        let r = r.min(last_line);
        rope.line_to_char(r) + c.min(rope_line_content_len(&rope, r))
    };
    let head = to_char(row, output_col);
    let anchor = output_anchor.map(|(r, c)| to_char(r, c)).unwrap_or(head);

    let new_sel = f(&rope, Selection::new(anchor, head), extend);
    let to_rowcol = |pos: usize| {
        let pos = pos.min(rope.len_chars());
        let li = rope.char_to_line(pos);
        (li, pos - rope.line_to_char(li))
    };
    let (nr, nc) = to_rowcol(new_sel.head);
    if let Some((_, state)) = app.notebook.as_mut() {
        state.output_row = Some(nr);
        state.output_col = nc;
        state.output_anchor = if extend { Some(to_rowcol(new_sel.anchor)) } else { None };
    }
    true
}

/// `y` while browsing an active output-text selection: the text spanning
/// `output_anchor` to the current `(output_row, output_col)`, inclusive of
/// the char under the head/anchor extreme (matching `text::yank_selection`'s
/// `end()+1`, so even a zero-width "selection" yanks the one char under the
/// cursor). Returns `None` when not browsing output at all, so the caller
/// can fall back to the ordinary buffer yank.
pub(super) fn yank_output_selection(app: &App) -> Option<String> {
    let rope = focused_output_rope(app)?;
    let (_, state) = app.notebook.as_ref()?;
    let row = state.output_row?;
    let last_line = rope.len_lines().saturating_sub(1);
    let to_char = |r: usize, c: usize| {
        let r = r.min(last_line);
        rope.line_to_char(r) + c.min(rope_line_content_len(&rope, r))
    };
    let head = to_char(row, state.output_col);
    let anchor = state.output_anchor.map(|(r, c)| to_char(r, c)).unwrap_or(head);
    let lo = anchor.min(head);
    let hi = (anchor.max(head) + 1).min(rope.len_chars());
    Some(rope.slice(lo..hi).to_string())
}

// ---------------------------------------------------------------------------
// Error-traceback navigation ("jump to the line that raised")
// ---------------------------------------------------------------------------

/// Resolve an [`ErrorFrame`] to a concrete `(cell index, 0-based line)` in the
/// currently-open notebook, preferring the stable cell id and falling back to
/// the 1-based cell number printed in the label.
pub(super) fn resolve_error_frame(
    nb: &crate::notebook::Notebook,
    frame: &crate::notebook::ErrorFrame,
) -> Option<(usize, usize)> {
    let idx = match &frame.cell_id {
        Some(id) => nb.cells.iter().position(|c| &c.id == id)?,
        None => frame.cell_number.checked_sub(1)?,
    };
    (idx < nb.cells.len()).then_some((idx, frame.line))
}

/// Move the cursor to `(cell_idx, line)` in the open notebook: focus that cell,
/// leave the output block, land on the line's first non-whitespace column, and
/// re-anchor the scroll. Shared by `:goto-error` and Enter-on-a-frame.
pub(super) fn jump_to_notebook_cell_line(app: &mut App, cell_idx: usize, line: usize) {
    if app.notebook.as_ref().map(|(_, s)| s.focused_cell) != Some(cell_idx) {
        switch_focused_cell(app, cell_idx);
    }
    if let Some((_, s)) = app.notebook.as_mut() {
        s.output_row = None;
    }
    let rope = &app.buffer.rope;
    let li = line.min(rope.len_lines().saturating_sub(1));
    let start = rope.line_to_char(li);
    app.selection = motion::move_line_first_non_ws(rope, Selection::point(start), false);
    super::update_scroll(app);
}

/// `:goto-error` — jump to the source line of the focused cell's error. Targets
/// the *innermost* frame (the last `File` line — where the exception actually
/// raised), which may be another cell when the culprit is a function defined
/// elsewhere.
pub(super) fn goto_focused_cell_error(app: &mut App) {
    let target = app.notebook.as_ref().and_then(|(nb, state)| {
        let cell = nb.cells.get(state.focused_cell)?;
        cell.outputs.iter().rev().find_map(|o| match o {
            crate::notebook::Output::Error { frames, .. } => {
                resolve_error_frame(nb, frames.last()?)
            }
            _ => None,
        })
    });
    match target {
        Some((idx, line)) => {
            jump_to_notebook_cell_line(app, idx, line);
            app.messages.show(format!("Jumped to cell [{}] line {}", idx + 1, line + 1));
        }
        None => app.messages.show("No navigable error in this cell"),
    }
}

/// Enter on a traceback frame line (while browsing the output block with `j`/`k`)
/// — jump to the exact source line that frame names. A no-op when the output
/// cursor isn't on a navigable frame.
pub(super) fn follow_output_error_link(app: &mut App) {
    let geo = geometry(app);
    let target = app.notebook.as_ref().and_then(|(nb, state)| {
        let orow = state.output_row?;
        let idx = state.focused_cell;
        let cell = nb.cells.get(idx)?;
        let limits = crate::notebook_ui::OutputLimits::new(
            &app.config.notebook, state.is_output_expanded(idx),
        );
        let frame = crate::notebook_ui::error_frame_at_output_row(
            cell, orow, limits, geo,
        )?;
        resolve_error_frame(nb, frame)
    });
    if let Some((idx, line)) = target {
        jump_to_notebook_cell_line(app, idx, line);
        app.messages.show(format!("Jumped to cell [{}] line {}", idx + 1, line + 1));
    }
}

/// Drain streamed output from the running kernel and apply it to the executing
/// cell. Called once per frame so outputs (incl. live progress bars) appear as
/// they are produced rather than only when the cell finishes. Also handles the
/// kernel-ready handshake and starts the next queued cell when the kernel
/// becomes idle. Returns true when state changed (the caller should redraw).
pub fn process_kernel_events(app: &mut App) -> bool {
    use crate::compute::{Consumer, KernelStatus, MessageBody};
    use crate::notebook::{append_stream, MimeData, Output};

    let mut refresh_images = false;
    let mut applied = false;
    // Status changes worth logging are collected and shown after the borrows
    // end; when several arrive in one frame the last (most recent) wins the
    // minibuffer and the log keeps them all.
    let mut announce: Vec<String> = Vec::new();

    // The active notebook's kernel went away (a failed send, a restart) while
    // one of its cells was marked as running.  Nothing will ever finish it.
    let active_orphaned = notebook_key(app)
        .is_some_and(|k| app.compute.get(&k).is_none());
    if let Some((_, ref mut state)) = app.notebook {
        if state.executing_cell.is_some() && active_orphaned {
            state.executing_cell = None;
            state.executing_since = None;
            applied = true;
        }
    }

    // Every session, not just the one on screen: a cell in a notebook the user
    // has navigated away from must still be able to finish.
    let batches = app.compute.poll_all();
    applied |= !batches.is_empty();

    for (key, msgs) in batches {
        // Whose kernel this is, named in a message only when it isn't the
        // notebook in front of the user — otherwise every line would be noise.
        let whose = match notebook_key(app) {
            Some(active) if active == key => String::new(),
            _ => format!(" for {}", key.label()),
        };
        for msg in msgs {
            // Resolve the message to whoever asked for it.  `Ready` and `Dead`
            // belong to the process rather than to a request, so they carry no
            // consumer; anything else with an unknown id is from a request that
            // has already been retired and must not be applied to what is in
            // view now.
            let consumer = app.compute.get(&key).and_then(|c| c.consumer(msg.id)).cloned();
            // A notebook cell is waiting on this reply — `cell` is where it is
            // now, which is `None` if it was deleted mid-run.  The distinction
            // matters on `Done`: the notebook's execution state must still be
            // cleared, whereas a reply nobody is waiting on must not touch it.
            let for_notebook = matches!(consumer, Some(Consumer::NotebookCell(_)));
            let cell = match consumer {
                Some(Consumer::NotebookCell(ref id)) => notebook_for_key(app, &key)
                    .and_then(|(nb, _)| nb.cells.iter().position(|c| &c.id == id)),
                _ => None,
            };
            match msg.body {
                MessageBody::Ready => {
                    if let Some(c) = app.compute.get_mut(&key) {
                        if *c.status() == KernelStatus::Starting {
                            c.kernel.status = KernelStatus::Idle;
                        }
                        announce.push(format!("Kernel ready{whose} ({})", c.kernel.python));
                    }
                }
                MessageBody::Stream { name, text } => {
                    if let (Some(idx), Some((nb, _))) = (cell, notebook_for_key(app, &key)) {
                        append_stream(&mut nb.cells[idx].outputs, &name, &text);
                    }
                }
                // Replies to the editor's own requests.  Routed by consumer,
                // like everything else: a listing that arrives after the popup
                // was dismissed, or from a kernel that has since been restarted,
                // belongs to nobody and is dropped rather than shown.
                MessageBody::Vars(items) => {
                    if matches!(consumer, Some(Consumer::VariableList)) {
                        super::bridge::show_variables(app, &items);
                    }
                }
                MessageBody::Export { path, rows } => {
                    if let Some(Consumer::ViewVariable(ref name)) = consumer {
                        let name = name.clone();
                        super::bridge::open_exported(app, &name, &path, rows);
                    }
                }
                MessageBody::Image { png } => {
                    if let (Some(idx), Some((nb, _))) = (cell, notebook_for_key(app, &key)) {
                        nb.cells[idx].outputs.push(Output::DisplayData {
                            data: MimeData { text_plain: None, image_png: Some(std::sync::Arc::new(png)) },
                        });
                        refresh_images = true;
                    }
                }
                MessageBody::Error { traceback } => {
                    if for_notebook {
                        if let (Some(idx), Some((nb, _))) = (cell, notebook_for_key(app, &key)) {
                            // Build against the whole cell list so `File "<id>"`
                            // frames resolve to jump targets; then push.
                            let out = crate::notebook::build_error_output(&traceback, &nb.cells);
                            nb.cells[idx].outputs.push(out);
                        }
                    } else if consumer.is_some() {
                        // An editor request failed — there is no cell to put a
                        // traceback in, and dropping it would leave the request
                        // looking like it simply never came back.
                        let line = traceback.lines().last().unwrap_or(&traceback);
                        announce.push(line.trim().to_string());
                    }
                }
                MessageBody::Done => {
                    let count = match app.compute.get_mut(&key) {
                        Some(c) => {
                            c.finish(msg.id);
                            c.kernel.status = KernelStatus::Idle;
                            c.kernel.execution_count += 1;
                            Some(c.kernel.execution_count)
                        }
                        None => None,
                    };
                    if for_notebook {
                        if let Some((nb, state)) = notebook_for_key(app, &key) {
                            let elapsed = state.executing_since.take().map(|t| format_duration(t.elapsed()));
                            if let Some(idx) = cell {
                                nb.cells[idx].execution_count = count;
                                let failed = nb.cells[idx].outputs.iter()
                                    .any(|o| matches!(o, Output::Error { .. }));
                                let verb = if failed { "failed" } else { "finished" };
                                // On failure, point at the jump-to-line affordances.
                                let hint = if failed { " — :goto-error (or ↵ on a File line) to jump" } else { "" };
                                announce.push(match elapsed {
                                    Some(t) => format!("Cell [{}]{whose} {verb} in {t}{hint}", idx + 1),
                                    None => format!("Cell [{}]{whose} {verb}{hint}", idx + 1),
                                });
                            }
                            state.executing_cell = None;
                            nb.modified = true;
                        }
                    }
                    refresh_images = true;
                }
                MessageBody::Dead => {
                    if let Some(c) = app.compute.get_mut(&key) {
                        c.kernel.status = KernelStatus::Dead;
                        // Nothing in flight will ever be answered.
                        c.abandon_all();
                    }
                    let dropped = match notebook_for_key(app, &key) {
                        Some((_, state)) => {
                            state.executing_cell = None;
                            state.executing_since = None;
                            let n = state.exec_queue.len();
                            state.exec_queue.clear();
                            n
                        }
                        None => 0,
                    };
                    announce.push(if dropped > 0 {
                        format!("Kernel died{whose} — {dropped} queued cell(s) dropped (:restart-kernel)")
                    } else {
                        format!("Kernel died{whose} (:restart-kernel to restart)")
                    });
                    refresh_images = true;
                }
            }
        }
    }
    for msg in announce {
        app.messages.show(msg);
    }
    if refresh_images {
        app.graphics.image_ids.clear();
    }
    // The kernel may have just become idle (Ready/Done) — start the next
    // queued cell. Its "Running cell [N]…" takes over the minibuffer.
    applied |= pump_execution_queue(app);
    applied
}

/// The notebook a kernel belongs to: the one on screen, or one that has been
/// navigated away from and stashed.
///
/// Output has to reach a stashed notebook, not just the visible one — a cell
/// left running while the user goes elsewhere still finishes, and its output
/// belongs in the notebook that asked for it.
pub(super) fn notebook_for_key<'a>(
    app: &'a mut App,
    key: &crate::source::SourceId,
) -> Option<&'a mut (crate::notebook::Notebook, crate::notebook_state::NotebookState)> {
    let is_active = app
        .notebook
        .as_ref()
        .is_some_and(|(nb, _)| crate::source::SourceId::of(&nb.path) == *key);
    if is_active {
        return app.notebook.as_mut();
    }
    app.notebook_buffers.get_mut(key)
}

/// Human-readable duration for the cell-completion log message.
pub(super) fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}
