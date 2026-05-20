use crate::notebook::Cell;

/// Per-session notebook UI state (not persisted).
pub struct NotebookState {
    /// Index into `notebook.cells` of the focused cell.
    pub focused_cell: usize,
    /// Index of the first visible cell (scroll is per-cell).
    pub scroll_cell: usize,
    /// Set while a cell is executing (for yellow border). None when idle.
    pub executing_cell: Option<usize>,
    /// Snapshots for structural undo (add/delete cell).
    /// Each entry: (focused_cell_at_snapshot, cells_at_snapshot).
    cell_snapshots: Vec<(usize, Vec<Cell>)>,
    /// Snapshots for structural redo.
    cell_redo: Vec<(usize, Vec<Cell>)>,
}

impl NotebookState {
    pub fn new() -> Self {
        Self {
            focused_cell: 0,
            scroll_cell: 0,
            executing_cell: None,
            cell_snapshots: Vec::new(),
            cell_redo: Vec::new(),
        }
    }

    /// Adjust scroll_cell so focused_cell is within a 15-cell visible window.
    pub fn ensure_focused_visible(&mut self) {
        const WINDOW: usize = 15;
        if self.focused_cell < self.scroll_cell {
            self.scroll_cell = self.focused_cell;
        } else if self.focused_cell + 1 > self.scroll_cell + WINDOW {
            self.scroll_cell = (self.focused_cell + 1).saturating_sub(WINDOW);
        }
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
