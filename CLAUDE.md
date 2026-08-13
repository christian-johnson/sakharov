# sakharov — personal TUI text editor

## What this is

A from-scratch TUI text editor written in Rust, built for personal use.
Invoked as `sv [file]`. Binary at `target/debug/sv` (or `target/release/sv`).

## Current status — Phase 3 complete

### Phase 1 (plain text editor) — complete
- Helix-style (selection-first) modal editing: Normal, Insert, Select modes
- Full motion set: `h/j/k/l`, `w/b/e`, `W/B/E`, `0/^/$`, `gg/G`, `f/t/F/T`
- Edit operations: `d/c/y/p/P`, `u` undo (session-coalesced), `U` redo
- `o/O` open line, `i/a/I/A` insert variants, `v` select mode, `x` select line, `%` select all
- `:` command line — every `Command` variant is accessible by name (see `docs/commands.md`)
- Tree-sitter syntax highlighting: Rust, Python, JavaScript, TOML, JSON, YAML, Bash, Go, C,
  HTML, CSS (see `highlight::Language` + `lang.rs`; a unit test compiles every grammar's
  highlight query and asserts it produces spans, so a broken query can't silently disable
  highlighting). Folding (`fold.rs`) and `gc` comment syntax cover the new languages too.
  **Language detection falls back to filename for extensionless shell dotfiles**
  (`.zshrc`, `.bashrc`, `.bash_profile`, `.profile`, ... — see `lang::SHELL_DOTFILES`) since
  `Path::extension()` returns `None` for a dotfile with no second `.`; both
  `Language::from_path` (highlighting) and `app::language_for_path` (LSP language id) check
  this list after the normal extension match fails
- Markdown (`.md`/`.markdown`/`.qmd`): custom (non-tree-sitter) highlighting in `markdown.rs` —
  per-level header colours, **bold**/*italic*, inline `code`/fenced blocks, links, blockquotes,
  list markers — plus header-section + code-fence folding (same `zc/zo/za` interface)
- Scroll with configurable `scroll_off`; horizontal scroll tracks cursor correctly
- **Vertical motion is visual when text is soft-wrapped** — `j`/`k` (and the arrows, and
  `Ctrl+d`/`Ctrl+u`, which are runs of them) step one *screen row*, preserving the display
  column, instead of skipping a whole wrapped paragraph. `motion::move_visual_up/_down` are
  geometry-agnostic: the caller passes a `motion::Wrap` whose `row_starts(line)` comes from
  whichever rule the view is drawn with — `render_util::scan_wrap_rows` (plain editor, a
  **hard** break at the text width) or `render_util::wrap_segments` (cells, **word**
  boundaries). `exec::wrap_kind` picks between them and returns `None` when nothing wraps,
  so the logical `motion::move_up/_down` still run. `0`/`^`/`$` stay logical on purpose —
  they are the only way to reach the real start/end of a long line.
  `notebook_move_down`/`_up` cross cells only at the last/first *visual* row
  (`at_last_visual_row`/`at_first_visual_row`), so a wrapped cell is walked row by row
  before `j` descends into its output block
- **Status line** (`statusline.rs`) — a single starship-style renderer shared by the plain
  editor and the notebook view. Layout is config-driven: `[statusline] left/right` (and
  `[statusline.notebook] left/right`) are ordered lists of module names, packed left /
  flush-right with automatic per-module padding. An unknown name renders as literal text
  (usable as a custom separator, e.g. `"│"`). Call sites build a `statusline::Ctx` and call
  `statusline::render(frame, area, ctx, left, right, separator, styles)`.

  **Available modules** (all aliases are interchangeable):

  | Module | Aliases | Renders | Visibility |
  |--------|---------|---------|------------|
  | `mode` | — | Coloured chip: `NOR` `INS` `SEL` `CMD` … | always |
  | `file` | `filename` | Filename + ` [+]` when unsaved | always |
  | `git` | `branch`, `git_branch` | ` branch-name` | hidden outside git repo |
  | `diagnostics` | `diag` | `●N` errors (red) · `◆N` warnings (yellow) | hidden when zero |
  | `position` | `pos` | `line:col` (1-based) | always |
  | `scroll` | `scroll_percent` | `N%` through file | always |
  | `spinner` | — | Animated Braille glyph (cyan) | hidden when idle |
  | `cell` | `cell_position` | `current/total` cell index | notebook only |
  | `kernel` | — | `[⠿ starting]` / `[idle]` / `[⠿ busy]` / `[dead]` / `[no kernel]` | notebook only |

  `kernel` folds the live spinner into itself when starting/busy. The default notebook layout
  includes the standalone `spinner` module anyway — it surfaces background work the kernel chip
  doesn't cover (in-flight LSP requests, exports) — and `statusline::render` automatically drops
  the standalone module while a `kernel` module in the layout is animating
  (`kernel_folds_spinner`), so the two never show together. `cell` and `kernel` produce nothing
  in the plain editor.

  **Separator / powerline** — `separator = ">"` (or `"/"`, `"\\"`, `"round"`) activates
  powerline mode: filled transition glyphs (Nerd Fonts required) tinted with adjacent module
  background colors. Any other non-empty string is printed literally between modules.

  **Per-module colors** — `[statusline.styles]` maps module names to `#rrggbb` hex strings.
  In powerline mode these are background colors (fg auto-chosen for contrast); in literal
  mode they override the foreground (text) color.

  **Per-mode colors** — `[theme.modes]` maps mode names (`normal`, `insert`, `select`,
  `command`, `notebook`, `goto`, `jump`, `fold`) to `#rrggbb` hex strings, overriding the
  default ANSI color for that mode's chip, cursor, and powerline tint.

  A "boiling" Braille spinner (`spinner.rs`) appears while a background task runs (a notebook
  cell executing, an in-flight LSP request) — it flips one random dot of an 8-dot Braille cell
  per tick rather than cycling fixed frames. Advanced once per frame from the run loop via
  `Spinner::update(background_active)`; surfaced via the `spinner` module (and folded into the
  `kernel` module's `[⠿ busy]` indicator)
- **Themes** (`theme.rs` + `config/themes/`): every renderer color comes from the resolved,
  process-wide active `Theme` (`theme::active()`, an `Arc` behind an `RwLock`).  Themes are TOML
  files (`[palette]`/`[ui]`/`[syntax]`/`[markdown]`/`[modes]`/`[notebook]`, all keys optional) —
  22 built-ins embedded from `config/themes/` (tokyonight ×4, catppuccin ×4, nord ×2,
  rose-pine ×3, dracula, gruvbox ×2, onedark, solarized ×2, kanagawa, everforest, monokai) plus
  user themes in `~/.config/sakharov/themes/*.toml` (a same-name user file shadows a built-in).
  Selected via `[theme] name = "..."` in config; `:theme` opens a picker with **live
  preview** (every selection move re-applies the highlighted theme via
  `exec::preview_selected_theme` on `PopupAction::Continue`; ESC/dismiss reverts to the
  committed `config.theme.name` via `revert_theme_preview`, Enter commits through
  `apply_theme`), `:theme <name>` switches directly (session-only; the message names the
  config key that persists it). Any
  theme key can be overridden under `[theme]` in config.toml — deep-merged over the chosen
  theme and kept across `:theme` switches. Resolution derives unset keys: syntax fallback
  chains (`number`→`constant`, `property`→`variable`→fg), bg/fg blends for the chrome once
  `ui.background` is set, and finally the classic terminal-ANSI defaults — `"default"` is
  exactly the old terminal-inherited look, no background painting (a test pins this).
  `config/themes/example.toml` is the fully commented schema reference (kept valid by a
  test); user docs in `docs/themes.md`
- Block cursor (white in Normal, cyan in Insert); hardware cursor positioned via `frame.set_cursor_position`
- Ctrl+S saves; Ctrl+C shows quit hint
- Config at `~/.config/sakharov/config.toml` — deep-merged over compiled-in `config/default.toml`.
  Search order: `$XDG_CONFIG_HOME`, then `~/.config`, then platform-native `dirs::config_dir()`.
  Covers `[theme]` (theme `name` + inline color overrides, incl. `[theme.modes]` per-mode
  chip/cursor colors), `tab_width`, `expand_tabs`,
  line numbers (absolute + relative), `scroll_off`, `git_gutter`, `word_wrap`, `max_undo`,
  `crash_recovery`, `lsp_signature_throttle_ms`, format-on-save, file-picker limits/external
  command, UI popup sizing +
  `jump_keys` + `symbol_icons` + `command_history`, `[statusline]` modeline layout (left/right
  module lists, `separator`, `[statusline.styles]` color overrides, separate notebook variant),
  notebook (`image_rows`/output caps), `[language_servers]`, `[formatters]`,
  `[languages.<lang>]` per-language overrides (`indent_width`).
  **Config loading is infallible** — any syntax error or type mismatch in the user file is
  reported to stderr and the built-in defaults are used instead.
- `/` and `?` incremental search, `n/N` cycle matches
- `gw` jump mode (2-char labels over visible word starts)
- **Code folding** (`fold.rs`): `za` toggle, `zc`/`zo` close/open one level (repeat to walk
  outward/inward), `zA` toggle-all, `zM`/`zR` close/open all, and **`zt`/`zT` fold or unfold
  every block *like the one at the cursor*** — matched on the `FoldRange`'s
  `(kind, depth, label)` identity, where `label` is the owning key for languages that have
  keys (JSON pairs, YAML mappings). That triple is why `zt` on a JSON `"payload": {…}` folds
  every other `payload` and leaves the `metadata` beside it open, and why `zt` on one record
  of a JSON log collapses every record. `depth` comes from `fold::assign_depths`
  (containment-based, applied by *both* producers — tree-sitter and `markdown.rs` — so
  "same nesting level" means one thing). `zt` folds *regions*, so a scalar field on a single
  line has nothing to collapse; hiding scalar rows across records is a different mechanism
  and is not implemented.
  The `z` which-key popup is generated from `exec::fold_hints`, pinned to
  `input::fold_command` by `fold_hints_only_advertise_real_bindings` — the same
  hint-vs-dispatch pairing as the `g` sub-mode. It exists because `zo`/`zc` were once
  documented as fold open/close while unconditionally running the notebook
  output-expand command, so in a plain file they answered "Not a notebook".
- Git gutter marks, word wrap toggle
- Multiple buffers (`H`/`L` cycle prev/next), clipboard integration
- **Bracketed paste** — enabled at startup (`EnableBracketedPaste`, released in
  `restore_terminal`); a terminal paste arrives as one `Event::Paste` and goes to
  `input::handle_paste`, which inserts it **verbatim**. It must never be replayed through
  the per-key handlers: the Enter handler auto-indents, so every embedded newline would add
  the enclosing block's indent on top of the pasted line's own and the block staircases
  right. `handle_paste` routes to the open popup's filter / the command line / the search
  query (newlines flattened) when one of those owns the keyboard, and outside Insert it
  behaves like `P` (replaces the selection)
- Auto-indent on Enter, format-on-save (`:fmt` or configurable)
- `gc` comment-region places the markers at the region's **shallowest indent**, not column 0,
  so a commented function body keeps its indentation and relative structure (and the
  uncomment path, which strips the prefix at each line's own indent, round-trips exactly)
- `indent-region` (`>` / `Ctrl+>`) / `dedent-region` (`<` / `Ctrl+<`) shift the selected lines by one indent unit
- **Spaces, never tabs, by default** — Tab key and all auto-indent insert `tab_width` spaces.
  `editor.expand_tabs` (default `true`) controls this; set `false` to indent with real tabs.
  **Indent width is language-aware**: `[languages.<lang>] indent_width` overrides
  `editor.tab_width` per language (defaults ship 2 for js/json/yaml/toml/html/css/markdown;
  Python/Rust follow the 4-space default). Call sites use `App::indent_unit()` /
  `App::indent_width()`; the raw helper is `indent::unit(expand_tabs, width)`
- **`o` is indent-aware from anywhere on the line** — open-line-below evaluates the indent
  trigger (`:`/`{`/`(`/`[`) against the whole line, not just the text before the cursor.
  In Markdown (`.md`/`.qmd` buffers and markdown notebook cells, via
  `App::buffer_is_markdown`), Enter and `o` continue list items — `- `/`* `/`+ ` bullets,
  `1.` → `2.` ordered markers, `- [ ]` task boxes, `> ` quotes
  (`indent::markdown_list_continuation`). Enter on an *empty* item ends the list
- **Fuzzy pickers / telescope-style popups** (see `popup*.rs`):
  - **Space** — command palette (all named commands, filterable)
  - **Ctrl+O** — file picker (built-in fuzzy file list, or an external picker like yazi/fzf
    via the `file_picker` config command; built-in is bounded by `file_picker_max_files`/`max_depth`)
  - **Ctrl+F** — grep current buffer; **Ctrl+G** — grep project (ripgrep/grep). Both are two-phase
    popups: type to filter, ESC to switch to `j/k` navigation, Enter to jump
  - `gb` buffer picker, `gs` symbol picker (tree-sitter symbols), `gD` diagnostic picker
  - Command palette floats recently-used commands toward the top (recency is a
    tiebreaker only — better fuzzy match always wins). `ui.command_history` =
    `session` (default, in-memory) / `global` (persisted to state dir) / `off`. See `history.rs`
- **Special buffers**: `*scratch*` (`:scratch`) and `*Messages*` (`:messages`, the message log).
  Scratch contents are stashed across buffer switches; `:bd` skips back to a real file when possible
- **Crash recovery** (`recovery.rs`): while a buffer has unsaved edits, its contents are
  periodically flushed (debounced, atomic, `0600`) to `$XDG_STATE_HOME/sakharov/recovery/`
  keyed by a path hash (literal `scratch` for the scratch buffer). Removed on clean save/quit,
  so a leftover file means an unclean exit → prompt to Restore/Discard on reopen. Covers files,
  scratch, and notebooks. `editor.crash_recovery = false` disables it. Shared state dir helper:
  `config::state_dir()`
- **Goto sub-mode** (`g` prefix): `gg`/`ge` file start/end, `gh`/`gl` line first-non-ws / end,
  `gd` definition, `gr` references, `gy` type-definition, `gi` implementation, `ga` code-actions,
  `gk` documentation, `gw` jump, `gc` comment-region, `gz` center cursor, `gs`/`gb`/`gD` pickers

### Phase 2 (Jupyter notebooks) — complete
- Opens `.ipynb` files automatically in the notebook view
- Displays cells as a vertical stack: code (syntax-highlighted), markdown, raw
- Bordered cells: rounded border (unfocused) / thick border (focused); background `Rgb(20,20,30)`
- Border colour encodes execution state: **dim blue** = unrun, **bright blue** = executing, **green** = success, **red** = error
- Cell header (`[N] CODE (python)`) lives in the top border line itself
- **No separate notebook mode** — the focused cell is edited in place with the ordinary
  Normal/Insert/Select modes, exactly like a plain buffer. While a notebook is open a small
  override map shadows the normal bindings: `J`/`K` page half a screen down/up
  (`Command::PageDown`/`PageUp`), `N`/`M` move to the next/previous cell (so `N` shadows
  search-prev inside a notebook), and
  `Ctrl+E` executes the focused cell (`Shift+Enter`/`Ctrl+Enter` also execute, but only on
  terminals with keyboard-enhancement reporting — see `app::run`; the Kitty keyboard protocol
  is force-enabled on Kitty and Ghostty even when the support query goes unanswered
  (`GraphicsTerminal::implements_kitty_keyboard`), so Shift/Ctrl+Enter work out of the box;
  WezTerm relies on the query since its support is opt-in; otherwise a modified Enter
  arrives as a bare Enter). Cell-execution keys are handled in `input::handle_key` before mode
  dispatch so they fire from Insert too. A plain `j` on a cell's last
  line steps into that cell's **output block** (so long errors/streams scroll into view) and
  then into the next cell; `k` is the exact inverse (climb the output block back to the source,
  then up into the previous cell — landing on *its* last output row when it has one). The
  output cursor is `NotebookState.output_row` (`Some(visual_row)` while browsing outputs, reset
  to `None` by any command other than `j`/`k`/`PageUp`/`PageDown`; horizontal motions
  (`h`/`l`/`w`/`b`/`0`/`$`/…) are **swallowed** while browsing — the output is read-only and has
  no horizontal scroll, so they'd otherwise snap the cursor back to the hidden source); a block
  cursor is drawn on that output row and the source cursor is hidden. Paging is literally a run of `j`/`k` steps
  (`notebook_vertical` + `notebook_move_down`/`_up`), so it crosses cells and output blocks
  too. Cell management (new/delete/clear-outputs/
  cell-type/structural-undo) has no default key — use the command palette or `:` command line
- **Output truncation is per-cell and expandable** — long output is capped at
  `notebook.max_output_lines` (tracebacks at `max_traceback_lines`) with a
  `... (N more lines — zO to expand)` row. `zO` (fold sub-mode) / `:expand-output`
  (`Command::NotebookToggleOutputExpand`) toggles `NotebookState.expanded_outputs` for the
  focused cell, lifting the caps so every line becomes a real output row that `j`/`k` and the
  row-granular scroll reach. The caps are resolved once into a `notebook_ui::OutputLimits`
  (`OutputLimits::new(&config.notebook, expanded)`) which is threaded through *both* the
  height model and the renderer — they must derive it identically or cell heights drift from
  what is drawn. `expanded_outputs` is keyed by cell index, so `after_structural_edit` clears it
- **Clickable error tracebacks (jump-to-line)** — the kernel compiles each cell under its
  stable **cell id** as the filename (sent as a `__KI_META__<id>` control line before the code,
  stripped by the runner so line numbers stay 1-based to the cell) and registers the source with
  `linecache`, so a traceback both shows the offending source line inline *and* reports
  `File "<id>", line N`. `notebook::build_error_output` (runtime) resolves those `<id>` frames
  against the cell list into `Output::Error.frames: Vec<ErrorFrame>` (`tb_index`, `cell_id`,
  `cell_number`, 0-based `line`) and rewrites the filename to a friendly `Cell [N]` label;
  `frames` is runtime-only (not nbformat) and is rebuilt on reload from those labels by
  `frames_from_display`. On a frame row only the *visible text span* is recoloured + underlined
  (`notebook_ui::draw_traceback_row` — the underline must not bleed across the row's padding),
  so it reads like a hyperlink; and there are
  two ways to follow them: **`Enter`** while the output cursor (`output_row`) sits on a frame row
  (`Command::NotebookFollowError`, bound in the notebook keymap; `error_frame_at_output_row`
  maps the row → frame — and the command is exempted from the per-command `output_row` reset in
  `execute()`), and **`:goto-error`** (`Command::NotebookGotoError`) which jumps straight to the
  focused cell's *innermost* frame. Both go through `exec::jump_to_notebook_cell_line` /
  `resolve_error_frame` (id-preferred, cell-number fallback), and a failing cell's completion
  message hints at both. Frames may target another cell when the culprit is a function defined
  elsewhere
- **Output text is navigable, selectable, and copyable like real text** — the output cursor
  (`NotebookState.output_row` + `output_col`) is a char position, not just a row index:
  `h`/`l`/`w`/`b`/`e`/`W`/`B`/`E`/`0`/`^`/`$` move it by reusing `motion::*` against a **virtual
  rope** built from the block's rendered rows (`notebook_ui::output_rows_content` /
  `output_virtual_rope` — one rope line per output row, in `output_row`'s index space), so
  output-text motion behaves exactly like the plain buffer (including this app's own
  in-line-only `h`/`l`, unlike word motions which do cross rows through the rope's line
  breaks) without a second hand-rolled implementation of word/char boundaries. `v` while
  browsing output starts a selection (`NotebookState.output_anchor`, seeded immediately so a
  `y` right after `v` still works); motions extend it, rendered as a background tint per
  affected row (`OutputCtx::sel`, intersected per-row in `OutputCtx::advance`); `y` yanks the
  spanned text to the system clipboard (`exec::yank_output_selection`) instead of the buffer's
  own yank. Esc collapses the selection but stays in the output block. Because `Mode::Select`
  never consults the notebook keymap (see `input.rs`), `Enter`/`NotebookFollowError` is
  naturally unreachable while a selection is active — link-follow only ever fires on a bare
  cursor. Image rows are addressable too (as a one-line `[image]` placeholder), so a
  selection/motion can pass through an image without special-casing it
- **The output cursor never renders under an image's own pixels** — a terminal's native cursor
  can be visually swallowed by whatever the Kitty graphics protocol painted on top of it. A
  reserved `IMAGE_GUTTER` (2 cols, matching the text rows' own left pad) keeps every image
  starting two columns right of the cell border; both the height model
  (`image_available_cols`, used by `cell_output_rows`/`output_rows_content`) and the renderer
  (`render_mime_data`) size the image against that narrowed width so they never disagree on
  row count, and the cursor for any row inside an image is drawn in that gutter instead
  (skinny, always on a color the app controls, never Kitty's raster)
- **Persistent kernel session** — one Python subprocess per notebook; namespace shared across all cells
  - Auto-detected venv: checks `.venv`, `venv`, `.env`, `env` in notebook dir and cwd before falling back to `python3`
  - Runner script embedded in binary; the editor sends an optional `__KI_META__<cell-id>` line,
    then a code block terminated by `__KI_CODE_END__` (the id becomes the compile filename — see
    clickable tracebacks above)
  - `exec(compile(code, '<cell>', 'exec'), shared_ns)` — full statement support, persistent imports/variables
- **Asynchronous, streaming execution** — nothing about the kernel ever blocks the UI:
  - **Kernel startup is async**: `KernelSession::new` spawns python and returns immediately
    with `KernelStatus::Starting`; the reader thread performs the `__KI_READY__` handshake and
    sends `KernelMessage::Ready`, which flips the status to `Idle` (and logs "Kernel ready").
    The status line shows `[⠿ starting]` while booting
  - `KernelSession::start_execution` writes the code and returns immediately; the background
    reader thread parses one JSON message per line (`{"t":"stream"|"image"|"error"|"done"}`)
    onto an mpsc channel
  - `exec::process_kernel_events` (run-loop, once per frame) drains the channel and appends to the
    executing cell's outputs, so stdout/stderr — including in-place progress bars (tqdm, `\r`) — render live
  - The executing cell's border is **bright blue** (`Color::LightBlue`); navigation/editing of other cells
    stays responsive while a cell runs
  - `notebook::append_stream` applies carriage-return line discipline so `\r`-overwrite bars show one updating line
- **Execution queue** — `NotebookState.exec_queue` holds *cell IDs* (stable across structural
  edits; deleted/converted cells are skipped at start time). `:run` while a cell is executing
  (or the kernel is booting) **enqueues** instead of refusing; `exec::notebook::pump_execution_queue`
  (called from `process_kernel_events` and after queueing) starts the next cell whenever the
  kernel is idle. Cell completion is logged with timing ("Cell [2] finished in 1.3s" /
  "failed in …"); an end-to-end test (`async_kernel_executes_queued_cells_in_order`) drives
  the whole pipeline against a real python3
- `:run` — execute focused cell; `:run-next` — execute and advance;
  `:run-all` — queue every cell in order; `:run-all-below` — queue focused cell and below
  (markdown cells render as they're passed)
- **Quarto export** (`exec/export.rs`) — `:export [fmt]` (default `pdf`; alias `:quarto`)
  saves the notebook (or a `.md`/`.qmd` buffer) and runs `quarto render --to <fmt>` on a
  background thread (`app.export_pending`, polled by `exec::poll_export` in the run loop;
  spinner active while rendering). Reports quarto's "Output created:" artifact on success,
  the stderr tail on failure, and a friendly hint when quarto isn't installed
- **Markdown cells** render like a regular Jupyter notebook: a markdown cell shows its
  formatted view (same highlighter as `.md` documents, via `markdown::highlight`) when
  `Cell.rendered` is set. `:run` / `Shift+Enter` / `Ctrl+Enter` on a markdown cell "renders" it
  (`rendered = true`, no kernel involvement); entering Insert **or Select** on it reveals the
  source (`rendered = false`). `:cell-md` converts a cell to markdown, `:cell-code`
  back to code (clears outputs + reopens the cell's LSP doc under the new language id).
  `Cell.rendered` is runtime-only (not serialised); cells load from disk rendered
- **Notebook cells word-wrap** at word boundaries to the cell's text width
  (`render_util::wrap_segments`; a single over-long word hard-breaks). Markdown cells
  always wrap — rendered view *and* editable source view alike; other cells follow the
  `editor.word_wrap` toggle (`:wrap`). The single predicate `notebook_ui::cell_wraps`
  decides wrapping in the renderer, `cell_display_height`, **and** the scroll math, so
  cell heights and scroll offsets always match what is drawn. Cells have **no
  horizontal scroll** — a non-wrapped long line (code cell, `:wrap` off) clips at the
  border; toggle `:wrap` to see it all
- **Output text always wraps**, independent of `editor.word_wrap` — unlike cell source,
  the output block has no horizontal scroll *and* no cursor column past the rendered
  rows, so an unwrapped long line (a `print()` of a wide dataframe row) would be
  permanently unreachable rather than merely clipped. Wrap width is
  `notebook_ui::output_text_width(available_cols)`; the truncation caps
  (`max_output_lines` / `max_traceback_lines`) still count **logical** lines, not
  screen rows. `truncated_rows` is the shared row-count primitive used by
  `single_output_height_count` (height model), `output_rows_content` (the navigable
  row list backing `output_row`/`output_col` and the virtual rope) and the renderer,
  so all three agree row-for-row; `error_frame_at_output_row` maps a frame's link to
  *every* row its traceback line wrapped onto
- **Seamless, row-granular notebook scroll** — the whole notebook is one vertical stack of
  cells (each `nb_cell_height` rows tall, separated by a 1-row gap) and the viewport is a
  window into it, anchored by `(NotebookState.scroll_cell, scroll_offset)` measured in
  *visual rows* (not whole cells). `exec::scroll::notebook_update_scroll` finds the cursor's
  absolute row in that stack — in the source, or (when `output_row` is set) in the output
  block — and nudges the anchor the minimum needed to keep it within `scroll_off`, so
  scrolling moves one line at a time instead of jumping a cell. The renderer
  (`notebook_ui::render_cell`) draws the first cell clipped by `scroll_offset` and clips the
  last cell at the viewport bottom — a clipped edge drops its border line, so the cell visibly
  continues past the screen edge. `nb_cell_height` is the single height model shared by the
  renderer and the scroll math; `cell_output_rows` sizes the output block (`OutputLimits`
  truncation, mirrored by the renderer via `truncated_rows` / `draw_truncation_row` — a test,
  `output_block_height_matches_what_is_drawn`, pins the two together). Both are measured by
  `exec::scroll::nb_layout`, the one place the whole cell stack is laid out.
  (The old per-cell `ensure_focused_visible` + in-cell
  `app.scroll_row` model is gone — `app.scroll_row` is now used only by the plain editor.)
- **Rich display / LaTeX** — the kernel runner evaluates a cell's trailing bare expression
  (like Jupyter's `execute_result`) and prefers a rich repr: `_repr_latex_` is rasterised to
  PNG via matplotlib mathtext and shown through the normal image pipeline (so SymPy output
  renders as math), then `_repr_png_`, then `repr()`. Requires matplotlib + a graphics
  terminal for the LaTeX→image path; otherwise the text repr is shown
- `Ctrl+R` / `:restart-kernel` — kill and restart kernel (clears all state + the execution queue)
- `:interrupt-kernel` — send SIGINT to the running kernel **and drop any queued cells**; the
  streaming read loop surfaces the resulting `KeyboardInterrupt` and returns the cell to idle
- Kernel status shown in status bar: `[starting]` / `[idle]` / `[busy]` / `[dead]` / `[no kernel]`
- **Kernel/cell lifecycle is logged to *Messages*** — kernel starting (with interpreter path),
  ready, restarting, died (with queue-drop count), cell running/queued, and per-cell
  completion with duration (`format_duration` in `exec/mod.rs`)
- `o/O` new cell below/above, `d` delete cell, `x` clear outputs
- Saves back to valid nbformat 4 JSON (`:w`)
- **Kitty/Ghostty/WezTerm graphics** — matplotlib figures captured automatically via Agg backend;
  displayed using the Kitty graphics protocol with aspect-ratio-correct sizing. Image height scales
  naturally with `figsize`; `image_rows` in config acts as a cap (default 40). Terminal detection is
  by env var (`GraphicsTerminal::detect` — Kitty via `KITTY_WINDOW_ID`/`TERM`, Ghostty via
  `TERM`=`xterm-ghostty`/`TERM_PROGRAM`/`GHOSTTY_RESOURCES_DIR`, WezTerm via `TERM_PROGRAM`/
  `WEZTERM_UNIX_SOCKET`); images suppressed in unsupporting terminals. An image straddling the
  viewport edge is **vertically cropped** (`ImageRequest.crop` → the protocol's `y=`/`h=` source
  rectangle) so the visible band keeps its natural scale instead of squashing the whole figure —
  this is what makes images scroll smoothly with the seamless notebook scroll.
- **`gw` jump mode works inside notebook cells** — labels overlaid on the focused cell.
  The label set is generated over the cell's *on-screen* source lines
  (`exec::scroll::notebook_visible_source_lines`, derived from the cell-stack scroll anchor),
  not from line 0: in a cell taller than the viewport the top is scrolled off, and labelling
  from the top put every label off-screen so none appeared at all
- All notebook commands accessible via `:` (e.g. `:run`, `:restart-kernel`, `:notebook-next-cell`)

### Phase 3 (LSP) — complete
- JSON-RPC client over stdio (`lsp.rs` / `lsp_manager.rs`)
- Language server lifecycle: spawn, initialize, shutdown
- **LSP multiplexing** — multiple servers per language, with per-server feature scoping
  (e.g. `pylsp` for intelligence + `ruff server` for code-actions/format). Configured via
  `[language_servers.<lang>]` + nested `[[language_servers.<lang>.extra_servers]]`; each server's
  `features` list (`completion`/`hover`/`definition`/`references`/`type-definition`/`implementation`/`code-actions`/`diagnostics`/`format`) routes requests
- Incremental document sync (`textDocument/didOpen`, `didChange`, `didClose`)
- Diagnostics inline (underline) + status bar count; diagnostic picker (`gD`)
- Completions — passive popup (typing) + focused mode (`Tab` to engage, `j/k`/arrows/`Ctrl-n/p`
  to navigate, `Enter` to confirm). Inside the focused popup: `/` opens a fuzzy-search row at the
  top (same scoring as the command palette — `ListState::search` overrides the word-prefix filter)
  and `K` toggles a documentation side panel for the selected item. The doc panel pulls inline
  `documentation` from the completion item, falling back to a `completionItem/resolve` request
  (one in flight at a time, gated on `completionProvider.resolveProvider`) to fetch it on demand.
  ESC ladder: in search → back to nav; in nav → close docs if open, else dismiss. `Tab` from any
  focused state returns to passive typing.
- Hover float (`K` / `gk`)
- **Signature help** — typing `(` or `,` in Insert mode requests `textDocument/signatureHelp`;
  the active call's argument list shows in the minibuffer with the current parameter marked
  `‹like this›`, refreshed as you type and cleared when the call closes / on leaving Insert
- Go-to-definition (`gd`), references (`gr`), type-definition (`gy`), implementation (`gi`).
  `gr` jumps directly when there's a single result; multiple results open a navigate popup
  (one line of source per reference, `cell N:line` / `file:line` detail, Enter to jump —
  notebook references jump to the cell in-place)
- Code actions (`ga`). The request carries the owning server's **own published
  diagnostics** for the range back in `context.diagnostics` (kept as raw JSON in
  `LspManager::server_raw_diagnostics`, since the parsed `Diagnostic` drops the
  server-private `data` a quickfix is built from) — with an empty context a server
  only ever answers with whole-file actions ("fix all", "organize imports"), never
  the fix for the error under the cursor. A cursor/selection **within one line**
  matches diagnostics anywhere on that line, since `ga` is pressed on the offending
  line far more often than on the offending token; a multi-line selection is literal
- Formatting (`gf` / `:fmt`, format-on-save option). Shell formatters via `[formatters.<lang>]`
  take priority over LSP formatting when configured
- **Notebook LSP** — `notebookDocument/didOpen` sync; virtual cell paths for per-cell diagnostics and completions.
  Notebook-aware servers (e.g. `pylsp`) see the whole notebook, so completions/diagnostics resolve **cross-cell**
  (an `import` in one cell is visible to every later cell). The notebook is (re)opened to the LSP on every
  entry path — startup, buffer-picker open, and restore-from-stash — not just the first launch.
  Go-to-definition / references that land in another cell jump to that cell **in-place**
  (`notebook::cell_index_for_virtual_path` maps the returned virtual-cell path → cell index in
  `exec::lsp::jump_to_location`) rather than opening the nonexistent virtual file as a blank buffer.
  **Notebook sync is broadcast to every server, per server**: `LspManager::notebook_did_open/`
  `did_change_cell/did_close` send `notebookDocument/*` to each initialized server advertising
  `notebookDocumentSync` and fall back to per-cell `textDocument/*` on the virtual cell docs for
  servers that don't (so e.g. ruff's diagnostics stay live alongside pylsp, regardless of which
  server initialized first). Open is idempotent per server — the per-server `Initialized` event
  retriggers `notebook_lsp_open` and only the new server actually receives it.
  **Markup (markdown/raw) cells are never transmitted** — they are omitted from BOTH
  `notebookDocument.cells` and `cellTextDocuments` (`lsp::notebook_did_open_params`), and
  `notebook_did_change_cell` drops changes for cells not in the opened code-cell list. Listing
  a cell without its backing text document crashes pylsp's notebook handling
  (`cell_document.line_count` on `None`), which used to kill **every** LSP request against any
  notebook containing a markdown cell (a unit test pins the payload shape).
- **Shadow concatenated document** — pylsp only concatenates notebook cells internally for
  *completion* and *definition*; hover, signature-help, and references run against the lone cell
  and can't see cross-cell context. So those three requests are routed through a **shadow
  document**: all code cells joined with `\n` (`notebook::concat_source`, with the focused cell's
  live buffer text substituted) synced as a plain text doc under `notebook::concat_virtual_path`
  (`{stem}__concat.py` — a URI only, never written to disk) to just the server that owns the
  feature (`LspManager::request_via_shadow_doc`), with the cursor position offset by the cell's
  start line. References results in the shadow doc map back to cells via
  `notebook::cell_for_concat_line` in `jump_to_location`.
- **pylsp jedi options**: `build_init_options` always sends `auto_import_modules: []` — pylsp's
  default (`["numpy"]`) makes jedi resolve numpy by importing it, which cannot enumerate numpy's
  lazily-bound submodules (`np.random`/`np.fft`/`np.ma` would return zero completions/hovers/
  signatures). Static analysis handles numpy correctly.
- **LSP performance** (all behavior-preserving):
  - **Writes are off the UI thread** — each `LspClient` owns a writer thread; `send_request`/
    `send_notification` enqueue a `serde_json::Value` on a channel (ordering preserved), the
    thread serializes + writes + flushes. A wedged server pipe can no longer stall typing.
    `Drop` closes the channel and joins the writer (flushing the `exit` notification) before kill.
  - **Completion / signature-help / hover requests supersede their predecessor**
    (`LspClient::supersede_pending`): the stale id is dropped from `pending` (its response is
    ignored) and `$/cancelRequest` is sent, so at most one such request is in flight per server
    and a typing burst can't queue stale jedi work ahead of the request that matters.
  - **Incremental `textDocument/didChange`** for Insert-mode keystrokes in plain files:
    `exec::lsp_did_change_insert/_remove` send a range delta (UTF-16 positions via
    `lsp::char_to_lsp_pos_utf16`) to servers advertising incremental sync, full text to the rest
    (`LspManager::did_change_delta`). **Guard invariant**: deltas are only valid against an
    exactly-synced server copy, but command edits (open-line, delete, paste, undo…) mutate the
    buffer without notifying the LSP. `Buffer::lsp_synced_chars` records the char-length as of
    the last sync; on mismatch the delta functions fall back to `lsp_did_change` (full text),
    which re-arms the guard. Notebook cells keep full-cell sync (cells are small).
  - **Signature help is throttled** (`editor.lsp_signature_throttle_ms`, default 50, 0 = off):
    inside a call it used to re-request on every keystroke — for a notebook that rebuilt +
    retransmitted the whole concatenated shadow doc each time. Requests inside the window set
    `app.sig_help_deferred`; `exec::pump_signature_help` (run loop, once per frame) fires the
    trailing refresh, so the hint always settles on the final cursor position.
  - **Shadow-doc sync is fingerprint-gated** (`LspClient::sync_full_doc`): the concatenated
    notebook is retransmitted only when its content hash changed since the last request.
  - **pylsp lint/format plugins are disabled when another configured server owns the feature**
    (`build_init_options`): a `features = ["diagnostics"]` server (e.g. `ruff server`) disables
    pycodestyle/pyflakes/mccabe/pylint/flake8/pydocstyle; `"format"` disables autopep8/yapf.
    jedi plugins always stay on. A pylsp-only setup (no feature-scoped servers) is untouched.
- **Python venv is required, never the system interpreter** — `notebook::venv_python_up` (the single
  venv discovery shared by the LSP and the kernel) walks up from the file's/notebook's location for
  `.venv`/`venv`/`.env`/`env`; the path is passed to the server as the jedi environment. If no venv is
  found, the Python language server is **not started** (no autocomplete is preferred over autocomplete
  resolved against the wrong/system environment). The notebook *kernel* (`find_python_executable`)
  uses the same discovery but still falls back to system `python3` for execution.
- **LSP lifecycle is logged to *Messages*** — venv discovery result (path found, or "no virtualenv
  … not started"), each server launched / failed-to-launch (with the spawn error), and each server
  ready (initialize handshake complete). Lines are deduped once per session
  (`LspManager::log_once` — `ensure_server` re-runs on every cell/buffer switch) and drained once
  per frame by `exec::lsp::process_lsp_events` into `app.messages`.
- **Server stderr is surfaced in *Messages*** (`ServerMessage::Stderr` →
  `LspManager::log_server_stderr`). A server whose internals have failed still speaks
  perfect JSON-RPC and answers every request with an *empty* result — pylsp against a
  venv its bundled jedi/parso can't parse ("Python version 3.14 is currently not
  supported") is the classic case — so its stderr is the only evidence that anything
  is wrong, and discarding it made the editor look like it simply had no completions.
  Indented traceback frames and routine INFO/DEBUG chatter are filtered
  (`is_notable_stderr`), and the rest is capped at `MAX_STDERR_LINES` per server.
  stderr is **piped, never inherited** — inheriting paints tracebacks over the TUI.
- **Feature-name config is validated at launch** — `LspManager::FEATURE_NAMES` is the
  accepted set; an unknown name in a `features` list is warned about, and so is any
  feature no configured server claims (a config where *every* server is
  feature-scoped silently leaves e.g. `gd`/`gr` routed to nobody). A request that
  finds no owner says which feature and which config key, instead of the old
  misleading "LSP server initializing".

### Data safety (Phase B hardening)
- **Buffer switching never loses edits**: plain-file buffers are stashed in memory
  (`app.file_buffers`, keyed by canonical path — rope, modified flag, *and* undo history) when
  navigated away from, and restored on return; notebooks were already stashed in
  `app.notebook_buffers`. `:bd` removes the stash entry.
- **`:q` sweeps every buffer** (`exec::unsaved_buffer_names`): the active buffer/notebook, stashed
  notebooks, and stashed plain files. Any unsaved one blocks quit (`:q!` forces). `:wq` saves the
  active buffer and refuses to quit while others are dirty. Special buffers are exempt by design.
- **Saves are atomic** (`buffer::atomic_write`: temp file + fsync + rename, permissions preserved)
  for both plain files and notebooks — a crash mid-save can't truncate the file.
- **External-modification check**: `Buffer::save` records the file's mtime at load/save and refuses
  a plain `:w` when the file changed on disk since (message suggests `:w!` / `Command::WriteForce`).
  `Buffer::refresh_disk_mtime` re-arms the check after a shell formatter legitimately rewrites the file.
- **LSP URIs are percent-encoded** (`lsp::path_to_uri`/`uri_to_path`) so paths with spaces or
  non-ASCII work; `diagnostic_key` round-trips through the same transform.

### Performance (Phase C hardening)
- **Dirty-flag rendering** — the run loop (`app::run_loop`) draws only when something changed
  (a key was handled, an LSP/kernel/git event was applied, the spinner is animating, resize).
  Idle CPU is just a 16 ms event poll; there is no 60 fps idle redraw. The frame itself lives in
  `app::draw_frame`. `exec::process_lsp_events` / `process_kernel_events` / `poll_git` return
  `bool` ("anything applied?") to feed the flag.
- **Notebook highlight cache** (`notebook_ui::CellHighlightCache`, stored as `app.nb_highlight`) —
  per-cell highlight spans keyed by a content fingerprint, plus one persistent tree-sitter
  highlighter per kernel language. Previously every visible cell was re-parsed (and the highlight
  query re-compiled) on every frame.
- **Git is fully async** — `git::refresh(path)` spawns a thread and returns a `GitRefresh` handle;
  `exec::poll_git` applies the result when it arrives. The old API blocked the UI thread for up to
  2 s per save/open on a slow filesystem. `exec::refresh_git(app)` is the standard trigger.
- **`ListState::filtered_indices` is memoised** keyed by the effective filter string; item-content
  mutations (completion resolve) call `invalidate_filter_cache`.

### Phase T1 (tabular data view) — complete
- **`:csv` / `:table`** opens the current file as a grid; `.csv`/`.tsv`/`.tab` open
  that way automatically (`[table] auto_open`, default true). `:table-close` returns
  to the raw text of the same file, so grid and text are two views on one file
- **`App::view() -> View { Text, Notebook, Table }`** is the single view-dispatch
  accessor. The three places that decide who owns the screen and the keyboard
  (`app::draw_frame`, `exec::update_scroll`, the `input` keymap layer) all match on
  it instead of hand-rolling conditions from `app.notebook`/`app.table`
- **`exec::buffers::open_path`** is the single "user picked a file, show it"
  dispatcher (special buffer / notebook / table / text). The buffer picker, file
  picker, buffer cycling and `:bd`'s fallback all route through it, so a new view is
  taught to one function rather than five
- **The view is read-only.** While it is open `app.buffer` is a *detached empty
  buffer with no path*, so no save path in the editor can write over the data file;
  `exec::table::is_text_mutation` refuses the edit/write commands on top of that
  (a `:wq` that fell through to the text path would have truncated the CSV)
- **Modularity**: a new backend implements `table::TableSource`
  (`columns`/`row_count`/`loaded_rows`/`cell`/`ensure_rows`/`describe`) and nothing
  else changes. `ensure_rows(range)` is called once per frame with the window about
  to be drawn, so a windowed source (SQL, a lazily-indexed CSV) can fetch exactly
  what is drawn; `CsvSource` holds every row, so its impl is the default no-op
- **`table::layout` is the single geometry model** — column widths, which columns are
  on screen, the row window (`visible_rows`), and cell truncation (`fit_cell`).
  The renderer (`table_ui`) and the scroll math (`exec::table::update_scroll` via
  `layout::scroll_col_for_cursor`) MUST both derive geometry from it, exactly as the
  notebook renderer and scroll math share `nb_cell_height`. Pinned by
  `layout_contains_cursor_after_scroll`, which asserts that for every viewport width
  and cursor column the drawn layout shows the cursor's column in full
- **Every cell is exactly one row tall and no wider than its column.** `fit_cell`
  flattens the value (newlines → `↵`, tabs/control chars → space) and truncates to
  the column width with a `…` in `theme.table_truncation`; nothing wraps. That is
  what stops a column of paragraph-length free text from swallowing the grid —
  the full text stays reachable (see Phase T2 below).
  Widths are clamped to `[table.min_col_width, table.max_col_width]` and measured
  in *display* columns (`unicode_width`), so CJK/emoji don't shear the grid
- Navigation reuses the ordinary motions, reinterpreted against the grid by
  `exec::table::handle`: `h/j/k/l` by cell, `w/b` by column, `0/$` first/last
  column, `gg/G` first/last row, `J` half-screen page down (the `Keymap::table`
  override map, same mechanism as the notebook's; `K` is the cell peek here, so
  PageUp stays on `Ctrl+u`/`PgUp`). Anything the table doesn't
  implement is classified by `exec::table::refusal` (see Phase T2); anything
  not text-specific (`:q`, the palette, `:theme`, buffer switching) falls through unchanged
- **Column types are inferred by sampling** `table.sample_rows` rows
  (`table::infer_type`, narrowest-first so a `0`/`1` column is Int, not Bool);
  numeric columns are right-aligned — header included — so digits line up
- **Delimiter sniffing** (`csv::sniff_delimiter`): tab for `.tsv`/`.tab`, otherwise
  whichever of `, ; \t |` appears most often in the header line, counted outside
  quoted regions. A semicolon export rendering as one wide text column is the
  classic way a CSV viewer looks broken
- **Loading is async and bounded**: a background thread parses (spinner runs,
  `app.table_pending`), `exec::poll_table_load` installs it from the run loop, and
  the load stops at `table.max_rows` and says so. A failed load falls back to the
  text view rather than stranding the user in the blank detached buffer
- Config `[table]`: `auto_open`, `max_col_width`, `min_col_width`, `row_numbers`,
  `max_rows`, `sample_rows`, `null_display`. Theme `[table]`: `header`,
  `header_background`, `grid`, `row_highlight`, `cursor`, `truncation`, `numeric`,
  `null`. Status line: `[statusline.table]` layout + `table_position`,
  `table_column`, `table_shape` modules
- **Not yet** (planned): sort / hide / freeze / resize columns and `/` search (T3);
  lazy byte-offset row indexing for files bigger than RAM, and a second
  `TableSource` backend to validate the trait (T4)

### Phase T2 (reading a cell in full) — complete
The grid shows one clipped line per value on purpose, so the whole value has to
be reachable some other way. Two ways, both in `exec/table.rs`:
- **`Enter` (`Command::TableOpenCell`)** opens the untruncated value in its own
  buffer named `*cell <row>:<column>*` — an ordinary buffer, so search, motions
  and wrap all work. Word-wrap is forced on and restored on the way out
  (`CellOrigin.prev_word_wrap`, undone by `leave_cell_buffer`, which
  `teardown_current_buffer` calls so *every* exit path is covered, not just
  `:bd`). Only one cell buffer exists at a time — opening another evicts the
  previous `*cell …*` rope, which would otherwise leak for the session
- **`gk` / `K` (`Command::TablePeekCell`)** peeks the same text in a scrollable
  float. `Command::LspShowDocumentation` is routed to it: `K`/`gk` mean "tell me
  more about the thing under the cursor" editor-wide, and in a grid that is the
  cell. `K` is deliberately **not** in the `Keymap::table` override map, so a
  user's own `[keys.normal] K` rebinding still wins there (unlike the notebook
  map, which shadows it); `gk` always peeks. The text popup **clips rather than
  wraps**, so the peek pre-wraps with `render_util::wrap_segments` at
  `popup_text_width` (which mirrors `popup_ui::compute_width`'s 0.6-of-terminal
  fraction — they must agree)
- **`PopupContent::Text` floats are passive → focused**, the same two-state model
  as the completion popup (`TextState.focused`, handled at the top of
  `popup_input::handle_key`): passive is a hint overlay any key dismisses with
  passthrough, `Tab` engages, then `j`/`k` scroll a line, `J`/`K` + `Ctrl+d`/`u`
  half a float, `g`/`G` the ends, `Tab` disengages, `Esc` closes. A focused float
  swallows everything else rather than leaking keys into the view behind it. The
  renderer brightens the border and shows the `Tab to scroll` / `j/k scroll ·
  Esc close` footer. This covers LSP hover as well as the cell peek — they are
  the same popup kind
- **The `g` which-key popup is generated from `exec::goto_hints(app)`** and is
  **view-aware**: in the table it lists the grid meanings (`gg` first row, `gh`
  first column, `gk` peek cell) and omits what would do nothing there. Every
  advertised key must be one `input::goto_command` dispatches — that function is
  now the single dispatch table for the `g` sub-mode, and
  `goto_hints_only_advertise_real_bindings` pins the two together
- **`y` / `x`** copy the full cell value / the row as a tab-separated line
  (`row_tsv`, values flattened through `layout::sanitize` so an embedded newline
  can't split one row into two). TSV because it pastes correctly into
  spreadsheets and needs no quoting for the commas already inside values
- **`is_special_path` is now the `*…*` shape**, not a fixed list of two names, so
  a new virtual buffer is automatically kept out of saving, LSP sync, crash
  recovery and the unsaved-changes sweep. `switch_to_special_buffer` reads any
  other `*…*` name's rope from `special_buffer_ropes` (`*Messages*` stays the one
  special buffer rebuilt from a live source)
- **Table sessions are stashed, not dropped** (`app.table_buffers`, keyed by
  canonical path, mirroring `file_buffers`/`notebook_buffers`):
  `teardown_current_buffer` stashes and `open_as_table` restores, so `Enter` into
  a cell buffer and `:bd` back is a round trip to the *same cursor cell* rather
  than a re-parse. `:table-close` and `:bd` drop the stash (an explicit exit
  should re-read a file that may have been edited as text since)
- **`q` (and `:bd`) in a cell buffer returns to its table** rather than hitting
  the "cannot close special buffer" refusal — the only place it makes sense to
  go back to. `q` lives in a fourth keymap override map (`Keymap::cell`,
  selected by `App::in_cell_buffer()`, since the *view* is still `Text`);
  visidata muscle memory, and `q` is otherwise unbound in Normal mode. `H`/`L`
  also treat a cell buffer as sitting at its origin table's position in the
  buffer list
- **`exec::table::refusal(cmd) -> Option<Refusal>`** classifies everything the
  grid doesn't implement — `ReadOnly` (edits/writes), `NeedsText` (LSP requests,
  `f`/`t`, `v`, jump/symbol/fold — all of which would read the empty buffer
  behind the grid and answer about nothing), `NotImplemented` (search, until
  T3) — and each is refused with a message naming the command. `None` means
  "falls through", which is the default for anything not text-specific (`:q`,
  the palette, `:theme`, buffer switching, the toggles). `Command::GotoLine`
  (`:42`) is *implemented* rather than refused: it addresses a row

### Known rough edges / not yet implemented
- No split panes
- The kernel is a single REPL, so cells still *run* one at a time — but they queue (`:run-all`,
  repeated `:run`) and the kernel boots asynchronously, so the UI never blocks
- Highlight recompute is whole-buffer per edit (incremental tree-sitter parsing not adopted yet)
- Gutter overflows at >9999 lines (cosmetic)
- Notebook cell rendering assumes width-1 characters (tabs/CJK render at the wrong width inside cells)
- Notebook cells have no horizontal scroll: with `:wrap` off, a long code-cell line clips at
  the cell border (markdown cells always wrap, so this only affects code/raw cells —
  cell *outputs* always wrap and are never clipped)
- The table view is read-only, loads the whole file into memory (capped at
  `table.max_rows`), and has no sort/filter/search yet — see Phase T1's closing note.
  A stashed session keeps its whole parse in memory until `:bd`/`:table-close`

## Architecture

```
src/
  main.rs             — entry point, CLI arg parsing; detects .ipynb
  app.rs              — App struct + terminal setup/teardown + render loop
                        App has both `buffer` (plain text) and `notebook` (Option)
                        After terminal.draw(), flushes pending Kitty image requests
  buffer.rs           — Rope buffer (ropey), undo/redo, file I/O
                        insert_raw/remove_raw for session-coalesced undo
  selection.rs        — Selection { anchor, head } (char indices into rope)
  mode.rs             — Mode enum: Normal, Insert, Select, Command, Goto,
                        FindChar, Search, Jump, Fold, Prompt
  command.rs          — Command enum + parse()/name()/palette_entries(), all
                        generated from ONE `commands!` macro table (canonical
                        name, aliases, palette description per row). Add a command
                        by adding a row; argument-taking variants (GotoLine/
                        WriteAs/Shell) get bespoke parsing in Command::parse().
  exec/               — execute(app, cmd): the only place that mutates state in
                        response to commands. The execute() match is largely a
                        routing table; bodies live in the submodules below.
    mod.rs            — execute() dispatch, folding/notebook-motion handlers,
                        refresh_git/poll_git, process_kernel_events, diag cache
    buffers.rs        — buffer-list management: special buffers, buffer switch +
                        stashes (plain-file & via notebook), open_as_notebook,
                        new-file/new-notebook, unsaved_buffer_names quit sweep
    table.rs          — table view: Session (source + state + path), async load
                        + poll, open/close, command routing (motions → cells,
                        edits refused), cursor-follow scroll, session stash, and
                        cell reading (Enter → *cell* buffer, K peek, y/x yank)
    scroll.rs         — update_scroll (the single authoritative scroll fn) +
                        notebook_update_scroll (row-granular cell-stack scroll) +
                        wrap helpers + fold-aware cursor normalisation
    export.rs         — Quarto export (:export): background `quarto render` + poll_export
    format.rs         — external shell formatters ([formatters.<lang>])
    text.rs           — text-editing command helpers (delete/change/paste/comment…)
    search.rs         — incremental search match computation + jump
    lsp.rs            — LSP request dispatch, event handling, did_change, jumps,
                        code actions / workspace-edit application
    pickers.rs        — popup pickers + grep front-ends (command palette, file/
                        buffer/symbol/diagnostic pickers, grep buffer/project)
    notebook.rs       — cell load/save/stash, notebook LSP open/close/reopen,
                        kernel exec/restart/interrupt, structural-edit helpers
                        (ensure_focused_visible / after_structural_edit bundle the
                        focus-fixup ritual; insert_new_cell / delete_cell / convert_cell)
  keymap.rs           — KeyBinding type + Keymap (HashMap-based, overrideable)
                        Separate notebook_navigate / notebook_edit maps
  input.rs            — Thin key dispatch; notebook mode + popups take priority
  motion.rs           — Pure motion functions: (Rope, Selection, extend) → Selection,
                        plus move_visual_up/_down (one screen row through a caller-
                        supplied `Wrap` geometry)
  indent.rs           — Auto-indent computation on Enter / open-line; indent::unit()
                        gives the configured indent string (spaces unless expand_tabs=false)
  fold.rs             — FoldRange {start, end, depth, kind, label} + FoldState (which are
                        closed, open/close/toggle, and the (kind, depth, label) type-matching
                        behind zt/zT); tree-sitter fold ranges + assign_depths, shared with
                        markdown.rs so depth means the same thing in both
  markdown.rs         — custom Markdown (.md/.markdown/.qmd) highlighter + section/fence
                        folding; produces the same Vec<Span> / Vec<FoldRange> (no tree-sitter)
  jump.rs             — `gw` label-jump: generate 2-char labels over word starts
  highlight.rs        — tree-sitter-highlight integration; produces Vec<Span>.
                        Highlighter dispatches to markdown.rs for .md/.qmd (highlight + fold_ranges).
                        MD_* highlight-index constants (markup names appended to HIGHLIGHT_NAMES)
  theme.rs            — theming engine: ThemeSpec (TOML schema) → resolved Theme (every
                        renderer color + a Style per highlight index incl. MD_* markup),
                        process-wide active theme (theme::active()/set_active), built-in
                        registry (embeds config/themes/*.toml), user themes dir, derivation
                        rules, fill_background, mode/cursor/selection styles, contrast_fg;
                        terminal OSC color queries
  lang.rs             — language id ↔ file extension mapping
  symbols.rs          — tree-sitter symbol extraction (buffer completions, picker)
  table/              — tabular data view (CSV today, SQL/parquet later)
    mod.rs            — TableSource trait (the one thing a new backend implements),
                        Column/ColumnType, sampling-based type inference
    layout.rs         — THE geometry model: column widths, visible columns,
                        cell truncation (fit_cell), row window, column scroll.
                        Renderer + scroll math must both derive geometry here
    state.rs          — TableState: cursor (row, col) + scroll anchor
    csv.rs            — CsvSource: delimiter sniffing + `csv`-crate parse, row cap
  table_ui.rs         — ratatui renderer for the grid (header, gutter, cells)
  render_util.rs      — helpers shared by ui.rs and notebook_ui.rs: SingleLineWidget,
                        jump-label overlay, diagnostic underline, char_display_width,
                        scan_wrap_rows/wrap_row_starts (THE plain-editor soft-wrap rule:
                        renderer, scroll math and visual j/k all derive rows from it)
  spinner.rs          — "boiling" Braille status-bar spinner (random-dot-flip animation)
  statusline.rs       — starship-style status line: config-driven module lists (left/right),
                        shared by the plain editor + notebook view (Ctx + render)
  clipboard.rs        — system clipboard integration (OSC 52 / external command)
  git.rs              — git gutter diff marks + current branch
  config.rs           — TOML config load + deep-merge over compiled-in defaults;
                        state_dir() helper for runtime state (recovery + history)
  recovery.rs         — crash recovery: debounced atomic 0600 flush of unsaved
                        buffers to the state dir, startup scan + Restore/Discard prompt
  history.rs          — command-palette recency history (session/global/off)
  lsp.rs              — JSON-RPC client over stdio: one LspClient per server,
                        request/notification builders, path↔uri + diagnostic_key
  lsp_manager.rs      — LspManager: multiple servers per language, feature routing,
                        diagnostics merge, notebookDocument sync
  popup.rs            — Popup data model (list/completion/docs/code-actions)
  popup_input.rs      — key handling for popups (filter, navigate, confirm)
  popup_ui.rs         — ratatui rendering for popups + floats
  ui.rs               — ratatui rendering for plain text editor + status bar
  notebook.rs         — Notebook/Cell/Output data model; from_path, save, Cell::execute(session)
                        KernelSession: persistent Python subprocess; start_execution + background
                        reader thread stream KernelMessages (async, non-blocking)
                        KernelStatus enum; find_python_executable for venv detection
                        cell_virtual_path() = LSP document identity for a cell
  notebook_state.rs   — NotebookState: focused_cell, (scroll_cell, scroll_offset) row-granular
                        scroll anchor, output_row (output-block cursor), exec queue, undo
                        snapshots, folded cells
  notebook_ui.rs      — ratatui rendering for notebooks; returns Vec<kitty::ImageRequest>
  kitty.rs            — Kitty graphics protocol (Kitty/Ghostty/WezTerm): upload/place/crop
                        images, clear/delete; ImageRequest (what a renderer emits, flushed
                        by app::flush_images after the draw — any view may emit them);
                        GraphicsTerminal detection + keyboard-protocol capability

docs/
  commands.md    — full command reference (keep this up to date with command.rs)
```

### Key invariants
- The `exec/` module is the only place that mutates `App` state in response to commands
- **Nothing that can block goes between entering the alternate screen and the first
  `draw_frame`.** `app::run` paints one frame before calling
  `negotiate_keyboard_enhancement`, because that query waits up to two seconds for a
  reply that never comes on a terminal which doesn't answer it (a bare pty, an ssh
  hop, `sv` launched as `$EDITOR` from inside another TUI such as visidata) — and an
  entered-but-unpainted alternate screen is an empty screen with a lone cursor, which
  reads as a hang. The query is also skipped outright on Kitty/Ghostty, where the
  flags are pushed regardless of the answer
- Minibuffer messages go through `app.messages.show(...)` (see `app::Messages`), which appends
  to the *Messages* log at show time — never write a message field directly
- **Every renderer color comes from the active theme** — grab `let th = theme::active();` and
  use its fields (`th.popup_bg`, `th.accent`, `th.error`, …); never write `Color::Rgb`/ANSI
  literals in renderers. A new kind of colored element gets a `Theme` field (with a documented
  derivation fallback in `theme::resolve`, + a `ThemeSpec` key if themes should set it
  directly), not a constant. The `"default"` theme must keep reproducing the classic
  terminal look (`default_theme_matches_classic_look` test)
- `Command::parse()`, `name()`, and the palette are generated from the single
  `commands!` table in `command.rs`, so they cannot drift. A test
  (`palette_entries_round_trip_through_parse`) enforces that every palette entry parses back.
- When adding a new `Command` variant: add a row to the `commands!` table (name +
  aliases + optional `palette:`), add an arm to `exec::execute()`, add a row to `docs/commands.md`
- Insert-mode edits use `buffer.insert_raw` / `buffer.remove_raw` (no per-keystroke undo snapshot).
  `begin_insert_edit()` in `input.rs` snapshots once per Insert session; `EnterNormal` in `exec/mod.rs` resets the flag.
- **LSP sync after edits**: Insert-mode keystroke sites call `exec::lsp_did_change_insert/_remove`
  (incremental range delta); every other mutation path either calls `exec::lsp_did_change`
  (full text) or relies on the `Buffer::lsp_synced_chars` length guard to force a full resync on
  the next Insert keystroke. When adding a new edit path, prefer calling `lsp_did_change` —
  an equal-length unsynced mutation is the one case the guard cannot detect.
- `exec::update_scroll` is the authoritative scroll function; the run loop calls it once per
  frame (after refreshing `viewport_height`/`viewport_width`) so scroll always reflects the
  current terminal size. It has two paths: the plain-editor fold/wrap-aware path
  (maintaining `app.scroll_row`/`scroll_col`), and the **notebook path** (`notebook_update_scroll`,
  whenever a notebook is open and not in the full-screen overlay). The notebook path treats the
  notebook as one row-tall stack and maintains the `(scroll_cell, scroll_offset)` row anchor plus
  `output_row` so the cursor — in source *or* in the output block — tracks like a text buffer,
  scrolling one visual row at a time. `nb_cell_height` / `cell_output_rows` in `notebook_ui`
  are the shared height model: the scroll math and the renderer MUST agree row-for-row, so any
  change to how a cell (or its output block) is sized must go through those two functions.
  Because scroll always follows the cursor now, the command-only `notebook-scroll-down`/`-up`
  nudges snap back to the focused cell on the next frame.
- **The table view's geometry lives in `table::layout`** — `column_width`,
  `compute`, `visible_rows`, `fit_cell`, `scroll_col_for_cursor`. The renderer and
  the scroll math must agree cell-for-cell, so any change to how a column is sized
  or which columns/rows are on screen goes through those functions (the table's
  equivalent of `nb_cell_height`/`cell_output_rows` for notebooks).
- **The table view never holds a writable handle on the data file** — `app.buffer`
  is detached (`Buffer::new_empty()`, `path = None`) while a table is open. Adding a
  code path that saves `app.buffer` must not assume it has a path, and any new
  command that writes must be listed in `exec::table::is_text_mutation`.
- **Opening a file by path goes through `exec::open_path`**, which picks the view
  from the extension. Don't call `lsp::open_file_at` directly from a "user picked a
  file" site — that bypasses the notebook and table views.
- **LSP document identity**: a document's URI is `lsp::path_to_uri(path)` (absolute +
  canonicalized, with a plain-absolute fallback for nonexistent virtual cell paths).
  Diagnostics arrive keyed by the URI the server echoes back, so any code looking up
  diagnostics for a local path MUST key with `lsp::diagnostic_key(path)` — never the raw
  `path.to_string_lossy()`, or the lookup silently misses for relative paths.
- Notebook cells are addressed by `notebook::cell_virtual_path(nb_path, lang, idx)`. The
  index is part of the identity, so structural edits (add/delete cell) shift every later
  cell's URI — handlers call `notebook::notebook_lsp_reopen` to resync after such changes.

### Extensibility hooks (ready to use)
- **Custom keybindings**: `app.keymap.set_normal(KeyBinding, Vec<Command>)`
- **Command sequences**: `Command::Sequence(vec![cmd1, cmd2, ...])`
- **Shell integration**: `Command::Shell("sh -c '...'")`
- **Config-driven keybindings**: parse TOML → `Command::parse(name)` + `KeyBinding` → `keymap.set_*`

## Checks (local == CI)

`./scripts/check.sh` is the **one** definition of "does this pass": clippy with
`-D warnings` over all targets, then the tests. CI (`.github/workflows/ci.yml`)
runs `./scripts/check.sh --full` (same, plus the release build) and the
pre-commit hook runs it bare, so the two cannot drift apart.

The toolchain is pinned in `rust-toolchain.toml` (currently 1.97.1). That is not
ceremony: clippy gains lints every release, so a floating `stable` means an
older local toolchain passes and CI fails on a lint it has never seen. To take
newer lints, `rustup update stable`, bump `channel`, and fix what it finds.

Enable the hook once per clone:

```bash
git config core.hooksPath .githooks
```

There is **no `cargo fmt` check** — the source is hand-formatted in places
(aligned match arms, the `commands!` table) and running rustfmt over it would
produce a huge unrelated diff. Don't add one without agreeing to reformat.

## Dependency versions
```toml
ratatui = "0.29"
crossterm = "0.28"
ropey = "1.6"
tree-sitter = "0.22"
tree-sitter-highlight = "0.22"
tree-sitter-rust = "0.21"        # default-features = false
tree-sitter-python = "0.21"      # default-features = false
tree-sitter-javascript = "0.21"  # default-features = false
tree-sitter-toml-ng = "0.6"      # plus json 0.21, yaml 0.6, bash 0.21,
tree-sitter-json = "0.21"        # go 0.21, c 0.21, html 0.20, css 0.21 —
tree-sitter-yaml = "0.6"         # all pinned to versions whose `language()`
tree-sitter-bash = "0.21"        # is ABI-compatible with tree-sitter 0.22
tree-sitter-go = "0.21"
tree-sitter-c = "0.21"
tree-sitter-html = "0.20"
tree-sitter-css = "0.21"
serde = "1"                      # features = ["derive"]
serde_json = "1"
toml = "0.8"
anyhow = "1"
dirs = "5"
unicode-width = "0.2"
base64 = "0.22"
libc = "0.2"
```
The LSP client is synchronous (a background reader thread per server drains stdout
into an mpsc channel; `LspManager::poll` is called once per frame). There is no
`tokio` dependency.

## Roadmap

Phases 1–3 are complete (see "Current status" above), and most of the original
Phase 4 list has also shipped: `/`?`/`n`/`N` search, multiple buffers + buffer
picker, and config-driven keybinding overrides in TOML.

### Still open
- Split panes
- User-defined named commands in TOML (`[commands]` section)
- Incremental tree-sitter highlighting (avoid full reparse on every keystroke)
