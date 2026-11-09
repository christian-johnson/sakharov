use crate::{app::App, mode::Mode, selection::Selection};

/// `/` or `?`: start typing a query, remembering where to come back to.
pub(super) fn begin(app: &mut App, forward: bool) {
    app.search.origin = Some((app.selection, std::mem::take(&mut app.search.query)));
    app.mode = Mode::Search { forward };
    search_compute_matches(app);
}

/// The query changed: land on the first match from where the search began,
/// or back there when nothing matches.
pub fn search_update(app: &mut App, forward: bool) {
    search_compute_matches(app);
    let Some((origin, _)) = app.search.origin else { return };
    let from = origin.head;
    let matches = &app.search.matches;
    let found = if forward {
        matches.iter().position(|&m| m >= from).or((!matches.is_empty()).then_some(0))
    } else {
        matches.iter().rposition(|&m| m <= from).or(matches.len().checked_sub(1))
    };
    match found {
        Some(idx) => {
            app.search.current = idx;
            app.selection = Selection::point(app.search.matches[idx]);
        }
        None => app.selection = origin,
    }
    super::update_scroll(app);
}

/// Enter: keep the match the cursor is on.
pub fn search_confirm(app: &mut App) {
    app.search.origin = None;
    app.mode = Mode::Normal;
    if app.search.query.is_empty() {
        return;
    }
    let count = app.search.matches.len();
    app.messages.show(if count == 0 {
        format!("No matches for \"{}\"", app.search.query)
    } else {
        format!("Match {}/{count} for \"{}\"", app.search.current + 1, app.search.query)
    });
}

/// Esc: back to where the search began, with the previous query.
pub fn search_cancel(app: &mut App) {
    if let Some((origin, query)) = app.search.origin.take() {
        app.selection = origin;
        app.search.query = query;
        search_compute_matches(app);
        super::update_scroll(app);
    }
    app.mode = Mode::Normal;
}

/// Stop highlighting matches; `n` / `N` still repeat the search.
pub(super) fn clear_highlight(app: &mut App) {
    app.search.matches.clear();
    app.search.searched = None;
}

/// Recompute `app.search.matches` for the current query across the whole buffer.
pub fn search_compute_matches(app: &mut App) {
    app.search.matches.clear();
    app.search.current = 0;
    app.search.searched = Some(app.buffer.rope.clone());
    if app.search.query.is_empty() {
        return;
    }
    // Case-insensitive by default: ASCII folding keeps every byte offset and
    // char boundary where it was (unlike full Unicode lowercasing), so the
    // folded copy alone serves for both finding and counting chars.
    let mut haystack = app.buffer.rope.to_string();
    haystack.make_ascii_lowercase();
    let text = haystack.as_str();
    let query = app.search.query.to_ascii_lowercase();
    let mut byte_pos = 0usize;
    let mut char_pos = 0usize; // chars counted up to byte_pos — updated incrementally
    while byte_pos < haystack.len() {
        if let Some(rel) = haystack[byte_pos..].find(query.as_str()) {
            // Advance char_pos from the previous byte_pos to the match start.
            let match_byte = byte_pos + rel;
            char_pos += text[byte_pos..match_byte].chars().count();
            app.search.matches.push(char_pos);
            // Advance past this match for the next iteration.
            let next_byte = match_byte + query.len().max(1);
            char_pos += text[match_byte..next_byte.min(text.len())].chars().count();
            byte_pos = next_byte;
        } else {
            break;
        }
    }
}

/// Jump to the next (or previous if `reverse`) search match relative to cursor.
pub fn search_jump(app: &mut App, reverse: bool) {
    // The text was edited (or another buffer opened) since the last search.
    if !app.search.is_current(&app.buffer.rope) {
        search_compute_matches(app);
    }
    if app.search.matches.is_empty() {
        if !app.search.query.is_empty() {
            app.messages.show(format!("No matches for \"{}\"", app.search.query));
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
    app.messages.show(format!(
        "Match {}/{} for \"{}\"",
        app.search.current + 1,
        count,
        app.search.query,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use ropey::Rope;

    fn app_with_text(src: &str) -> App {
        let mut app = App::new(None, Config::load());
        app.viewport_height = 40;
        app.viewport_width = 80;
        app.buffer.rope = Rope::from_str(src);
        app
    }

    fn type_keys(app: &mut App, keys: &str) {
        for c in keys.chars() {
            crate::input::handle_key(
                app,
                crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char(c)),
            );
        }
    }

    fn press(app: &mut App, code: crossterm::event::KeyCode) {
        crate::input::handle_key(app, crossterm::event::KeyEvent::from(code));
    }

    #[test]
    fn search_is_case_insensitive_by_default() {
        let mut app = app_with_text("Hello world\nHELLO again\nhello there");
        app.search.query = "hello".to_string();
        search_compute_matches(&mut app);
        assert_eq!(app.search.matches.len(), 3);
    }

    #[test]
    fn entering_search_forward_clears_a_stale_query() {
        let mut app = app_with_text("foobar");
        app.search.query = "foo".to_string();
        search_compute_matches(&mut app);
        assert_eq!(app.search.matches.len(), 1);

        crate::exec::execute(&mut app, &crate::command::Command::SearchForward);
        assert!(app.search.query.is_empty());
        assert!(app.search.matches.is_empty());
    }

    /// Typing narrows onto the first match after where the search began, and
    /// Enter stays there.  Each keystroke used to search on from the match the
    /// last one landed on, so the first match was skipped.
    #[test]
    fn typing_a_query_lands_on_the_first_match_and_enter_keeps_it() {
        let mut app = app_with_text("x foo foo\nfoo\n");
        type_keys(&mut app, "/foo");
        assert_eq!(app.selection.head, 2);
        press(&mut app, crossterm::event::KeyCode::Enter);
        assert_eq!(app.selection.head, 2);
        assert_eq!(app.mode, Mode::Normal);

        type_keys(&mut app, "n");
        assert_eq!(app.selection.head, 6);
    }

    #[test]
    fn escape_returns_to_where_the_search_began_with_the_previous_query() {
        let mut app = app_with_text("one two three two\n");
        type_keys(&mut app, "/two");
        press(&mut app, crossterm::event::KeyCode::Enter);
        assert_eq!(app.selection.head, 4);

        type_keys(&mut app, "/thr");
        assert_eq!(app.selection.head, 8);
        press(&mut app, crossterm::event::KeyCode::Esc);
        assert_eq!(app.selection.head, 4);
        assert_eq!(app.search.query, "two");
    }

    /// Match positions are char offsets into the text as it was searched;
    /// after an edit `n` has to search again rather than jump to stale ones.
    #[test]
    fn next_match_searches_again_after_an_edit() {
        let mut app = app_with_text("aa foo\n");
        type_keys(&mut app, "/foo");
        press(&mut app, crossterm::event::KeyCode::Enter);
        app.selection = Selection::point(0);
        type_keys(&mut app, "d"); // "a foo"
        type_keys(&mut app, "n");
        assert_eq!(app.selection.head, 2);
    }
}
