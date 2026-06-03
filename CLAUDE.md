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
- Status bar: mode indicator (colour-coded), filename, modified flag, line:col, scroll %
- Block cursor (white in Normal, cyan in Insert); hardware cursor positioned via `frame.set_cursor_position`
- Ctrl+S saves; Ctrl+C shows quit hint
- Config at `~/.config/sakharov/config.toml` — deep-merged over compiled-in `config/default.toml`.
  Search order: `$XDG_CONFIG_HOME`, then `~/.config`, then platform-native `dirs::config_dir()`.
  Covers theme colours, `tab_width`, `expand_tabs`, line numbers (absolute + relative), `scroll_off`, `git_gutter`,
  `word_wrap`, `max_undo`, `crash_recovery`, format-on-save, file-picker limits/external command, UI popup sizing +
  `jump_keys` + `symbol_icons` + `command_history`, notebook (`image_rows`/output caps), `[language_servers]`, `[formatters]`
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
- Opens `.ipynb` files automatically in notebook mode
- Displays cells as a vertical stack: code (syntax-highlighted), markdown, raw
- Bordered cells: rounded border (unfocused) / thick border (focused); background `Rgb(20,20,30)`
- Border colour encodes execution state: **dim blue** = unrun, **bright blue** = executing, **green** = success, **red** = error
- Cell header (`[N] CODE (python)`) lives in the top border line itself
- **Navigate mode** (`j/k` between cells) and **Edit mode** (`i` or `Enter` to edit a cell)
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
- Completions — passive popup (typing) + focused mode (Tab to navigate, Enter to confirm)
- Hover float (`K` / `gk`)
- Go-to-definition (`gd`), references (`gr`), type-definition (`gy`), implementation (`gi`)
- Code actions (`ga`)
- Formatting (`gf` / `:fmt`, format-on-save option). Shell formatters via `[formatters.<lang>]`
  take priority over LSP formatting when configured
- **Notebook LSP** — `notebookDocument/didOpen` sync; virtual cell paths for per-cell diagnostics and completions.
  Notebook-aware servers (e.g. `pylsp`) see the whole notebook, so completions/diagnostics resolve **cross-cell**
  (an `import` in one cell is visible to every later cell).
- **Python venv is required, never the system interpreter** — `lsp_manager::detect_python_venv` walks up from the
  file's/notebook's location for `.venv`/`venv`/`.env`/`env`; the path is passed to the server as the jedi
  environment. If no venv is found, the Python language server is **not started** (no autocomplete is preferred
  over autocomplete resolved against the wrong/system environment). The notebook *kernel* (`find_python_executable`)
  is separate and still falls back to system `python3` for execution.

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
                        FindChar, Search, Notebook, Jump, Fold
  command.rs          — Command enum: every editor action as a named variant
                        Command::parse() maps `:` input → Command
                        Command::name() gives the canonical string name
  exec/               — execute(app, cmd): the only place that mutates state in
                        response to commands. Split by concern:
    mod.rs            — execute() dispatch + buffers/files/folding/kernel handlers
    text.rs           — text-editing command helpers (delete/change/paste/comment…)
    search.rs         — incremental search match computation + jump
    lsp.rs            — LSP request dispatch, event handling, did_change, jumps,
                        code actions / workspace-edit application
    notebook.rs       — cell load/save/stash, notebook LSP open/close/reopen
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
- `Command::parse()` and `Command::name()` must stay in sync — every variant needs both
- When adding a new `Command` variant: add to `parse()`, add to `exec::execute()`, add a row to `docs/commands.md`
- Insert-mode edits use `buffer.insert_raw` / `buffer.remove_raw` (no per-keystroke undo snapshot).
  `begin_insert_edit()` in `input.rs` snapshots once per Insert session; `EnterNormal` in `exec/mod.rs` resets the flag.
- `update_scroll_to_fit` in `app.rs` is the authoritative scroll function (has terminal size).
  `exec::update_scroll` is a lightweight post-command nudge.
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
- Reference/location-list popup (currently `gr` jumps to the first reference only)
