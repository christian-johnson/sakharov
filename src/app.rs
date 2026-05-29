use anyhow::Result;
use crossterm::{
    event::{self, Event},
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
    lang::lang_to_ext,
    lsp_manager::{DiagnosticSeverity, LspManager},
    mode::Mode,
    notebook::Notebook,
    notebook_state::NotebookState,
    notebook_ui::ImageRequest,
    selection::Selection,
    ui,
};

// ---------------------------------------------------------------------------
// Search state
// ---------------------------------------------------------------------------

pub struct SearchState {
    pub query: String,
    pub matches: Vec<usize>,
    pub current: usize,
    pub active: bool,
    /// True when search was just opened — allows the first typed char to
    /// replace the previous query instead of appending to it.
    pub just_opened: bool,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            active: false,
            just_opened: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Cell edit session
// ---------------------------------------------------------------------------

/// Tracks the notebook cell currently loaded into `app.buffer`.
pub struct CellEditSession {
    pub cell_index: usize,
    /// Kernel language, e.g. `"python"` — becomes the LSP `languageId`.
    pub language: String,
    /// Path of the parent notebook — becomes the `notebookDocument` URI.
    pub notebook_path: std::path::PathBuf,
    /// True while the full-screen cell overlay (Enter from notebook nav) is active.
    pub focused_edit: bool,
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
    pub message: Option<String>,
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
    /// Image draw requests collected by the last notebook render pass.
    pub pending_images: Vec<ImageRequest>,
    /// Maps Arc-pointer-as-usize → Kitty image ID so pixel data is uploaded
    /// only once per image.  Must be cleared whenever outputs change or the
    /// terminal is resized (Kitty evicts pixel cache on resize).
    pub kitty_image_ids: std::collections::HashMap<usize, u32>,
    /// Counter for assigning unique Kitty image IDs (wraps at u32::MAX).
    pub next_kitty_id: u32,
    /// Terminal size at the last frame images were uploaded.  Used to detect
    /// resizes that invalidate Kitty's pixel cache.
    pub kitty_last_size: (u16, u16),
    /// Actual terminal cell pixel dimensions `(cell_h_px, cell_w_px)` queried
    /// from the OS via TIOCGWINSZ.  Used to size image placeholders precisely
    /// so they match what Kitty renders.  `None` until first successful query.
    pub cell_pixel_size: Option<(u16, u16)>,
    /// Which terminal graphics backend is available (Kitty, WezTerm, or none).
    /// Detected once at startup from environment variables.
    pub graphics_terminal: kitty::GraphicsTerminal,
    /// Tracks which cell is loaded in `buffer` and whether the full-screen
    /// overlay is active. Always `Some` while a notebook is open.
    pub notebook_cell_edit: Option<CellEditSession>,
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
    /// Current git branch name (read at startup, refreshed on write).
    pub git_branch: Option<String>,
    /// Code actions returned by the last LSP `textDocument/codeAction` request.
    /// Indexed by popup item payload (as a string-encoded usize).
    pub pending_code_actions: Vec<serde_json::Value>,
    /// (char_pos, label) pairs computed when entering Jump mode.
    pub jump_labels: Vec<(usize, String)>,
    /// Characters typed so far in Jump mode (used to filter labels).
    pub jump_typed: String,
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
    /// Chronological log of every message shown in the minibuffer.
    /// Powers the *Messages* special buffer.
    pub messages_log: Vec<String>,
    /// The last message that was appended to `messages_log`, used to avoid
    /// logging the same message twice when it persists across frames.
    pub last_logged_message: Option<String>,
    /// Persisted rope content for special buffers (currently only *scratch*).
    pub special_buffer_ropes: std::collections::HashMap<String, ropey::Rope>,
    /// When true, the next `FormattingResult` event will also trigger a save.
    pub pending_format_save: bool,
    /// Word prefix at which the last completion popup was dismissed due to no
    /// matches.  While the current prefix extends this value, we skip firing new
    /// completion requests — typing more characters can only reduce results further.
    /// Cleared on Backspace, non-identifier chars, and trigger chars (`.` / `:`).
    pub completion_suppressed_prefix: Option<String>,
    /// In-memory stash of notebooks that have been navigated away from.
    /// Keyed by the canonicalized `.ipynb` path.  When the user navigates back
    /// to a notebook, its state is restored from here rather than reloading from
    /// disk, so unsaved edits are preserved across buffer switches.
    pub notebook_buffers: std::collections::HashMap<std::path::PathBuf, (Notebook, NotebookState)>,
}

impl App {
    /// Returns true when the focused-cell full-screen overlay is active.
    pub fn notebook_focused_edit(&self) -> bool {
        self.notebook_cell_edit
            .as_ref()
            .map_or(false, |s| s.focused_edit)
    }

    /// The language id for the document currently in the editor buffer.
    pub fn current_language(&self) -> Option<&str> {
        if let Some(ref session) = self.notebook_cell_edit {
            return Some(&session.language);
        }
        self.lsp_language.as_deref()
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
        let (buffer, notebook_cell_edit, lsp_language) = if let Some((ref nb, _)) = notebook {
            let lang = nb.metadata.kernel_language.clone();
            let ext = lang_to_ext(&lang);
            let stem = nb.path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "notebook".into());
            let dir = nb.path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let vpath = dir.join(format!("{stem}__cell0.{ext}"));
            let mut buf = Buffer::new_empty();
            if let Some(cell) = nb.cells.first() {
                buf.rope = cell.source.clone();
            }
            buf.path = Some(vpath.clone());
            let session = nb.cells.first().map(|_cell| CellEditSession {
                cell_index: 0,
                language: lang.clone(),
                notebook_path: nb.path.clone(),
                focused_edit: false,
            });
            (buf, session, Some(lang))
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
            (buf, None, lang)
        };

        let mut highlighter = Highlighter::new(buffer.path.as_deref());
        let highlight_spans = highlighter.highlight(&buffer.rope).unwrap_or_default();
        // Compute fold ranges immediately so folding works before the first edit.
        let initial_fold_ranges = highlighter.language
            .map(|lang| crate::fold::compute_fold_ranges(&buffer.rope, lang))
            .unwrap_or_default();

        let initial_mode = if notebook.is_some() { Mode::Notebook } else { Mode::Normal };

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

        let git_diff = buffer
            .path
            .as_deref()
            .map(crate::git::diff_marks)
            .unwrap_or_default();

        let mut keymap = Keymap::default_bindings();
        keymap.apply_custom_bindings(&config.keys);

        Ok(Self {
            buffer,
            selection: Selection::point(0),
            scroll_row: 0,
            scroll_col: 0,
            mode: initial_mode,
            command_buf: String::new(),
            message: None,
            clipboard: String::new(),
            should_quit: false,
            insert_session_active: false,
            highlighter,
            highlight_spans,
            config,
            keymap,
            notebook,
            pending_images: Vec::new(),
            kitty_image_ids: std::collections::HashMap::new(),
            next_kitty_id: 1,
            kitty_last_size: (0, 0),
            cell_pixel_size: None,
            graphics_terminal: kitty::GraphicsTerminal::detect(),
            notebook_cell_edit,
            popup: None,
            lsp: LspManager::new(),
            lsp_language,
            search: SearchState::default(),
            viewport_height: 24,
            viewport_width: 80,
            open_buffers,
            git_diff,
            git_branch: crate::git::current_branch(),
            pending_code_actions: Vec::new(),
            jump_labels: Vec::new(),
            jump_typed: String::new(),
            needs_clear: false,
            highlights_dirty: false,
            diag_by_line: std::collections::HashMap::new(),
            last_rendered_mode: None,
            fold: {
                let mut f = FoldState::default();
                f.ranges = initial_fold_ranges;
                f
            },
            messages_log: Vec::new(),
            last_logged_message: None,
            pending_format_save: false,
            completion_suppressed_prefix: None,
            notebook_buffers: std::collections::HashMap::new(),
            special_buffer_ropes: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "*scratch*".to_string(),
                    ropey::Rope::from_str(
                        ";; This buffer is for notes you don't want to save.\n\
                         ;; Use it for scratch text.\n",
                    ),
                );
                m
            },
        })
    }
}

/// Map a file extension to an LSP language id.
pub fn language_for_path(path: Option<&std::path::Path>) -> Option<&'static str> {
    let ext = path?.extension()?.to_str()?;
    match ext {
        "py" => Some("python"),
        "rs" => Some("rust"),
        "js" | "ts" | "jsx" | "tsx" => Some("javascript"),
        _ => None,
    }
}

/// Set up terminal, run the event loop, then restore terminal.
pub fn run(path: Option<&str>) -> Result<()> {
    let config = Config::load().unwrap_or_else(|e| {
        eprintln!("sv: config error: {e} — using built-in defaults");
        toml::from_str(include_str!("../config/default.toml"))
            .expect("default config must parse")
    });

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

    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    terminal::enable_raw_mode()?;
    crate::theme::initialize_color_cache();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app);

    restore_terminal()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        // Update stored viewport dimensions, then recompute scroll.
        // This runs before every render so the scroll is always based on the
        // current terminal size (handles resize events too).
        if let Ok(size) = terminal.size() {
            app.viewport_height = size.height.saturating_sub(2) as usize;
            app.viewport_width = size.width as usize;
        }
        // Query actual terminal cell pixel dimensions (TIOCGWINSZ).
        // Used to size image placeholders to match Kitty's rendering exactly.
        if let Ok(ws) = crossterm::terminal::window_size() {
            if ws.columns > 0 && ws.rows > 0 && ws.width > 0 && ws.height > 0 {
                app.cell_pixel_size = Some((ws.height / ws.rows, ws.width / ws.columns));
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
            // Recompute foldable ranges from the updated syntax tree.
            if let Some(lang) = app.highlighter.language {
                app.fold.ranges =
                    crate::fold::compute_fold_ranges(&app.buffer.rope, lang);
                // Discard any stored folds whose start lines no longer exist.
                let valid: std::collections::BTreeSet<usize> =
                    app.fold.ranges.iter().map(|&(s, _)| s).collect();
                app.fold.folded.retain(|s| valid.contains(s));
            } else {
                app.fold.ranges.clear();
                app.fold.folded.clear();
            }
        }

        // After an external program (file picker etc.) suspends and resumes the
        // terminal, ratatui's diffing state is stale — force a full repaint.
        if app.needs_clear {
            app.needs_clear = false;
            let _ = terminal.clear();
        }

        if app.notebook.is_some() && !app.notebook_focused_edit() {
            // Notebook multi-cell view — the focused cell is in app.buffer.
            terminal.draw(|f| {
                let size = f.area();
                let mut nb_cursor: Option<(u16, u16)> = None;

                if size.height >= 3 {
                    if let Some((ref nb, ref state)) = app.notebook {
                        let active = crate::notebook_ui::ActiveCellView {
                            rope: &app.buffer.rope,
                            cursor: app.selection.head,
                            sel_anchor: app.selection.anchor,
                            scroll_row: app.scroll_row,
                            mode: &app.mode,
                            jump_labels: &app.jump_labels,
                            jump_typed: &app.jump_typed,
                        };
                        let (images, cursor_pos) =
                            crate::notebook_ui::render(f, state, nb, &active, &app.lsp.diagnostics, &app.config.notebook, app.cell_pixel_size);
                        app.pending_images = images;
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
                        let ks = nb.kernel.as_ref().map(|k| &k.status);
                        crate::notebook_ui::render_notebook_status(
                            f, nb, state, ks, status_area, app.mode.label(),
                        );
                        ui::render_command_nb(f, app, cmd_area);
                    }
                }
                if let Some(ref popup) = app.popup {
                    crate::popup_ui::render(f, popup, nb_cursor, &app.config.ui);
                }
            })?;

            // If the terminal was resized, Kitty evicts its pixel cache, so
            // any cached image IDs are invalid — flush them before placing.
            let cur_size = (app.viewport_width as u16, app.viewport_height as u16);
            if cur_size != app.kitty_last_size {
                app.kitty_image_ids.clear();
                app.kitty_last_size = cur_size;
            }

            // Clear visible placements so images that scrolled off screen
            // (or were replaced) disappear.  q=2 suppresses OK responses.
            // Always clear in notebook mode so that cell-output clears take
            // effect even when kitty_image_ids was just emptied by the command.
            if app.graphics_terminal.supports_graphics()
                && (app.notebook.is_some()
                    || !app.pending_images.is_empty()
                    || !app.kitty_image_ids.is_empty())
            {
                let _ = kitty::clear_images();
            }

            if app.graphics_terminal.supports_graphics()
                && !app.pending_images.is_empty()
                && app.popup.is_none()
            {
                let images = std::mem::take(&mut app.pending_images);
                for req in &images {
                    let ptr_key = std::sync::Arc::as_ptr(&req.png_data) as usize;
                    if let Some(&kid) = app.kitty_image_ids.get(&ptr_key) {
                        // Pixel data already cached in the terminal — re-place cheaply.
                        let _ = kitty::place_image(req.col, req.row, kid, req.rows, req.cols);
                    } else {
                        // First time seeing this image — upload pixel data once.
                        let kid = app.next_kitty_id;
                        app.next_kitty_id = if app.next_kitty_id == u32::MAX { 1 } else { app.next_kitty_id + 1 };
                        let _ = kitty::upload_and_place(req.col, req.row, kid, req.rows, req.cols, &req.png_data);
                        app.kitty_image_ids.insert(ptr_key, kid);
                    }
                }
            } else {
                app.pending_images.clear();
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
            set_cursor_shape(&app.mode);
        }

        // Block up to 16 ms for the first event (≈60 fps idle rate).
        // Once an event arrives, drain every additional event that is already
        // queued before yielding back to the render loop.  This ensures that a
        // burst of key-repeat events is consumed in a single frame rather than
        // trickling in one per frame, which caused the cursor to keep moving
        // briefly after a key was released.
        if event::poll(std::time::Duration::from_millis(16))? {
            loop {
                match event::read()? {
                    Event::Key(key) => input::handle_key(app, key),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
                if !event::poll(std::time::Duration::from_millis(0))? {
                    break;
                }
            }
        }

        crate::exec::process_lsp_events(app);
        crate::exec::process_kernel_events(app);

        // Append any new minibuffer message to the *Messages* log.
        if app.message.as_deref() != app.last_logged_message.as_deref() {
            if let Some(ref msg) = app.message {
                app.messages_log.push(msg.clone());
                app.last_logged_message = app.message.clone();
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn restore_terminal() -> Result<()> {
    use std::io::Write;
    terminal::disable_raw_mode()?;
    let mut stdout = io::stdout();
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
