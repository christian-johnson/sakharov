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
#[derive(Clone, PartialEq)]
#[allow(dead_code)]
pub enum PopupTarget {
    /// Parse the confirmed text as a Command name and execute it.
    ExecuteCommand,
    /// Insert the confirmed text at the cursor position (completion popup).
    /// Non-navigation keys dismiss with passthrough so typing continues normally.
    InsertText,
    /// Just close the popup (for Text / informational List).
    Dismiss,
    /// Jump to the location encoded in the confirmed item's payload.
    /// Payload format: `"path\0line\0col"` (all fields, line/col zero-indexed).
    Navigate,
    /// Apply the code action whose index is encoded in the confirmed item's payload.
    ApplyCodeAction,
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
    /// When true, first ESC switches to navigation mode instead of dismissing.
    /// Used by grep popups so the user can type a query then switch to j/k nav.
    pub two_phase: bool,
    /// True once the user has pressed ESC in a two_phase popup.
    /// In this mode printable keys navigate (j/k) instead of updating the filter.
    pub navigating: bool,
}

pub struct ListItem {
    pub label: String,
    pub detail: Option<String>,
    pub kind: Option<String>,
    /// Returned verbatim as the `Confirm` payload instead of `label`.
    /// Use `NavigateTarget::encode` or set manually to `"path\0line\0col"`.
    pub payload: Option<String>,
}

impl ListItem {
    /// Convenience constructor for navigate items (buffer/symbol/diagnostic pickers).
    pub fn navigate(
        label: impl Into<String>,
        detail: impl Into<String>,
        path: &std::path::Path,
        line: usize,
        col: usize,
    ) -> Self {
        Self {
            label: label.into(),
            detail: Some(detail.into()),
            kind: None,
            payload: Some(format!(
                "{}\0{}\0{}",
                path.to_string_lossy(),
                line,
                col
            )),
        }
    }
}

impl ListState {
    /// Return indices of matching items sorted by relevance.
    ///
    /// When the filter is empty all items are returned in their original order
    /// (LSP servers and the command palette both provide a meaningful default
    /// order).  When the filter is non-empty items are scored and sorted:
    ///
    ///   0 — exact match
    ///   1 — label starts with filter  (prefix)
    ///   2 — a `_`-delimited word segment starts with filter  (word boundary)
    ///   3 — filter appears as a contiguous substring
    ///   4 — filter characters appear in order but non-contiguously  (subsequence)
    ///
    /// Within each tier items are sorted alphabetically so the list is stable
    /// as the user types.
    pub fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.items.len()).collect();
        }

        let mut scored: Vec<(usize, u32)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                match_score_item(item, &self.filter).map(|s| (i, s))
            })
            .collect();

        // Primary sort: score (lower = better match).
        // Secondary sort: alphabetical within each tier for a stable, predictable list.
        scored.sort_by(|&(ai, a_score), &(bi, b_score)| {
            a_score.cmp(&b_score).then_with(|| {
                self.items[ai]
                    .label
                    .to_ascii_lowercase()
                    .cmp(&self.items[bi].label.to_ascii_lowercase())
            })
        });

        scored.into_iter().map(|(i, _)| i).collect()
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

    /// Retreat selected, wrapping around to the last item.
    pub fn move_up(&mut self) {
        let count = self.filtered_indices().len();
        if count == 0 {
            return;
        }
        if self.selected == 0 {
            self.selected = count - 1;
        } else {
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

/// Score an item against the user's filter string.
///
/// Two normalizations are applied before scoring:
///  * Spaces in the filter are treated as dashes, so `"write quit"` finds `"write-quit"`.
///  * Both the item label and its detail string are checked; detail matches receive a +5
///    score penalty so label matches always rank above them.  This lets aliases/keybindings
///    embedded in the detail (e.g. `[:q!]`) be searched without burying label matches.
fn match_score_item(item: &ListItem, filter: &str) -> Option<u32> {
    let normalized = filter.replace(' ', "-");

    let label_score = match_score(&item.label, &normalized);

    // Detail matches get a +5 penalty so they rank below any label match.
    let detail_score = item
        .detail
        .as_deref()
        .and_then(|d| match_score(d, &normalized))
        .map(|s| s + 5);

    match (label_score, detail_score) {
        (Some(l), Some(d)) => Some(l.min(d)),
        (Some(l), None) => Some(l),
        (None, Some(d)) => Some(d),
        (None, None) => None,
    }
}

/// Score how well `label` matches `filter` (case-insensitive).
/// Returns `None` when there is no match at all.
fn match_score(label: &str, filter: &str) -> Option<u32> {
    if filter.is_empty() {
        return Some(u32::MAX); // won't be used — empty filter bypasses scoring
    }
    let ll = label.to_ascii_lowercase();
    let fl = filter.to_ascii_lowercase();

    if ll == fl {
        return Some(0); // exact
    }
    if ll.starts_with(fl.as_str()) {
        return Some(1); // prefix
    }
    // Word-boundary prefix: filter matches the start of any `_`-separated segment.
    if ll
        .split('_')
        .skip(1)
        .any(|seg| seg.starts_with(fl.as_str()))
    {
        return Some(2);
    }
    if ll.contains(fl.as_str()) {
        return Some(3); // contiguous substring
    }
    if is_subsequence(&ll, &fl) {
        return Some(4); // non-contiguous subsequence
    }
    None
}

/// Returns the char indices in `label` that were matched by `filter` (case-insensitive),
/// using the same tier logic as `match_score`. Returns `None` when there is no match.
///
/// Spaces in `filter` are treated as dashes to match the normalization applied during scoring.
pub fn match_positions(label: &str, filter: &str) -> Option<Vec<usize>> {
    if filter.is_empty() {
        return Some(vec![]);
    }
    let ll = label.to_ascii_lowercase();
    // Mirror the space→dash normalization used in match_score_item.
    let fl = filter.replace(' ', "-").to_ascii_lowercase();
    let fl_char_count = fl.chars().count();

    // Exact or prefix: 0..fl_char_count
    if ll.starts_with(fl.as_str()) {
        return Some((0..fl_char_count).collect());
    }

    // Word-boundary prefix: find the first non-first segment that starts with filter.
    let chars: Vec<char> = ll.chars().collect();
    for i in 1..chars.len() {
        if chars[i - 1] == '_' {
            let seg: String = chars[i..].iter().collect();
            if seg.starts_with(fl.as_str()) {
                return Some((i..i + fl_char_count).collect());
            }
        }
    }

    // Contiguous substring
    if let Some(byte_pos) = ll.find(fl.as_str()) {
        let char_start = ll[..byte_pos].chars().count();
        return Some((char_start..char_start + fl_char_count).collect());
    }

    // Subsequence
    subsequence_positions(&chars, &fl.chars().collect::<Vec<_>>())
}

fn subsequence_positions(label: &[char], filter: &[char]) -> Option<Vec<usize>> {
    let mut positions = Vec::with_capacity(filter.len());
    let mut fi = 0;
    for (li, &c) in label.iter().enumerate() {
        if fi < filter.len() && c == filter[fi] {
            positions.push(li);
            fi += 1;
        }
    }
    if fi == filter.len() { Some(positions) } else { None }
}

/// True if every character of `filter` appears in `label` in order.
fn is_subsequence(label: &str, filter: &str) -> bool {
    let mut filter_chars = filter.chars();
    let Some(mut target) = filter_chars.next() else {
        return true;
    };
    for c in label.chars() {
        if c == target {
            match filter_chars.next() {
                None => return true,
                Some(next) => target = next,
            }
        }
    }
    false
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
                two_phase: false,
                navigating: false,
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
                two_phase: false,
                navigating: false,
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

    /// Fuzzy-filterable navigate list (buffer picker, symbol picker, diagnostics).
    pub fn navigate(title: &str, items: Vec<ListItem>) -> Self {
        Self {
            title: Some(title.into()),
            content: PopupContent::List(ListState {
                items,
                filter: String::new(),
                selected: 0,
                two_phase: false,
                navigating: false,
            }),
            anchor: PopupAnchor::Center,
            width: PopupSize::FractionOfScreen(0.65),
            on_confirm: PopupTarget::Navigate,
        }
    }

    /// Grep popup — two-phase: type to filter, ESC → j/k navigation, Enter → jump.
    pub fn grep(title: &str, items: Vec<ListItem>, initial_filter: String) -> Self {
        Self {
            title: Some(title.into()),
            content: PopupContent::List(ListState {
                items,
                filter: initial_filter,
                selected: 0,
                two_phase: true,
                navigating: false,
            }),
            anchor: PopupAnchor::Center,
            width: PopupSize::FractionOfScreen(0.75),
            on_confirm: PopupTarget::Navigate,
        }
    }

    /// Filterable list of LSP code actions; confirms by applying the selected action.
    pub fn code_actions(items: Vec<ListItem>) -> Self {
        Self {
            title: Some("code actions".into()),
            content: PopupContent::List(ListState {
                items,
                filter: String::new(),
                selected: 0,
                two_phase: false,
                navigating: false,
            }),
            anchor: PopupAnchor::CursorBelow,
            width: PopupSize::FractionOfScreen(0.5),
            on_confirm: PopupTarget::ApplyCodeAction,
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
        ("write", "Write file  [ctrl+s, :w]"),
        ("write-as", "Write to new path  [:w <path>]"),
        ("quit", "Quit  [:q]"),
        ("force-quit", "Quit without saving  [:q!]"),
        ("write-quit", "Write and quit  [:wq]"),
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
        // Notebook
        ("enter-notebook", "Enter notebook navigation mode  [n]"),
        // Search / Grep
        ("search-forward", "Search forward  [/]"),
        ("search-backward", "Search backward  [?]"),
        ("search-next", "Next match  [n]"),
        ("search-prev", "Previous match  [N]"),
        ("grep-buffer", "Grep current buffer  [ctrl+f]"),
        ("grep-project", "Grep project files  [ctrl+g]"),
        // Scroll
        ("page-down", "Scroll half page down  [ctrl+d, PgDn]"),
        ("page-up", "Scroll half page up  [ctrl+u, PgUp]"),
        // Shell
        ("shell", "Run a shell command  [:shell <cmd>]"),
        // Editing
        ("enter-jump-mode", "Jump to label in view  [gw]"),
        ("comment-region", "Toggle comment/uncomment  [gc]"),
        // LSP
        ("lsp-show-documentation", "Show hover documentation  [gk, K]"),
        ("lsp-code-actions", "Show code actions  [ga]"),
        ("lsp-goto-definition", "Go to definition  [gd]"),
        ("lsp-goto-references", "Go to references  [gr]"),
        ("lsp-goto-type-definition", "Go to type definition  [gy]"),
        ("lsp-goto-implementation", "Go to implementation  [gi]"),
        ("lsp-request-completion", "Request completions  [ctrl+space]"),
        // Buffers
        ("buffer-close",       "Close current buffer  [:bd]"),
        ("buffer-force-close", "Force-close current buffer (discard changes)  [:bd!]"),
        ("buffer-next",        "Switch to next buffer  [L, :bn]"),
        ("buffer-prev",        "Switch to previous buffer  [H, :bp]"),
        ("switch-to-scratch",  "Switch to *scratch* buffer  [:scratch]"),
        ("switch-to-messages", "Switch to *Messages* log buffer  [:messages]"),
        // UI
        ("open-command-palette", "Open fuzzy-searchable command palette  [Space]"),
        ("toggle-git-gutter",          "Toggle git gutter indicators  [:toggle-git-gutter]"),
        ("toggle-line-numbers",        "Toggle line numbers  [:toggle-line-numbers]"),
        ("toggle-relative-line-numbers", "Toggle relative line numbers  [:toggle-relative-line-numbers]"),
        // Pickers
        ("open-file-picker",       "Open file  [ctrl+o, :e]"),
        ("open-buffer-picker",     "Switch buffer  [gb]"),
        ("open-symbol-picker",     "Jump to symbol in file  [gs]"),
        ("open-diagnostic-picker", "Jump to diagnostic  [gD]"),
        // Code folding
        ("fold-toggle",     "Toggle fold at cursor  [za]"),
        ("fold-toggle-all", "Toggle all folds  [zA]"),
        // Notebook folding
        ("notebook-toggle-fold-cell",  "Toggle cell fold  [z]"),
        ("notebook-toggle-all-folds",  "Toggle all cell folds  [Z]"),
    ];
    entries
        .iter()
        .map(|(label, detail)| ListItem {
            label: label.to_string(),
            detail: Some(detail.to_string()),
            kind: None,
            payload: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, detail: &str) -> ListItem {
        ListItem { label: label.into(), detail: Some(detail.into()), kind: None, payload: None }
    }

    #[test]
    fn space_matches_dash_in_label() {
        // "write quit" should find "write-quit"
        assert!(match_score_item(&item("write-quit", "Write and quit [:wq]"), "write quit").is_some());
    }

    #[test]
    fn alias_in_detail_is_matched() {
        // ":q!" should find "force-quit" via its detail string
        let force_quit = item("force-quit", "Quit without saving  [:q!]");
        assert!(match_score_item(&force_quit, ":q!").is_some());
    }

    #[test]
    fn label_match_ranks_above_detail_match() {
        // "quit" directly matches the label "quit"; "force-quit" only matches via detail's ":q".
        // The label match should score lower (better).
        let quit = item("quit", "Quit  [:q]");
        let force_quit = item("force-quit", "Quit without saving  [:q!]");
        let qs = match_score_item(&quit, "quit").unwrap();
        let fqs = match_score_item(&force_quit, "quit").unwrap();
        assert!(qs < fqs, "label match ({qs}) should beat detail match ({fqs})");
    }

    #[test]
    fn match_positions_normalizes_spaces() {
        // Highlight positions for "write quit" against label "write-quit" should be non-empty.
        let pos = match_positions("write-quit", "write quit");
        assert!(pos.is_some() && !pos.unwrap().is_empty());
    }
}
