//! The one way text changes in `app.buffer`.
//!
//! Everything derived from the text follows it from here: the highlighter's
//! parse tree is told what changed (so the next parse is incremental), the
//! highlights are marked stale, and the language server gets the change as a
//! delta.  An edit that bypassed this module would leave the server holding
//! different text from the editor.
//!
//! Undo grouping stays with the caller: call `app.buffer.begin_edit_session()`
//! first to make the edit (or a run of them) one undo step.

use crate::app::App;
use crate::highlight::input_edit;

/// Insert `text` at char `pos`.
pub fn insert(app: &mut App, pos: usize, text: &str) {
    if text.is_empty() {
        return;
    }
    let edit = input_edit(&app.buffer.rope, pos, pos, text);
    app.buffer.insert_raw(pos, text);
    app.highlighter.edit(&edit);
    app.highlights_dirty = true;
    super::lsp_did_change_insert(app, pos, text);
}

/// Remove chars `start..end`.
pub fn remove(app: &mut App, start: usize, end: usize) {
    if start >= end {
        return;
    }
    let edit = input_edit(&app.buffer.rope, start, end, "");
    let removed = app.buffer.rope.slice(start..end).to_string();
    app.buffer.remove_raw(start, end);
    app.highlighter.edit(&edit);
    app.highlights_dirty = true;
    super::lsp_did_change_remove(app, start, &removed);
}

/// The whole text was swapped at once (undo, redo, a formatter's output, a
/// recovered file): nothing can be updated incrementally.
pub fn replaced(app: &mut App) {
    app.highlighter.invalidate();
    app.highlights_dirty = true;
    super::lsp_did_change(app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::selection::Selection;

    /// Normal-mode edits reach the language server too.  They used to be
    /// skipped, relying on a length check at the next Insert keystroke — which
    /// a delete followed by a same-length paste slips straight past, leaving
    /// the server with different text from the editor.
    #[test]
    fn command_edits_keep_the_language_server_in_step() {
        let mut app = App::new(None, crate::config::Config::load()).unwrap();
        app.buffer.path = Some(std::path::PathBuf::from("/tmp/sv-edit-test.py"));
        app.lsp_language = Some("python".into());
        app.buffer.rope = ropey::Rope::from_str("abc def\n");
        app.buffer.lsp_synced_chars = Some(app.buffer.rope.len_chars());

        app.selection = Selection::new(0, 2);
        crate::exec::execute(&mut app, &Command::DeleteSelection);
        assert_eq!(app.buffer.lsp_synced_chars, Some(5), "the delete was not sent");
        crate::exec::execute(&mut app, &Command::PasteBefore);

        assert_eq!(app.buffer.lsp_synced_chars, Some(8), "the paste was not sent");
        assert!(app.highlights_dirty);
    }
}
