/// A floating overlay rendered on top of the editor.
pub struct Popup {
    pub title: Option<String>,
    pub content: PopupContent,
    pub anchor: PopupAnchor,
    pub width: PopupSize,
    /// What to do when the user confirms a selection.
    pub on_confirm: PopupTarget,
}

/// The three interaction patterns all popups reduce to.
#[allow(dead_code)]
pub enum PopupContent {
    /// Filterable, scrollable list of items. Covers completions,
    /// command palette, buffer picker, diagnostics.
    List(ListState),
    /// Scrollable read-only prose. Covers hover docs, changelogs.
    Text(TextState),
    /// Two-column key → description table. Covers which-key.
    /// Purely informational — any keypress dismisses with passthrough.
    KeyHints(KeyHintsState),
}

#[allow(dead_code)]
pub enum PopupAnchor {
    /// Just below the cursor (completions). Flips above if near bottom.
    CursorBelow,
    /// Centered on screen (command palette, buffer picker).
    Center,
    /// Bordered window in the bottom-right corner (which-key).
    BottomRight,
    /// Full-width strip at the bottom (kept for future use).
    BottomStrip,
}

#[allow(dead_code)]
pub enum PopupSize {
    /// Use the content's natural width, up to the terminal width.
    Auto,
    /// Exactly N columns wide.
    Fixed(u16),
    /// Fraction of the terminal width (0.0–1.0).
    FractionOfScreen(f32),
}

/// What happens when the user confirms (presses Enter).
#[derive(Clone)]
#[allow(dead_code)]
pub enum PopupTarget {
    /// Parse the confirmed text as a Command name and execute it.
    ExecuteCommand,
    /// Insert the confirmed text at the cursor position.
    InsertText,
    /// Just close the popup (for Text / informational List).
    Dismiss,
}

/// Returned by popup input handling.
pub enum PopupAction {
    /// Popup stays open, caller should not process the key further.
    Continue,
    /// Popup closes, key is consumed.
    Dismiss,
    /// Popup closes, key is NOT consumed (falls through to normal handler).
    DismissPassthrough,
    /// Popup closes, caller should act on the payload.
    Confirm(String),
}

// ---------------------------------------------------------------------------
// ListState
// ---------------------------------------------------------------------------

pub struct ListState {
    pub items: Vec<ListItem>,
    pub filter: String,
    pub selected: usize,
}

pub struct ListItem {
    pub label: String,
    pub detail: Option<String>,
    pub kind: Option<String>,
}

impl ListState {
    /// Return indices of items matching `self.filter` via fuzzy match.
    pub fn filtered_indices(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| fuzzy_match(&item.label, &self.filter))
            .map(|(i, _)| i)
            .collect()
    }

    /// The item at `self.selected` in the filtered set.
    pub fn selected_item(&self) -> Option<&ListItem> {
        let indices = self.filtered_indices();
        indices.get(self.selected).map(|&i| &self.items[i])
    }

    /// Advance selected, wrapping around.
    pub fn move_down(&mut self) {
        let count = self.filtered_indices().len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected + 1) % count;
    }

    /// Retreat selected, clamping at 0.
    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Append char, reset selected to 0.
    pub fn push_filter_char(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
    }

    /// Remove last char, reset selected to 0.
    pub fn pop_filter_char(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }

    /// Clear filter and selection.
    #[allow(dead_code)]
    pub fn reset_filter(&mut self) {
        self.filter.clear();
        self.selected = 0;
    }
}

/// Case-insensitive subsequence match.
fn fuzzy_match(label: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let mut label_chars = label.chars().map(|c| c.to_ascii_lowercase());
    for fc in filter.chars().map(|c| c.to_ascii_lowercase()) {
        if !label_chars.any(|lc| lc == fc) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// TextState
// ---------------------------------------------------------------------------

pub struct TextState {
    pub lines: Vec<String>,
    pub scroll: usize,
}

impl TextState {
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn scroll_down(&mut self, max_visible: usize) {
        let max_scroll = self.lines.len().saturating_sub(max_visible);
        if self.scroll < max_scroll {
            self.scroll += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// KeyHintsState
// ---------------------------------------------------------------------------

pub struct KeyHintsState {
    pub prefix: String,
    pub hints: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Popup constructors
// ---------------------------------------------------------------------------

impl Popup {
    /// Fuzzy-filterable command list, confirms by executing a command.
    pub fn command_palette(items: Vec<ListItem>) -> Self {
        Self {
            title: Some("command palette".into()),
            content: PopupContent::List(ListState {
                items,
                filter: String::new(),
                selected: 0,
            }),
            anchor: PopupAnchor::Center,
            width: PopupSize::FractionOfScreen(0.55),
            on_confirm: PopupTarget::ExecuteCommand,
        }
    }

    /// Generic filterable list that inserts the selection.
    #[allow(dead_code)]
    pub fn completion(items: Vec<ListItem>) -> Self {
        Self {
            title: None,
            content: PopupContent::List(ListState {
                items,
                filter: String::new(),
                selected: 0,
            }),
            anchor: PopupAnchor::CursorBelow,
            width: PopupSize::Auto,
            on_confirm: PopupTarget::InsertText,
        }
    }

    /// Scrollable documentation / hover text.
    #[allow(dead_code)]
    pub fn documentation(title: &str, content: &str) -> Self {
        Self {
            title: Some(title.into()),
            content: PopupContent::Text(TextState {
                lines: content.lines().map(str::to_owned).collect(),
                scroll: 0,
            }),
            anchor: PopupAnchor::Center,
            width: PopupSize::FractionOfScreen(0.6),
            on_confirm: PopupTarget::Dismiss,
        }
    }

    /// Which-key strip shown at the bottom of the screen.
    pub fn which_key(prefix: &str, hints: Vec<(String, String)>) -> Self {
        Self {
            // Title shows the prefix key (e.g. " g " in the border)
            title: Some(format!(" {prefix} ")),
            content: PopupContent::KeyHints(KeyHintsState {
                prefix: prefix.into(),
                hints,
            }),
            anchor: PopupAnchor::BottomRight,
            width: PopupSize::Auto,
            on_confirm: PopupTarget::Dismiss,
        }
    }
}

// ---------------------------------------------------------------------------
// Static command list
// ---------------------------------------------------------------------------

/// All editor commands with short descriptions and default key hints.
/// Used to populate the command palette.
pub fn command_palette_items() -> Vec<ListItem> {
    let entries: &[(&str, &str)] = &[
        // File
        ("save", "Save file  [ctrl+s, :w]"),
        ("save-as", "Save to new path  [:w <path>]"),
        ("quit", "Quit  [:q]"),
        ("force-quit", "Quit without saving  [:q!]"),
        ("write-quit", "Save and quit  [:wq]"),
        // Motions
        ("move-left", "Move cursor left  [h]"),
        ("move-right", "Move cursor right  [l]"),
        ("move-up", "Move cursor up  [k]"),
        ("move-down", "Move cursor down  [j]"),
        ("move-word-forward", "Next word  [w]"),
        ("move-word-backward", "Previous word  [b]"),
        ("move-word-end", "End of word  [e]"),
        ("move-line-start", "Start of line  [0]"),
        ("move-line-end", "End of line  [$]"),
        ("goto-file-start", "Go to file start  [gg]"),
        ("goto-file-end", "Go to file end  [G]"),
        ("select-line", "Select current line  [x]"),
        ("select-all", "Select entire file  [%]"),
        // Editing
        ("delete-selection", "Delete selection  [d]"),
        ("change-selection", "Delete selection and insert  [c]"),
        ("yank-selection", "Yank (copy) selection  [y]"),
        ("paste-after", "Paste after cursor  [p]"),
        ("paste-before", "Paste before cursor  [P]"),
        ("undo", "Undo  [u]"),
        ("redo", "Redo  [U]"),
        ("open-line-below", "New line below  [o]"),
        ("open-line-above", "New line above  [O]"),
        // Mode transitions
        ("enter-insert", "Enter insert mode  [i]"),
        ("enter-insert-after", "Insert after cursor  [a]"),
        ("enter-insert-at-line-start", "Insert at line start  [I]"),
        ("enter-insert-at-line-end", "Insert at line end  [A]"),
        ("enter-select", "Enter select mode  [v]"),
        ("enter-normal", "Return to normal mode  [Esc]"),
        ("enter-command-mode", "Open command line  [:]"),
        // Notebook
        ("notebook-next-cell", "Next cell  [j]"),
        ("notebook-prev-cell", "Previous cell  [k]"),
        ("notebook-execute-cell", "Execute cell  [e, ctrl+enter]"),
        ("notebook-execute-and-advance", "Execute cell and advance  [E]"),
        ("notebook-new-cell-below", "New cell below  [o]"),
        ("notebook-new-cell-above", "New cell above  [O]"),
        ("notebook-delete-cell", "Delete cell  [d]"),
        ("notebook-clear-outputs", "Clear cell outputs  [x]"),
        ("notebook-restart-kernel", "Restart kernel  [ctrl+r]"),
        ("notebook-interrupt-kernel", "Interrupt kernel  [:interrupt-kernel]"),
        ("notebook-open-cell-edit", "Open cell in editor  [Enter, i]"),
        ("notebook-close-cell-edit", "Save cell and return  [ctrl+enter]"),
        ("notebook-discard-cell-edit", "Discard cell edits  [:discard-cell]"),
        // Shell
        ("shell", "Run a shell command  [:shell <cmd>]"),
        // UI
        ("open-command-palette", "Open fuzzy-searchable command palette  [Space]"),
    ];
    entries
        .iter()
        .map(|(label, detail)| ListItem {
            label: label.to_string(),
            detail: Some(detail.to_string()),
            kind: None,
        })
        .collect()
}
