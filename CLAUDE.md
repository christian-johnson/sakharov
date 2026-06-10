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
- Tree-sitter syntax highlighting: `.rs`, `.py`, `.js`
- Markdown (`.md`/`.markdown`/`.qmd`): custom (non-tree-sitter) highlighting in `markdown.rs` —
  per-level header colours, **bold**/*italic*, inline `code`/fenced blocks, links, blockquotes,
  list markers — plus header-section + code-fence folding (same `zc/zo/za` interface)
- Scroll with configurable `scroll_off`; horizontal scroll tracks cursor correctly
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
  | `kernel` | — | `[idle]` / `[⠿ busy]` / `[dead]` / `[no kernel]` | notebook only |

  `kernel` folds the live spinner into itself when busy; no need for both `kernel` and
  `spinner` in the notebook layout. `cell` and `kernel` produce nothing in the plain editor.

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
- Block cursor (white in Normal, cyan in Insert); hardware cursor positioned via `frame.set_cursor_position`
- Ctrl+S saves; Ctrl+C shows quit hint
- Config at `~/.config/sakharov/config.toml` — deep-merged over compiled-in `config/default.toml`.
  Search order: `$XDG_CONFIG_HOME`, then `~/.config`, then platform-native `dirs::config_dir()`.
  Covers theme colours + `[theme.modes]` per-mode chip/cursor colors, `tab_width`, `expand_tabs`,
  line numbers (absolute + relative), `scroll_off`, `git_gutter`, `word_wrap`, `max_undo`,
  `crash_recovery`, format-on-save, file-picker limits/external command, UI popup sizing +
  `jump_keys` + `symbol_icons` + `command_history`, `[statusline]` modeline layout (left/right
  module lists, `separator`, `[statusline.styles]` color overrides, separate notebook variant),
  notebook (`image_rows`/output caps), `[language_servers]`, `[formatters]`.
  **Config loading is infallible** — any syntax error or type mismatch in the user file is
  reported to stderr and the built-in defaults are used instead.
- `/` and `?` incremental search, `n/N` cycle matches
- `gw` jump mode (2-char labels over visible word starts)
- Code folding (`zc/zo/za`), git gutter marks, word wrap toggle
- Multiple buffers (`H`/`L` cycle prev/next), clipboard integration
- Auto-indent on Enter, format-on-save (`:fmt` or configurable)
- `indent-region` (`Ctrl+>`) / `dedent-region` (`Ctrl+<`) shift the selected lines by one indent unit
- **Spaces, never tabs, by default** — Tab key and all auto-indent insert `tab_width` spaces.
  `editor.expand_tabs` (default `true`) controls this; set `false` to indent with real tabs.
  Indent unit comes from `indent::unit(expand_tabs, tab_width)`
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
  override map shadows the normal bindings: `J`/`K` move to the next/previous cell, and
  `Ctrl+E` executes the focused cell (`Shift+Enter`/`Ctrl+Enter` also execute, but only on
  terminals with keyboard-enhancement reporting — see `app::run`; otherwise a modified Enter
  arrives as a bare Enter). Cell-execution keys are handled in `input::handle_key` before mode
  dispatch so they fire from Insert too. A plain `j` on a cell's last
  line crosses into the next cell (and `k` on the first line into the previous one), so
  vertical motion flows continuously across cells. Cell management (new/delete/clear-outputs/
  cell-type/structural-undo) has no default key — use the command palette or `:` command line
- **Persistent kernel session** — one Python subprocess per notebook; namespace shared across all cells
  - Auto-detected venv: checks `.venv`, `venv`, `.env`, `env` in notebook dir and cwd before falling back to `python3`
  - Runner script embedded in binary; the editor sends a code block terminated by `__KI_CODE_END__`
  - `exec(compile(code, '<cell>', 'exec'), shared_ns)` — full statement support, persistent imports/variables
- **Asynchronous, streaming execution** — execution never blocks the UI:
  - `KernelSession::start_execution` writes the code and returns immediately; a background reader thread
    parses one JSON message per line (`{"t":"stream"|"image"|"error"|"done"}`) onto an mpsc channel
  - `exec::process_kernel_events` (run-loop, once per frame) drains the channel and appends to the
    executing cell's outputs, so stdout/stderr — including in-place progress bars (tqdm, `\r`) — render live
  - The executing cell's border is **bright blue** (`Color::LightBlue`); navigation/editing of other cells
    stays responsive while a cell runs. Only one cell runs at a time (a second `:run` reports "Kernel busy")
  - `notebook::append_stream` applies carriage-return line discipline so `\r`-overwrite bars show one updating line
- `e` / `:run` — execute focused cell; `E` / `:run-next` — execute and advance
- **Markdown cells** render like a regular Jupyter notebook: a markdown cell shows its
  formatted view (same highlighter as `.md` documents, via `markdown::highlight`) when
  `Cell.rendered` is set. `:run` / `Shift+Enter` / `Ctrl+Enter` on a markdown cell "renders" it
  (`rendered = true`, no kernel involvement); entering Insert on it reveals the source
  (`rendered = false`). `:cell-md` converts a cell to markdown, `:cell-code`
  back to code (clears outputs + reopens the cell's LSP doc under the new language id).
  `Cell.rendered` is runtime-only (not serialised); cells load from disk rendered
- **Rich display / LaTeX** — the kernel runner evaluates a cell's trailing bare expression
  (like Jupyter's `execute_result`) and prefers a rich repr: `_repr_latex_` is rasterised to
  PNG via matplotlib mathtext and shown through the normal image pipeline (so SymPy output
  renders as math), then `_repr_png_`, then `repr()`. Requires matplotlib + a graphics
  terminal for the LaTeX→image path; otherwise the text repr is shown
- `Ctrl+R` / `:restart-kernel` — kill and restart kernel (clears all state)
- `:interrupt-kernel` — send SIGINT to the running kernel; the streaming read loop surfaces the resulting
  `KeyboardInterrupt` and returns the cell to idle (now effective, not best-effort)
- Kernel status shown in status bar: `[idle]` / `[busy]` / `[dead]` / `[no kernel]`
- `o/O` new cell below/above, `d` delete cell, `x` clear outputs
- Saves back to valid nbformat 4 JSON (`:w`)
- **Kitty/WezTerm graphics** — matplotlib figures captured automatically via Agg backend; displayed
  using the Kitty graphics protocol with aspect-ratio-correct sizing. Image height scales naturally
  with `figsize`; `image_rows` in config acts as a cap (default 40). Terminal detection via env
  vars; images suppressed in unsupporting terminals.
- **`gw` jump mode works inside notebook cells** — labels overlaid on the focused cell
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
- Code actions (`ga`)
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
- **Python venv is required, never the system interpreter** — `notebook::venv_python_up` (the single
  venv discovery shared by the LSP and the kernel) walks up from the file's/notebook's location for
  `.venv`/`venv`/`.env`/`env`; the path is passed to the server as the jedi environment. If no venv is
  found, the Python language server is **not started** (no autocomplete is preferred over autocomplete
  resolved against the wrong/system environment). The notebook *kernel* (`find_python_executable`)
  uses the same discovery but still falls back to system `python3` for execution.

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

### Known rough edges / not yet implemented
- No split panes
- Only one notebook cell executes at a time (no run-queue); a second `:run` while busy reports "Kernel busy"
- Kernel startup (first `:run`) is synchronous — the UI briefly blocks while Python boots; execution itself is async
- Highlight recompute is whole-buffer on every keystroke (fine for now)
- Gutter overflows at >9999 lines (cosmetic)

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
    mod.rs            — execute() dispatch + buffers/files/folding handlers
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
  motion.rs           — Pure motion functions: (Rope, Selection, extend) → Selection
  indent.rs           — Auto-indent computation on Enter / open-line; indent::unit()
                        gives the configured indent string (spaces unless expand_tabs=false)
  fold.rs             — tree-sitter-driven fold ranges for the plain-text editor
  markdown.rs         — custom Markdown (.md/.markdown/.qmd) highlighter + section/fence
                        folding; produces the same Vec<Span> / Vec<FoldRange> (no tree-sitter)
  jump.rs             — `gw` label-jump: generate 2-char labels over word starts
  highlight.rs        — tree-sitter-highlight integration; produces Vec<Span>.
                        Highlighter dispatches to markdown.rs for .md/.qmd (highlight + fold_ranges).
                        MD_* highlight-index constants (markup names appended to HIGHLIGHT_NAMES)
  theme.rs            — highlight index → ratatui Style (incl. MD_* markup); terminal color queries
  lang.rs             — language id ↔ file extension mapping
  symbols.rs          — tree-sitter symbol extraction (buffer completions, picker)
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
  notebook_state.rs   — NotebookState: focused_cell, cursor_pos, scroll_cell, mode, undo
                        ensure_focused_visible() keeps focused cell in a 15-cell window
  notebook_ui.rs      — ratatui rendering for notebooks; returns Vec<ImageRequest>
  kitty.rs            — Kitty terminal graphics protocol: render_image, clear_images

docs/
  commands.md    — full command reference (keep this up to date with command.rs)
```

### Key invariants
- The `exec/` module is the only place that mutates `App` state in response to commands
- `Command::parse()`, `name()`, and the palette are generated from the single
  `commands!` table in `command.rs`, so they cannot drift. A test
  (`palette_entries_round_trip_through_parse`) enforces that every palette entry parses back.
- When adding a new `Command` variant: add a row to the `commands!` table (name +
  aliases + optional `palette:`), add an arm to `exec::execute()`, add a row to `docs/commands.md`
- Insert-mode edits use `buffer.insert_raw` / `buffer.remove_raw` (no per-keystroke undo snapshot).
  `begin_insert_edit()` in `input.rs` snapshots once per Insert session; `EnterNormal` in `exec/mod.rs` resets the flag.
- `exec::update_scroll` is the authoritative scroll function; the run loop calls it once per
  frame (after refreshing `viewport_height`/`viewport_width`) so scroll always reflects the
  current terminal size. It has two paths: the plain-editor fold/wrap-aware path, and a
  **notebook editing path** (whenever a notebook is open and not in the full-screen overlay).
  The notebook path runs `ensure_focused_visible` to keep the focused cell on-screen at cell
  granularity, then scrolls *within* the focused cell (`app.scroll_row`) using the rows actually
  available below the cell's content-top offset (preceding cells + gaps + top border), so the
  cursor tracks like a text buffer instead of sliding off the bottom. Because scroll always
  follows the cursor/focused cell now, the command-only `notebook-scroll-down`/`-up` nudges snap
  back to the focused cell.
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
