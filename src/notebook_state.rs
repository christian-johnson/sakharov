use std::collections::BTreeSet;

use crate::notebook::Cell;

/// Per-session notebook UI state (not persisted).
pub struct NotebookState {
    /// Index into `notebook.cells` of the focused cell.
    pub focused_cell: usize,
    /// Index of the first cell intersecting the viewport.  Together with
    /// [`scroll_offset`](Self::scroll_offset) this forms the row-granular
    /// scroll anchor, so the notebook scrolls seamlessly line-by-line rather
    /// than jumping a whole cell at a time.
    pub scroll_cell: usize,
    /// Visual rows of `scroll_cell` hidden above the top of the viewport
    /// (0 = the cell's top border is the first visible row; equal to the
    /// cell's height = only the gap row below it is visible).  Maintained by
    /// `exec::update_scroll`, which renormalizes it every frame.
    pub scroll_offset: usize,
    /// When `Some(r)`, the cursor sits on visual row `r` of the focused
    /// cell's *output block* (0 = the first row after the `── output ──`
    /// divider).  `j`/`k` traverse output rows so long errors/streams scroll
    /// into view naturally; any other command snaps the cursor back to the
    /// cell source.  The buffer selection is untouched while set (it stays on
    /// the source line the cursor descended from).
    pub output_row: Option<usize>,
    /// Set while a cell is executing (for yellow border). None when idle.
    pub executing_cell: Option<usize>,
    /// Cell IDs waiting to execute, run in order as the kernel becomes idle.
    /// IDs rather than indices, so structural edits (add/delete cell) can't
    /// redirect the queue — a deleted cell is simply skipped at start time.
    pub exec_queue: std::collections::VecDeque<String>,
    /// When the currently-executing cell started (drives the "finished in …"
    /// log message). Runtime-only.
    pub executing_since: Option<std::time::Instant>,
    /// Snapshots for structural undo (add/delete cell).
    /// Each entry: (focused_cell_at_snapshot, cells_at_snapshot).
    cell_snapshots: Vec<(usize, Vec<Cell>)>,
    /// Snapshots for structural redo.
    cell_redo: Vec<(usize, Vec<Cell>)>,
    /// Indices of cells that are currently folded (collapsed to one line).
    /// Session-only; not persisted to .ipynb.
    pub folded_cells: BTreeSet<usize>,
}

impl NotebookState {
    pub fn new() -> Self {
        Self {
            focused_cell: 0,
            scroll_cell: 0,
            scroll_offset: 0,
            output_row: None,
            executing_cell: None,
            exec_queue: std::collections::VecDeque::new(),
            executing_since: None,
            cell_snapshots: Vec::new(),
            cell_redo: Vec::new(),
            folded_cells: BTreeSet::new(),
        }
    }

    /// True if cell `idx` is currently folded.
    pub fn is_cell_folded(&self, idx: usize) -> bool {
        self.folded_cells.contains(&idx)
    }

    pub fn toggle_cell_fold(&mut self, idx: usize) {
        if self.folded_cells.contains(&idx) {
            self.folded_cells.remove(&idx);
        } else {
            self.folded_cells.insert(idx);
        }
    }

    pub fn fold_all_cells(&mut self, cell_count: usize) {
        for i in 0..cell_count {
            if i != self.focused_cell {
                self.folded_cells.insert(i);
            }
        }
    }

    pub fn unfold_all_cells(&mut self) {
        self.folded_cells.clear();
    }

    /// Snapshot the full cell list before a structural mutation (add/delete).
    /// Clears the redo stack — a new branch has been created.
    pub fn push_snapshot(&mut self, focused: usize, cells: &[Cell]) {
        self.cell_snapshots.push((focused, cells.to_vec()));
        self.cell_redo.clear();
    }

    /// Pop the most recent structural snapshot for undo.
    /// Saves current state onto the redo stack first.
    /// Returns `(focused_cell_to_restore, cells_to_restore)` or None if empty.
    pub fn pop_snapshot_undo(
        &mut self,
        current_focused: usize,
        current_cells: &[Cell],
    ) -> Option<(usize, Vec<Cell>)> {
        let snap = self.cell_snapshots.pop()?;
        self.cell_redo.push((current_focused, current_cells.to_vec()));
        Some(snap)
    }

    /// Pop the most recent redo snapshot.
    /// Saves current state back onto the undo stack first.
    pub fn pop_snapshot_redo(
        &mut self,
        current_focused: usize,
        current_cells: &[Cell],
    ) -> Option<(usize, Vec<Cell>)> {
        let snap = self.cell_redo.pop()?;
        self.cell_snapshots.push((current_focused, current_cells.to_vec()));
        Some(snap)
    }
}

