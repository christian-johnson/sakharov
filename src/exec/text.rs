use crate::{app::App, mode::Mode, selection::Selection};

pub(super) fn delete_selection(app: &mut App) {
    let start = app.selection.start();
    let end = app.selection.end();
    let del_end = (end + 1).min(app.buffer.rope.len_chars());
    app.buffer.remove(start, del_end);
    let new_pos = start.min(app.buffer.rope.len_chars());
    app.selection = Selection::point(new_pos);
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn yank_selection(app: &mut App) {
    let start = app.selection.start();
    let end = (app.selection.end() + 1).min(app.buffer.rope.len_chars());
    app.clipboard = app.buffer.rope.slice(start..end).to_string();
    app.message = Some(format!("Yanked {} chars", end - start));
}

pub(super) fn paste_after(app: &mut App) {
    let text = app.clipboard.clone();
    if text.is_empty() {
        return;
    }
    let pos = app.selection.head;
    let len = app.buffer.rope.len_chars();
    let insert_pos = if len > 0 { (pos + 1).min(len) } else { 0 };
    app.buffer.insert(insert_pos, &text);
    app.selection = Selection::point(insert_pos);
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn paste_before(app: &mut App) {
    let text = app.clipboard.clone();
    if text.is_empty() {
        return;
    }
    let pos = app.selection.head;
    app.buffer.insert(pos, &text);
    app.selection = Selection::point(pos);
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn open_line_below(app: &mut App) {
    let rope = &app.buffer.rope;
    let pos = app.selection.head;
    let le = if rope.len_chars() == 0 {
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
    };
    app.buffer.insert(le, "\n");
    app.selection = Selection::point(le + 1);
    app.mode = Mode::Insert;
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn open_line_above(app: &mut App) {
    let rope = &app.buffer.rope;
    let pos = app.selection.head;
    let ls = if rope.len_chars() == 0 {
        0
    } else {
        let line_idx = rope.char_to_line(pos.min(rope.len_chars()));
        rope.line_to_char(line_idx)
    };
    app.buffer.insert(ls, "\n");
    app.selection = Selection::point(ls);
    app.mode = Mode::Insert;
    super::recompute_highlights(app);
    super::update_scroll(app);
}

pub(super) fn clamp_selection(app: &mut App) {
    let len = app.buffer.rope.len_chars();
    let head = app.selection.head.min(len);
    let anchor = app.selection.anchor.min(len);
    app.selection = Selection::new(anchor, head);
}
