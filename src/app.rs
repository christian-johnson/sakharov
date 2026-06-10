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
    kitty,
    lsp_manager::{DiagnosticSeverity, LspManager},
    mode::Mode,
    notebook::Notebook,
    notebook_state::NotebookState,
    notebook_ui::ImageRequest,
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

/// Terminal-graphics (Kitty/WezTerm) image state.
pub struct GraphicsState {
    /// Which terminal graphics backend is available (Kitty, WezTerm, or none).
    /// Detected once at startup from environment variables.
    pub terminal: kitty::GraphicsTerminal,
    /// Image draw requests collected by the last notebook render pass.
    pub pending: Vec<ImageRequest>,
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
    pub open_buffers: Vec<std::path::PathBuf>,
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
    /// In-memory stash of notebooks that have been navigated away from.
    /// Keyed by the canonicalized `.ipynb` path.  When the user navigates back
    /// to a notebook, its state is restored from here rather than reloading from
    /// disk, so unsaved edits are preserved across buffer switches.
    pub notebook_buffers: std::collections::HashMap<std::path::PathBuf, (Notebook, NotebookState)>,
    /// In-memory stash of plain-file buffers navigated away from, keyed by
    /// canonicalized path.  Preserves unsaved edits and undo history across
    /// buffer switches (the file is otherwise reloaded from disk).  Entries
    /// are removed when restored or when the buffer is closed with `:bd`.
    pub file_buffers: std::collections::HashMap<std::path::PathBuf, Buffer>,
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
    /// Returns true when the focused-cell full-screen overlay is active.
    pub fn notebook_focused_edit(&self) -> bool {
        self.notebook.is_some() && self.cell_focused_edit
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
        let mut open_buffers: Vec<std::path::PathBuf> = vec![
            std::path::PathBuf::from("*scratch*"),
            std::path::PathBuf::from("*Messages*"),
        ];
        if let Some((ref nb, _)) = notebook {
            // For notebooks, always track the .ipynb file — never the virtual cell paths.
            open_buffers.push(nb.path.canonicalize().unwrap_or_else(|_| nb.path.clone()));
        } else if let Some(p) = buffer.path.as_ref() {
            // Always store canonical absolute paths so dedup comparisons work reliably.
            open_buffers.push(p.canonicalize().unwrap_or_else(|_| p.clone()));
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
            notebook_buffers: std::collections::HashMap::new(),
            file_buffers: std::collections::HashMap::new(),
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

/// Map a file path to an LSP language id via its extension (see [`crate::lang`]).
pub fn language_for_path(path: Option<&std::path::Path>) -> Option<&'static str> {
    crate::lang::ext_to_lang(path?.extension()?.to_str()?)
}

/// Set up terminal, run the event loop, then restore terminal.
pub fn run(path: Option<&str>) -> Result<()> {
    let config = Config::load();

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

    // Opt into the kitty keyboard protocol when the terminal supports it, so
    // modified keys like Shift+Enter / Ctrl+Enter are reported as distinct events
    // instead of collapsing into a bare Enter. DISAMBIGUATE_ESCAPE_CODES is the
    // safe level for this — it disambiguates modified special keys without
    // altering how ordinary text (incl. shifted symbols) is reported.
    let kbd_support = terminal::supports_keyboard_enhancement();
    if matches!(kbd_support, Ok(true)) {
        use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
        if execute!(stdout, PushKeyboardEnhancementFlags(flags)).is_ok() {
            KEYBOARD_ENHANCED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    // Surface what was negotiated when key debugging is on (SV_DEBUG_KEYS=1).
    if std::env::var_os("SV_DEBUG_KEYS").is_some() {
        app.messages.show(format!(
            "keyboard enhancement: support={kbd_support:?} active={}  (logging keys to {})",
            KEYBOARD_ENHANCED.load(std::sync::atomic::Ordering::SeqCst),
            key_debug_log_path().display(),
        ));
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

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

        // Background work: anything applied means the screen is stale.
        needs_redraw |= crate::exec::process_lsp_events(app);
        needs_redraw |= crate::exec::process_kernel_events(app);
        needs_redraw |= crate::exec::poll_git(app);
        needs_redraw |= crate::exec::poll_export(app);

        // Advance the status-bar spinner.  It's "active" whenever a notebook
        // cell is executing or queued, the kernel is booting, an LSP request
        // is in flight, or an export is running — and animating it requires a
        // redraw per tick.
        let background_active = app
            .notebook
            .as_ref()
            .map(|(nb, state)| {
                state.executing_cell.is_some()
                    || !state.exec_queue.is_empty()
                    || nb.kernel.as_ref()
                        .is_some_and(|k| k.status == crate::notebook::KernelStatus::Starting)
            })
            .unwrap_or(false)
            || app.lsp.has_pending_requests()
            || app.export_pending.is_some();
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
                app.fold.ranges.iter().map(|&(s, _)| s).collect();
            app.fold.folded.retain(|s| valid.contains(s));
        }

        // After an external program (file picker etc.) suspends and resumes the
        // terminal, ratatui's diffing state is stale — force a full repaint.
        if app.needs_clear {
            app.needs_clear = false;
            let _ = terminal.clear();
        }

        if app.show_splash {
            terminal.draw(|f| {
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
        } else if app.notebook.is_some() && !app.notebook_focused_edit() {
            // Notebook multi-cell view — the focused cell is in app.buffer.
            // Lifted out of the draw closure so we can restore the hardware
            // cursor to it *after* the Kitty image flush (which moves the
            // terminal cursor to each image's origin and would otherwise leave
            // the block cursor sitting on top of an image).
            let mut nb_cursor: Option<(u16, u16)> = None;
            terminal.draw(|f| {
                let size = f.area();

                if size.height >= 3 {
                    if let Some((ref nb, ref state)) = app.notebook {
                        let active = crate::notebook_ui::ActiveCellView {
                            rope: &app.buffer.rope,
                            cursor: app.selection.head,
                            sel_anchor: app.selection.anchor,
                            scroll_row: app.scroll_row,
                            mode: &app.mode,
                            mode_colors: &app.config.theme.modes,
                            jump_labels: &app.jump.labels,
                            jump_typed: &app.jump.typed,
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

            // If the terminal was resized, Kitty evicts its pixel cache, so
            // any cached image IDs are invalid — flush them before placing.
            let cur_size = (app.viewport_width as u16, app.viewport_height as u16);
            if cur_size != app.graphics.last_size {
                app.graphics.image_ids.clear();
                app.graphics.last_size = cur_size;
            }

            // Clear visible placements so images that scrolled off screen
            // (or were replaced) disappear.  q=2 suppresses OK responses.
            // Always clear in notebook mode so that cell-output clears take
            // effect even when kitty_image_ids was just emptied by the command.
            if app.graphics.terminal.supports_graphics()
                && (app.notebook.is_some()
                    || !app.graphics.pending.is_empty()
                    || !app.graphics.image_ids.is_empty())
            {
                let _ = kitty::clear_images();
            }

            if app.graphics.terminal.supports_graphics()
                && !app.graphics.pending.is_empty()
                && app.popup.is_none()
            {
                let images = std::mem::take(&mut app.graphics.pending);
                for req in &images {
                    let ptr_key = std::sync::Arc::as_ptr(&req.png_data) as usize;
                    if let Some(&kid) = app.graphics.image_ids.get(&ptr_key) {
                        // Pixel data already cached in the terminal — re-place cheaply.
                        let _ = kitty::place_image(req.col, req.row, kid, req.rows, req.cols);
                    } else {
                        // First time seeing this image — upload pixel data once.
                        let kid = app.graphics.next_id;
                        app.graphics.next_id = if app.graphics.next_id == u32::MAX { 1 } else { app.graphics.next_id + 1 };
                        let _ = kitty::upload_and_place(req.col, req.row, kid, req.rows, req.cols, &req.png_data);
                        app.graphics.image_ids.insert(ptr_key, kid);
                    }
                }

                // Placing images moved the terminal cursor to the last image's
                // origin; put it back where ratatui drew the text cursor so the
                // block cursor doesn't appear stuck on an image.  (When the
                // focused cell has no visible cursor — e.g. a rendered markdown
                // cell — nb_cursor is None and ratatui already hid the cursor.)
                if let Some((cx, cy)) = nb_cursor {
                    use std::io::Write;
                    let mut out = io::stdout();
                    let _ = write!(out, "\x1b[{};{}H", cy + 1, cx + 1);
                    let _ = out.flush();
                }
            } else {
                app.graphics.pending.clear();
            }
        } else {
            // Plain text editor or full-screen focused-cell overlay.
            terminal.draw(|f| {
                ui::render(f, app);
                if let Some(ref popup) = app.popup {
                    let cursor_pos = ui::cursor_screen_pos(app, f.area());
                    crate::popup_ui::render(f, popup, cursor_pos, &app.config.ui);
                }
            })?;
        }

        // Only write cursor-shape OSC sequences when the mode actually changes.
        if app.last_rendered_mode.as_ref() != Some(&app.mode) {
            app.last_rendered_mode = Some(app.mode.clone());
            set_cursor_shape(&app.mode, &app.config.theme.modes);
        }
    }
    Ok(())
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
    execute!(
        stdout,
        LeaveAlternateScreen,
        crossterm::cursor::SetCursorStyle::DefaultUserShape,
    )?;
    let _ = write!(stdout, "\x1b]112\x07");
    let _ = stdout.flush();
    Ok(())
}

fn set_cursor_shape(mode: &Mode, colors: &crate::config::ModeColorsConfig) {
    use crossterm::cursor::SetCursorStyle;
    use std::io::Write;
    let _ = execute!(io::stdout(), SetCursorStyle::SteadyBlock);
    if let Some(color_spec) = crate::theme::color_to_osc_spec(crate::theme::mode_color(mode, colors)) {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b]12;{}\x07", color_spec);
        let _ = stdout.flush();
    }
}
