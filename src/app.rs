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
    mode::Mode,
    notebook::Notebook,
    notebook_state::NotebookState,
    notebook_ui::ImageRequest,
    selection::Selection,
    ui,
};

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

        let buffer = match path {
            Some(p) if !is_notebook => Buffer::from_path(p).unwrap_or_else(|_| {
                let mut b = Buffer::new_empty();
                b.path = Some(std::path::PathBuf::from(p));
                b
            }),
            _ => Buffer::new_empty(),
        };

        let highlighter = Highlighter::new(buffer.path.as_deref());
        let highlight_spans = highlighter
            .highlight(&buffer.rope)
            .unwrap_or_default();

        Ok(Self {
            buffer,
            selection: Selection::point(0),
            scroll_row: 0,
            scroll_col: 0,
            mode: Mode::Normal,
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
        })
    }
}

/// Set up terminal, run the event loop, then restore terminal.
pub fn run(path: Option<&str>) -> Result<()> {
    let config = Config::load().unwrap_or_else(|_| {
        toml::from_str(include_str!("../config/default.toml"))
            .expect("default config must parse")
    });

    let mut app = App::new(path, config)?;

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
        if app.notebook.is_none() {
            // Update scroll to keep cursor in view (text editor only).
            update_scroll_to_fit(terminal, app);
        }

        if app.notebook.is_some() {
            // Notebook rendering path.
            terminal.draw(|f| {
                let size = f.area();
                if size.height >= 3 {
                    // Content area.
                    if let Some((ref nb, ref state)) = app.notebook {
                        let images = crate::notebook_ui::render(f, state, nb, &app.config);
                        app.pending_images = images;

                        // Status bar and command line.
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
                        crate::notebook_ui::render_notebook_status(f, nb, state, ks, status_area);
                        ui::render_command_nb(f, app, cmd_area);
                    }
                }
            })?;

            // Flush Kitty images after ratatui draw.
            if !app.pending_images.is_empty() {
                let _ = kitty::clear_images();
                let images = std::mem::take(&mut app.pending_images);
                for req in &images {
                    let _ = kitty::render_image(req.col, req.row, req.rows, &req.png_data);
                }
            }
        } else {
            terminal.draw(|f| ui::render(f, app))?;
        }

        match event::read()? {
            Event::Key(key) => {
                input::handle_key(app, key);
            }
            Event::Resize(_, _) => {
                // Terminal will redraw on next iteration.
            }
            _ => {}
        }

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
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}
