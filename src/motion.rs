use ropey::Rope;

use crate::selection::Selection;

/// Returns true if `c` is a word character (alphanumeric or `_`).
pub(crate) fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Return the char index of the start of the word ending at `pos`.
pub(crate) fn word_start_at(rope: &Rope, pos: usize) -> usize {
    let mut i = pos;
    while i > 0 {
        let c = rope.char(i - 1);
        if word_char(c) { i -= 1; } else { break; }
    }
    i
}

/// Return the word prefix (alphanumeric / `_` chars) immediately before `pos`.
pub(crate) fn word_prefix_at(rope: &Rope, pos: usize) -> String {
    let start = word_start_at(rope, pos);
    rope.slice(start..pos).to_string()
}

/// Returns true if `c` is a WORD character (any non-whitespace).
fn big_word_char(c: char) -> bool {
    !c.is_whitespace()
}

// ---------------------------------------------------------------------------
// Helper: clamp a char index to [0, max_valid] where max_valid is the last
// addressable char position. For an empty rope that is 0.
// ---------------------------------------------------------------------------
fn clamp_char(rope: &Rope, idx: usize) -> usize {
    let len = rope.len_chars();
    if len == 0 {
        0
    } else {
        idx.min(len - 1)
    }
}

/// The index of the line containing `pos`.
///
/// Clamps to `len_chars()` — *not* `len_chars() - 1`.  A rope ending in a
/// newline has a real, addressable final line (`"a\n"` is two lines, the
/// second empty) whose only position is `len_chars()`; clamping one lower
/// folds that line onto its predecessor, which made vertical motion off it
/// a silent no-op and reported the wrong column for it.
fn line_of(rope: &Rope, pos: usize) -> usize {
    rope.char_to_line(pos.min(rope.len_chars()))
}

/// Return the char index of the last non-newline char on the line containing
/// `pos`, or the line start if the line is empty.
fn line_end_char(rope: &Rope, pos: usize) -> usize {
    if rope.len_chars() == 0 {
        return 0;
    }
    let line_idx = line_of(rope, pos);
    let line_start = rope.line_to_char(line_idx);
    let line_str = rope.line(line_idx);
    let line_len = line_str.len_chars();
    // Trim trailing newline
    let content_len = if line_len > 0
        && (line_str.char(line_len - 1) == '\n' || line_str.char(line_len - 1) == '\r')
    {
        line_len - 1
    } else {
        line_len
    };
    if content_len == 0 {
        line_start
    } else {
        line_start + content_len - 1
    }
}

/// Return the char index of the first char on the line containing `pos`.
fn line_start_char(rope: &Rope, pos: usize) -> usize {
    if rope.len_chars() == 0 {
        return 0;
    }
    let line_idx = line_of(rope, pos);
    rope.line_to_char(line_idx)
}

/// Return the column of `pos` within its line (char offset from line start).
pub fn col_of(rope: &Rope, pos: usize) -> usize {
    if rope.len_chars() == 0 {
        return 0;
    }
    let p = pos.min(rope.len_chars());
    let line_idx = rope.char_to_line(p);
    p - rope.line_to_char(line_idx)
}

// ---------------------------------------------------------------------------
// Motion functions
// ---------------------------------------------------------------------------

fn apply_extend(sel: Selection, new_head: usize, extend: bool) -> Selection {
    if extend {
        Selection::new(sel.anchor, new_head)
    } else {
        Selection::point(new_head)
    }
}

/// Move left one char, staying on the same line.
pub fn move_left(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let pos = sel.head;
    let ls = line_start_char(rope, pos);
    let new_head = if pos > ls { pos - 1 } else { pos };
    apply_extend(sel, new_head, extend)
}

/// Move right one char, staying on the same line.
pub fn move_right(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let pos = sel.head;
    let le = line_end_char(rope, pos);
    let new_head = if pos < le { pos + 1 } else { pos };
    apply_extend(sel, new_head, extend)
}

/// Move down one line, preserving column.
pub fn move_down(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let pos = sel.head;
    if rope.len_chars() == 0 {
        return apply_extend(sel, 0, extend);
    }
    let line_idx = line_of(rope, pos);
    let col = col_of(rope, pos);
    if line_idx + 1 >= rope.len_lines() {
        return apply_extend(sel, pos, extend);
    }
    let next_line_start = rope.line_to_char(line_idx + 1);
    let next_line_str = rope.line(line_idx + 1);
    // Trim trailing newline for length calculation
    let nl = next_line_str.len_chars();
    let content_len = if nl > 0
        && (next_line_str.char(nl - 1) == '\n' || next_line_str.char(nl - 1) == '\r')
    {
        nl - 1
    } else {
        nl
    };
    let new_col = col.min(content_len.saturating_sub(1));
    // But if content_len == 0 we stay at the line start
    let new_head = if content_len == 0 {
        next_line_start
    } else {
        next_line_start + new_col
    };
    apply_extend(sel, new_head, extend)
}

/// Move up one line, preserving column.
pub fn move_up(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let pos = sel.head;
    if rope.len_chars() == 0 {
        return apply_extend(sel, 0, extend);
    }
    let line_idx = line_of(rope, pos);
    if line_idx == 0 {
        return apply_extend(sel, pos, extend);
    }
    let col = col_of(rope, pos);
    let prev_line_start = rope.line_to_char(line_idx - 1);
    let prev_line_str = rope.line(line_idx - 1);
    let nl = prev_line_str.len_chars();
    let content_len = if nl > 0
        && (prev_line_str.char(nl - 1) == '\n' || prev_line_str.char(nl - 1) == '\r')
    {
        nl - 1
    } else {
        nl
    };
    let new_col = col.min(content_len.saturating_sub(1));
    let new_head = if content_len == 0 {
        prev_line_start
    } else {
        prev_line_start + new_col
    };
    apply_extend(sel, new_head, extend)
}

// ---------------------------------------------------------------------------
// Visual (soft-wrapped) vertical motion
// ---------------------------------------------------------------------------

/// The wrap geometry `move_visual_up`/`_down` step through: where each logical
/// line breaks into visual rows, and how wide a character is.
///
/// Supplied by the caller because the two views wrap differently — the plain
/// editor breaks hard at the text width (`render_util::scan_wrap_rows`) while
/// notebook cells break at word boundaries (`render_util::wrap_segments`) — and
/// visual motion must follow whichever rows are actually on screen.
pub struct Wrap<'a> {
    /// Char offsets within logical line `n` at which each visual row starts.
    /// Must be non-empty and start with 0.
    pub row_starts: &'a dyn Fn(usize) -> Vec<usize>,
    pub tab_width: usize,
}

impl Wrap<'_> {
    /// Display column of char offset `off` within the visual row starting at
    /// `row_start`, and the inverse: the offset in `[row_start, row_end)` whose
    /// column is closest to (but not past) `goal`.
    fn column_of(&self, rope: &Rope, line_start: usize, row_start: usize, off: usize) -> usize {
        let mut col = 0;
        for i in row_start..off {
            col += crate::render_util::char_display_width(
                rope.char(line_start + i),
                col,
                self.tab_width,
            );
        }
        col
    }

    fn offset_at_column(
        &self,
        rope: &Rope,
        line_start: usize,
        row: std::ops::Range<usize>,
        goal: usize,
    ) -> usize {
        let mut col = 0;
        for i in row.clone() {
            let w = crate::render_util::char_display_width(
                rope.char(line_start + i),
                col,
                self.tab_width,
            );
            if col + w > goal {
                return i;
            }
            col += w;
        }
        // Past the last character of the row: sit on it, not beyond it.
        row.end.saturating_sub(1).max(row.start)
    }
}

/// The visual rows of `line_idx` as char ranges within the line, excluding any
/// trailing newline.  A trailing empty row (a line whose width is an exact
/// multiple of the wrap width) is dropped: the renderer draws it, but there is
/// no character there for a cursor to land on.
fn visual_rows(rope: &Rope, line_idx: usize, w: &Wrap) -> Vec<std::ops::Range<usize>> {
    let content = line_content_len(rope, line_idx);
    let starts = (w.row_starts)(line_idx);
    let mut rows: Vec<std::ops::Range<usize>> = Vec::with_capacity(starts.len());
    for (i, &s) in starts.iter().enumerate() {
        let e = starts.get(i + 1).copied().unwrap_or(content).min(content);
        let s = s.min(content);
        if s >= e && i > 0 {
            continue; // phantom trailing row
        }
        rows.push(s..e);
    }
    if rows.is_empty() {
        rows.push(0..0);
    }
    rows
}

/// Chars on line `line_idx`, not counting the line terminator.
fn line_content_len(rope: &Rope, line_idx: usize) -> usize {
    if line_idx >= rope.len_lines() {
        return 0;
    }
    let line = rope.line(line_idx);
    let n = line.len_chars();
    if n > 0 && (line.char(n - 1) == '\n' || line.char(n - 1) == '\r') {
        n - 1
    } else {
        n
    }
}

/// Locate the cursor in wrap geometry: `(line, visual row index, rows)`.
fn locate(rope: &Rope, pos: usize, w: &Wrap) -> (usize, usize, Vec<std::ops::Range<usize>>) {
    let line = line_of(rope, pos);
    let off = pos - rope.line_to_char(line);
    let rows = visual_rows(rope, line, w);
    let idx = rows
        .iter()
        .rposition(|r| off >= r.start)
        .unwrap_or(0);
    (line, idx, rows)
}

/// `(visual row index of `pos` within its logical line, rows in that line)`.
pub fn visual_row_of(rope: &Rope, pos: usize, w: &Wrap) -> (usize, usize) {
    if rope.len_chars() == 0 {
        return (0, 1);
    }
    let (_, idx, rows) = locate(rope, pos.min(rope.len_chars()), w);
    (idx, rows.len())
}

/// Move down one *visual* row: to the next wrapped row of the same logical
/// line when there is one, else to the first row of the next logical line.
/// The display column within the row is preserved.
pub fn move_visual_down(rope: &Rope, sel: Selection, extend: bool, w: &Wrap) -> Selection {
    if rope.len_chars() == 0 {
        return apply_extend(sel, 0, extend);
    }
    let pos = sel.head.min(rope.len_chars());
    let (line, row_idx, rows) = locate(rope, pos, w);
    let line_start = rope.line_to_char(line);
    let goal = w.column_of(rope, line_start, rows[row_idx].start, pos - line_start);

    if row_idx + 1 < rows.len() {
        let row = rows[row_idx + 1].clone();
        let off = w.offset_at_column(rope, line_start, row, goal);
        return apply_extend(sel, line_start + off, extend);
    }
    if line + 1 >= rope.len_lines() {
        return apply_extend(sel, pos, extend);
    }
    let next_start = rope.line_to_char(line + 1);
    let next_rows = visual_rows(rope, line + 1, w);
    let off = w.offset_at_column(rope, next_start, next_rows[0].clone(), goal);
    apply_extend(sel, next_start + off, extend)
}

/// Move up one *visual* row — the inverse of [`move_visual_down`], landing on
/// the **last** wrapped row of the previous logical line.
pub fn move_visual_up(rope: &Rope, sel: Selection, extend: bool, w: &Wrap) -> Selection {
    if rope.len_chars() == 0 {
        return apply_extend(sel, 0, extend);
    }
    let pos = sel.head.min(rope.len_chars());
    let (line, row_idx, rows) = locate(rope, pos, w);
    let line_start = rope.line_to_char(line);
    let goal = w.column_of(rope, line_start, rows[row_idx].start, pos - line_start);

    if row_idx > 0 {
        let row = rows[row_idx - 1].clone();
        let off = w.offset_at_column(rope, line_start, row, goal);
        return apply_extend(sel, line_start + off, extend);
    }
    if line == 0 {
        return apply_extend(sel, pos, extend);
    }
    let prev_start = rope.line_to_char(line - 1);
    let prev_rows = visual_rows(rope, line - 1, w);
    let row = prev_rows.last().cloned().unwrap_or(0..0);
    let off = w.offset_at_column(rope, prev_start, row, goal);
    apply_extend(sel, prev_start + off, extend)
}

/// Move to the start of the current line.
pub fn move_line_start(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let new_head = line_start_char(rope, sel.head);
    apply_extend(sel, new_head, extend)
}

/// Move to the first non-whitespace char on the current line.
pub fn move_line_first_non_ws(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let ls = line_start_char(rope, sel.head);
    let le = line_end_char(rope, sel.head);
    let mut pos = ls;
    while pos <= le {
        if rope.len_chars() == 0 {
            break;
        }
        let c = rope.char(pos);
        if !c.is_whitespace() || c == '\n' {
            break;
        }
        pos += 1;
    }
    apply_extend(sel, pos, extend)
}

/// Move to the last non-newline char on the current line.
pub fn move_line_end(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let new_head = line_end_char(rope, sel.head);
    apply_extend(sel, new_head, extend)
}

/// Go to the first char of the file.
pub fn goto_file_start(_rope: &Rope, sel: Selection, extend: bool) -> Selection {
    apply_extend(sel, 0, extend)
}

/// Go to the first char of the last line.
pub fn goto_file_end(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    if rope.len_chars() == 0 {
        return apply_extend(sel, 0, extend);
    }
    let last_line = rope.len_lines().saturating_sub(1);
    let line_start = rope.line_to_char(last_line);
    let new_head = clamp_char(rope, line_start);
    apply_extend(sel, new_head, extend)
}

// ---------------------------------------------------------------------------
// Word motions
// ---------------------------------------------------------------------------

/// Move to the start of the next word.
pub fn move_word_forward(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let len = rope.len_chars();
    if len == 0 {
        return apply_extend(sel, 0, extend);
    }
    let mut pos = sel.head;
    if pos >= len {
        return apply_extend(sel, len - 1, extend);
    }
    let c = rope.char(pos);
    // Skip current word or punctuation cluster
    if word_char(c) {
        while pos < len && word_char(rope.char(pos)) {
            pos += 1;
        }
    } else if !c.is_whitespace() {
        while pos < len && !word_char(rope.char(pos)) && !rope.char(pos).is_whitespace() {
            pos += 1;
        }
    }
    // Skip whitespace
    while pos < len && rope.char(pos).is_whitespace() {
        pos += 1;
    }
    let new_head = clamp_char(rope, pos);
    apply_extend(sel, new_head, extend)
}

/// Move to the start of the next WORD (non-whitespace).
pub fn move_big_word_forward(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let len = rope.len_chars();
    if len == 0 {
        return apply_extend(sel, 0, extend);
    }
    let mut pos = sel.head;
    if pos >= len {
        return apply_extend(sel, len - 1, extend);
    }
    // Skip current WORD
    while pos < len && big_word_char(rope.char(pos)) {
        pos += 1;
    }
    // Skip whitespace
    while pos < len && rope.char(pos).is_whitespace() {
        pos += 1;
    }
    let new_head = clamp_char(rope, pos);
    apply_extend(sel, new_head, extend)
}

/// Move to the start of the previous/current word.
pub fn move_word_backward(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let len = rope.len_chars();
    if len == 0 {
        return apply_extend(sel, 0, extend);
    }
    let mut pos = sel.head;
    if pos == 0 {
        return apply_extend(sel, 0, extend);
    }
    // Move left one first
    pos -= 1;
    // Skip whitespace going left
    while pos > 0 && rope.char(pos).is_whitespace() {
        pos -= 1;
    }
    // Skip word chars going left
    if word_char(rope.char(pos)) {
        while pos > 0 && word_char(rope.char(pos - 1)) {
            pos -= 1;
        }
    } else {
        while pos > 0 && !word_char(rope.char(pos - 1)) && !rope.char(pos - 1).is_whitespace() {
            pos -= 1;
        }
    }
    apply_extend(sel, pos, extend)
}

/// Move to the start of the previous WORD.
pub fn move_big_word_backward(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let len = rope.len_chars();
    if len == 0 {
        return apply_extend(sel, 0, extend);
    }
    let mut pos = sel.head;
    if pos == 0 {
        return apply_extend(sel, 0, extend);
    }
    pos -= 1;
    // Skip whitespace
    while pos > 0 && rope.char(pos).is_whitespace() {
        pos -= 1;
    }
    // Skip WORD chars
    while pos > 0 && big_word_char(rope.char(pos - 1)) {
        pos -= 1;
    }
    apply_extend(sel, pos, extend)
}

/// Move to the end of the current word.
pub fn move_word_end(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let len = rope.len_chars();
    if len == 0 {
        return apply_extend(sel, 0, extend);
    }
    let mut pos = sel.head;
    if pos >= len {
        return apply_extend(sel, len - 1, extend);
    }
    // If already at word end, move right first
    if pos + 1 < len && word_char(rope.char(pos)) && word_char(rope.char(pos + 1)) {
        // not at end yet
    } else if !word_char(rope.char(pos)) {
        // On non-word, skip to next word
        while pos < len && !word_char(rope.char(pos)) {
            pos += 1;
        }
    } else {
        // At word char end
        pos += 1;
        // Skip whitespace
        while pos < len && rope.char(pos).is_whitespace() {
            pos += 1;
        }
        // Skip to word end
    }
    // Now skip to end of word
    while pos + 1 < len && word_char(rope.char(pos + 1)) {
        pos += 1;
    }
    let new_head = clamp_char(rope, pos);
    apply_extend(sel, new_head, extend)
}

/// Move to the end of the current WORD.
pub fn move_big_word_end(rope: &Rope, sel: Selection, extend: bool) -> Selection {
    let len = rope.len_chars();
    if len == 0 {
        return apply_extend(sel, 0, extend);
    }
    let mut pos = sel.head;
    if pos >= len {
        return apply_extend(sel, len - 1, extend);
    }
    if !big_word_char(rope.char(pos)) {
        while pos < len && !big_word_char(rope.char(pos)) {
            pos += 1;
        }
    } else {
        pos += 1;
        while pos < len && rope.char(pos).is_whitespace() {
            pos += 1;
        }
    }
    while pos + 1 < len && big_word_char(rope.char(pos + 1)) {
        pos += 1;
    }
    let new_head = clamp_char(rope, pos);
    apply_extend(sel, new_head, extend)
}

// ---------------------------------------------------------------------------
// Find-char motions
// ---------------------------------------------------------------------------

/// Find char `target` forward on the current line.
/// `till` = true: land before the char. `till` = false: land on it.
pub fn find_char_forward(
    rope: &Rope,
    sel: Selection,
    target: char,
    till: bool,
    extend: bool,
) -> Selection {
    if rope.len_chars() == 0 {
        return apply_extend(sel, 0, extend);
    }
    let le = line_end_char(rope, sel.head);
    let mut pos = sel.head + 1;
    while pos <= le {
        if rope.char(pos) == target {
            let new_head = if till && pos > 0 { pos - 1 } else { pos };
            return apply_extend(sel, new_head, extend);
        }
        pos += 1;
    }
    apply_extend(sel, sel.head, extend)
}

/// Find char `target` backward on the current line.
pub fn find_char_backward(
    rope: &Rope,
    sel: Selection,
    target: char,
    till: bool,
    extend: bool,
) -> Selection {
    if rope.len_chars() == 0 || sel.head == 0 {
        return apply_extend(sel, sel.head, extend);
    }
    let ls = line_start_char(rope, sel.head);
    let mut pos = sel.head;
    loop {
        if pos == 0 || pos < ls {
            break;
        }
        pos -= 1;
        if rope.char(pos) == target {
            let new_head = if till { pos + 1 } else { pos };
            return apply_extend(sel, new_head, extend);
        }
        if pos == 0 {
            break;
        }
    }
    apply_extend(sel, sel.head, extend)
}

// ---------------------------------------------------------------------------
// Select current line / entire file
// ---------------------------------------------------------------------------

/// Select the current line (including newline).
pub fn select_line(rope: &Rope, sel: Selection) -> Selection {
    if rope.len_chars() == 0 {
        return Selection::point(0);
    }
    let line_idx = line_of(rope, sel.head);
    let start = rope.line_to_char(line_idx);
    // Include the newline if present
    let end_line = (line_idx + 1).min(rope.len_lines() - 1);
    let end = if line_idx + 1 < rope.len_lines() {
        rope.line_to_char(end_line) - 1
    } else {
        // Last line — up to its final char. On the empty virtual line after a
        // trailing newline that is the line start itself.
        rope.len_chars().saturating_sub(1).max(start)
    };
    Selection::new(start, end)
}

/// Select the entire file.
pub fn select_all(rope: &Rope) -> Selection {
    if rope.len_chars() == 0 {
        return Selection::point(0);
    }
    Selection::new(0, rope.len_chars() - 1)
}

/// Go to a specific 1-based line number.
pub fn goto_line(rope: &Rope, sel: Selection, line_number: usize, extend: bool) -> Selection {
    if rope.len_chars() == 0 {
        return apply_extend(sel, 0, extend);
    }
    let line_idx = line_number.saturating_sub(1).min(rope.len_lines().saturating_sub(1));
    let new_head = rope.line_to_char(line_idx);
    apply_extend(sel, new_head, extend)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Visual (soft-wrapped) vertical motion
    // -----------------------------------------------------------------------

    /// Hard wrap at `width`, the plain editor's rule.
    fn hard_wrap(rope: &Rope, width: usize) -> impl Fn(usize) -> Vec<usize> + '_ {
        move |line| crate::render_util::wrap_row_starts(rope.line(line), width, 4)
    }

    #[test]
    fn visual_down_walks_the_rows_of_one_wrapped_line() {
        // Two logical lines; the first wraps to three rows at width 10.
        let rope = Rope::from_str("0123456789abcdefghijABCDE\nsecond line\n");
        let f = hard_wrap(&rope, 10);
        let w = Wrap { row_starts: &f, tab_width: 4 };

        let mut sel = Selection::point(2);
        sel = move_visual_down(&rope, sel, false, &w);
        assert_eq!(sel.head, 12, "second row of the same logical line");
        sel = move_visual_down(&rope, sel, false, &w);
        assert_eq!(sel.head, 22, "third row");
        sel = move_visual_down(&rope, sel, false, &w);
        assert_eq!(sel.head, 26 + 2, "column 2 of the next logical line");

        // …and back up again, landing on the *last* row of the wrapped line.
        sel = move_visual_up(&rope, sel, false, &w);
        assert_eq!(sel.head, 22);
        sel = move_visual_up(&rope, sel, false, &w);
        assert_eq!(sel.head, 12);
        sel = move_visual_up(&rope, sel, false, &w);
        assert_eq!(sel.head, 2);
    }

    /// The logical-line motion for comparison: one `j` skips the whole
    /// paragraph, which is what this change is here to fix.
    #[test]
    fn logical_down_still_skips_the_whole_wrapped_line() {
        let rope = Rope::from_str("0123456789abcdefghijABCDE\nsecond line\n");
        let sel = move_down(&rope, Selection::point(2), false);
        assert_eq!(sel.head, 26 + 2);
    }

    #[test]
    fn a_short_target_row_clamps_to_its_last_character() {
        // Row 1 of line 0 is "abcde" (5 chars); moving down from column 8 of
        // row 0 has nowhere near column 8 to land.
        let rope = Rope::from_str("0123456789abcde\nxy\n");
        let f = hard_wrap(&rope, 10);
        let w = Wrap { row_starts: &f, tab_width: 4 };
        let sel = move_visual_down(&rope, Selection::point(8), false, &w);
        assert_eq!(sel.head, 14, "last char of the short row, not past it");

        // Same for a short logical line below.
        let sel = move_visual_down(&rope, sel, false, &w);
        assert_eq!(sel.head, 16 + 1, "'y', the last char of \"xy\"");
    }

    #[test]
    fn visual_motion_stops_at_the_ends_of_the_buffer() {
        let rope = Rope::from_str("abc\ndef\n");
        let f = hard_wrap(&rope, 10);
        let w = Wrap { row_starts: &f, tab_width: 4 };
        let top = move_visual_up(&rope, Selection::point(1), false, &w);
        assert_eq!(top.head, 1, "already on the first row");
        let bottom = Selection::point(rope.len_chars());
        assert_eq!(
            move_visual_down(&rope, bottom, false, &w).head,
            bottom.head,
            "already on the last row"
        );
    }

    /// A line whose width is an exact multiple of the wrap width gets a
    /// trailing empty row from the renderer; there is no character there, so
    /// `j` must not park the cursor on it.
    #[test]
    fn an_exactly_full_line_does_not_trap_the_cursor_on_a_phantom_row() {
        let rope = Rope::from_str("0123456789\nnext\n");
        let f = hard_wrap(&rope, 10);
        let w = Wrap { row_starts: &f, tab_width: 4 };
        assert_eq!(crate::render_util::wrap_row_starts(rope.line(0), 10, 4), vec![0, 10]);
        let sel = move_visual_down(&rope, Selection::point(3), false, &w);
        assert_eq!(sel.head, 11 + 3, "straight to the next logical line");
    }

    #[test]
    fn visual_motion_extends_a_selection() {
        let rope = Rope::from_str("0123456789abcdefghij\n");
        let f = hard_wrap(&rope, 10);
        let w = Wrap { row_starts: &f, tab_width: 4 };
        let sel = move_visual_down(&rope, Selection::new(0, 2), true, &w);
        assert_eq!(sel.anchor, 0, "anchor stays put");
        assert_eq!(sel.head, 12);
    }

    /// Word-wrapped geometry (notebook cells) drives the same motion code.
    #[test]
    fn visual_motion_follows_word_wrapped_rows_too() {
        let rope = Rope::from_str("hello brave new world\n");
        let f = |line: usize| -> Vec<usize> {
            let text = rope.line(line).to_string();
            crate::render_util::wrap_segments(text.trim_end_matches('\n'), 12)
                .into_iter()
                .map(|(off, _)| off)
                .collect()
        };
        let w = Wrap { row_starts: &f, tab_width: 4 };
        // "hello brave" / "new world"
        let sel = move_visual_down(&rope, Selection::point(0), false, &w);
        assert_eq!(rope.slice(sel.head..sel.head + 3), "new");
    }

    /// A rope ending in a newline has a real final line whose only position is
    /// `len_chars()`.  Clamping char→line lookups to `len_chars() - 1` folded
    /// that line onto its predecessor, so `k` off it was a silent no-op and its
    /// column read as the previous line's last column.  (Hit constantly in
    /// notebook cells: type a comment, press Enter, and the cursor was stuck.)
    #[test]
    fn trailing_empty_line_is_addressable() {
        let rope = Rope::from_str("# comment\n");
        let last = rope.len_chars(); // 10 — start of the empty second line
        assert_eq!(rope.len_lines(), 2);

        // The empty line is column 0 of line 1, not column 9 of line 0.
        assert_eq!(col_of(&rope, last), 0);
        assert_eq!(line_of(&rope, last), 1);

        // `k` from it climbs to the line above rather than doing nothing.
        let up = move_up(&rope, Selection::point(last), false);
        assert_eq!(rope.char_to_line(up.head), 0);

        // `j` onto it, then `k` back off it, round-trips.
        let down = move_down(&rope, Selection::point(0), false);
        assert_eq!(down.head, last);
        assert_eq!(move_up(&rope, down, false).head, 0);

        // Line-wise ops resolve against the empty line, not the one above.
        assert_eq!(line_start_char(&rope, last), last);
        assert_eq!(line_end_char(&rope, last), last);
        let sel = select_line(&rope, Selection::point(last));
        assert_eq!((sel.anchor, sel.head), (last, last));
    }

    /// The same clamp applies at the end of a rope with no trailing newline —
    /// behaviour there must be unchanged.
    #[test]
    fn last_line_without_trailing_newline_is_unchanged() {
        let rope = Rope::from_str("ab\ncd");
        assert_eq!(col_of(&rope, 4), 1);
        assert_eq!(move_up(&rope, Selection::point(4), false).head, 1);
        assert_eq!(move_down(&rope, Selection::point(4), false).head, 4);
    }
}
