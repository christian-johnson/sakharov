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

    /// Adjust scroll_cell so the focused cell is visible.
    /// - If it is longer than the viewport_height, it is scrolled to the top.
    /// - If it is shorter, its bottom is aligned with the bottom of the viewport.
    pub fn ensure_focused_visible(
        &mut self,
        cells: &[Cell],
        viewport_height: usize,
        active_rope: &ropey::Rope,
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
            let h = if idx == focused {
                crate::notebook_ui::focused_cell_display_height(active_rope, cell) as usize
            } else {
                crate::notebook_ui::cell_display_height(cell) as usize
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
            // Calculate bottom position of the focused cell starting from current scroll_cell.
            let mut y_end = 0;
            for idx in self.scroll_cell..=focused {
                y_end += cell_heights[idx];
                if idx > self.scroll_cell {
                    y_end += 1; // +1 gap between cells
                }
            }

            // If the bottom of the focused cell is below the bottom of the viewport, scroll down.
            if y_end > viewport_height {
                // Find the largest scroll_cell `s` <= focused such that cells from `s` to `focused` fit.
                let mut s = focused;
                while s > 0 {
                    let mut test_y_end = 0;
                    for idx in (s - 1)..=focused {
                        test_y_end += cell_heights[idx];
                        if idx > s - 1 {
                            test_y_end += 1;
                        }
                    }
                    if test_y_end <= viewport_height {
                        s -= 1;
                    } else {
                        break;
                    }
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
        state.ensure_focused_visible(&cells, 15, &active_rope);
        assert_eq!(state.scroll_cell, 0);

        // 2. Focus cell 1.
        state.focused_cell = 1;
        let active_rope1 = Rope::from_str("l1\nl2\nl3\nl4");
        state.ensure_focused_visible(&cells, 15, &active_rope1);
        assert_eq!(state.scroll_cell, 0);

        // 3. Focus cell 2.
        state.focused_cell = 2;
        let active_rope2 = Rope::from_str("l1\nl2");
        state.ensure_focused_visible(&cells, 15, &active_rope2);
        assert_eq!(state.scroll_cell, 1);

        // 4. Focus cell 3.
        state.focused_cell = 3;
        let active_rope3 = Rope::from_str("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8");
        state.ensure_focused_visible(&cells, 15, &active_rope3);
        assert_eq!(state.scroll_cell, 2);

        // 5. Test Rule 2: Cell longer than viewport height (focused_h >= viewport_height).
        state.ensure_focused_visible(&cells, 8, &active_rope3);
        assert_eq!(state.scroll_cell, 3);

        // 6. Test Rule 1: focused_cell < scroll_cell (scrolling up).
        state.focused_cell = 1;
        state.scroll_cell = 3;
        state.ensure_focused_visible(&cells, 15, &active_rope1);
        assert_eq!(state.scroll_cell, 1);
    }
}
