//! What a top-level view *is*, and the concerns every view has to answer for.
//!
//! A view is whatever currently owns the screen and the keyboard.  There is no
//! `dyn View` trait: nearly every operation a view performs needs `&mut App`,
//! so a trait object would have to be moved out of `App`, called, and put back
//! on every frame — fighting the borrow checker for no safety gain.
//!
//! What that trait would have bought is enforcement: *adding a view must not
//! silently skip a dispatch site*.  This module buys it a different way.  Every
//! per-view decision is an **exhaustive `match` on [`View`]** — never an
//! `if view == X { … }` early return with an implicit `else` for "everything
//! else".  Adding a variant is then a compile error at each site that has to
//! think about it, which is the property that actually matters.
//!
//! The sites, and what each one owes a new variant:
//!
//! | Site | Owes |
//! |------|------|
//! | [`View::has_text_buffer`] | whether `app.buffer` is real or detached |
//! | `app::draw_frame` | a render arm (geometry comes from [`Chrome`]) |
//! | `exec::scroll::update_scroll` | a scroll arm |
//! | `input::keymap_layer` | a keymap override layer |
//! | `exec::execute` | a `handle` interception |
//! | `exec::goto_hints` / `input::goto_command` | the `g` sub-mode's meanings |
//! | `ui::status_ctx` | how the status line names what is open |
//! | `exec::buffers` | open / teardown / identity |
//!
//! [`Refusal`] lives here rather than in `exec::table` because it is not the
//! grid's idea: it is what *any* view that isn't a text buffer says when a
//! text command reaches it.

use ratatui::layout::Rect;

use crate::command::Command;

// ---------------------------------------------------------------------------
// The view enum
// ---------------------------------------------------------------------------

/// Which top-level view owns the screen and the keyboard.
///
/// The views are mutually exclusive by construction: each non-`Text` variant
/// requires its own `App` field to be populated, and opening one tears the
/// other down (see `exec::buffers::teardown_current_buffer`).  Derive it with
/// [`crate::app::App::view`] rather than testing the individual `Option`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Plain text buffer.  Also the view for a notebook's full-screen
    /// focused-cell overlay, which is edited exactly like a file.
    Text,
    /// Notebook cell-stack view.
    Notebook,
    /// Tabular data grid (CSV/TSV/parquet — see [`crate::table`]).
    Table,
}

impl View {
    /// True when `app.buffer` holds the text this view is showing, so the
    /// editing commands, the LSP and the save paths all mean something.
    ///
    /// `false` means `app.buffer` is a **detached, path-less buffer** and the
    /// view is a window onto data that lives elsewhere.  Such a view must
    /// classify every text command through [`Refusal`] rather than let it run
    /// against an empty buffer.  (`Notebook` is `true`: the focused cell's
    /// source really is in `app.buffer`.)
    pub fn has_text_buffer(self) -> bool {
        match self {
            View::Text | View::Notebook => true,
            View::Table => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Screen geometry
// ---------------------------------------------------------------------------

/// The editor's fixed screen furniture: a content area with a status line and
/// a command line stacked under it.
///
/// Every view gets the same two rows at the bottom, so every view should get
/// them from here.  They used to be hand-built from `size.height - 2` at three
/// call sites in `draw_frame` (plus a `Layout` in `ui::render`), which is four
/// independent chances to be one row off.
#[derive(Debug, Clone, Copy)]
pub struct Chrome {
    /// Where the view draws itself.
    pub content: Rect,
    /// The status line (modeline).
    pub status: Rect,
    /// The command line / minibuffer.
    pub command: Rect,
}

/// Rows the chrome reserves at the bottom of the screen.
pub const CHROME_ROWS: u16 = 2;

impl Chrome {
    /// Split `area` into content + status + command.
    ///
    /// Returns `None` when the terminal is too short to show all three, which
    /// is the caller's signal to draw nothing rather than to draw something
    /// overlapping.
    pub fn split(area: Rect) -> Option<Self> {
        if area.height < CHROME_ROWS + 1 {
            return None;
        }
        let content_height = area.height - CHROME_ROWS;
        Some(Chrome {
            content: Rect { height: content_height, ..area },
            status: Rect {
                x: area.x,
                y: area.y + content_height,
                width: area.width,
                height: 1,
            },
            command: Rect {
                x: area.x,
                y: area.y + content_height + 1,
                width: area.width,
                height: 1,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Why a command doesn't run in a view that has no text buffer behind it.
///
/// A view whose [`View::has_text_buffer`] is false is a window onto data that
/// isn't a rope: a grid over a parquet file, a commit graph.  The editing and
/// text-structure commands still *arrive* — they are bound to the same keys —
/// and the buffer behind the view is empty and path-less, so running them
/// answers nothing at best and writes over the wrong thing at worst.
///
/// Classifying them is how a view says "I heard you, here is why not", instead
/// of appearing to do nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Would edit or save.  The view is a read-only window on its data.
    ReadOnly,
    /// Operates on text structure (words, symbols, folds, the LSP's idea of
    /// the document).  There is no document here to have that structure.
    NeedsText,
    /// Meaningful in this view, just not built yet.
    NotImplemented,
}

impl Refusal {
    /// The message shown in the minibuffer.  `escape` is what the user types
    /// to get to somewhere the command *would* work — the view names it, since
    /// only the view knows (`:table-close` for a grid, and so on).
    pub fn message(self, cmd: &Command, escape: &str) -> String {
        match self {
            Refusal::ReadOnly => format!("This view is read-only ({escape} to edit as text)"),
            Refusal::NeedsText => {
                format!("`{}` needs a text buffer ({escape} to edit as text)", cmd.name())
            }
            Refusal::NotImplemented => {
                format!("`{}` isn't implemented for this view yet", cmd.name())
            }
        }
    }
}

/// Classify a command that reached a view with no text buffer behind it.
///
/// `None` means "not text-specific — let it fall through", which is the right
/// answer for `:q`, the command palette, `:theme`, buffer switching and the
/// display toggles: they mean the same thing in every view.
///
/// Shared across views on purpose.  The set of commands that need a rope is a
/// property of the *commands*, not of the grid that happened to be the first
/// view to have to say so.  A view that wants a different answer for one
/// command intercepts it before asking (the grid reinterprets `MoveUp` as a
/// row move, and only unhandled commands get here).
pub fn refusal(cmd: &Command) -> Option<Refusal> {
    Some(match cmd {
        // --- would write ---
        Command::EnterInsert
        | Command::EnterInsertAfter
        | Command::EnterInsertAtLineStart
        | Command::EnterInsertAtLineEnd
        | Command::DeleteSelection
        | Command::ChangeSelection
        | Command::PasteAfter
        | Command::PasteBefore
        | Command::OpenLineBelow
        | Command::OpenLineAbove
        | Command::Redo
        | Command::CommentRegion
        | Command::IndentRegion
        | Command::DedentRegion
        | Command::KillToEndOfLine
        | Command::Write
        | Command::WriteForce
        | Command::WriteQuit
        | Command::WriteAs(_)
        | Command::FormatDocument => Refusal::ReadOnly,

        // --- LSP: there is no document under the cursor to ask about ---
        Command::LspCodeActions
        | Command::LspGotoDefinition
        | Command::LspGotoReferences
        | Command::LspGotoTypeDefinition
        | Command::LspGotoImplementation
        | Command::LspRequestCompletion
        // --- text structure: characters, words, symbols, folds ---
        | Command::FindCharForward
        | Command::FindCharBackward
        | Command::TillCharForward
        | Command::TillCharBackward
        | Command::EnterJumpMode
        | Command::EnterSelect
        | Command::SelectLine
        | Command::SelectAll
        | Command::OpenSymbolPicker
        | Command::OpenDiagnosticPicker
        | Command::GrepBuffer
        | Command::EnterFoldMode
        | Command::FoldToggle
        | Command::FoldToggleAll
        | Command::ScrollCursorCenter => Refusal::NeedsText,

        // --- real features, not built yet ---
        Command::SearchForward
        | Command::SearchBackward
        | Command::SearchNext
        | Command::SearchPrev => Refusal::NotImplemented,

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_reserves_exactly_two_rows_and_never_overlaps() {
        let area = Rect { x: 0, y: 0, width: 80, height: 24 };
        let c = Chrome::split(area).expect("24 rows is plenty");
        assert_eq!(c.content.height, 22);
        // Status sits directly under the content, command directly under that,
        // and the command line is the last row of the screen.
        assert_eq!(c.status.y, c.content.y + c.content.height);
        assert_eq!(c.command.y, c.status.y + 1);
        assert_eq!(c.command.y + 1, area.y + area.height);
        // Every view gets the full width for all three.
        for r in [c.content, c.status, c.command] {
            assert_eq!(r.width, area.width);
            assert_eq!(r.x, area.x);
        }
    }

    /// A terminal too short for content + both chrome rows draws nothing at
    /// all, rather than a status line painted over the content.
    #[test]
    fn chrome_refuses_to_split_a_screen_that_cannot_hold_it() {
        for h in 0..=CHROME_ROWS {
            let area = Rect { x: 0, y: 0, width: 80, height: h };
            assert!(Chrome::split(area).is_none(), "height {h} should not split");
        }
        assert!(Chrome::split(Rect { x: 0, y: 0, width: 80, height: 3 }).is_some());
    }

    /// The refusal set is about commands, not about one view.  A command that
    /// needs a rope must be refused for *any* bufferless view, so this pins the
    /// classification rather than the grid's use of it.
    #[test]
    fn text_commands_are_refused_and_universal_ones_fall_through() {
        assert_eq!(refusal(&Command::Write), Some(Refusal::ReadOnly));
        assert_eq!(refusal(&Command::LspGotoDefinition), Some(Refusal::NeedsText));
        assert_eq!(refusal(&Command::SearchForward), Some(Refusal::NotImplemented));
        // Commands that mean the same thing everywhere are not view-specific.
        for cmd in [
            Command::Quit,
            Command::OpenCommandPalette,
            Command::BufferNext,
            Command::ToggleWordWrap,
        ] {
            assert_eq!(refusal(&cmd), None, "{} should fall through", cmd.name());
        }
    }

    /// Painting the screen yourself is not the same as having a buffer behind
    /// you: the notebook does both, the grid only the first.  Conflating them
    /// is what let `ga` ask the LSP about an empty document.
    #[test]
    fn only_a_view_with_a_rope_behind_it_takes_the_text_path() {
        assert!(View::Text.has_text_buffer());
        assert!(View::Notebook.has_text_buffer());
        assert!(!View::Table.has_text_buffer());
    }
}
