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
use crate::highlight::{input_edit, Highlighter};

/// Insert `text` at char `pos`.
pub fn insert(app: &mut App, pos: usize, text: &str) {
    if text.is_empty() {
        return;
    }
    let edit = input_edit(&app.buffer.rope, pos, pos, text);
    app.buffer.insert_raw(pos, text);
    limit_large_buffer(app);
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
    limit_large_buffer(app);
    app.highlighter.invalidate();
    app.highlights_dirty = true;
    super::lsp_did_change(app);
}

/// Past `editor.large_file_mb`, turn off the buffer's syntax highlighting and
/// language server: parsing and syncing that much text stalls every edit.
/// Called wherever a buffer can grow that large (opening, pasting, a
/// wholesale replace); it stays off for the buffer until it is reopened.
pub fn limit_large_buffer(app: &mut App) {
    let limit_mb = app.config.editor.large_file_mb;
    let large = app.buffer.rope.len_bytes() > limit_mb.saturating_mul(1024 * 1024);
    let attached = app.highlighter.is_active() || app.lsp_language.is_some();
    if !large || !attached || app.notebook.is_some() {
        return;
    }
    if let (Some(lang), Some(path)) = (app.lsp_language.take(), app.buffer.path.as_ref()) {
        app.lsp.did_close(&lang, path);
        app.lsp.diagnostics.remove(&crate::lsp::diagnostic_key(path));
    }
    app.diag_by_line.clear();
    app.highlighter = Highlighter::new(None);
    app.highlight_spans.clear();
    app.highlights_dirty = true;
    app.messages.show(format!(
        "Large file (over {limit_mb} MB): syntax highlighting and language server off"
    ));
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
        let mut app = App::new(None, crate::config::Config::load());
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

    /// Past `editor.large_file_mb` a buffer drops syntax highlighting and its
    /// language server, whether it got there by opening or by pasting.
    #[test]
    fn large_buffers_turn_off_highlighting_and_the_language_server() {
        let dir = std::env::temp_dir().join(format!("sv-large-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = "[1]\n".repeat(300_000);
        let opened = dir.join("opened.json");
        std::fs::write(&opened, &big).unwrap();

        let mut app = App::new(None, crate::config::Config::load());
        app.config.editor.large_file_mb = 1;
        crate::exec::open_path(&mut app, &opened);
        assert!(!app.highlighter.is_active(), "opened: still highlighted");
        assert_eq!(app.lsp_language, None, "opened: language server still attached");

        let pasted = dir.join("pasted.json");
        crate::exec::open_path(&mut app, &pasted);
        assert!(app.highlighter.is_active(), "a new, empty .json file is highlighted");
        assert_eq!(app.lsp_language.as_deref(), Some("json"));
        crate::input::handle_paste(&mut app, &big);
        assert!(!app.highlighter.is_active(), "pasted: still highlighted");
        assert_eq!(app.lsp_language, None, "pasted: language server still attached");
        assert!(app.messages.log.iter().any(|m| m.contains("Large file")));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
