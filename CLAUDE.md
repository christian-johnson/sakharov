# majorana — personal TUI text editor

## What this is

A from-scratch TUI text editor written in Rust, built for personal use.
Invoked as `mj [file]`. Binary at `target/debug/mj` (or `target/release/mj`).

## Current status — Phase 3 complete

### Phase 1 (plain text editor) — complete
- Helix-style (selection-first) modal editing: Normal, Insert, Select modes
- Full motion set: `h/j/k/l`, `w/b/e`, `W/B/E`, `0/^/$`, `gg/G`, `f/t/F/T`
- Edit operations: `d/c/y/p/P`, `u` undo (session-coalesced), `U` redo
- `o/O` open line, `i/a/I/A` insert variants, `v` select mode, `x` select line, `%` select all
- `:` command line — every `Command` variant is accessible by name (see `docs/commands.md`)
- Tree-sitter syntax highlighting: `.rs`, `.py`, `.js`
- Scroll with configurable `scroll_off`; horizontal scroll tracks cursor correctly
- Status bar: mode indicator (colour-coded), filename, modified flag, line:col, scroll %
- Block cursor (white in Normal, cyan in Insert); hardware cursor positioned via `frame.set_cursor_position`
- Ctrl+S saves; Ctrl+C shows quit hint
- Config at `~/.config/majorana/config.toml` (theme colours, tab width, line numbers, scroll_off)
- `/` and `?` incremental search, `n/N` cycle matches
- `gw` jump mode (2-char labels over visible word starts)
- Code folding (`zc/zo/za`), git gutter marks, word wrap toggle
- Multiple buffers + buffer picker (`<space>b`), clipboard integration
- Auto-indent on Enter, format-on-save (`:fmt` or configurable)

### Phase 2 (Jupyter notebooks) — complete
- Opens `.ipynb` files automatically in notebook mode
- Displays cells as a vertical stack: code (syntax-highlighted), markdown, raw
- Bordered cells: rounded border (unfocused) / thick border (focused); background `Rgb(20,20,30)`
- Border colour encodes execution state: **blue** = unrun, **green** = success, **red** = error, **yellow** = executing
- Cell header (`[N] CODE (python)`) lives in the top border line itself
- **Navigate mode** (`j/k` between cells) and **Edit mode** (`i` or `Enter` to edit a cell)
- **Persistent kernel session** — one Python subprocess per notebook; namespace shared across all cells
  - Auto-detected venv: checks `.venv`, `venv`, `.env`, `env` in notebook dir and cwd before falling back to `python3`
  - Runner script embedded in binary; communicates over stdin/stdout with `__KI_CODE_END__` / `__KI_OUTPUT_END__` delimiters
  - `exec(compile(code, '<cell>', 'exec'), shared_ns)` — full statement support, persistent imports/variables
- `e` / `:run` — execute focused cell; `E` / `:run-next` — execute and advance
- `Ctrl+R` / `:restart-kernel` — kill and restart kernel (clears all state)
- `:interrupt-kernel` — send SIGINT to running kernel (stops stuck cells)
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
- **LSP multiplexing** — multiple servers per language (e.g. `pylsp` for intelligence + `ruff` for code actions/format)
- Incremental document sync (`textDocument/didOpen`, `didChange`, `didClose`)
- Diagnostics inline (underline) + status bar count
- Completions — passive popup (typing) + focused mode (Tab to navigate, Enter to confirm)
- Hover float (`K`)
- Go-to-definition (`gd`), references
- Code actions (`ga`)
- Formatting (`gf` / `:fmt`, format-on-save option)
- **Notebook LSP** — `notebookDocument/didOpen` sync; virtual cell paths for per-cell diagnostics and completions

### Known rough edges / not yet implemented
- No split panes
- Kernel interrupt (SIGINT) is best-effort with synchronous execution — for truly stuck cells, `:restart-kernel`
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
  mode.rs             — Mode enum: Normal, Insert, Select, Command, Goto, FindChar
  command.rs          — Command enum: every editor action as a named variant
                        Command::parse() maps `:` input → Command
                        Command::name() gives the canonical string name
  exec.rs             — execute(app, cmd): single place that mutates state
                        Notebook commands operate on app.notebook directly
  keymap.rs           — KeyBinding type + Keymap (HashMap-based, overrideable)
                        Separate notebook_navigate / notebook_edit maps
  input.rs            — Thin key dispatch; notebook mode takes priority
  motion.rs           — Pure motion functions: (Rope, Selection, extend) → Selection
  highlight.rs        — tree-sitter-highlight integration; produces Vec<Span>
  theme.rs            — highlight index → ratatui Style
  config.rs           — TOML config load (theme, editor settings)
  ui.rs               — ratatui rendering for plain text editor
  notebook.rs         — Notebook/Cell/Output data model; from_path, save, Cell::execute(session)
                        KernelSession: persistent Python subprocess with RUNNER_SCRIPT protocol
                        KernelStatus enum; find_python_executable for venv detection
  notebook_state.rs   — NotebookState: focused_cell, cursor_pos, scroll_cell, mode, undo
                        ensure_focused_visible() keeps focused cell in a 15-cell window
  notebook_ui.rs      — ratatui rendering for notebooks; returns Vec<ImageRequest>
  kitty.rs            — Kitty terminal graphics protocol: render_image, clear_images

docs/
  commands.md    — full command reference (keep this up to date with command.rs)
```

### Key invariants
- `exec.rs` is the only place that mutates `App` state in response to commands
- `Command::parse()` and `Command::name()` must stay in sync — every variant needs both
- When adding a new `Command` variant: add to `parse()`, add to `exec::execute()`, add a row to `docs/commands.md`
- Insert-mode edits use `buffer.insert_raw` / `buffer.remove_raw` (no per-keystroke undo snapshot).
  `begin_insert_edit()` in `input.rs` snapshots once per Insert session; `EnterNormal` in `exec.rs` resets the flag.
- `update_scroll_to_fit` in `app.rs` is the authoritative scroll function (has terminal size).
  `exec::update_scroll` is a lightweight post-command nudge.

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
tree-sitter-rust = "0.21"
tree-sitter-python = "0.21"
tree-sitter-javascript = "0.21"
serde_json = "1"
base64 = "0.22"
```

## Roadmap

### Phase 3 — LSP
- JSON-RPC client over stdio (`tokio` async)
- Language server lifecycle: spawn, initialize, shutdown
- Incremental document sync
- Diagnostics (inline + status bar count)
- Completions (popup menu)
- Hover (float)
- Go-to-definition

### Phase 4 — Quality of life
- Search: `/pattern`, `?pattern`, `n/N` to cycle
- Multiple buffers + buffer list
- Split panes
- Config-driven keybinding overrides in TOML
- User-defined named commands in TOML (`[commands]` section)
- Incremental tree-sitter highlighting (avoid full reparse on every keystroke)
