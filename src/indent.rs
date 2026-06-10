use ropey::Rope;

/// The indentation unit a fresh indent should insert, per editor config.
///
/// With `expand_tabs` (the default) this is `tab_width` spaces, so the editor
/// never writes a literal tab; otherwise it is a single tab character.
pub fn unit(expand_tabs: bool, tab_width: usize) -> String {
    if expand_tabs {
        " ".repeat(tab_width.max(1))
    } else {
        "\t".to_string()
    }
}

/// Extract the leading whitespace of a rope line slice as an owned string.
pub fn line_indent(line: ropey::RopeSlice) -> String {
    line.chars().take_while(|&c| c == ' ' || c == '\t').collect()
}

/// True if the line content ends with a character that increases indent.
/// Strips trailing whitespace/newlines before checking.
fn is_indent_trigger(line: &str) -> bool {
    let trimmed = line.trim_end_matches(|c: char| c.is_whitespace());
    matches!(trimmed.chars().last(), Some(':') | Some('{') | Some('(') | Some('['))
}

/// Compute the indentation string to insert after a newline at `pos`.
///
/// Used by both Enter (insert mode) and `o` (open line below). `unit` is the
/// extra indentation added after an indent trigger (`:`/`{`/`(`/`[`), supplied
/// by the caller from editor config (see [`unit`]). The returned string does
/// NOT include the newline itself.
pub fn for_new_line(rope: &Rope, pos: usize, unit: &str) -> String {
    if rope.len_chars() == 0 {
        return String::new();
    }
    let pos = pos.min(rope.len_chars());
    let line_idx = rope.char_to_line(pos);
    let line = rope.line(line_idx);
    let base = line_indent(line);

    // Only consider content up to the cursor position within this line to
    // correctly handle Enter mid-line as well as at end-of-line.
    let line_start = rope.line_to_char(line_idx);
    let cursor_off = pos.saturating_sub(line_start);
    let content_before_cursor: String = line.chars().take(cursor_off).collect();

    if is_indent_trigger(&content_before_cursor) {
        format!("{base}{unit}")
    } else {
        base
    }
}

/// Compute the indentation to copy when opening a line above the current one.
/// Always matches the current line's own leading whitespace.
pub fn for_line_above(rope: &Rope, pos: usize) -> String {
    if rope.len_chars() == 0 {
        return String::new();
    }
    let line_idx = rope.char_to_line(pos.min(rope.len_chars()));
    line_indent(rope.line(line_idx))
}

/// True if `pos` sits between a matching opening/closing bracket pair —
/// i.e. the char immediately before `pos` is `{`/`(`/`[` and the char at
/// `pos` is the corresponding `}`/`)`/`]`.
///
/// Used to decide whether Enter should do bracket expansion:
///   `{|}` → `{\n    |\n}` (middle line indented, closing bracket on its own line)
pub fn is_bracket_pair(rope: &Rope, pos: usize) -> bool {
    if pos == 0 || pos >= rope.len_chars() {
        return false;
    }
    matches!(
        (rope.char(pos - 1), rope.char(pos)),
        ('{', '}') | ('(', ')') | ('[', ']')
    )
}
