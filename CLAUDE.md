# ki — personal TUI text editor

## What this is

A from-scratch TUI text editor written in Rust, built for personal use.
Invoked as `ki [file]`. Binary at `target/debug/ki` (or `target/release/ki`).

## Current status — Phase 1 complete

Core editor is functional and daily-driveable for plain text and code files.

### What works
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
- Config at `~/.config/ki/config.toml` (theme colours, tab width, line numbers, scroll_off)

### Known rough edges / not yet implemented
- No search (`/` and `?`)
- No LSP
- No multiple buffers / split panes
- No Kitty graphics
- No Jupyter notebook support
- Highlight recompute is whole-buffer on every keystroke (fine for now, will need incremental for large files)
- Gutter overflows at >9999 lines (cosmetic)

## Architecture

```
src/
  main.rs        — entry point, CLI arg parsing
  app.rs         — App struct + terminal setup/teardown + render loop
  buffer.rs      — Rope buffer (ropey), undo/redo, file I/O
  selection.rs   — Selection { anchor, head } (char indices into rope)
  mode.rs        — Mode enum: Normal, Insert, Select, Command, Goto, FindChar
  command.rs     — Command enum: every editor action as a named variant
                   Command::parse() maps `:` input → Command
                   Command::name() gives the canonical string name
  exec.rs        — execute(app, cmd): single place that mutates state
                   also: run_many, recompute_highlights, update_scroll
  keymap.rs      — KeyBinding type + Keymap (HashMap-based, overrideable)
  input.rs       — Thin key dispatch: Normal/Select → keymap lookup → exec
                   Insert/Command/Goto/FindChar handled directly
  motion.rs      — Pure motion functions: (Rope, Selection, extend) → Selection
  highlight.rs   — tree-sitter-highlight integration; produces Vec<Span>
  theme.rs       — highlight index → ratatui Style
  config.rs      — TOML config load (theme, editor settings)
  ui.rs          — ratatui rendering: gutter, text, cursor, status bar, command line

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
```

## Roadmap

### Phase 2 — Jupyter notebook support
- `.ipynb` document model: heterogeneous cells (code, markdown, raw, output)
- Cell navigation in Normal mode (jump between cells)
- Execute code cells (delegate to running kernel via Jupyter messaging protocol, or just shell out)
- Render output cells: text, stdout/stderr, images
- **Kitty graphics protocol** for image outputs (base64 PNG via terminal escape sequences)
- LSP scoped to code cells (language detected per cell)

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
