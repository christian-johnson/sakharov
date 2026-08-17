use crate::{app::App, selection::Selection};

/// Recompute `app.search.matches` for the current query across the whole buffer.
pub fn search_compute_matches(app: &mut App) {
    app.search.matches.clear();
    app.search.current = 0;
    if app.search.query.is_empty() {
        return;
    }
    let text = app.buffer.rope.to_string();
    // Case-insensitive by default: fold both sides with to_ascii_lowercase, which
    // preserves byte offsets 1:1 with the original text (unlike full Unicode
    // lowercasing), so the char_pos/byte_pos accounting below stays correct.
    let haystack = text.to_ascii_lowercase();
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
        let mut app = App::new(None, Config::load()).unwrap();
        app.viewport_height = 40;
        app.viewport_width = 80;
        app.buffer.rope = Rope::from_str(src);
        app
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
}
