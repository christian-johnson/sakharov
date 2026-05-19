use ropey::Rope;

/// Which editing mode a notebook is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotebookEditMode {
    /// Moving between cells (normal-mode-like navigation).
    Navigate,
    /// Editing the focused cell (like Insert mode).
    Edit,
}

/// Per-session notebook UI state (not persisted).
pub struct NotebookState {
    /// Index into `notebook.cells` of the focused cell.
    pub focused_cell: usize,
    /// Char index within focused cell's source rope.
    pub cursor_pos: usize,
    /// Index of the first visible cell (scroll is per-cell).
    pub scroll_cell: usize,
    pub mode: NotebookEditMode,
    /// Undo history: (cell_index, rope_before_edit)
    pub undo_stack: Vec<(usize, Rope)>,
    pub redo_stack: Vec<(usize, Rope)>,
    /// True once the first keystroke of the current Edit session has been made.
    pub insert_session_active: bool,
    /// Set while a cell is executing (for yellow border). None when idle.
    pub executing_cell: Option<usize>,
}

impl NotebookState {
    /// Create a fresh state for a newly loaded notebook.
    pub fn new() -> Self {
        Self {
            focused_cell: 0,
            cursor_pos: 0,
            scroll_cell: 0,
            mode: NotebookEditMode::Navigate,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            insert_session_active: false,
            executing_cell: None,
        }
    }

    /// Adjust scroll_cell so focused_cell is within a visible window.
    /// Uses a conservative 15-cell window; called after any focus change.
    pub fn ensure_focused_visible(&mut self) {
        const WINDOW: usize = 15;
        if self.focused_cell < self.scroll_cell {
            self.scroll_cell = self.focused_cell;
        } else if self.focused_cell + 1 > self.scroll_cell + WINDOW {
            self.scroll_cell = (self.focused_cell + 1).saturating_sub(WINDOW);
        }
    }
}
