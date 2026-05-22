use crate::{app::App, selection::Selection};

/// Recompute `app.search.matches` for the current query across the whole buffer.
pub fn search_compute_matches(app: &mut App) {
    app.search.matches.clear();
    app.search.current = 0;
    if app.search.query.is_empty() {
        return;
    }
    let text = app.buffer.rope.to_string();
    let query = app.search.query.clone();
    let mut start = 0;
    while start < text.len() {
        if let Some(rel) = text[start..].find(query.as_str()) {
            let byte_pos = start + rel;
            let char_idx = text[..byte_pos].chars().count();
            app.search.matches.push(char_idx);
            start = byte_pos + query.len().max(1);
        } else {
            break;
        }
    }
}

/// Jump to the next (or previous if `reverse`) search match relative to cursor.
pub fn search_jump(app: &mut App, reverse: bool) {
    if app.search.matches.is_empty() {
        if !app.search.query.is_empty() {
            app.message = Some(format!("No matches for \"{}\"", app.search.query));
        }
        return;
    }
    let cursor = app.selection.head;
    let count = app.search.matches.len();
    if reverse {
        let idx = app.search.matches.iter().rposition(|&m| m < cursor)
            .unwrap_or(count - 1);
        app.search.current = idx;
    } else {
        let idx = app.search.matches.iter().position(|&m| m > cursor)
            .unwrap_or(0);
        app.search.current = idx;
    }
    app.selection = Selection::point(app.search.matches[app.search.current]);
    super::update_scroll(app);
    app.message = Some(format!(
        "Match {}/{} for \"{}\"",
        app.search.current + 1,
        count,
        app.search.query,
    ));
}
