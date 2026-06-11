use std::collections::BTreeSet;

use crate::notebook::Cell;

/// Per-session notebook UI state (not persisted).
pub struct NotebookState {
    /// Index into `notebook.cells` of the focused cell.
    pub focused_cell: usize,
    /// Index of the first visible cell (scroll is per-cell).
    pub scroll_cell: usize,
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

    /// Adjust scroll_cell so the focused cell is visible.
    /// - If it is longer than the viewport_height, it is scrolled to the top.
    /// - If it is shorter, its bottom is aligned with the bottom of the viewport.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_focused_visible(
        &mut self,
        cells: &[Cell],
        viewport_height: usize,
        active_rope: &ropey::Rope,
        image_rows: u16,
        cell_pixel_size: Option<(u16, u16)>,
        available_cols: u16,
        word_wrap: bool,
    ) {
        let num_cells = cells.len();
        if num_cells == 0 || viewport_height == 0 {
            self.scroll_cell = 0;
            return;
        }

        // Clamp focused_cell to valid cell range
        let focused = self.focused_cell.min(num_cells - 1);

        // 1. Calculate display heights of cells
        let mut cell_heights = Vec::with_capacity(num_cells);
        for (idx, cell) in cells.iter().enumerate() {
            let h = if self.is_cell_folded(idx) && idx != focused {
                3usize // top border + 1 summary line + bottom border
            } else {
                let source = if idx == focused { active_rope } else { &cell.source };
                crate::notebook_ui::cell_display_height(
                    source, cell, image_rows, cell_pixel_size, available_cols, word_wrap,
                ) as usize
            };
            cell_heights.push(h);
        }

        // 2. Adjust scroll_cell
        // Rule 1: scroll_cell must be <= focused_cell.
        if focused < self.scroll_cell {
            self.scroll_cell = focused;
        }

        // Rule 2: If the focused cell is longer than the viewport, it must start at the top.
        let focused_h = cell_heights[focused];
        if focused_h >= viewport_height {
            self.scroll_cell = focused;
        } else {
            // Rule 3: Otherwise, ensure the focused cell is fully visible.
            // Rows from cell `from` through the focused cell, incl. 1-row gaps.
            let span = |from: usize| -> usize {
                let slice = &cell_heights[from..=focused];
                slice.iter().sum::<usize>() + slice.len().saturating_sub(1)
            };

            // If the bottom of the focused cell is below the bottom of the viewport,
            // scroll down to the largest `s` <= focused where cells s..=focused fit.
            if span(self.scroll_cell) > viewport_height {
                let mut s = focused;
                while s > 0 && span(s - 1) <= viewport_height {
                    s -= 1;
                }
                self.scroll_cell = s;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::CellType;
    use ropey::Rope;

    fn make_test_cell(lines: &str) -> Cell {
        Cell {
            id: "test".to_string(),
            cell_type: CellType::Code,
            source: Rope::from_str(lines),
            outputs: vec![],
            execution_count: None,
            rendered: false,
        }
    }

    #[test]
    fn test_ensure_focused_visible_scrolling() {
        let cells = vec![
            make_test_cell("l1\nl2\nl3"),
            make_test_cell("l1\nl2\nl3\nl4"),
            make_test_cell("l1\nl2"),
            make_test_cell("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8"),
        ];

        let mut state = NotebookState::new();
        let active_rope = Rope::from_str("l1\nl2\nl3");

        // Viewport height = 15.
        // 1. Initial state: focused = 0, scroll_cell = 0.
        state.focused_cell = 0;
        state.ensure_focused_visible(&cells, 15, &active_rope, 12, None, 80, false);
        assert_eq!(state.scroll_cell, 0);

        // 2. Focus cell 1.
        state.focused_cell = 1;
        let active_rope1 = Rope::from_str("l1\nl2\nl3\nl4");
        state.ensure_focused_visible(&cells, 15, &active_rope1, 12, None, 80, false);
        assert_eq!(state.scroll_cell, 0);

        // 3. Focus cell 2.
        state.focused_cell = 2;
        let active_rope2 = Rope::from_str("l1\nl2");
        state.ensure_focused_visible(&cells, 15, &active_rope2, 12, None, 80, false);
        assert_eq!(state.scroll_cell, 1);

        // 4. Focus cell 3.
        state.focused_cell = 3;
        let active_rope3 = Rope::from_str("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8");
        state.ensure_focused_visible(&cells, 15, &active_rope3, 12, None, 80, false);
        assert_eq!(state.scroll_cell, 2);

        // 5. Test Rule 2: Cell longer than viewport height (focused_h >= viewport_height).
        state.ensure_focused_visible(&cells, 8, &active_rope3, 12, None, 80, false);
        assert_eq!(state.scroll_cell, 3);

        // 6. Test Rule 1: focused_cell < scroll_cell (scrolling up).
        state.focused_cell = 1;
        state.scroll_cell = 3;
        state.ensure_focused_visible(&cells, 15, &active_rope1, 12, None, 80, false);
        assert_eq!(state.scroll_cell, 1);
    }
}
