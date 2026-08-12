//! Table-view state: opening/closing a tabular data source, the background
//! load, cursor movement, and scroll.
//!
//! The table view is **read-only**.  While it is active `app.buffer` is a
//! detached empty buffer with no path, so no save path in the editor can write
//! over the data file, and the write commands are refused explicitly on top of
//! that.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::{
    app::{App, View},
    command::Command,
    table::{self, csv::CsvSource, layout, TableSource},
};

/// An open tabular data source plus where the cursor is in it.
pub struct Session {
    /// The data.  Boxed so a future source (SQL, parquet) needs no changes here.
    pub source: Box<dyn TableSource>,
    pub state: table::TableState,
    /// File the table was opened from — what `:table-close` returns to, and
    /// the name shown in the status line (`app.buffer` has no path here).
    pub path: PathBuf,
}

impl Session {
    /// Name shown in the status line.
    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("table")
            .to_string()
    }

    /// The cursor cell's **untruncated** value — what the grid can only show a
    /// clipped, single-line rendering of.
    pub fn cursor_value(&self) -> Option<&str> {
        self.source
            .cell(self.state.cursor_row, self.state.cursor_col)
    }

    /// Header of the cursor's column.
    pub fn cursor_column_name(&self) -> Option<&str> {
        self.source
            .columns()
            .get(self.state.cursor_col)
            .map(|c| c.name.as_str())
    }

    /// Every value in `row`, in column order (missing cells read as empty).
    fn row_values(&self, row: usize) -> Vec<&str> {
        (0..self.source.columns().len())
            .map(|c| self.source.cell(row, c).unwrap_or(""))
            .collect()
    }
}

/// An in-flight background load, polled once per frame by the run loop.
pub struct TableLoad {
    path: PathBuf,
    rx: Receiver<Result<CsvSource, String>>,
}

impl TableLoad {
    /// File name shown in the status line while the parse is in flight.
    pub fn display_name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("table")
    }
}

/// True when `path` is a delimited-text file the table view can open.
pub fn is_table_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("csv" | "tsv" | "tab")
    )
}

/// Open `path` in the table view, replacing whatever is currently open.
///
/// The parse runs on a background thread: a large CSV must not stall a frame,
/// and there is no upper bound on what someone will try to open.  Until it
/// lands the editor shows an empty buffer with a "Loading…" message and the
/// status spinner running.
pub fn open_as_table(app: &mut App, path: &Path) {
    super::save_current_special_buffer(app);
    super::teardown_current_buffer(app);
    app.table = None;
    app.table_cell_origin = None;

    // Detached buffer: the table view renders from the source, and nothing that
    // could write `app.buffer` may be pointed at the data file.
    app.buffer = crate::buffer::Buffer::new_empty();
    app.selection = crate::selection::Selection::point(0);
    app.scroll_row = 0;
    app.scroll_col = 0;
    app.insert_session_active = false;
    app.lsp_language = None;
    app.highlighter = crate::highlight::Highlighter::new(None);
    app.mode = crate::mode::Mode::Normal;
    app.git_diff.clear();
    super::recompute_highlights(app);
    super::rebuild_diag_cache(app);

    super::register_buffer(&mut app.open_buffers, path);

    // A session stashed on the way out comes back whole — same parse, same
    // cursor cell.  This is what makes `Enter` into a cell buffer and back a
    // round trip rather than a reload.
    if let Some(session) = app.table_buffers.remove(&super::canon(path)) {
        app.table = Some(session);
        return;
    }
    start_load(app, path);
}

/// `:csv` — open the current buffer's file in the table view.
pub(super) fn open_current_as_table(app: &mut App) {
    if app.table.is_some() {
        app.messages.show("Already in the table view");
        return;
    }
    let Some(path) = app.buffer.path.clone().filter(|p| !super::is_special_path(p)) else {
        app.messages.show("No file to open as a table");
        return;
    };
    if app.buffer.modified {
        // The load reads from disk, so it would silently ignore the edits.
        app.messages
            .show("Save the buffer first — the table view reads from disk");
        return;
    }
    open_as_table(app, &path);
}

/// `:table-close` — leave the grid and open the same file as text.
pub(super) fn close_table(app: &mut App) {
    let Some(session) = app.table.take() else {
        app.messages.show("No table open");
        return;
    };
    let path = session.path.clone();
    drop(session);
    // Deliberate exit from the grid: drop the stash too, so a later `:csv`
    // re-reads the file (which the user may have just edited as text) rather
    // than resurrecting the parse from before.
    app.table_buffers.remove(&super::canon(&path));
    super::lsp::open_file_at(app, &path, 0, 0);
}

// ---------------------------------------------------------------------------
// Reading a cell
// ---------------------------------------------------------------------------

/// Where an open `*cell …*` buffer came from, and what to undo when leaving it.
pub struct CellOrigin {
    /// The table the cell was read out of — what `:bd` returns to.
    pub path: PathBuf,
    /// `editor.word_wrap` as it was before the cell buffer forced it on.
    prev_word_wrap: bool,
}

/// Called when leaving a cell buffer (from `teardown_current_buffer`, so every
/// exit path is covered): put the word-wrap setting back as it was.
pub(super) fn leave_cell_buffer(app: &mut App) {
    if let Some(origin) = app.table_cell_origin.take() {
        app.config.editor.word_wrap = origin.prev_word_wrap;
    }
}

/// Name of the virtual buffer a cell's text is read in.  The `*…*` form marks
/// it special (see [`super::is_special_path`]) so nothing tries to save it.
fn cell_buffer_name(session: &Session) -> String {
    let col = session.cursor_column_name().unwrap_or("?");
    format!("*cell {}:{}*", session.state.cursor_row + 1, col)
}

/// `Enter` — open the cursor cell's full text in its own buffer.
///
/// This is the point of the whole view: the grid deliberately shows one
/// truncated line per cell, and a paragraph-length value is only readable once
/// it is text in a buffer, with wrapping, motions and search.  The table is
/// stashed on the way out, so returning lands on the same cell.
pub(super) fn open_cell_buffer(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        app.messages.show("No table open");
        return;
    };
    let Some(value) = session.cursor_value() else {
        app.messages.show("No cell here");
        return;
    };
    if value.is_empty() {
        app.messages.show("Cell is empty");
        return;
    }

    let name = cell_buffer_name(session);
    let origin = session.path.clone();
    let short = session.display_name();
    let mut text = value.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }

    // One cell buffer at a time: without this every cell ever read would keep
    // its rope alive for the rest of the session.
    app.special_buffer_ropes.retain(|k, _| !k.starts_with("*cell "));
    app.special_buffer_ropes
        .insert(name.clone(), ropey::Rope::from_str(&text));

    super::switch_to_special_buffer(app, &name);
    // The value is prose far more often than it is code, and it is the long
    // ones the user came here to read — so wrap, and put the setting back when
    // the buffer is left (`leave_cell_buffer`).
    app.table_cell_origin = Some(CellOrigin {
        path: origin,
        prev_word_wrap: app.config.editor.word_wrap,
    });
    app.config.editor.word_wrap = true;
    app.messages
        .show(format!("Reading cell — :bd returns to {short}"));
}

/// `:bd` in a `*cell …*` buffer — go back to the table it came from.
/// Returns false when the current buffer is not a cell buffer.
pub(super) fn close_cell_buffer(app: &mut App) -> bool {
    let Some(origin) = app.table_cell_origin.as_ref().map(|o| o.path.clone()) else {
        return false;
    };
    let name = app
        .buffer
        .path
        .as_ref()
        .and_then(|p| p.to_str())
        .unwrap_or_default()
        .to_string();
    if !name.starts_with("*cell ") {
        return false;
    }
    app.special_buffer_ropes.remove(&name);
    open_as_table(app, &origin);
    true
}

/// `K` / `gk` — peek the cursor cell's full text without leaving the grid.
///
/// Same question as `Enter` answers, asked without giving up your place: the
/// float is read-only and scrollable, and any key dismisses it.
pub(super) fn peek_cell(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        app.messages.show("No table open");
        return;
    };
    let Some(value) = session.cursor_value() else {
        app.messages.show("No cell here");
        return;
    };
    if value.is_empty() {
        app.messages.show("Cell is empty");
        return;
    }

    let title = format!(
        " {} · row {} ",
        session.cursor_column_name().unwrap_or("cell"),
        session.state.cursor_row + 1
    );
    // Wrap to the float's inner width: the text popup clips rather than wraps,
    // and a cell worth peeking at is exactly the one that would be clipped.
    let inner = popup_text_width(app.viewport_width);
    let wrapped: Vec<String> = value
        .lines()
        .flat_map(|line| {
            crate::notebook_ui::wrap_segments(line, inner)
                .into_iter()
                .map(|(_, seg)| seg.to_string())
        })
        .collect();
    app.popup = Some(crate::popup::Popup::documentation(&title, &wrapped.join("\n")));
}

/// Inner text width of a `FractionOfScreen(0.6)` centered float, mirroring
/// `popup_ui::compute_width` (0.6 of the terminal, at least 20) minus borders.
fn popup_text_width(viewport_width: usize) -> usize {
    let w = ((viewport_width as f32 * 0.6) as usize).max(20);
    w.saturating_sub(2).max(1)
}

/// `y` — copy the cursor cell's full value to the system clipboard.
pub(super) fn yank_cell(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        return;
    };
    let Some(value) = session.cursor_value().map(str::to_string) else {
        app.messages.show("No cell here");
        return;
    };
    let n = value.chars().count();
    crate::clipboard::write(&value);
    app.messages.show(format!("Yanked cell ({n} chars)"));
}

/// One row rendered as a tab-separated line.  Values are flattened
/// ([`layout::sanitize`]) so an embedded newline can't split one row into two.
fn row_tsv(session: &Session, row: usize) -> String {
    session
        .row_values(row)
        .iter()
        .map(|v| layout::sanitize(v))
        .collect::<Vec<_>>()
        .join("\t")
}

/// `x` — copy the cursor row to the clipboard as a tab-separated line.
///
/// TSV rather than the file's own delimiter: it is what spreadsheets and chat
/// windows paste correctly, and it needs no quoting rules for the commas that
/// are already inside the values.
pub(super) fn yank_row(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        return;
    };
    let row = session.state.cursor_row;
    if row >= session.source.loaded_rows() {
        app.messages.show("No row here");
        return;
    }
    let line = row_tsv(session, row);
    let cols = session.source.columns().len();
    crate::clipboard::write(&line);
    app.messages
        .show(format!("Yanked row {} ({cols} columns)", row + 1));
}

/// Spawn the background parse for `path`.
fn start_load(app: &mut App, path: &Path) {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let (tx, rx) = mpsc::channel();
    let owned = path.to_path_buf();
    let cfg = app.config.table.clone();
    let thread_path = owned.clone();
    std::thread::spawn(move || {
        let result = CsvSource::load(&thread_path, &cfg).map_err(|e| e.to_string());
        let _ = tx.send(result);
    });
    app.table_pending = Some(TableLoad { path: owned, rx });
    app.messages.show(format!("Loading {name}…"));
}

/// Install a finished load, if one is ready.  Returns true when state changed.
pub fn poll_table_load(app: &mut App) -> bool {
    let Some(job) = &app.table_pending else {
        return false;
    };
    let result = match job.rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("load worker died".to_string()),
    };
    let path = job.path.clone();
    app.table_pending = None;

    match result {
        Ok(source) => {
            let truncated = source.truncated();
            let rows = source.loaded_rows();
            let cols = source.columns().len();
            let shape = source.describe();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("table")
                .to_string();
            app.table = Some(Session {
                source: Box::new(source),
                state: table::TableState::new(),
                path,
            });
            if cols == 0 {
                app.messages
                    .show(format!("{name} has no columns — nothing to show"));
            } else if truncated {
                app.messages.show(format!(
                    "{name}: {shape} — stopped at table.max_rows ({rows} rows)"
                ));
            } else {
                app.messages.show(format!("{name}: {shape}"));
            }
        }
        Err(e) => {
            app.messages.show(format!("Failed to load table: {e}"));
            // Fall back to the text view of the same file rather than leaving
            // the user in a blank detached buffer with no way back.
            super::lsp::open_file_at(app, &path, 0, 0);
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Command routing
// ---------------------------------------------------------------------------

/// Handle `cmd` in the table view.  Returns true when it was consumed, so
/// `execute()` skips the text-buffer path entirely; anything not listed here
/// (`:q`, the command palette, `:theme`, buffer switching, …) falls through and
/// behaves exactly as it does elsewhere.
pub(super) fn handle(app: &mut App, cmd: &Command) -> bool {
    debug_assert_eq!(app.view(), View::Table);
    let rows = app.table.as_ref().map_or(0, |s| s.source.loaded_rows());
    let cols = app.table.as_ref().map_or(0, |s| s.source.columns().len());
    let page = (visible_rows(app) / 2).max(1);

    let moved = |app: &mut App, f: &dyn Fn(&mut table::TableState)| {
        if let Some(session) = app.table.as_mut() {
            f(&mut session.state);
            session.state.clamp(rows, cols);
        }
    };

    match cmd {
        Command::MoveLeft => moved(app, &|s| s.cursor_col = s.cursor_col.saturating_sub(1)),
        Command::MoveRight => moved(app, &|s| s.cursor_col += 1),
        Command::MoveUp => moved(app, &|s| s.cursor_row = s.cursor_row.saturating_sub(1)),
        Command::MoveDown => moved(app, &|s| s.cursor_row += 1),

        // Word motions step a column at a time — the column is the table's
        // unit of horizontal structure, so `w`/`b` mean what they always mean.
        Command::MoveWordForward | Command::MoveWordEnd | Command::MoveBigWordForward
        | Command::MoveBigWordEnd => moved(app, &|s| s.cursor_col += 1),
        Command::MoveWordBackward | Command::MoveBigWordBackward => {
            moved(app, &|s| s.cursor_col = s.cursor_col.saturating_sub(1))
        }

        Command::MoveLineStart | Command::MoveLineFirstNonWs => moved(app, &|s| s.cursor_col = 0),
        Command::MoveLineEnd => moved(app, &|s| s.cursor_col = cols.saturating_sub(1)),
        Command::GotoFileStart => moved(app, &|s| s.cursor_row = 0),
        Command::GotoFileEnd => moved(app, &|s| s.cursor_row = rows.saturating_sub(1)),
        Command::PageDown => moved(app, &|s| s.cursor_row += page),
        Command::PageUp => moved(app, &|s| s.cursor_row = s.cursor_row.saturating_sub(page)),

        Command::TableClose => {
            close_table(app);
            return true;
        }
        Command::OpenAsTable => {
            app.messages.show("Already in the table view");
            return true;
        }

        // --- reading a cell ---
        Command::TableOpenCell => {
            open_cell_buffer(app);
            return true;
        }
        // `K` / `gk` mean "tell me more about the thing under the cursor"
        // everywhere in the editor; in a grid that is the cell's full text.
        Command::TablePeekCell | Command::LspShowDocumentation => {
            peek_cell(app);
            return true;
        }
        Command::TableYankCell => {
            yank_cell(app);
            return true;
        }
        Command::TableYankRow => {
            yank_row(app);
            return true;
        }
        Command::TableCloseCell => {
            app.messages.show("No cell buffer open");
            return true;
        }

        // Read-only: refuse anything that would edit or write, rather than
        // letting it operate on the detached buffer behind the grid.
        _ if is_text_mutation(cmd) => {
            app.messages
                .show("The table view is read-only (:table-close to edit as text)");
            return true;
        }
        _ => return false,
    }

    update_scroll(app);
    true
}

/// Commands that edit or save the text buffer — meaningless in the table view,
/// and refused there so they can't quietly act on the detached buffer.
fn is_text_mutation(cmd: &Command) -> bool {
    matches!(
        cmd,
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
            | Command::Undo
            | Command::Redo
            | Command::CommentRegion
            | Command::IndentRegion
            | Command::DedentRegion
            | Command::KillToEndOfLine
            | Command::Write
            | Command::WriteForce
            | Command::WriteQuit
            | Command::FormatDocument
    )
}

/// Data rows that fit on screen (the header takes one row of the grid area).
fn visible_rows(app: &App) -> usize {
    layout::visible_rows(app.viewport_height as u16)
}

/// Keep the cursor cell on screen.  The column half delegates to
/// [`layout::scroll_col_for_cursor`] so the scroll and the renderer share one
/// definition of "fits".
pub(super) fn update_scroll(app: &mut App) {
    let rows_visible = visible_rows(app);
    let scroll_off = app.config.editor.scroll_off;
    let width = app.viewport_width as u16;
    let cfg = app.config.table.clone();

    let Some(session) = app.table.as_mut() else {
        return;
    };
    let total_rows = session.source.loaded_rows();
    session
        .state
        .clamp(total_rows, session.source.columns().len());

    // --- rows ---
    if rows_visible > 0 {
        let cursor = session.state.cursor_row;
        // Margin, shrunk so it can't exceed half the viewport on a short screen.
        let margin = scroll_off.min(rows_visible.saturating_sub(1) / 2);
        let top = session.state.scroll_row;
        if cursor < top + margin {
            session.state.scroll_row = cursor.saturating_sub(margin);
        } else if cursor + margin >= top + rows_visible {
            session.state.scroll_row = (cursor + margin + 1).saturating_sub(rows_visible);
        }
        // Never scroll past the end while rows remain to fill the screen.
        let max_top = total_rows.saturating_sub(rows_visible);
        session.state.scroll_row = session.state.scroll_row.min(max_top);
    }

    // --- columns ---
    session.state.scroll_col = layout::scroll_col_for_cursor(
        session.source.as_ref(),
        session.state.scroll_col,
        session.state.cursor_col,
        width,
        &cfg,
    );

    // Make the visible window available to the source (a no-op for CSV, the
    // fetch trigger for a windowed source later).
    let first = session.state.scroll_row;
    let last = (first + rows_visible.max(1)).min(total_rows);
    session.source.ensure_rows(first..last);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TableConfig;

    fn source(rows: usize, cols: usize) -> CsvSource {
        let header: Vec<String> = (0..cols).map(|c| format!("col{c}")).collect();
        let mut text = header.join(",");
        text.push('\n');
        for r in 0..rows {
            let row: Vec<String> = (0..cols).map(|c| format!("{r}-{c}")).collect();
            text.push_str(&row.join(","));
            text.push('\n');
        }
        CsvSource::from_reader(text.as_bytes(), b',', &TableConfig::default()).unwrap()
    }

    /// An app in the table view over a `rows × cols` table.
    fn app_with_table(rows: usize, cols: usize) -> App {
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.viewport_height = 11; // 1 header + 10 data rows
        app.viewport_width = 80;
        // Pin the settings the geometry depends on — `Config::load()` picks up
        // the developer's own config file.
        app.config.editor.scroll_off = 2;
        app.config.table = TableConfig::default();
        app.table = Some(Session {
            source: Box::new(source(rows, cols)),
            state: table::TableState::new(),
            path: PathBuf::from("t.csv"),
        });
        app
    }

    fn cursor(app: &App) -> (usize, usize) {
        let s = &app.table.as_ref().unwrap().state;
        (s.cursor_row, s.cursor_col)
    }

    /// End-to-end: a real file goes through the background load, the run loop's
    /// poll, the table view, and back out to text — the wiring the pure-function
    /// tests above can't reach.
    #[test]
    fn opening_a_file_loads_asynchronously_and_closing_returns_to_text() {
        let dir = std::env::temp_dir().join(format!("sv-table-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "a,b\n1,x\n2,y\n").unwrap();

        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.config.table = TableConfig::default();
        open_as_table(&mut app, &path);

        // The load is off-thread: the view is still Text and the buffer is
        // detached, so nothing here can write over the data file.
        assert_eq!(app.view(), View::Text);
        assert!(app.table_pending.is_some());
        assert!(app.buffer.path.is_none());

        // Poll like the run loop does until it lands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.table.is_none() && std::time::Instant::now() < deadline {
            poll_table_load(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let session = app.table.as_ref().expect("table loaded");
        assert_eq!(session.source.loaded_rows(), 2);
        assert_eq!(session.source.cell(1, 1), Some("y"));
        assert_eq!(session.display_name(), "data.csv");
        assert_eq!(app.view(), View::Table);
        assert!(app.table_pending.is_none());

        // `:table-close` gives back an editable text buffer on the same file.
        close_table(&mut app);
        assert!(app.table.is_none());
        assert_eq!(app.view(), View::Text);
        assert_eq!(app.buffer.path.as_deref(), Some(path.as_path()));
        assert!(app.buffer.rope.to_string().starts_with("a,b"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The external file picker (yazi/fzf) opens a file without going through a
    /// popup confirm, so `open_path` is the only thing that can retire the
    /// dashboard — otherwise it stays painted over the opened file until the
    /// next keypress.  Also covers the load being visibly in progress.
    #[test]
    fn opening_from_the_dashboard_retires_the_splash_and_names_the_load() {
        let dir = std::env::temp_dir().join(format!("sv-splash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "a,b\n1,x\n").unwrap();

        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.config.table = TableConfig::default();
        app.show_splash = true;

        crate::exec::open_path(&mut app, &path);
        assert!(!app.show_splash, "the dashboard must not stay over the file");

        // While the parse is in flight the status line names it and the spinner
        // has something to be active about.
        assert_eq!(app.table_load_name(), Some("data.csv"));
        assert!(app.table_pending.is_some());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.table.is_none() && std::time::Instant::now() < deadline {
            poll_table_load(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.view(), View::Table);
        assert_eq!(app.table_load_name(), None, "cleared once the load lands");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_reports_the_error_and_does_not_strand_the_view() {
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.config.table = TableConfig::default();
        let path = std::env::temp_dir().join("sv-table-does-not-exist.csv");
        let _ = std::fs::remove_file(&path);
        open_as_table(&mut app, &path);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.table_pending.is_some() && std::time::Instant::now() < deadline {
            poll_table_load(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.table.is_none());
        assert_eq!(app.view(), View::Text);
        assert!(app
            .messages
            .log
            .iter()
            .any(|m| m.contains("Failed to load table")));
    }

    #[test]
    fn hjkl_moves_one_cell_and_stops_at_the_edges() {
        let mut app = app_with_table(3, 3);
        handle(&mut app, &Command::MoveDown);
        handle(&mut app, &Command::MoveRight);
        assert_eq!(cursor(&app), (1, 1));

        // Clamped at the far edges rather than wrapping or overflowing.
        for _ in 0..10 {
            handle(&mut app, &Command::MoveDown);
            handle(&mut app, &Command::MoveRight);
        }
        assert_eq!(cursor(&app), (2, 2));
        for _ in 0..10 {
            handle(&mut app, &Command::MoveUp);
            handle(&mut app, &Command::MoveLeft);
        }
        assert_eq!(cursor(&app), (0, 0));
    }

    #[test]
    fn line_and_file_motions_address_columns_and_rows() {
        let mut app = app_with_table(50, 4);
        handle(&mut app, &Command::MoveLineEnd);
        assert_eq!(cursor(&app), (0, 3), "$ goes to the last column");
        handle(&mut app, &Command::GotoFileEnd);
        assert_eq!(cursor(&app), (49, 3), "G goes to the last row, keeping the column");
        handle(&mut app, &Command::MoveLineStart);
        assert_eq!(cursor(&app), (49, 0));
        handle(&mut app, &Command::GotoFileStart);
        assert_eq!(cursor(&app), (0, 0));
    }

    /// Driven through `input::handle_key`, not `handle` directly: the goto
    /// sub-mode used to call `motion::*` on the buffer itself, so `gg`/`ge`/
    /// `gh`/`gl` never reached the table router and did nothing at all.
    #[test]
    fn goto_submode_keys_move_the_grid_cursor() {
        use crossterm::event::{KeyCode, KeyEvent};

        let mut app = app_with_table(50, 4);
        let press = |app: &mut App, c: char| {
            crate::input::handle_key(app, KeyEvent::from(KeyCode::Char(c)));
        };

        press(&mut app, 'g');
        press(&mut app, 'e');
        assert_eq!(cursor(&app), (49, 0), "ge → last row");

        press(&mut app, 'g');
        press(&mut app, 'l');
        assert_eq!(cursor(&app), (49, 3), "gl → last column");

        press(&mut app, 'g');
        press(&mut app, 'h');
        assert_eq!(cursor(&app), (49, 0), "gh → first column");

        press(&mut app, 'g');
        press(&mut app, 'g');
        assert_eq!(cursor(&app), (0, 0), "gg → first row");
    }

    #[test]
    fn word_motions_step_by_column() {
        let mut app = app_with_table(3, 5);
        handle(&mut app, &Command::MoveWordForward);
        handle(&mut app, &Command::MoveWordForward);
        assert_eq!(cursor(&app), (0, 2));
        handle(&mut app, &Command::MoveWordBackward);
        assert_eq!(cursor(&app), (0, 1));
    }

    #[test]
    fn paging_moves_half_a_screen_of_rows() {
        let mut app = app_with_table(100, 2);
        handle(&mut app, &Command::PageDown);
        assert_eq!(cursor(&app).0, 5, "half of the 10 visible data rows");
        handle(&mut app, &Command::PageUp);
        assert_eq!(cursor(&app).0, 0);
    }

    #[test]
    fn scroll_follows_the_cursor_with_a_margin() {
        let mut app = app_with_table(100, 2);
        // Walking down: the top only moves once the cursor reaches the margin.
        for _ in 0..7 {
            handle(&mut app, &Command::MoveDown);
        }
        let scroll = app.table.as_ref().unwrap().state.scroll_row;
        assert_eq!(scroll, 0, "cursor at row 7 of 10 visible is still inside");
        handle(&mut app, &Command::MoveDown);
        assert_eq!(app.table.as_ref().unwrap().state.scroll_row, 1);

        // The cursor is always within the visible window.
        handle(&mut app, &Command::GotoFileEnd);
        let s = &app.table.as_ref().unwrap().state;
        assert!(s.cursor_row >= s.scroll_row);
        assert!(s.cursor_row < s.scroll_row + 10);
    }

    #[test]
    fn scroll_does_not_run_past_the_last_screenful() {
        let mut app = app_with_table(100, 2);
        handle(&mut app, &Command::GotoFileEnd);
        assert_eq!(
            app.table.as_ref().unwrap().state.scroll_row,
            90,
            "the last screen is full, not one row of data under the header"
        );
    }

    #[test]
    fn a_table_shorter_than_the_screen_never_scrolls() {
        let mut app = app_with_table(4, 2);
        handle(&mut app, &Command::GotoFileEnd);
        assert_eq!(app.table.as_ref().unwrap().state.scroll_row, 0);
    }

    #[test]
    fn edits_and_writes_are_refused_not_applied_to_the_hidden_buffer() {
        let mut app = app_with_table(3, 2);
        for cmd in [
            Command::EnterInsert,
            Command::DeleteSelection,
            Command::PasteAfter,
            Command::Write,
            Command::WriteQuit,
        ] {
            assert!(handle(&mut app, &cmd), "{} should be consumed", cmd.name());
            assert_eq!(app.mode, crate::mode::Mode::Normal, "no mode change");
            assert!(app.buffer.rope.len_chars() == 0, "buffer untouched");
            assert!(!app.buffer.modified);
            assert!(app.messages.current().is_some_and(|m| m.contains("read-only")));
        }
    }

    // -----------------------------------------------------------------------
    // Reading a cell (the point of the view)
    // -----------------------------------------------------------------------

    /// A table whose one long cell is exactly what the grid cannot show.
    fn app_with_long_cell() -> (App, String) {
        let long: String = "lorem ipsum dolor ".repeat(30).trim_end().to_string();
        let text = format!("id,notes\n1,short\n2,\"{long}\"\n");
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.viewport_height = 11;
        app.viewport_width = 80;
        app.config.editor.scroll_off = 2;
        app.config.editor.word_wrap = false;
        app.config.table = TableConfig::default();
        app.table = Some(Session {
            source: Box::new(
                CsvSource::from_reader(text.as_bytes(), b',', &TableConfig::default()).unwrap(),
            ),
            state: table::TableState::new(),
            path: PathBuf::from("notes.csv"),
        });
        // Cursor on the long value.
        handle(&mut app, &Command::MoveDown);
        handle(&mut app, &Command::MoveRight);
        (app, long)
    }

    #[test]
    fn enter_opens_the_full_cell_text_in_its_own_buffer() {
        let (mut app, long) = app_with_long_cell();
        assert!(handle(&mut app, &Command::TableOpenCell));

        // The buffer holds the value in full — untruncated, unlike the grid.
        assert_eq!(app.buffer.rope.to_string().trim_end(), long);
        assert_eq!(
            app.buffer.path.as_ref().unwrap().to_str(),
            Some("*cell 2:notes*"),
            "named for the cell it came from"
        );
        assert_eq!(app.view(), View::Text, "the grid gives up the screen");
        // Nothing here can write the data file: a virtual buffer, no path.
        assert!(crate::exec::is_special_path(app.buffer.path.as_ref().unwrap()));
        assert!(app.config.editor.word_wrap, "wrapped, so it is readable");
    }

    #[test]
    fn returning_from_a_cell_buffer_lands_on_the_same_cell() {
        let (mut app, _) = app_with_long_cell();
        handle(&mut app, &Command::TableOpenCell);
        // The parsed table is stashed, not dropped.
        assert_eq!(app.table_buffers.len(), 1);

        assert!(close_cell_buffer(&mut app));
        assert_eq!(app.view(), View::Table, "back in the grid");
        assert_eq!(cursor(&app), (1, 1), "on the cell we were reading");
        assert!(
            app.table_pending.is_none(),
            "restored from the stash, not re-parsed from disk"
        );
        assert!(!app.config.editor.word_wrap, "wrap setting put back");
        assert!(app.table_cell_origin.is_none());
    }

    /// `:bd` is the natural "close this" for the cell buffer, and it must not
    /// hit the "cannot close special buffer" refusal.
    #[test]
    fn bd_in_a_cell_buffer_returns_to_the_table() {
        let (mut app, _) = app_with_long_cell();
        handle(&mut app, &Command::TableOpenCell);
        crate::exec::execute(&mut app, &Command::BufferClose);
        assert_eq!(app.view(), View::Table);
        assert_eq!(cursor(&app), (1, 1));
    }

    #[test]
    fn peek_shows_the_whole_value_wrapped_without_leaving_the_grid() {
        let (mut app, long) = app_with_long_cell();
        assert!(handle(&mut app, &Command::TablePeekCell));
        assert_eq!(app.view(), View::Table, "still in the grid");

        let popup = app.popup.as_ref().expect("peek float");
        let crate::popup::PopupContent::Text(state) = &popup.content else {
            panic!("peek should be a text float");
        };
        // Wrapped to the float, and complete: the rejoined rows are the value.
        let width = popup_text_width(app.viewport_width);
        assert!(state.lines.len() > 1, "a long value wraps to many rows");
        assert!(state.lines.iter().all(|l| l.chars().count() <= width));
        assert_eq!(state.lines.join(" "), long);
    }

    /// `K` and `gk` mean "tell me more about this" everywhere else, so they
    /// must peek here rather than firing an LSP request against the empty
    /// buffer behind the grid.
    #[test]
    fn k_peeks_instead_of_asking_the_lsp() {
        let (mut app, _) = app_with_long_cell();
        assert!(handle(&mut app, &Command::LspShowDocumentation));
        assert!(app.popup.is_some());
    }

    #[test]
    fn empty_and_missing_cells_say_so_instead_of_opening_a_blank_buffer() {
        let mut app = app_with_table(3, 3);
        // Blank the cell under the cursor by pointing at a column past the data.
        let source =
            CsvSource::from_reader("a,b\n1,\n".as_bytes(), b',', &TableConfig::default()).unwrap();
        app.table.as_mut().unwrap().source = Box::new(source);
        handle(&mut app, &Command::MoveRight);

        handle(&mut app, &Command::TableOpenCell);
        assert_eq!(app.view(), View::Table, "no buffer opened");
        assert!(app.messages.current().is_some_and(|m| m.contains("empty")));

        handle(&mut app, &Command::TablePeekCell);
        assert!(app.popup.is_none());
    }

    /// The yank commands themselves are not driven here: `clipboard::write`
    /// shells out to the real system clipboard, which a test must neither
    /// depend on nor clobber.  What they copy is this.
    #[test]
    fn yank_text_is_the_full_cell_and_the_row_as_tsv() {
        let (app, long) = app_with_long_cell();
        let session = app.table.as_ref().unwrap();

        assert_eq!(session.cursor_value(), Some(long.as_str()), "untruncated");
        assert_eq!(
            row_tsv(session, 1),
            format!("2\t{long}"),
            "every column of the row, tab-separated"
        );
    }

    /// An embedded newline must not turn one copied row into two lines.
    #[test]
    fn a_multiline_value_stays_on_one_line_when_the_row_is_yanked() {
        let mut app = app_with_table(1, 1);
        let source = CsvSource::from_reader(
            "a,b\n\"one\ntwo\",x\n".as_bytes(),
            b',',
            &TableConfig::default(),
        )
        .unwrap();
        app.table.as_mut().unwrap().source = Box::new(source);
        let line = row_tsv(app.table.as_ref().unwrap(), 0);
        assert!(!line.contains('\n'));
        assert_eq!(line, "one↵two\tx");
    }

    #[test]
    fn unrelated_commands_fall_through_to_the_normal_path() {
        let mut app = app_with_table(3, 2);
        assert!(!handle(&mut app, &Command::Quit));
        assert!(!handle(&mut app, &Command::OpenCommandPalette));
        assert!(!handle(&mut app, &Command::EnterCommandMode));
    }
}
