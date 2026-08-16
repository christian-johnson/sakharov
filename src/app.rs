use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::panic;

use crate::{
    buffer::Buffer,
    config::Config,
    fold::FoldState,
    git::GutterMark,
    highlight::{Highlighter, Span},
    input,
    keymap::Keymap,
    kitty::{self, ImageRequest},
    lsp_manager::{DiagnosticSeverity, LspManager},
    mode::Mode,
    notebook::Notebook,
    notebook_state::NotebookState,
    selection::Selection,
    ui,
};

// ---------------------------------------------------------------------------
// Termination-signal handling
// ---------------------------------------------------------------------------

/// Set by the signal handler to the number of a received catchable termination
/// signal (SIGTERM/SIGHUP/SIGINT), or 0 when none.  The run loop polls this and
/// shuts down gracefully — restoring the terminal and flushing recovery — which
/// the process otherwise can't do for these signals (and can never do for the
/// uncatchable SIGKILL).
static PENDING_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Set once we've pushed the kitty keyboard-enhancement flags, so the matching
/// pop happens exactly once on teardown (and only when we actually pushed).
static KEYBOARD_ENHANCED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Signal handler: async-signal-safe — performs only an atomic store.
#[cfg(unix)]
extern "C" fn handle_term_signal(sig: libc::c_int) {
    PENDING_SIGNAL.store(sig, std::sync::atomic::Ordering::SeqCst);
}

/// Install handlers for the catchable termination signals.  SIGKILL and SIGSTOP
/// cannot be caught and are intentionally not listed.
#[cfg(unix)]
fn install_signal_handlers() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction =
            handle_term_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

/// Path of the key-event debug log (used only when `SV_DEBUG_KEYS` is set).
fn key_debug_log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("sv-keys.log")
}

/// Append a received key event to the debug log (best-effort).
fn log_key_event(key: &crossterm::event::KeyEvent) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(key_debug_log_path())
    {
        let _ = writeln!(f, "{key:?}");
    }
}

/// The pending termination signal, if one has been received.
fn pending_signal() -> Option<i32> {
    match PENDING_SIGNAL.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        s => Some(s),
    }
}

/// True when stdin's controlling terminal has hung up (`POLLHUP`) — the
/// slave pty is still open but its master side has closed. A process
/// orphaned from its terminal session (e.g. reparented to init/launchd
/// before the terminal closed) never receives `SIGHUP` for this, and
/// crossterm's own `event::poll` can retry-loop forever *inside a single
/// call* reading the resulting stream of 0-byte reads — pinning a CPU core
/// indefinitely without ever returning to the caller. Checked with our own
/// zero-timeout `poll(2)` so we can detect it before ever making the call
/// that gets stuck.
#[cfg(unix)]
fn stdin_hung_up() -> bool {
    let mut pfd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
    ready > 0 && pfd.revents & libc::POLLHUP != 0
}

#[cfg(not(unix))]
fn stdin_hung_up() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Search state
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub matches: Vec<usize>,
    pub current: usize,
    pub active: bool,
    /// True when search was just opened — allows the first typed char to
    /// replace the previous query instead of appending to it.
    pub just_opened: bool,
}


// ---------------------------------------------------------------------------
// Grouped sub-state
// ---------------------------------------------------------------------------

/// Name of the SQL query buffer.  A `*…*` name, so nothing tries to save it.
pub const SQL_BUFFER: &str = "*sql*";

/// Terminal-graphics (Kitty/WezTerm) image state.
pub struct GraphicsState {
    /// Which terminal graphics backend is available (Kitty, WezTerm, or none).
    /// Detected once at startup from environment variables.
    pub terminal: kitty::GraphicsTerminal,
    /// Image draw requests collected by the last render pass.  Any view may
    /// fill this — `flush_images` places whatever is here after the draw, so a
    /// renderer that wants a raster does not have to know how images reach the
    /// terminal.
    pub pending: Vec<ImageRequest>,
    /// Whether the last flush left placements on screen.  Drives the clear
    /// pass: without it, images from a view we have since left would stay
    /// painted over the new one.
    pub placed: bool,
    /// Maps Arc-pointer-as-usize → Kitty image ID so pixel data is uploaded
    /// only once per image.  Must be cleared whenever outputs change or the
    /// terminal is resized (Kitty evicts pixel cache on resize).
    pub image_ids: std::collections::HashMap<usize, u32>,
    /// Counter for assigning unique Kitty image IDs (wraps at u32::MAX).
    pub next_id: u32,
    /// Terminal size at the last frame images were uploaded.  Used to detect
    /// resizes that invalidate Kitty's pixel cache.
    pub last_size: (u16, u16),
    /// Actual terminal cell pixel dimensions `(cell_h_px, cell_w_px)` queried
    /// from the OS via TIOCGWINSZ.  Used to size image placeholders precisely
    /// so they match what Kitty renders.  `None` until first successful query.
    pub cell_pixel_size: Option<(u16, u16)>,
}

impl Default for GraphicsState {
    fn default() -> Self {
        Self {
            terminal: kitty::GraphicsTerminal::detect(),
            pending: Vec::new(),
            placed: false,
            image_ids: std::collections::HashMap::new(),
            next_id: 1,
            last_size: (0, 0),
            cell_pixel_size: None,
        }
    }
}

/// LSP completion-popup bookkeeping.
#[derive(Default)]
pub struct CompletionState {
    /// Word prefix at which the last completion popup was dismissed due to no
    /// matches.  While the current prefix extends this value, we skip firing new
    /// completion requests — typing more characters can only reduce results further.
    /// Cleared on Backspace, non-identifier chars, and trigger chars (`.` / `:`).
    pub suppressed_prefix: Option<String>,
    /// Absolute `items` index of the completion item awaiting a
    /// `completionItem/resolve` reply (for the `K` doc panel). At most one
    /// resolve is in flight; the reply fills this item's documentation.
    pub pending_resolve: Option<usize>,
}

/// `gw` label-jump transient state.
#[derive(Default)]
pub struct JumpState {
    /// (char_pos, label) pairs computed when entering Jump mode.
    pub labels: Vec<(usize, String)>,
    /// Characters typed so far in Jump mode (used to filter labels).
    pub typed: String,
}

/// The transient minibuffer message plus the persistent message log that
/// powers the *Messages* special buffer.  `show` records to both, so the log
/// is complete by construction (no frame-diffing needed).
#[derive(Default)]
pub struct Messages {
    current: Option<String>,
    /// Chronological log of every message shown in the minibuffer.
    pub log: Vec<String>,
}

impl Messages {
    /// Show `msg` in the minibuffer and append it to the *Messages* log.
    pub fn show(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.log.push(msg.clone());
        self.current = Some(msg);
    }

    /// Clear the minibuffer (the log keeps everything shown so far).
    pub fn clear(&mut self) {
        self.current = None;
    }

    /// The message currently shown in the minibuffer, if any.
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Top-level view
// ---------------------------------------------------------------------------

/// Which top-level view owns the screen and the keyboard.
///
/// The views are mutually exclusive by construction: each non-`Text` variant
/// requires its own `App` field to be populated, and opening one tears the
/// other down (see `exec::buffers::teardown_current_buffer`).  Derive it with
/// [`App::view`] rather than testing the individual `Option`s — the three
/// dispatch points (`app::draw_frame`, `exec::update_scroll`, and the
/// `input` keymap layer) must agree on which view is active, and a
/// hand-rolled condition at each one drifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Plain text buffer.  Also the view for a notebook's full-screen
    /// focused-cell overlay, which is edited exactly like a file.
    Text,
    /// Notebook cell-stack view.
    Notebook,
    /// Tabular data grid (CSV/TSV today — see [`crate::table`]).
    Table,
}

// ---------------------------------------------------------------------------
// Central application state
// ---------------------------------------------------------------------------

/// Central application state.
pub struct App {
    pub buffer: Buffer,
    pub selection: Selection,
    pub scroll_row: usize,
    pub scroll_col: usize,
    pub mode: Mode,
    pub command_buf: String,
    /// Minibuffer message + the *Messages* log (see [`Messages`]).
    pub messages: Messages,
    pub clipboard: String,
    pub should_quit: bool,
    /// True once the first edit has been made in the current Insert session.
    /// Resets to false when leaving Insert mode. Used to coalesce undo entries.
    pub insert_session_active: bool,
    pub highlighter: Highlighter,
    pub highlight_spans: Vec<Span>,
    pub config: Config,
    pub keymap: Keymap,
    /// Loaded notebook + UI state, present when a `.ipynb` file is opened.
    pub notebook: Option<(Notebook, NotebookState)>,
    /// Every live Python kernel: one per notebook (Jupyter semantics — a `df` in
    /// one notebook must not answer a cell in another), plus the *active* one a
    /// view with no kernel of its own talks to.  Owned here rather than by the
    /// notebook so any view can reach an interpreter — see [`crate::compute`],
    /// including the rule that a view may only *borrow* a session and must
    /// tolerate it being absent, busy or restarted between frames.
    pub compute: crate::compute::ComputePool,
    /// Open tabular data source + cursor state, present in the table view
    /// (`:csv`).  Mutually exclusive with `notebook`; while it is set
    /// `buffer` is a detached empty buffer, so nothing can write the data file.
    pub table: Option<crate::exec::table::Session>,
    /// In-flight background table load, polled once per frame by the run loop.
    pub table_pending: Option<crate::exec::table::TableLoad>,
    /// Table sessions stashed by path when navigating away, so coming back
    /// restores the exact cursor cell instead of re-parsing the file from disk.
    /// (The counterpart of `file_buffers` / `notebook_buffers`.)
    pub table_buffers: std::collections::HashMap<crate::source::SourceId, crate::exec::table::Session>,
    /// Directory a bare filename in a `:sql` query resolves against.
    ///
    /// Captured when the SQL buffer is opened, because switching into it makes
    /// `app.buffer` the `*sql*` buffer — which has no path, and so no idea which
    /// project's `data.csv` the query means.
    pub sql_dir: Option<std::path::PathBuf>,
    /// What was on screen when the SQL buffer was opened — where `q` goes back
    /// to.  The query buffer is a temporary thing you back out of, like a
    /// `*cell …*` buffer, and `:bd` refuses to close a `*…*` name outright.
    pub sql_origin: Option<crate::source::SourceId>,
    /// Local database files attached read-only (`:attach`), replayed onto every
    /// connection the editor opens.  Paths and names only: a remote or
    /// authenticated database is reached in the kernel by the user's own code,
    /// so nothing here is ever a credential.
    pub attachments: Vec<crate::table::Attachment>,
    /// While a `*cell …*` buffer is open, the table it was read out of (what
    /// `:bd` returns to) plus the settings the cell buffer overrode.
    pub table_cell_origin: Option<crate::exec::table::CellOrigin>,
    /// Per-cell highlight-span cache + shared highlighter for the notebook
    /// view.  Lives outside `notebook` so the renderer can borrow it mutably
    /// alongside an immutable borrow of the notebook itself.
    pub nb_highlight: crate::notebook_ui::CellHighlightCache,
    /// Terminal-graphics (Kitty/WezTerm) image state.
    pub graphics: GraphicsState,
    /// True while the full-screen focused-cell overlay (Enter from notebook nav)
    /// is active. Only meaningful while a notebook is open. The cell currently
    /// loaded into `buffer` is identified by the open notebook's
    /// `state.focused_cell` — see [`App::notebook_language`] / cell virtual paths.
    pub cell_focused_edit: bool,
    /// Active floating popup overlay, if any.
    pub popup: Option<crate::popup::Popup>,
    /// LSP client manager — one server per language.
    pub lsp: LspManager,
    /// Language id of the currently edited document (e.g. "python", "rust").
    pub lsp_language: Option<String>,
    /// Buffer search state (query, match list, current index, etc.).
    pub search: SearchState,
    /// Visible text rows in the editor area — updated each render frame.
    pub viewport_height: usize,
    /// Visible text columns — updated each render frame, used by scroll logic.
    pub viewport_width: usize,
    /// All file paths opened in this session (for the buffer picker).
    pub open_buffers: Vec<crate::source::SourceId>,
    /// Git diff marks for the current buffer, keyed by 0-indexed line number.
    pub git_diff: std::collections::HashMap<usize, GutterMark>,
    /// Current git branch name (refreshed in the background at startup and on write).
    pub git_branch: Option<String>,
    /// In-flight background git refresh (branch + diff marks), polled once per
    /// frame by the run loop.  `None` when no refresh is pending.
    pub git_pending: Option<crate::git::GitRefresh>,
    /// In-flight background `quarto render` export, polled once per frame.
    pub export_pending: Option<crate::exec::ExportJob>,
    /// Code actions returned by the last LSP `textDocument/codeAction` request.
    /// Indexed by the popup item's `ConfirmPayload::CodeAction(idx)`.
    pub pending_code_actions: Vec<serde_json::Value>,
    /// `gw` label-jump transient state.
    pub jump: JumpState,
    /// Set after suspending and resuming the terminal (e.g. external file picker).
    /// Causes the render loop to call `terminal.clear()` once to force a full repaint.
    pub needs_clear: bool,
    /// True when `buffer.rope` has changed and `highlight_spans` needs recomputing.
    /// The render loop recomputes lazily once per frame instead of once per keystroke.
    pub highlights_dirty: bool,
    /// Per-line diagnostic ranges for the current file, rebuilt on LSP diagnostics
    /// events and on file switches.  Avoids rebuilding this map every render frame.
    pub diag_by_line: std::collections::HashMap<usize, Vec<(usize, usize, DiagnosticSeverity)>>,
    /// The mode that was active during the last rendered frame.  Used to skip the
    /// cursor-shape OSC write when the mode hasn't changed.
    pub last_rendered_mode: Option<Mode>,
    /// Code fold state for the plain-text editor (fold ranges + which are closed).
    pub fold: FoldState,
    /// Persisted rope content for special buffers (currently only *scratch*).
    pub special_buffer_ropes: std::collections::HashMap<String, ropey::Rope>,
    /// When true, the next `FormattingResult` event will also trigger a save.
    pub pending_format_save: bool,
    /// LSP completion-popup bookkeeping (suppression prefix + in-flight resolve).
    pub completion: CompletionState,
    /// Active call signature shown in the minibuffer while typing arguments in
    /// Insert mode (from `textDocument/signatureHelp`). `None` when not in a call.
    pub signature_help: Option<String>,
    /// When the last signature-help request was sent (throttle anchor).
    pub sig_help_last: Option<std::time::Instant>,
    /// A signature-help refresh arrived inside the throttle window and was
    /// deferred; `exec::pump_signature_help` fires it once the window elapses
    /// (trailing edge), so the hint still settles on the final cursor state.
    pub sig_help_deferred: bool,
    /// In-memory stash of notebooks that have been navigated away from.
    /// Keyed by the canonicalized `.ipynb` path.  When the user navigates back
    /// to a notebook, its state is restored from here rather than reloading from
    /// disk, so unsaved edits are preserved across buffer switches.
    pub notebook_buffers: std::collections::HashMap<crate::source::SourceId, (Notebook, NotebookState)>,
    /// In-memory stash of plain-file buffers navigated away from, keyed by
    /// canonicalized path.  Preserves unsaved edits and undo history across
    /// buffer switches (the file is otherwise reloaded from disk).  Entries
    /// are removed when restored or when the buffer is closed with `:bd`.
    pub file_buffers: std::collections::HashMap<crate::source::SourceId, Buffer>,
    /// Where the cursor was when a text buffer was last left, as `(line, column)`
    /// — the position a later switch back to it restores.  Kept apart from
    /// `file_buffers` because it outlives it: a buffer whose stash was dropped
    /// (or that was never stashed, like `*scratch*`) still deserves to reopen
    /// where it was left.  Lines/columns rather than a char index so the
    /// restore can go through `lsp::open_file_at`, which speaks positions.
    pub cursor_positions: std::collections::HashMap<crate::source::SourceId, (usize, usize)>,
    /// Crash-recovery bookkeeping (recovery dir, debounce, written-file index).
    pub recovery: crate::recovery::Recovery,
    /// Recovery prompts queued at startup / on open, shown one at a time.
    pub pending_recoveries: std::collections::VecDeque<crate::recovery::PendingRecovery>,
    /// The recovery currently shown in the prompt popup, awaiting a choice.
    pub active_recovery: Option<crate::recovery::PendingRecovery>,
    /// Most-recently-used command names for the palette (front = most recent).
    /// Empty / unused when `ui.command_history = "off"`.
    pub command_history: std::collections::VecDeque<String>,
    /// Parsed `ui.command_history` mode (off / session / global).
    pub command_history_mode: crate::config::CommandHistoryMode,
    /// "Boiling" Braille spinner shown in the status bar during background work
    /// (cell execution, in-flight LSP requests).  Advanced once per frame.
    pub spinner: crate::spinner::Spinner,
    /// True when the welcome/splash screen should be shown instead of the editor.
    /// Set on launch with no file argument; cleared on the first keypress.
    pub show_splash: bool,
}

impl App {
    /// The view that currently owns the screen and the keyboard.
    pub fn view(&self) -> View {
        debug_assert!(
            !(self.table.is_some() && self.notebook.is_some()),
            "a table and a notebook must never be open at once"
        );
        if self.table.is_some() {
            View::Table
        } else if self.in_notebook_nav() {
            View::Notebook
        } else {
            View::Text
        }
    }

    /// Name of the table file currently loading in the background, if any.
    pub fn table_load_name(&self) -> Option<&str> {
        self.table_pending.as_ref().map(|l| l.display_name())
    }

    /// Returns true when the focused-cell full-screen overlay is active.
    pub fn notebook_focused_edit(&self) -> bool {
        self.notebook.is_some() && self.cell_focused_edit
    }

    /// True while a notebook is open and the cursor is navigating between
    /// cells rather than editing focused-cell text in place (the notebook
    /// keymap override + cell-sync-after-edit only apply in this state).
    pub fn in_notebook_nav(&self) -> bool {
        self.notebook.is_some() && !self.notebook_focused_edit()
    }

    /// True while a `*cell …*` buffer — a table cell's text opened for reading
    /// — is the current buffer.  The view is still [`View::Text`]; this only
    /// selects the small `q`-closes-it keymap override.
    pub fn in_cell_buffer(&self) -> bool {
        self.table_cell_origin.is_some()
            && self
                .buffer
                .path
                .as_ref()
                .and_then(|p| p.to_str())
                .is_some_and(|n| n.starts_with("*cell "))
    }

    /// True while the `*sql*` query buffer is the active buffer.
    ///
    /// An ordinary text buffer with one addition: the execute keys run its
    /// contents as a query (see `input::handle_key`), the way they run a cell.
    pub fn in_sql_buffer(&self) -> bool {
        self.buffer.path.as_ref().and_then(|p| p.to_str()) == Some(SQL_BUFFER)
    }

    /// The kernel language of the open notebook (e.g. `"python"`), if any.
    /// This is the LSP `languageId` for every code cell.
    pub fn notebook_language(&self) -> Option<&str> {
        self.notebook
            .as_ref()
            .map(|(nb, _)| nb.metadata.kernel_language.as_str())
    }

    /// The language id for the document currently in the editor buffer.
    pub fn current_language(&self) -> Option<&str> {
        self.notebook_language().or(self.lsp_language.as_deref())
    }

    /// Indent width for the current buffer's language: the per-language
    /// `[languages.<lang>] indent_width` override when set, otherwise the
    /// global `editor.tab_width`.
    pub fn indent_width(&self) -> usize {
        self.current_language()
            .and_then(|l| self.config.languages.get(l))
            .and_then(|lc| lc.indent_width)
            .unwrap_or(self.config.editor.tab_width)
    }

    /// The string one indent level inserts in this buffer (spaces unless
    /// `editor.expand_tabs = false`).
    pub fn indent_unit(&self) -> String {
        crate::indent::unit(self.config.editor.expand_tabs, self.indent_width())
    }

    /// True when the text being edited is Markdown — a `.md`/`.qmd` buffer or
    /// the focused cell of a notebook when it is a markdown cell.
    pub fn buffer_is_markdown(&self) -> bool {
        if let Some((nb, state)) = self.notebook.as_ref() {
            nb.cells
                .get(state.focused_cell)
                .map(|c| c.cell_type == crate::notebook::CellType::Markdown)
                .unwrap_or(false)
        } else {
            self.highlighter.markdown
        }
    }

    /// Create a new App, loading `path` if provided.
    pub fn new(path: Option<&str>, config: Config) -> Result<Self> {
        let is_notebook = path.map(|p| p.ends_with(".ipynb")).unwrap_or(false);

        let notebook = if is_notebook {
            let p = path.expect("checked above");
            match Notebook::from_path(std::path::Path::new(p)) {
                Ok(nb) => Some((nb, NotebookState::new())),
                Err(e) => {
                    eprintln!("sv: failed to load notebook: {e}");
                    None
                }
            }
        } else {
            None
        };

        // For notebooks, pre-load cell 0 into the buffer so editing works immediately.
        let (buffer, lsp_language) = if let Some((ref nb, _)) = notebook {
            let lang = nb.metadata.kernel_language.clone();
            let vpath = crate::notebook::cell_virtual_path(&nb.path, &lang, 0);
            let mut buf = Buffer::new_empty();
            if let Some(cell) = nb.cells.first() {
                buf.rope = cell.source.clone();
            }
            buf.path = Some(vpath);
            (buf, Some(lang))
        } else {
            let buf = match path {
                Some(p) => Buffer::from_path(p).unwrap_or_else(|_| {
                    let mut b = Buffer::new_empty();
                    b.path = Some(std::path::PathBuf::from(p));
                    b
                }),
                None => Buffer::new_empty(),
            };
            let lang = language_for_path(buf.path.as_deref()).map(str::to_owned);
            (buf, lang)
        };

        let mut highlighter = Highlighter::new(buffer.path.as_deref());
        let highlight_spans = highlighter.highlight(&buffer.rope).unwrap_or_default();
        // Compute fold ranges immediately so folding works before the first edit.
        let initial_fold_ranges = highlighter.fold_ranges(&buffer.rope);

        let initial_mode = Mode::Normal;

        // *scratch* and *Messages* are always present at the front of the buffer list.
        let mut open_buffers: Vec<crate::source::SourceId> = vec![
            crate::source::SourceId::virtual_named("scratch"),
            crate::source::SourceId::virtual_named("Messages"),
        ];
        if let Some((ref nb, _)) = notebook {
            // For notebooks, always track the .ipynb file — never the virtual cell paths.
            open_buffers.push(crate::source::SourceId::of(&nb.path));
        } else if let Some(p) = buffer.path.as_ref() {
            open_buffers.push(crate::source::SourceId::of(p));
        }

        // Branch + diff marks arrive asynchronously; the run loop polls this.
        let git_pending = Some(crate::git::refresh(
            if notebook.is_some() { None } else { buffer.path.clone() },
        ));

        let mut keymap = Keymap::default_bindings();
        keymap.apply_custom_bindings(&config.keys);

        let recovery = crate::recovery::Recovery::new(config.editor.crash_recovery);
        let command_history_mode =
            crate::config::CommandHistoryMode::parse(&config.ui.command_history);
        let command_history = crate::history::load(command_history_mode);

        Ok(Self {
            buffer,
            compute: crate::compute::ComputePool::default(),
            selection: Selection::point(0),
            scroll_row: 0,
            scroll_col: 0,
            mode: initial_mode,
            command_buf: String::new(),
            messages: Messages::default(),
            clipboard: String::new(),
            should_quit: false,
            insert_session_active: false,
            highlighter,
            highlight_spans,
            config,
            keymap,
            notebook,
            table: None,
            table_pending: None,
            table_buffers: std::collections::HashMap::new(),
            sql_dir: None,
            sql_origin: None,
            attachments: Vec::new(),
            table_cell_origin: None,
            nb_highlight: crate::notebook_ui::CellHighlightCache::default(),
            graphics: GraphicsState::default(),
            cell_focused_edit: false,
            popup: None,
            lsp: LspManager::new(),
            lsp_language,
            search: SearchState::default(),
            viewport_height: 24,
            viewport_width: 80,
            open_buffers,
            git_diff: std::collections::HashMap::new(),
            git_branch: None,
            git_pending,
            export_pending: None,
            pending_code_actions: Vec::new(),
            jump: JumpState::default(),
            needs_clear: false,
            highlights_dirty: false,
            diag_by_line: std::collections::HashMap::new(),
            last_rendered_mode: None,
            fold: FoldState {
                ranges: initial_fold_ranges,
                ..FoldState::default()
            },
            pending_format_save: false,
            completion: CompletionState::default(),
            signature_help: None,
            sig_help_last: None,
            sig_help_deferred: false,
            notebook_buffers: std::collections::HashMap::new(),
            file_buffers: std::collections::HashMap::new(),
            cursor_positions: std::collections::HashMap::new(),
            recovery,
            pending_recoveries: std::collections::VecDeque::new(),
            active_recovery: None,
            command_history,
            command_history_mode,
            spinner: crate::spinner::Spinner::default(),
            show_splash: path.is_none(),
            special_buffer_ropes: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "*scratch*".to_string(),
                    ropey::Rope::from_str(crate::exec::SCRATCH_INTRO),
                );
                m
            },
        })
    }
}

/// Map a file path to an LSP language id via its extension (see [`crate::lang`]),
/// falling back to a filename match for extensionless shell dotfiles like `.zshrc`.
pub fn language_for_path(path: Option<&std::path::Path>) -> Option<&'static str> {
    let path = path?;
    if let Some(lang) = path.extension().and_then(|e| e.to_str()).and_then(crate::lang::ext_to_lang) {
        return Some(lang);
    }
    crate::lang::filename_to_lang(path.file_name()?.to_str()?)
}

/// Set up terminal, run the event loop, then restore terminal.
pub fn run(path: Option<&str>) -> Result<()> {
    let config = Config::load();

    crate::theme::init_from_config(&config);
    crate::buffer::configure_max_undo(config.editor.max_undo);

    let mut app = App::new(path, config)?;

    // Start LSP server for the opened file if configured.
    if let Some(ref lang) = app.lsp_language.clone() {
        if let Some(server_config) = app.config.language_servers.get(lang).cloned() {
            let fallback_root = app.buffer.path.as_ref().and_then(|p| p.parent()).and_then(
                |p| {
                    if p.as_os_str().is_empty() {
                        None
                    } else {
                        Some(p.to_path_buf())
                    }
                },
            );
            app.lsp
                .ensure_server(lang, &server_config, fallback_root.as_deref());
        }
    }

    // Also start the LSP server for the notebook's kernel language.
    if let Some((ref nb, _)) = app.notebook {
        let lang = nb.metadata.kernel_language.clone();
        if !lang.is_empty() {
            if let Some(server_config) = app.config.language_servers.get(lang.as_str()).cloned() {
                let nb_dir = nb.path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.to_path_buf());
                app.lsp.ensure_server(&lang, &server_config, nb_dir.as_deref());
                if app.lsp_language.is_none() {
                    app.lsp_language = Some(lang);
                }
            }
        }
    }

    // A data file opens in the table view.  Done here rather than in
    // `App::new` because the load runs on a background thread and is applied by
    // the run loop's poll — `App::new` has no loop to poll it.
    if let Some(p) = path.map(std::path::PathBuf::from) {
        if app.config.table.auto_open && crate::exec::is_table_path(&p) {
            crate::exec::open_as_table(&mut app, &p);
        }
    }

    // Surface any unsaved buffers recoverable from a previous unclean exit.
    crate::recovery::startup_scan(&mut app);

    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Best-effort: persist the latest recovery snapshot before unwinding.
        crate::recovery::flush_panic_snapshot();
        let _ = restore_terminal();
        original_hook(info);
    }));

    terminal::enable_raw_mode()?;
    crate::theme::initialize_color_cache();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // Bracketed paste: without it the terminal replays a paste as ordinary
    // keystrokes, so every embedded newline runs the Enter handler and
    // auto-indent stacks on top of the pasted line's own indentation — a block
    // pasted into Insert mode staircases to the right. With it enabled the
    // whole paste arrives as one `Event::Paste` and is inserted verbatim.
    execute!(stdout, crossterm::event::EnableBracketedPaste)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Paint the file BEFORE negotiating terminal capabilities. The keyboard
    // query below blocks for up to two seconds when nothing answers it (a raw
    // pty, an ssh hop, an editor launched from inside another TUI such as
    // visidata) — and with the alternate screen already entered, doing it
    // first leaves an empty screen with a lone cursor for that whole time,
    // which reads as a hang.
    draw_frame(&mut terminal, &mut app)?;

    negotiate_keyboard_enhancement(&mut app);

    // Catch terminating signals so we can restore the terminal and flush unsaved
    // work before dying (pkill/kill/SIGHUP on window close). SIGKILL is exempt.
    install_signal_handlers();

    let result = run_loop(&mut terminal, &mut app);

    // On a termination signal (which may have surfaced as an EINTR error from
    // the event poll, hence handling it here rather than only in the loop),
    // persist the latest unsaved edits before tearing anything down.  The
    // recovery files are kept — a signal kill is an unclean exit.
    let signal = pending_signal();
    if signal.is_some() {
        crate::recovery::flush_now(&mut app);
    }

    restore_terminal()?;

    // Re-raise the signal with the default disposition so the exit status
    // correctly reflects it (and SIGHUP propagates as expected).
    #[cfg(unix)]
    if let Some(sig) = signal {
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    result
}

/// Opt into the kitty keyboard protocol when the terminal supports it, so
/// modified keys like Shift+Enter / Ctrl+Enter are reported as distinct events
/// instead of collapsing into a bare Enter. DISAMBIGUATE_ESCAPE_CODES is the
/// safe level for this — it disambiguates modified special keys without
/// altering how ordinary text (incl. shifted symbols) is reported.
///
/// The support query can go unanswered (the reply lost in startup output, a
/// slow ssh hop timing out the poll), so on terminals *known* to implement the
/// protocol — Kitty and Ghostty — the flags are pushed even when the query
/// fails. Since the answer can't change the outcome there, the query is
/// skipped outright on those: it costs a two-second stall whenever the reply
/// doesn't come. WezTerm is not forced — it only speaks the protocol when the
/// user enables it, and then it answers the query anyway.
fn negotiate_keyboard_enhancement(app: &mut App) {
    let known_good = app.graphics.terminal.implements_kitty_keyboard();
    let support = (!known_good).then(terminal::supports_keyboard_enhancement);
    if matches!(support, Some(Ok(true))) || known_good {
        use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
        if execute!(io::stdout(), PushKeyboardEnhancementFlags(flags)).is_ok() {
            KEYBOARD_ENHANCED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    // Surface what was negotiated when key debugging is on (SV_DEBUG_KEYS=1).
    // `support = None` means the query was skipped as redundant.
    if std::env::var_os("SV_DEBUG_KEYS").is_some() {
        app.messages.show(format!(
            "keyboard enhancement: support={support:?} terminal={:?} active={}  (logging keys to {})",
            app.graphics.terminal,
            KEYBOARD_ENHANCED.load(std::sync::atomic::Ordering::SeqCst),
            key_debug_log_path().display(),
        ));
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let debug_keys = std::env::var_os("SV_DEBUG_KEYS").is_some();
    // Draw only when something actually changed (a key was handled, an LSP or
    // kernel event was applied, the spinner is animating, the terminal was
    // resized).  When idle the loop just polls for events — zero render work,
    // zero tree-sitter work, instead of a full redraw 60× per second.
    let mut needs_redraw = true;
    loop {
        if needs_redraw {
            needs_redraw = false;
            draw_frame(terminal, app)?;
        }

        // A hung-up terminal (see `stdin_hung_up`) never delivers SIGHUP to an
        // orphaned process, and the crossterm poll below can spin forever
        // inside itself reading it — so check first and shut down the same
        // way a real SIGHUP would, rather than ever making that call.
        if stdin_hung_up() {
            #[cfg(unix)]
            PENDING_SIGNAL.store(libc::SIGHUP, std::sync::atomic::Ordering::SeqCst);
            break;
        }

        // Block up to 16 ms for the first event (keeps input latency low while
        // background channels are still polled regularly).  Once an event
        // arrives, drain every additional queued event before redrawing so a
        // key-repeat burst is consumed in a single frame.
        if event::poll(std::time::Duration::from_millis(16))? {
            loop {
                match event::read()? {
                    Event::Key(key) => {
                        if debug_keys {
                            log_key_event(&key);
                        }
                        // With the keyboard-enhancement protocol active some
                        // terminals also emit key-release events; only act on
                        // press/repeat.
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            input::handle_key(app, key);
                            needs_redraw = true;
                        }
                    }
                    Event::Paste(text) => {
                        input::handle_paste(app, &text);
                        needs_redraw = true;
                    }
                    Event::Resize(_, _) => {
                        needs_redraw = true;
                    }
                    _ => {}
                }
                if !event::poll(std::time::Duration::from_millis(0))? {
                    break;
                }
            }
        }

        // Trailing edge of the signature-help throttle: fire a deferred
        // refresh once the rate-limit window has elapsed.
        crate::exec::pump_signature_help(app);

        // Background work: anything applied means the screen is stale.
        needs_redraw |= crate::exec::process_lsp_events(app);
        needs_redraw |= crate::exec::process_kernel_events(app);
        needs_redraw |= crate::exec::poll_git(app);
        needs_redraw |= crate::exec::poll_export(app);
        needs_redraw |= crate::exec::poll_table_load(app);

        // Advance the status-bar spinner.  It's "active" whenever a notebook
        // cell is executing or queued, the kernel is booting, an LSP request
        // is in flight, or an export is running — and animating it requires a
        // redraw per tick.
        let background_active = app
            .notebook
            .as_ref()
            .map(|(_, state)| !state.exec_queue.is_empty())
            .unwrap_or(false)
            // Any kernel booting or running something — including one belonging
            // to a notebook that isn't on screen.
            || app.compute.any_busy()
            || app.lsp.has_pending_requests()
            || app.export_pending.is_some()
            || app.table_pending.is_some();
        app.spinner.update(background_active);
        needs_redraw |= background_active;

        // Belt-and-braces: state flagged dirty by any path above.
        needs_redraw |= app.needs_clear || app.highlights_dirty;

        // Debounced crash-recovery flush of any unsaved buffers.
        crate::recovery::tick(app);

        // (Messages are appended to the *Messages* log by `Messages::show`
        // at the moment they are shown — no per-frame diffing needed.)

        // A catchable termination signal was received: break promptly. run()
        // flushes recovery, restores the terminal, and re-raises the signal.
        if pending_signal().is_some() {
            break;
        }

        if app.should_quit {
            // Clean exit — nothing to recover next time.
            crate::recovery::cleanup_on_quit(app);
            break;
        }
    }
    Ok(())
}

/// Render one frame: refresh viewport dimensions, recompute scroll and any
/// stale highlights, draw the active view (splash / notebook / plain editor),
/// then flush Kitty images and the cursor shape.
fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    {
        // Update stored viewport dimensions, then recompute scroll, so the
        // scroll always reflects the current terminal size.
        if let Ok(size) = terminal.size() {
            app.viewport_height = size.height.saturating_sub(2) as usize;
            app.viewport_width = size.width as usize;
        }
        // Query actual terminal cell pixel dimensions (TIOCGWINSZ).
        // Used to size image placeholders to match Kitty's rendering exactly.
        if let Ok(ws) = crossterm::terminal::window_size() {
            if ws.columns > 0 && ws.rows > 0 && ws.width > 0 && ws.height > 0 {
                app.graphics.cell_pixel_size = Some((ws.height / ws.rows, ws.width / ws.columns));
            }
        }
        crate::exec::update_scroll(app);

        // Recompute syntax highlights and fold ranges at most once per frame.
        // Individual edits only set the dirty flag; the cost is paid here.
        if app.highlights_dirty {
            app.highlights_dirty = false;
            app.highlight_spans = app
                .highlighter
                .highlight(&app.buffer.rope)
                .unwrap_or_default();
            // Recompute foldable ranges (tree-sitter or markdown) from the update.
            app.fold.ranges = app.highlighter.fold_ranges(&app.buffer.rope);
            // Discard any stored folds whose start lines no longer exist.
            let valid: std::collections::BTreeSet<usize> =
                app.fold.ranges.iter().map(|r| r.start).collect();
            app.fold.folded.retain(|s| valid.contains(s));
        }

        // After an external program (file picker etc.) suspends and resumes the
        // terminal, ratatui's diffing state is stale — force a full repaint.
        if app.needs_clear {
            app.needs_clear = false;
            let _ = terminal.clear();
        }

        // Where the view drew its text cursor, when it draws images and needs it
        // restored after the flush (see `flush_images`).  Views that emit no
        // images leave it `None`.
        let mut frame_cursor: Option<(u16, u16)> = None;

        if app.show_splash {
            terminal.draw(|f| {
                crate::theme::fill_background(f);
                let size = f.area();
                // In command mode the user is typing a command — show the
                // command input bar at the bottom and shrink the splash area.
                let in_cmd = matches!(app.mode, crate::mode::Mode::Command);
                let splash_area = if in_cmd {
                    ratatui::layout::Rect {
                        x: size.x,
                        y: size.y,
                        width: size.width,
                        height: size.height.saturating_sub(1),
                    }
                } else {
                    size
                };
                crate::splash::render(f, splash_area, app);
                if in_cmd {
                    let cmd_area = ratatui::layout::Rect {
                        x: size.x,
                        y: size.y + size.height.saturating_sub(1),
                        width: size.width,
                        height: 1,
                    };
                    crate::ui::render_command(f, app, cmd_area);
                }
                // If a popup was opened from the dashboard (e.g. file picker),
                // render it on top of the splash background.
                if let Some(ref popup) = app.popup {
                    crate::popup_ui::render(f, popup, None, &app.config.ui);
                }
            })?;
        } else if app.view() == View::Table {
            // Tabular data grid.  No text cursor: the cursor is the highlighted
            // cell, drawn by the renderer (a terminal cursor in a grid of cells
            // reads as a text caret inside the value, which it isn't).
            terminal.draw(|f| {
                crate::theme::fill_background(f);
                let size = f.area();
                if size.height >= 3 {
                    if let Some(ref session) = app.table {
                        let grid = ratatui::layout::Rect {
                            height: size.height.saturating_sub(2),
                            ..size
                        };
                        crate::table_ui::render(f, grid, session, &app.config.table);

                        let status_area = ratatui::layout::Rect {
                            x: size.x,
                            y: size.y + size.height.saturating_sub(2),
                            width: size.width,
                            height: 1,
                        };
                        let ctx = ui::status_ctx(app);
                        crate::statusline::render(
                            f, status_area, &ctx,
                            &app.config.statusline.table.left,
                            &app.config.statusline.table.right,
                            &app.config.statusline.separator,
                            &app.config.statusline.styles,
                        );
                        let cmd_area = ratatui::layout::Rect {
                            x: size.x,
                            y: size.y + size.height.saturating_sub(1),
                            width: size.width,
                            height: 1,
                        };
                        ui::render_command(f, app, cmd_area);
                    }
                }
                if let Some(ref popup) = app.popup {
                    crate::popup_ui::render(f, popup, None, &app.config.ui);
                }
            })?;
        } else if app.view() == View::Notebook {
            // Notebook multi-cell view — the focused cell is in app.buffer.
            // Lifted out of the draw closure so we can restore the hardware
            // cursor to it *after* the Kitty image flush (which moves the
            // terminal cursor to each image's origin and would otherwise leave
            // the block cursor sitting on top of an image).
            let mut nb_cursor: Option<(u16, u16)> = None;
            terminal.draw(|f| {
                crate::theme::fill_background(f);
                let size = f.area();

                if size.height >= 3 {
                    if let Some((ref nb, ref state)) = app.notebook {
                        let active = crate::notebook_ui::ActiveCellView {
                            rope: &app.buffer.rope,
                            cursor: app.selection.head,
                            sel_anchor: app.selection.anchor,
                            output_row: app.notebook.as_ref().and_then(|(_, s)| s.output_row),
                            output_col: app.notebook.as_ref().map(|(_, s)| s.output_col).unwrap_or(0),
                            output_anchor: app.notebook.as_ref().and_then(|(_, s)| s.output_anchor),
                            mode: &app.mode,
                            jump_labels: &app.jump.labels,
                            jump_typed: &app.jump.typed,
                            word_wrap: app.config.editor.word_wrap,
                        };
                        let (images, cursor_pos) =
                            crate::notebook_ui::render(f, state, nb, &active, &app.lsp.diagnostics, &app.config.notebook, app.graphics.cell_pixel_size, &mut app.nb_highlight);
                        app.graphics.pending = images;
                        nb_cursor = cursor_pos;

                        let status_area = ratatui::layout::Rect {
                            x: size.x,
                            y: size.y + size.height.saturating_sub(2),
                            width: size.width,
                            height: 1,
                        };
                        let cmd_area = ratatui::layout::Rect {
                            x: size.x,
                            y: size.y + size.height.saturating_sub(1),
                            width: size.width,
                            height: 1,
                        };
                        // Status-line context is built the same way for both the
                        // plain editor and the notebook view (see ui::status_ctx);
                        // only the module *layout* differs (notebook variant here).
                        let ctx = ui::status_ctx(app);
                        crate::statusline::render(
                            f, status_area, &ctx,
                            &app.config.statusline.notebook.left,
                            &app.config.statusline.notebook.right,
                            &app.config.statusline.separator,
                            &app.config.statusline.styles,
                        );
                        ui::render_command(f, app, cmd_area);
                    }
                }
                if let Some(ref popup) = app.popup {
                    crate::popup_ui::render(f, popup, nb_cursor, &app.config.ui);
                }
            })?;
            frame_cursor = nb_cursor;
        } else {
            // Plain text editor or full-screen focused-cell overlay.
            terminal.draw(|f| {
                crate::theme::fill_background(f);
                ui::render(f, app);
                if let Some(ref popup) = app.popup {
                    let cursor_pos = ui::cursor_screen_pos(app, f.area());
                    crate::popup_ui::render(f, popup, cursor_pos, &app.config.ui);
                }
            })?;
        }

        // Every view goes through the same flush: ratatui owns the screen during
        // the draw, so pixel data can only be written once it has finished.
        flush_images(app, frame_cursor);

        // Only write cursor-shape OSC sequences when the mode actually changes.
        if app.last_rendered_mode.as_ref() != Some(&app.mode) {
            app.last_rendered_mode = Some(app.mode.clone());
            set_cursor_shape(&app.mode);
        }
    }
    Ok(())
}

/// Place the images the frame just asked for, and clear the previous frame's.
///
/// View-agnostic on purpose: a renderer's whole contract is to push
/// [`ImageRequest`]s onto `app.graphics.pending`, and this decides how (and
/// whether) they reach the terminal.  `cursor` is where the view drew its text
/// cursor — placing an image leaves the terminal cursor at that image's origin,
/// so it has to be put back or the block cursor appears stuck on the image.
///
/// Images are suppressed entirely while a popup is open: a float drawn by
/// ratatui cannot cover a Kitty raster, so the image would sit on top of it.
fn flush_images(app: &mut App, cursor: Option<(u16, u16)>) {
    if !app.graphics.terminal.supports_graphics() {
        app.graphics.pending.clear();
        return;
    }

    // If the terminal was resized, Kitty evicts its pixel cache, so any cached
    // image IDs are invalid — drop them so the next placement re-uploads.
    let cur_size = (app.viewport_width as u16, app.viewport_height as u16);
    if cur_size != app.graphics.last_size {
        app.graphics.image_ids.clear();
        app.graphics.last_size = cur_size;
    }

    // Clear last frame's placements so images that scrolled off screen, were
    // replaced, or belong to a view we have since left disappear.  Keyed on
    // `placed` rather than on `image_ids` so a command that empties the ID
    // cache (`:clear-outputs`) still gets its placements taken down.
    if app.graphics.placed || !app.graphics.pending.is_empty() {
        let _ = kitty::clear_images();
    }

    let images = std::mem::take(&mut app.graphics.pending);
    if images.is_empty() || app.popup.is_some() {
        app.graphics.placed = false;
        return;
    }

    for req in &images {
        let ptr_key = std::sync::Arc::as_ptr(&req.png_data) as usize;
        if let Some(&kid) = app.graphics.image_ids.get(&ptr_key) {
            // Pixel data already cached in the terminal — re-place cheaply.
            let _ = kitty::place_image(req.col, req.row, kid, req.rows, req.cols, req.crop);
        } else {
            // First time seeing this image — upload pixel data once.
            let kid = app.graphics.next_id;
            app.graphics.next_id = if app.graphics.next_id == u32::MAX { 1 } else { app.graphics.next_id + 1 };
            let _ = kitty::upload_and_place(req.col, req.row, kid, req.rows, req.cols, req.crop, &req.png_data);
            app.graphics.image_ids.insert(ptr_key, kid);
        }
    }
    app.graphics.placed = true;

    // Put the cursor back where the view drew it.  (`None` when the view has no
    // visible cursor — a rendered markdown cell, the grid — in which case
    // ratatui already hid it.)
    if let Some((cx, cy)) = cursor {
        use std::io::Write;
        let mut out = io::stdout();
        let _ = write!(out, "\x1b[{};{}H", cy + 1, cx + 1);
        let _ = out.flush();
    }
}

fn restore_terminal() -> Result<()> {
    use std::io::Write;
    terminal::disable_raw_mode()?;
    let mut stdout = io::stdout();
    // Release the keyboard-enhancement flags if we pushed them. `swap` makes a
    // second restore (e.g. panic hook then normal exit) a no-op.
    if KEYBOARD_ENHANCED.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let _ = execute!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout, crossterm::event::DisableBracketedPaste);
    execute!(
        stdout,
        LeaveAlternateScreen,
        crossterm::cursor::SetCursorStyle::DefaultUserShape,
    )?;
    let _ = write!(stdout, "\x1b]112\x07");
    let _ = stdout.flush();
    Ok(())
}

fn set_cursor_shape(mode: &Mode) {
    use crossterm::cursor::SetCursorStyle;
    use std::io::Write;
    let _ = execute!(io::stdout(), SetCursorStyle::SteadyBlock);
    if let Some(color_spec) = crate::theme::color_to_osc_spec(crate::theme::mode_color(mode)) {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b]12;{}\x07", color_spec);
        let _ = stdout.flush();
    }
}
