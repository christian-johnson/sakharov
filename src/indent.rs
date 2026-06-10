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

/// If the line containing `pos` is a non-empty Markdown list item (or
/// blockquote), return the prefix — indent + marker — that continues it on a
/// new line. Ordered markers increment (`2.` after `1.`), task-list
/// checkboxes continue unchecked, and `None` is returned when the item has no
/// content yet (so Enter on an empty `- ` ends the list instead of repeating
/// it) or when the cursor sits before the marker.
pub fn markdown_list_continuation(rope: &Rope, pos: usize) -> Option<String> {
    if rope.len_chars() == 0 {
        return None;
    }
    let pos = pos.min(rope.len_chars());
    let line_idx = rope.char_to_line(pos);
    let line: String = rope
        .line(line_idx)
        .chars()
        .take_while(|&c| c != '\n' && c != '\r')
        .collect();
    let indent: String = line.chars().take_while(|&c| c == ' ' || c == '\t').collect();
    let rest = &line[indent.len()..]; // indent is ASCII, byte slicing is safe
    let col = pos - rope.line_to_char(line_idx);

    // Continue only when the marker (ASCII) and some content sit before the
    // cursor — Enter at the start of a list line just splits it.
    let continuation = |marker_len: usize, content: &str, cont: String| -> Option<String> {
        if content.trim().is_empty() || col < indent.len() + marker_len {
            None
        } else {
            Some(cont)
        }
    };

    // Task list ("- [ ] " / "- [x] ") before plain bullets — longest match first.
    for bullet in ['-', '*', '+'] {
        for boxmark in ["[ ] ", "[x] ", "[X] "] {
            let marker = format!("{bullet} {boxmark}");
            if let Some(content) = rest.strip_prefix(&marker) {
                return continuation(marker.len(), content, format!("{indent}{bullet} [ ] "));
            }
        }
    }
    // Bullet list.
    for marker in ["- ", "* ", "+ "] {
        if let Some(content) = rest.strip_prefix(marker) {
            return continuation(marker.len(), content, format!("{indent}{marker}"));
        }
    }
    // Ordered list: "12. " or "12) " — continue with the next number.
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        let after = &rest[digits.len()..];
        for sep in [". ", ") "] {
            if let Some(content) = after.strip_prefix(sep) {
                let n: u64 = digits.parse().unwrap_or(0);
                return continuation(
                    digits.len() + sep.len(),
                    content,
                    format!("{indent}{}{sep}", n.saturating_add(1)),
                );
            }
        }
    }
    // Blockquote.
    if let Some(content) = rest.strip_prefix("> ") {
        return continuation(2, content, format!("{indent}> "));
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cont(text: &str, pos: usize) -> Option<String> {
        markdown_list_continuation(&Rope::from_str(text), pos)
    }

    #[test]
    fn list_markers_continue() {
        assert_eq!(cont("- item", 6), Some("- ".into()));
        assert_eq!(cont("  * item", 8), Some("  * ".into()));
        assert_eq!(cont("1. first", 8), Some("2. ".into()));
        assert_eq!(cont("9) ninth", 8), Some("10) ".into()));
        assert_eq!(cont("- [x] done", 10), Some("- [ ] ".into()));
        assert_eq!(cont("> quoted", 8), Some("> ".into()));
    }

    #[test]
    fn empty_items_and_plain_text_do_not_continue() {
        assert_eq!(cont("- ", 2), None, "empty item ends the list");
        assert_eq!(cont("-no space", 9), None);
        assert_eq!(cont("plain text", 10), None);
        // Cursor before the marker: splitting the line, not continuing the list.
        assert_eq!(cont("- item", 0), None);
    }

    #[test]
    fn open_line_indent_trigger_is_position_dependent() {
        let rope = Rope::from_str("def f():");
        let unit = "    ";
        // At end of line the trailing ':' triggers an extra indent level…
        assert_eq!(for_new_line(&rope, 8, unit), "    ");
        // …but mid-line (before the colon) it does not.
        assert_eq!(for_new_line(&rope, 0, unit), "");
    }
}
