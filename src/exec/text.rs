use crate::{app::App, indent, mode::Mode, selection::Selection};

pub(super) fn delete_selection(app: &mut App) {
    let start = app.selection.start();
    let end = app.selection.end();
    let del_end = (end + 1).min(app.buffer.rope.len_chars());
    let text = app.buffer.rope.slice(start..del_end).to_string();
    app.clipboard = text.clone();
    crate::clipboard::write(&text);
    app.buffer.remove(start, del_end);
    let new_pos = start.min(app.buffer.rope.len_chars());
    app.selection = Selection::point(new_pos);
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn yank_selection(app: &mut App) {
    let start = app.selection.start();
    let end = (app.selection.end() + 1).min(app.buffer.rope.len_chars());
    let text = app.buffer.rope.slice(start..end).to_string();
    app.clipboard = text.clone();
    crate::clipboard::write(&text);
    app.message = Some(format!("Yanked {} chars", end - start));
}

pub(super) fn paste_after(app: &mut App) {
    let text = crate::clipboard::read().unwrap_or_else(|| app.clipboard.clone());
    if text.is_empty() {
        return;
    }
    app.clipboard = text.clone();
    let pos = app.selection.head;
    let len = app.buffer.rope.len_chars();
    let insert_pos = if len > 0 { (pos + 1).min(len) } else { 0 };
    app.buffer.insert(insert_pos, &text);
    app.selection = Selection::point(insert_pos);
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn paste_before(app: &mut App) {
    let text = crate::clipboard::read().unwrap_or_else(|| app.clipboard.clone());
    if text.is_empty() {
        return;
    }
    app.clipboard = text.clone();
    let pos = app.selection.head;
    app.buffer.insert(pos, &text);
    app.selection = Selection::point(pos);
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn open_line_below(app: &mut App) {
    let pos = app.selection.head;
    // Compute indentation before borrowing mutably.
    let ind = indent::for_new_line(&app.buffer.rope, pos);

    let le = {
        let rope = &app.buffer.rope;
        if rope.len_chars() == 0 {
            0
        } else {
            let line_idx = rope.char_to_line(pos.min(rope.len_chars()));
            let line_str = rope.line(line_idx);
            let line_len = line_str.len_chars();
            if line_len > 0
                && (line_str.char(line_len - 1) == '\n' || line_str.char(line_len - 1) == '\r')
            {
                rope.line_to_char(line_idx) + line_len - 1
            } else {
                rope.line_to_char(line_idx) + line_len
            }
        }
    };

    let ind_len = ind.chars().count();
    let to_insert = format!("\n{ind}");
    app.buffer.insert(le, &to_insert);
    app.selection = Selection::point(le + 1 + ind_len);
    app.mode = Mode::Insert;
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn open_line_above(app: &mut App) {
    let pos = app.selection.head;
    let ind = indent::for_line_above(&app.buffer.rope, pos);

    let ls = {
        let rope = &app.buffer.rope;
        if rope.len_chars() == 0 {
            0
        } else {
            let line_idx = rope.char_to_line(pos.min(rope.len_chars()));
            rope.line_to_char(line_idx)
        }
    };

    let ind_len = ind.chars().count();
    let to_insert = format!("{ind}\n");
    app.buffer.insert(ls, &to_insert);
    // Cursor after the indentation, on the newline (end of new blank line).
    app.selection = Selection::point(ls + ind_len);
    app.mode = Mode::Insert;
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn comment_region(app: &mut App) {
    let lang = app.current_language().unwrap_or("").to_owned();
    let prefix: &str = match lang.as_str() {
        "python" => "# ",
        "rust" | "javascript" => "// ",
        _ => {
            app.message = Some("No comment syntax known for this file type".into());
            return;
        }
    };
    let prefix_token = prefix.trim_end(); // "# " → "#", "// " → "//"

    if app.buffer.rope.len_chars() == 0 {
        return;
    }

    let rope = &app.buffer.rope;
    let start_char = app.selection.start().min(rope.len_chars());
    let end_char = app.selection.end().min(rope.len_chars());
    let start_line = rope.char_to_line(start_char);
    // end_char could sit on the newline of the previous line when the selection
    // ends right at a line boundary — clamp to the line that actually contains content.
    let end_line = {
        let l = rope.char_to_line(end_char);
        // If end_char is exactly at the start of a line (newline boundary) and
        // that line is beyond start_line, prefer the previous line.
        if l > start_line && rope.line_to_char(l) == end_char {
            l - 1
        } else {
            l
        }
    };

    // Determine whether all non-empty lines already carry the comment token.
    let all_commented = (start_line..=end_line).all(|li| {
        let line = rope.line(li);
        let content: String = line
            .chars()
            .take_while(|&c| c != '\n' && c != '\r')
            .collect();
        let trimmed = content.trim_start();
        trimmed.is_empty() || trimmed.starts_with(prefix_token)
    });

    app.buffer.begin_edit_session();

    if all_commented {
        // Uncomment: strip the prefix (with its trailing space if present).
        for li in (start_line..=end_line).rev() {
            let line_start = app.buffer.rope.line_to_char(li);
            let content: String = app.buffer.rope
                .line(li)
                .chars()
                .take_while(|&c| c != '\n' && c != '\r')
                .collect();
            if content.trim_start().is_empty() {
                continue;
            }
            let indent: usize = content
                .chars()
                .take_while(|c| c.is_whitespace())
                .count();
            let after_indent = line_start + indent;
            let trimmed = &content[indent..]; // safe: indent counted in chars above
            if trimmed.starts_with(prefix) {
                app.buffer.remove_raw(after_indent, after_indent + prefix.chars().count());
            } else if trimmed.starts_with(prefix_token) {
                app.buffer.remove_raw(after_indent, after_indent + prefix_token.chars().count());
            }
        }
    } else {
        // Comment: prepend prefix at column 0 on each non-empty line.
        // Placing markers at col 0 (not after indentation) keeps the code
        // valid for whitespace-sensitive languages like Python: a commented-out
        // indented block remains syntactically inert when the comment is removed.
        for li in (start_line..=end_line).rev() {
            let line_start = app.buffer.rope.line_to_char(li);
            let content: String = app.buffer.rope
                .line(li)
                .chars()
                .take_while(|&c| c != '\n' && c != '\r')
                .collect();
            if content.trim_start().is_empty() {
                continue;
            }
            app.buffer.insert_raw(line_start, prefix);
        }
    }

    app.buffer.modified = true;
    super::recompute_highlights(app);
}

pub(super) fn clamp_selection(app: &mut App) {
    let len = app.buffer.rope.len_chars();
    let head = app.selection.head.min(len);
    let anchor = app.selection.anchor.min(len);
    app.selection = Selection::new(anchor, head);
}
