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
    highlight::{Highlighter, Span},
    input,
    keymap::Keymap,
    kitty,
    lsp_manager::LspManager,
    mode::Mode,
    notebook::Notebook,
    notebook_state::NotebookState,
    notebook_ui::ImageRequest,
    selection::Selection,
    ui,
};

/// Tracks an active full-screen cell-edit overlay session.
///
/// Carries everything needed for future LSP `notebookDocument` integration:
/// - `notebook_path` → the `notebookDocument` URI (`file:///…/notebook.ipynb`)
/// - `cell_index` + `cell_id` → stable cell identity; cell URI = `{notebook_path}#{cell_id}`
/// - `language` → LSP `languageId` for the cell's virtual `textDocument`
///
/// When the overlay is active, `app.buffer` holds the cell source and
/// `app.buffer.path` is a virtual path with the correct extension for syntax
/// highlighting (e.g. `notebook__cell2.py`). That path becomes the
/// `textDocument` URI when LSP is wired up.
pub struct CellEditSession {
    pub cell_index: usize,
    /// Stable cell id from the `.ipynb` — becomes the fragment of the LSP cell URI
    /// (`notebookDocument` cell `document` field). Unused until Phase 3 LSP.
    #[allow(dead_code)]
    pub cell_id: String,
    /// Kernel language, e.g. `"python"` — becomes the LSP `languageId`.
    pub language: String,
    /// Path of the parent notebook — becomes the `notebookDocument` URI.
    pub notebook_path: std::path::PathBuf,
}

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
    /// When Some, the plain-text editor renders over the notebook view,
    /// allowing full Helix-style editing of the focused cell.
    pub notebook_cell_edit: Option<CellEditSession>,
    /// Active floating popup overlay, if any.
    pub popup: Option<crate::popup::Popup>,
    /// LSP client manager — one server per language.
    pub lsp: LspManager,
    /// Language id of the currently edited document (e.g. "python", "rust").
    /// Updated when opening a cell edit overlay so LSP routes to the right server.
    pub lsp_language: Option<String>,
    /// True when the full-screen focused cell editor is active (Enter in notebook).
    /// False = multi-cell notebook view with in-place editing.
    pub notebook_focused_edit: bool,
    /// Current search query (set when entering Search mode).
    pub search_query: String,
    /// Char indices of the start of each match for the last search.
    pub search_matches: Vec<usize>,
    /// Index into search_matches pointing at the current match.
    pub search_current: usize,
    /// Visible text rows in the editor area — updated each render frame.
    pub viewport_height: usize,
}

impl App {
    /// Create a new App, loading `path` if provided.
    pub fn new(path: Option<&str>, config: Config) -> Result<Self> {
        // Detect .ipynb and load as a notebook.
        let is_notebook = path
            .map(|p| p.ends_with(".ipynb"))
            .unwrap_or(false);

        let notebook = if is_notebook {
            let p = path.expect("checked above");
            match Notebook::from_path(std::path::Path::new(p)) {
                Ok(nb) => Some((nb, NotebookState::new())),
                Err(e) => {
                    // Fall through to regular buffer with an error message.
                    eprintln!("ki: failed to load notebook: {e}");
                    None
                }
            }
        } else {
            None
        };

        // For notebooks, pre-load cell 0 into the buffer so editing works immediately.
        let (buffer, notebook_cell_edit, lsp_language) = if let Some((ref nb, _)) = notebook {
            let lang = nb.metadata.kernel_language.clone();
            let ext = nb_lang_ext(&lang);
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
            let session = nb.cells.first().map(|cell| crate::app::CellEditSession {
                cell_index: 0,
                cell_id: cell.id.clone(),
                language: lang.clone(),
                notebook_path: nb.path.clone(),
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

        let highlighter = Highlighter::new(buffer.path.as_deref());
        let highlight_spans = highlighter.highlight(&buffer.rope).unwrap_or_default();

        let initial_mode = if notebook.is_some() { Mode::Notebook } else { Mode::Normal };

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
            keymap: Keymap::default_bindings(),
            notebook,
            pending_images: Vec::new(),
            notebook_cell_edit,
            notebook_focused_edit: false,
            popup: None,
            lsp: LspManager::new(),
            lsp_language,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_current: 0,
            viewport_height: 24,
        })
    }

    /// The language id for the document currently in the editor buffer.
    /// In cell-edit mode this reflects the cell's language.
    pub fn current_language(&self) -> Option<&str> {
        if let Some(ref session) = self.notebook_cell_edit {
            return Some(&session.language);
        }
        self.lsp_language.as_deref()
    }
}

fn nb_lang_ext(lang: &str) -> &str {
    match lang {
        "python" | "python3" => "py",
        "javascript" | "js" => "js",
        "rust" => "rs",
        _ => "txt",
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
        eprintln!("ki: config error: {e} — using built-in defaults");
        toml::from_str(include_str!("../config/default.toml"))
            .expect("default config must parse")
    });

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
                // Expose the notebook language so which-key hints and other
                // UI that checks current_language() work in navigate mode.
                if app.lsp_language.is_none() {
                    app.lsp_language = Some(lang);
                }
            }
        }
    }

    // Install panic hook to restore terminal on panic
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    // Set up terminal
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app);

    // Always restore terminal
    restore_terminal()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        // Always update scroll so intra-cell cursor stays visible whether the
        // notebook view or the full-screen overlay is active.
        update_scroll_to_fit(terminal, app);
        if let Ok(size) = terminal.size() {
            app.viewport_height = size.height.saturating_sub(2) as usize;
        }

        if app.notebook.is_some() && !app.notebook_focused_edit {
            // Notebook multi-cell view — the focused cell is in app.buffer.
            terminal.draw(|f| {
                let size = f.area();
                // Track the cursor position returned by the notebook renderer so
                // completion popups anchor to the right spot inside the cell.
                let mut nb_cursor: Option<(u16, u16)> = None;

                if size.height >= 3 {
                    if let Some((ref nb, ref state)) = app.notebook {
                        let active = crate::notebook_ui::ActiveCellView {
                            rope: &app.buffer.rope,
                            cursor: app.selection.head,
                            sel_anchor: app.selection.anchor,
                            scroll_row: app.scroll_row,
                            mode: &app.mode,
                        };
                        let (images, cursor_pos) =
                            crate::notebook_ui::render(f, state, nb, &app.config, &active);
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
                    // Use the in-cell cursor position so completions appear
                    // directly below the insertion point, not at row 0.
                    crate::popup_ui::render(f, popup, nb_cursor);
                }
            })?;

            if !app.pending_images.is_empty() {
                let _ = kitty::clear_images();
                let images = std::mem::take(&mut app.pending_images);
                for req in &images {
                    let _ = kitty::render_image(req.col, req.row, req.rows, &req.png_data);
                }
            }
        } else {
            // Plain text editor: regular file or full-screen focused-cell overlay.
            terminal.draw(|f| {
                ui::render(f, app);
                if let Some(ref popup) = app.popup {
                    let cursor_pos = ui::cursor_screen_pos(app, f.area());
                    crate::popup_ui::render(f, popup, cursor_pos);
                }
            })?;
        }

        // Update terminal cursor shape to reflect the current mode.
        set_cursor_shape(app.mode.clone());

        // Poll with a short timeout so LSP responses are processed promptly
        // without requiring a keypress to trigger the next iteration.
        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    input::handle_key(app, key);
                }
                Event::Resize(_, _) => {
                    // Terminal will redraw on next iteration.
                }
                _ => {}
            }
        }

        // Process any pending LSP responses from background threads.
        // Runs every ~50 ms regardless of whether a key was pressed.
        crate::exec::process_lsp_events(app);

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Adjust scroll_row and scroll_col so the cursor is within the visible area.
fn update_scroll_to_fit(
    terminal: &Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) {
    let size = terminal.size().unwrap_or_default();
    let visible_rows = size.height.saturating_sub(2) as usize; // minus status + cmd bar
    let gutter_width: usize = if app.config.editor.line_numbers { 5 } else { 0 };
    let visible_cols = (size.width as usize).saturating_sub(gutter_width);

    if visible_rows == 0 || visible_cols == 0 {
        return;
    }

    let rope = &app.buffer.rope;
    if rope.len_chars() == 0 {
        app.scroll_row = 0;
        app.scroll_col = 0;
        return;
    }

    let pos = app.selection.head.min(rope.len_chars().saturating_sub(1));
    let line_idx = rope.char_to_line(pos);
    let scroll_off = app.config.editor.scroll_off;

    // Vertical: scroll up
    let top_bound = line_idx.saturating_sub(scroll_off);
    if app.scroll_row > top_bound {
        app.scroll_row = top_bound;
    }
    // Vertical: scroll down
    let bottom_bound = line_idx + scroll_off;
    if bottom_bound >= app.scroll_row + visible_rows {
        app.scroll_row = bottom_bound.saturating_sub(visible_rows) + 1;
    }

    // Horizontal: compute display column of cursor (tabs expand to tab stops)
    let line_start = rope.line_to_char(line_idx);
    let line_str = rope.line(line_idx);
    let cursor_off = pos - line_start;
    let tab_width = app.config.editor.tab_width;
    let mut display_col: usize = 0;
    for i in 0..cursor_off {
        let c = line_str.char(i);
        display_col += if c == '\t' {
            tab_width - (display_col % tab_width)
        } else {
            unicode_display_width(c)
        };
    }

    // Horizontal: scroll left
    if display_col < app.scroll_col {
        app.scroll_col = display_col;
    }
    // Horizontal: scroll right
    if display_col >= app.scroll_col + visible_cols {
        app.scroll_col = display_col.saturating_sub(visible_cols) + 1;
    }
}

fn unicode_display_width(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(1)
}

fn restore_terminal() -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(
        io::stdout(),
        LeaveAlternateScreen,
        crossterm::cursor::SetCursorStyle::DefaultUserShape,
    )?;
    Ok(())
}

fn set_cursor_shape(mode: Mode) {
    use crossterm::cursor::SetCursorStyle;
    let shape = match mode {
        Mode::Insert => SetCursorStyle::BlinkingBar,
        _ => SetCursorStyle::SteadyBlock,
    };
    let _ = execute!(io::stdout(), shape);
}
