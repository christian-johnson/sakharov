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

pub enum PopupAnchor {
    /// Just below the cursor (completions). Flips above if near bottom.
    CursorBelow,
    /// Centered on screen (command palette, buffer picker).
    Center,
    /// Bordered window in the bottom-right corner (which-key).
    BottomRight,
}

pub enum PopupSize {
    /// Use the content's natural width, up to the terminal width.
    Auto,
    /// Fraction of the terminal width (0.0–1.0).
    FractionOfScreen(f32),
}

/// What happens when the user confirms (presses Enter).
#[derive(Clone, PartialEq)]
pub enum PopupTarget {
    /// Parse the confirmed item's label as a Command name and execute it.
    ExecuteCommand,
    /// Insert the confirmed item's label at the cursor position (completion popup).
    /// Non-navigation keys dismiss with passthrough so typing continues normally.
    InsertText,
    /// Just close the popup (for Text / informational List).
    Dismiss,
    /// Jump to the location in the confirmed item's [`ConfirmPayload::Navigate`].
    Navigate,
    /// Apply the code action in the confirmed item's [`ConfirmPayload::CodeAction`].
    ApplyCodeAction,
    /// Resolve a crash-recovery prompt ([`ConfirmPayload::Choice`]).
    RestoreRecovery,
    /// Switch to the theme named by the confirmed item's [`ConfirmPayload::Choice`].
    SwitchTheme,
}

/// Typed data carried by a confirmed [`ListItem`]. Replaces the old
/// `"path\0line\0col"` string-encoding so confirmation handlers match on real
/// values instead of re-parsing strings.
#[derive(Clone)]
pub enum ConfirmPayload {
    /// No explicit payload — the item's label is the meaningful value
    /// (command palette, completion). Synthesised from `label` at confirm time.
    Label(String),
    /// A file location to jump to (buffer / symbol / diagnostic / grep pickers).
    Navigate {
        path: std::path::PathBuf,
        line: usize,
        col: usize,
    },
    /// Index into `app.pending_code_actions`.
    CodeAction(usize),
    /// A free-form choice token (e.g. crash-recovery "restore" / "discard").
    Choice(String),
}

impl ConfirmPayload {
    /// The label/text value for `Label`-style payloads (command name, completion
    /// text, or a choice token); empty for structured payloads.
    pub fn as_text(&self) -> &str {
        match self {
            ConfirmPayload::Label(s) | ConfirmPayload::Choice(s) => s,
            _ => "",
        }
    }
}

/// Returned by popup input handling.
pub enum PopupAction {
    /// Popup stays open, caller should not process the key further.
    Continue,
    /// Popup closes, key is consumed.
    Dismiss,
    /// Popup stays alive (completion) or closes (other), key falls through to normal handler.
    DismissPassthrough,
    /// Popup always closes immediately; key falls through to normal handler.
    ClosePassthrough,
    /// Popup closes, caller should act on the payload.
    Confirm(ConfirmPayload),
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
    /// For completion (InsertText) popups: true once the user has pressed Tab
    /// to explicitly engage with the list. In passive mode (focused=false) the
    /// popup is a hint overlay and all keys fall through to insert mode.
    pub focused: bool,
    /// Completion-only: when `Some`, the `/` fuzzy-search row is open and this
    /// string overrides `filter` for matching.  `None` = search row closed, the
    /// list is filtered by the word-prefix in `filter` as usual.
    pub search: Option<String>,
    /// Completion-only: the `K` documentation side panel, when open.
    pub doc: Option<DocPanel>,
    /// Optional command-name → recency rank (0 = most recent).  Populated only
    /// by the command palette; empty for every other popup.  Used as a
    /// tiebreaker between matches of equal fuzzy-match quality (recent first),
    /// and to order the list when the filter is empty.
    pub recency: std::collections::HashMap<String, usize>,
    /// Memoised `(effective_filter, result)` of the last `filtered_indices`
    /// call.  Scoring + sorting the whole item list on every navigation
    /// keystroke and render is wasted work; the key check keeps the cache
    /// correct even when `filter` is mutated directly.
    filtered_cache: std::cell::RefCell<Option<(String, Vec<usize>)>>,
}

#[derive(Default)]
pub struct ListItem {
    pub label: String,
    pub detail: Option<String>,
    pub kind: Option<String>,
    /// Typed value returned as the `Confirm` payload. When `None`, the item's
    /// `label` is used (wrapped in [`ConfirmPayload::Label`]).
    pub payload: Option<ConfirmPayload>,
    /// Completion-only: documentation shown in the `K` doc panel. `None` means
    /// it may still be fetchable via `completionItem/resolve` (see `resolve_data`).
    pub documentation: Option<String>,
    /// Completion-only: raw LSP completion-item JSON for `completionItem/resolve`.
    pub resolve_data: Option<String>,
}

/// The `K` documentation side panel attached to a focused completion popup.
pub struct DocPanel {
    pub lines: Vec<String>,
    /// True while a `completionItem/resolve` request is in flight for this item.
    pub loading: bool,
}

impl ListItem {
    /// The payload to return when this item is confirmed: its explicit typed
    /// payload, or its label wrapped in [`ConfirmPayload::Label`].
    pub fn confirm_payload(&self) -> ConfirmPayload {
        self.payload
            .clone()
            .unwrap_or_else(|| ConfirmPayload::Label(self.label.clone()))
    }

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
            payload: Some(ConfirmPayload::Navigate {
                path: path.to_path_buf(),
                line,
                col,
            }),
            ..Default::default()
        }
    }
}

impl ListState {
    /// A fresh list over `items`: empty filter, first item selected, all
    /// optional behaviours (two-phase nav, search row, doc panel, recency) off.
    pub fn new(items: Vec<ListItem>) -> Self {
        Self {
            items,
            filter: String::new(),
            selected: 0,
            two_phase: false,
            navigating: false,
            focused: false,
            search: None,
            doc: None,
            recency: std::collections::HashMap::new(),
            filtered_cache: std::cell::RefCell::new(None),
        }
    }

    /// Drop the memoised filter result.  Needed only when item *contents*
    /// change after construction (e.g. a `completionItem/resolve` reply filling
    /// in `detail`, which participates in match scoring) — plain filter edits
    /// are handled by the cache key.
    pub fn invalidate_filter_cache(&mut self) {
        *self.filtered_cache.borrow_mut() = None;
    }

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
    /// The string actually used for matching: the `/` search query when the
    /// search row is open, otherwise the (word-prefix) `filter`.
    pub fn effective_filter(&self) -> &str {
        self.search.as_deref().unwrap_or(self.filter.as_str())
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
        let key = self.effective_filter();
        if let Some((cached_key, cached)) = self.filtered_cache.borrow().as_ref() {
            if cached_key == key {
                return cached.clone();
            }
        }
        let result = self.compute_filtered_indices();
        *self.filtered_cache.borrow_mut() = Some((key.to_owned(), result.clone()));
        result
    }

    fn compute_filtered_indices(&self) -> Vec<usize> {
        let active_filter = self.effective_filter();
        if active_filter.is_empty() {
            // With recency tracking, float recently-used items to the top in
            // recency order; everything else keeps its original order below.
            if self.recency.is_empty() {
                return (0..self.items.len()).collect();
            }
            let mut idx: Vec<usize> = (0..self.items.len()).collect();
            idx.sort_by(|&a, &b| {
                self.recency_rank(a)
                    .cmp(&self.recency_rank(b))
                    .then(a.cmp(&b))
            });
            return idx;
        }

        let mut scored: Vec<(usize, u32)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                match_score_item(item, active_filter).map(|s| (i, s))
            })
            .collect();

        // Primary sort: match quality (lower tier = better).  Conservative
        // recency policy: quality always wins, so recency only breaks ties
        // *within* a tier (recent first), falling back to alphabetical when
        // there is no recency signal — keeping the list stable as you type.
        scored.sort_by(|&(ai, a_score), &(bi, b_score)| {
            a_score
                .cmp(&b_score)
                .then_with(|| self.recency_rank(ai).cmp(&self.recency_rank(bi)))
                .then_with(|| {
                    self.items[ai]
                        .label
                        .to_ascii_lowercase()
                        .cmp(&self.items[bi].label.to_ascii_lowercase())
                })
        });

        scored.into_iter().map(|(i, _)| i).collect()
    }

    /// Recency rank of item `i` (0 = most recent). Items with no recorded use
    /// sort after all recorded ones via `usize::MAX`.
    fn recency_rank(&self, i: usize) -> usize {
        self.recency
            .get(&self.items[i].label)
            .copied()
            .unwrap_or(usize::MAX)
    }

    /// The item at `self.selected` in the filtered set.
    pub fn selected_item(&self) -> Option<&ListItem> {
        let indices = self.filtered_indices();
        indices.get(self.selected).map(|&i| &self.items[i])
    }

    /// The absolute `items` index of the current selection (stable across
    /// re-filtering, unlike `selected` which indexes the filtered view).
    pub fn selected_index(&self) -> Option<usize> {
        self.filtered_indices().get(self.selected).copied()
    }

    /// Append a char to the `/` search query, resetting the selection.
    pub fn push_search_char(&mut self, c: char) {
        if let Some(s) = self.search.as_mut() {
            s.push(c);
        }
        self.selected = 0;
    }

    /// Remove the last char from the `/` search query, resetting the selection.
    pub fn pop_search_char(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.pop();
        }
        self.selected = 0;
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
    /// `recency` maps command names to a recency rank (0 = most recent); pass an
    /// empty map to disable recency weighting.
    pub fn command_palette(
        items: Vec<ListItem>,
        recency: std::collections::HashMap<String, usize>,
    ) -> Self {
        Self {
            title: Some("command palette".into()),
            content: PopupContent::List(ListState { recency, ..ListState::new(items) }),
            anchor: PopupAnchor::Center,
            width: PopupSize::FractionOfScreen(0.55),
            on_confirm: PopupTarget::ExecuteCommand,
        }
    }

    /// Two-choice crash-recovery prompt (Restore / Discard).
    pub fn recovery_prompt(title: String) -> Self {
        let items = vec![
            ListItem {
                label: "Restore unsaved changes".into(),
                detail: Some("Load the recovered contents into the buffer".into()),
                kind: None,
                payload: Some(ConfirmPayload::Choice("restore".into())),
                ..Default::default()
            },
            ListItem {
                label: "Discard recovered contents".into(),
                detail: Some("Delete the recovery file and keep the on-disk version".into()),
                kind: None,
                payload: Some(ConfirmPayload::Choice("discard".into())),
                ..Default::default()
            },
        ];
        Self {
            title: Some(title),
            content: PopupContent::List(ListState::new(items)),
            anchor: PopupAnchor::Center,
            width: PopupSize::FractionOfScreen(0.55),
            on_confirm: PopupTarget::RestoreRecovery,
        }
    }

    /// Generic filterable list that inserts the selection.
    pub fn completion(items: Vec<ListItem>) -> Self {
        Self {
            title: None,
            content: PopupContent::List(ListState::new(items)),
            anchor: PopupAnchor::CursorBelow,
            width: PopupSize::FractionOfScreen(0.45),
            on_confirm: PopupTarget::InsertText,
        }
    }

    /// Scrollable documentation / hover text.
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
            content: PopupContent::List(ListState::new(items)),
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
                filter: initial_filter,
                two_phase: true,
                ..ListState::new(items)
            }),
            anchor: PopupAnchor::Center,
            width: PopupSize::FractionOfScreen(0.75),
            on_confirm: PopupTarget::Navigate,
        }
    }

    /// Fuzzy-filterable theme list; confirms by switching to the selected theme.
    pub fn theme_picker(items: Vec<ListItem>) -> Self {
        Self {
            title: Some("themes".into()),
            content: PopupContent::List(ListState::new(items)),
            anchor: PopupAnchor::Center,
            width: PopupSize::FractionOfScreen(0.55),
            on_confirm: PopupTarget::SwitchTheme,
        }
    }

    /// Filterable list of LSP code actions; confirms by applying the selected action.
    pub fn code_actions(items: Vec<ListItem>) -> Self {
        Self {
            title: Some("code actions".into()),
            content: PopupContent::List(ListState::new(items)),
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

/// All palette-eligible editor commands with short descriptions and default key
/// hints. Sourced from [`crate::command::Command::palette_entries`] so the
/// palette never drifts from the canonical command table.
pub fn command_palette_items() -> Vec<ListItem> {
    crate::command::Command::palette_entries()
        .into_iter()
        .map(|(label, detail)| ListItem {
            label: label.to_string(),
            detail: Some(detail.to_string()),
            kind: None,
            payload: None,
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, detail: &str) -> ListItem {
        ListItem { label: label.into(), detail: Some(detail.into()), kind: None, payload: None, ..Default::default() }
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

    fn list(items: &[&str], filter: &str, recency: &[(&str, usize)]) -> ListState {
        ListState {
            filter: filter.into(),
            recency: recency.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            ..ListState::new(items.iter().map(|l| item(l, "")).collect())
        }
    }

    #[test]
    fn empty_filter_floats_recent_first() {
        // gamma (rank 0) then alpha (rank 1), then beta (no recency) in source order.
        let s = list(&["alpha", "beta", "gamma"], "", &[("gamma", 0), ("alpha", 1)]);
        assert_eq!(s.filtered_indices(), vec![2, 0, 1]);
    }

    #[test]
    fn empty_filter_without_recency_keeps_source_order() {
        let s = list(&["alpha", "beta", "gamma"], "", &[]);
        assert_eq!(s.filtered_indices(), vec![0, 1, 2]);
    }

    #[test]
    fn recency_breaks_ties_within_a_tier() {
        // Both are substring matches for "open" (same tier); the more recent one wins.
        let s = list(
            &["buffer-open", "file-open"],
            "open",
            &[("file-open", 0)],
        );
        assert_eq!(s.filtered_indices()[0], 1, "recent same-tier match first");
    }

    #[test]
    fn search_query_overrides_prefix_filter() {
        // With the `/` search row closed, the word-prefix `filter` drives matching.
        let mut s = list(&["alpha", "beta", "gamma"], "al", &[]);
        assert_eq!(s.filtered_indices(), vec![0]);

        // Opening search overrides the prefix filter with the typed query.
        s.search = Some("be".into());
        assert_eq!(s.effective_filter(), "be");
        assert_eq!(s.filtered_indices(), vec![1]);

        // An empty search row shows everything (source order, no recency).
        s.search = Some(String::new());
        assert_eq!(s.filtered_indices(), vec![0, 1, 2]);
    }

    #[test]
    fn selected_index_is_absolute() {
        let mut s = list(&["alpha", "beta", "gamma"], "", &[]);
        s.selected = 2;
        assert_eq!(s.selected_index(), Some(2));
        // When filtering reorders the view, selected_index follows the filtered set.
        s.search = Some("beta".into());
        s.selected = 0;
        assert_eq!(s.selected_index(), Some(1));
    }

    #[test]
    fn better_match_beats_more_recent_weaker_match() {
        // "goto-x" is a prefix match (tier 1) for "go"; "g-o" only a subsequence
        // (tier 4).  Even though the subsequence item is most recent, quality wins.
        let s = list(&["goto-x", "g-o"], "go", &[("g-o", 0)]);
        assert_eq!(s.filtered_indices()[0], 0, "prefix match beats recent subsequence");
    }
}
