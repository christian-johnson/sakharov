use ropey::Rope;

/// Detect the indentation unit in use by scanning the first 200 lines.
/// Returns a tab character if tabs are found first, otherwise the smallest
/// non-zero run of spaces seen (defaulting to 4 spaces if nothing is found).
pub fn detect_unit(rope: &Rope) -> String {
    let mut min_spaces: Option<usize> = None;
    for line_idx in 0..rope.len_lines().min(200) {
        let line = rope.line(line_idx);
        match line.chars().next() {
            Some('\t') => return "\t".to_string(),
            Some(' ') => {
                let n = line.chars().take_while(|&c| c == ' ').count();
                if n > 0 {
                    min_spaces = Some(match min_spaces {
                        None => n,
                        Some(cur) => cur.min(n),
                    });
                }
            }
            _ => {}
        }
    }
    " ".repeat(min_spaces.unwrap_or(4))
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
/// Used by both Enter (insert mode) and `o` (open line below).
/// The returned string does NOT include the newline itself.
pub fn for_new_line(rope: &Rope, pos: usize) -> String {
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
        let unit = detect_unit(rope);
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
