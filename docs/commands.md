# Sakharov Commands

All commands are accessible in Normal mode via `:command-name`. Arguments follow the name separated by a space.

## Motions

Motions move the cursor in Normal mode (point selection) or extend the selection in Select mode.

| Command | Default Key | Description |
|---------|-------------|-------------|
| `move-left` | `h`, `←` | Move cursor left one character (stays on current line) |
| `move-right` | `l`, `→` | Move cursor right one character (stays on current line) |
| `move-up` | `k`, `↑` | Move cursor up one line, preserving column |
| `move-down` | `j`, `↓` | Move cursor down one line, preserving column |
| `move-word-forward` | `w` | Move to the start of the next word |
| `move-word-backward` | `b` | Move to the start of the previous/current word |
| `move-word-end` | `e` | Move to the end of the current word |
| `move-big-word-forward` | `W` | Move to the start of the next WORD (non-whitespace sequence) |
| `move-big-word-backward` | `B` | Move to the start of the previous/current WORD |
| `move-big-word-end` | `E` | Move to the end of the current WORD |
| `move-line-start` | `0` | Move to the first character of the current line |
| `move-line-first-non-ws` | `^` | Move to the first non-whitespace character on the current line |
| `move-line-end` | `$` | Move to the last character of the current line |
| `goto-file-start` | `gg` (via Goto mode) | Go to the first character of the file |
| `goto-file-end` | `G` | Go to the first character of the last line |
| `goto-line <n>` | `:n` | Go to line number `n` (1-based) |

## Selection

| Command | Default Key | Description |
|---------|-------------|-------------|
| `select-line` | `x` | Select the current line (including newline) |
| `select-all` | `%` | Select the entire file |

## Two-character Pending Modes

These commands enter a sub-mode that awaits a second key.

| Command | Default Key | Description |
|---------|-------------|-------------|
| `enter-goto-mode` | `g` | Enter Goto mode; press `g` again to go to file start |
| `enter-jump-mode` | `gw` (via Goto mode) | Overlay 2-char labels on visible word starts; type label to jump |
| `find-char-forward` | `f` | Enter Find mode; next char moves cursor to that char forward |
| `till-char-forward` | `t` | Enter Till mode; next char moves cursor before that char forward |
| `find-char-backward` | `F` | Enter Find mode backward; next char moves cursor to that char backward |
| `till-char-backward` | `T` | Enter Till mode backward; next char moves cursor after that char backward |

## Editing

| Command | Default Key | Description |
|---------|-------------|-------------|
| `comment-region` | `gc` (via Goto mode) | Toggle comment/uncomment for the current selection or line (markers are placed at the region's shallowest indent, so an indented block stays indented) |
| `indent-region` | `>`, `Ctrl+>` | Indent the selected lines by one indentation unit (alias `:indent`). The unit is language-aware: `[languages.<lang>] indent_width` overrides `editor.tab_width` |
| `dedent-region` | `<`, `Ctrl+<` | Dedent the selected lines by one indentation unit (alias `:dedent`) |
| `delete-selection` | `d` | Delete the current selection |
| `change-selection` | `c` | Delete the current selection and enter Insert mode |
| `yank-selection` | `y` | Copy the current selection to the clipboard |
| `paste-after` | `p` | Paste clipboard contents after the cursor |
| `paste-before` | `P` | Paste clipboard contents before the cursor |
| `undo` | `u` | Undo the last edit |
| `redo` | `U` | Redo the last undone edit |
| `open-line-below` | `o` | Insert a new line below the current line and enter Insert mode |
| `open-line-above` | `O` | Insert a new line above the current line and enter Insert mode |

## Mode Transitions

| Command | Default Key | Description |
|---------|-------------|-------------|
| `enter-insert` | `i` | Enter Insert mode at the cursor position |
| `enter-insert-after` | `a` | Enter Insert mode after the cursor |
| `enter-insert-at-line-start` | `I` | Move to line start and enter Insert mode |
| `enter-insert-at-line-end` | `A` | Move to line end and enter Insert mode |
| `enter-normal` | `Esc` | Return to Normal mode; collapses selection to point |
| `enter-select` | `v` | Enter Select (visual) mode |
| `enter-command-mode` | `:` | Open the command line at the bottom of the screen |

## File Operations

| Command | Default Key | Vim Alias | Description |
|---------|-------------|-----------|-------------|
| `write` | `ctrl+s` | `:w` | Write (save) current file; refuses if the file changed on disk since it was loaded (`save` is a backward-compat alias) |
| `write-force` | — | `:w!` | Write current file, overwriting any external changes |
| `write-as <path>` | — | `:w <path>` | Write to a new path |
| `new-file` | — | `:new`, `:newfile` | Prompt in the minibuffer for a filename, then create a new empty file in the current buffer's directory (cwd for special buffers) and switch to it |
| `new-notebook` | — | `:new-nb`, `:newnotebook` | Prompt in the minibuffer for a filename, then create a new empty `.ipynb` notebook in the current buffer's directory (cwd for special buffers) and open it (`.ipynb` appended if omitted) |
| `open-file-picker` | `ctrl+o` | `:e` | Open a file (built-in fuzzy picker, or external via `editor.file_picker` config) |
| `quit` | — | `:q` | Quit (fails if *any* buffer in the session — active or stashed — has unsaved changes) |
| `force-quit` | — | `:q!` | Quit without saving |
| `write-quit` | — | `:wq`, `:x` | Write the active buffer, then quit if no other buffer has unsaved changes |
| `buffer-close` | — | `:bd` | Close the current buffer; warns if modified |
| `buffer-force-close` | — | `:bd!` | Close the current buffer, discarding unsaved changes |
| `buffer-next` | `L` | `:bn` | Switch to the next open buffer |
| `buffer-prev` | `H` | `:bp` | Switch to the previous open buffer |
| `switch-to-scratch` | — | `:scratch` | Switch to the persistent `*scratch*` buffer |
| `switch-to-messages` | — | `:messages` | Switch to the `*Messages*` buffer (minibuffer message log) |

### External file picker

Set `editor.file_picker` in `~/.config/sakharov/config.toml` to any shell command.
The command receives `SV_PICKER_FILE` (write the chosen path there) and `SV_CURRENT_DIR`
(directory of the current buffer). Stdout is used as a fallback if the temp file is empty.

```toml
# yazi (recommended — writes its choice to SV_PICKER_FILE automatically)
[editor]
file_picker = "yazi --chooser-file=$SV_PICKER_FILE"

# fzf (writes to stdout, which sakharov reads after it exits)
[editor]
file_picker = "find . -type f | fzf"
```

## Scripting

| Command | Description |
|---------|-------------|
| `shell <cmd>` | Run a shell command via `sh -c`; first 200 chars of stdout (or stderr) shown in the status bar |
| `sequence` | (programmatic only) Run a sequence of commands in order |

## Search

| Command | Default Key | Description |
|---------|-------------|-------------|
| `search-forward` | `/` | Enter forward search — type a pattern, Enter jumps to the first match below the cursor |
| `search-backward` | `?` | Enter backward search — same but jumps to the first match above the cursor |
| `search-next` | `n`, `ctrl+n` | Jump to the next match (wraps around) |
| `search-prev` | `N`, `ctrl+p` | Jump to the previous match (wraps around) |
| `grep-buffer` | `ctrl+f` | Telescope-style fuzzy line picker over the current buffer (`:grep-buffer`) |
| `grep-project` | `ctrl+g` | Project-wide grep popup via ripgrep/grep (`:grep`, `:rg`) |

Search is live: the cursor moves to the nearest match as you type. Press `Esc` to cancel and return the cursor to its original position.

## Scrolling

| Command | Default Key | Description |
|---------|-------------|-------------|
| `page-down` | `ctrl+d`, `PgDn`, `J` (notebooks) | Scroll half a page down (cursor moves with viewport, extending the selection in Select mode; in a notebook it flows across cells and output blocks) |
| `page-up` | `ctrl+u`, `PgUp`, `K` (notebooks) | Scroll half a page up (cursor moves with viewport, extending the selection in Select mode; in a notebook it flows across cells and output blocks) |
| `scroll-cursor-center` | `gz` (via Goto mode) | Scroll viewport so the cursor line is vertically centred |

## LSP

| Command | Default Key | Description |
|---------|-------------|-------------|
| `lsp-show-documentation` | `gk`, `K` | Show hover documentation for the symbol under the cursor |
| `lsp-code-actions` | `ga` (via Goto mode) | Show code actions for the current selection |
| `lsp-goto-definition` | `gd` (via Goto mode) | Jump to the definition of the symbol under the cursor |
| `lsp-goto-references` | `gr` (via Goto mode) | List all references to the symbol under the cursor |
| `lsp-goto-type-definition` | `gy` (via Goto mode) | Jump to the type definition of the symbol |
| `lsp-goto-implementation` | `gi` (via Goto mode) | Jump to the implementation of the symbol |
| `lsp-request-completion` | `ctrl+space` | Manually trigger completion suggestions |
| `format-document` | `gf` (via Goto mode) | Format the buffer (shell formatter if configured, else LSP `:fmt`/`:format`) |

Diagnostics are shown inline (underline) and as an error/warning count in the status
bar, for both plain files and per-cell in notebooks. They are keyed by the
document's resolved absolute path, so they work regardless of whether the file was
opened by a relative or absolute path.

## Editing

| Command | Default Key | Description |
|---------|-------------|-------------|
| `kill-to-end-of-line` | `ctrl+k` | Delete from cursor to end of line; killed text goes to clipboard |

## Popup / UI

| Command | Default Key | Description |
|---------|-------------|-------------|
| `open-command-palette` | `Space` | Open fuzzy-searchable command palette (`:palette`, `:commands`). Recently-used commands float toward the top — see *Command history* below |
| `open-buffer-picker` | — | Fuzzy picker over open buffers (`:buffers`) |
| `open-symbol-picker` | — | Fuzzy picker over tree-sitter symbols in the buffer (`:symbols`) |
| `open-diagnostic-picker` | — | Fuzzy picker over all LSP diagnostics (`:diagnostics`) |
| `open-config` | — | Open the user config file for editing (`:config`) |
| `reload-config` | — | Reload the config from disk without restarting (`:config-reload`) |
| `open-theme-picker` | — | Fuzzy picker over all color themes, built-in + user, with live preview as you scroll — ESC restores the current theme (`:theme`, `:themes`); see [themes.md](themes.md) |
| `theme <name>` | — | Switch directly to a named color theme for the session (`:theme tokyonight`) |
| `toggle-git-gutter` | — | Toggle visibility of the git gutter indicator column |
| `toggle-line-numbers` | — | Toggle line number display |
| `toggle-relative-line-numbers` | — | Toggle relative line numbers (shows distance from current line) |
| `toggle-word-wrap` | — | Toggle soft word-wrap (`:wrap` / `:word-wrap`) |

### Moving through wrapped text

When soft-wrap is on, `j` / `k` (and the arrow keys, and `Ctrl+d` / `Ctrl+u`,
which are runs of them) move by **visual row** — the row you can see — not by
logical line. A paragraph wrapped over four rows takes four `j`s to cross, and
the display column is preserved as you go. With wrap off they move by logical
line as before. The same applies inside notebook cells, which wrap at word
boundaries; `j` walks a wrapped cell's rows before stepping into the next cell.

`0` / `^` / `$` are deliberately **not** visual: they stay logical-line motions,
so the start and end of the actual line are always reachable.

### Command history (palette recency)

The command palette remembers commands you invoke (via the palette or the `:`
command line — never plain keystroke motions) and floats recent ones toward the
top. Recency is a **tiebreaker only**: a better fuzzy match always still wins, so
recency only reorders matches of equal match quality, and orders the list when the
filter is empty. Configure with `ui.command_history` in `config.toml`:

| Value | Behaviour |
|-------|-----------|
| `"session"` (default) | Recency kept in memory only, reset each launch. Nothing written to disk. |
| `"global"` | Persisted to `$XDG_STATE_HOME/sakharov/command_history.json` and restored across restarts. |
| `"off"` | No recency weighting (alphabetical-within-tier, as before). |

## Persistence & crash recovery

While a buffer has unsaved edits, its contents are periodically flushed to a
private recovery file under `$XDG_STATE_HOME/sakharov/recovery/` (owner-only
`0600`, written atomically). The file is removed on a clean save and on a clean
quit, so it only lingers after a crash or kill. Covered buffers: plain-text
files, the scratch buffer, and notebooks (`.ipynb`).

When you reopen a file (or the editor itself) and a recovery file exists whose
contents differ from what's on disk, sakharov prompts you to **Restore** the
unsaved contents or **Discard** them. Disable the whole feature by setting
`editor.crash_recovery = false` in `config.toml`.

## Code Folding (plain-text editor)

Press `z` in Normal mode to enter Fold sub-mode; the available keys are shown in a popup.

| Command | Default Key | Alias | Description |
|---------|-------------|-------|-------------|
| `enter-fold-mode` | `z` | `:fold` | Enter fold sub-mode (shows key hint popup) |
| `fold-toggle` | `za` | `:fold-toggle` | Toggle fold on the innermost foldable region at the cursor |
| `fold-close` | `zc` | `:fold-close` | Close the innermost *open* fold at the cursor — repeat to collapse outward one level at a time |
| `fold-open` | `zo` | `:fold-open` | Open the outermost folded region at the cursor — repeat to reveal one level at a time |
| `fold-toggle-all` | `zA` | `:fold-toggle-all` | Toggle all folds: close all if any are open, else open all |
| `fold-close-all` | `zM` | `:fold-close-all` | Close every fold |
| `fold-open-all` | `zR` | `:fold-open-all` | Open every fold |
| `fold-close-type` | `zt` | `:fold-close-type`, `:fold-type` | Fold **every block like the one at the cursor** — same kind, same nesting depth, same owning key |
| `fold-open-type` | `zT` | `:fold-open-type`, `:unfold-type` | Unfold every block like the one at the cursor |

Foldable constructs are detected via tree-sitter:
- **Python**: `def`, `class`, `for`, `while`, `if`, `with`, `try`, decorated definitions
- **Rust**: `fn`, `impl`, `struct`, `enum`, `trait`, `mod`, `match`, closures
- **JavaScript/TypeScript**: `function`, arrow functions, `class`, `if`, `for`, `while`, `switch`, `try`
- **JSON / YAML / TOML**: objects, arrays, mappings, tables
- **Markdown**: header sections and fenced code blocks

A fold indicator line shows the first line of the folded region with a `▶ N lines` badge.
The cursor is automatically snapped past folds when moving down, and to the fold-start when moving up.

### Folding repeated structure (`zt` / `zT`)

`zt` is for files that are the same shape over and over — a JSON log, a big
config, an array of records. Each foldable region is identified by three things:
its syntactic **kind** (`object`, `function_item`, `section`, …), its **nesting
depth**, and the **key it is the value of** where the language has keys (JSON
pairs, YAML mappings). `zt` folds every region sharing all three with the one at
the cursor, and reports the count.

Given a log of records like this:

```json
{ "entries": [
    { "ts": 1, "payload": { … }, "metadata": { … } },
    { "ts": 2, "payload": { … }, "metadata": { … } } ] }
```

- cursor anywhere inside the **first record** → `zt` collapses **every record**
  to one line each, giving you the shape of the whole file
- cursor inside a **`payload`** block → `zt` collapses **every `payload`** and
  leaves the `metadata` blocks beside them open, because the owning key is part
  of the identity
- cursor on an `##` heading in Markdown → `zt` folds every sibling `##` section
  and leaves the `###`s nested inside them alone

`zT` reverses it. The which-key popup names the block you are on (`fold every
"payload" block`), so you can see what `zt` will do before pressing it.

**What `zt` cannot do:** it folds *regions*, and a scalar field like
`"timestamp": "2024-01-01"` is a single line, so there is no region to collapse.
Hiding individual scalar fields across every record is a different mechanism
(dropping rows from the display rather than collapsing a range) and is not
implemented.

## Notebook Cell Folding

| Command | Default Key | Alias | Description |
|---------|-------------|-------|-------------|
| `notebook-toggle-fold-cell` | — | `:fold-cell` | Toggle collapse of the focused cell |
| `notebook-toggle-all-folds` | — | `:fold-all-cells` | Toggle all cells: fold all if any are expanded, else unfold all |
| `notebook-toggle-output-expand` | `zO` | `:expand-output`, `:output-expand` | Show the focused cell's output in full, ignoring the `max_output_lines` / `max_traceback_lines` caps |

A folded cell shows: first line of source + `▶ N lines · M outputs` indicator.
Entering Insert (`i`) on a folded cell auto-unfolds it.

## Tabular data (CSV/TSV)

| Command | Default Key | Description |
|---------|-------------|-------------|
| `open-as-table` | — | View the current file as a data table (`:csv`, `:table`) |
| `table-close` | — | Leave the table view and edit the same file as text (`:table-close`, `:close-table`) |
| `table-open-cell` | `Enter` | Read the cursor cell's full text in its own buffer (`:read-cell`, `:cell-buffer`) |
| `table-peek-cell` | `gk`, `K` | Peek the cursor cell's full text in a float (`:peek-cell`, `:peek`) |
| `table-yank-cell` | `y` | Copy the cursor cell's full value to the clipboard (`:yank-cell`) |
| `table-yank-row` | `x` | Copy the cursor row to the clipboard as a tab-separated line (`:yank-row`) |
| `table-close-cell` | `q` (in a cell buffer) | Return from a cell buffer to its table — also what `:bd` does there (`:cell-back`, `:back-to-table`) |

`.csv` / `.tsv` / `.tab` files open in the table view automatically (turn this off
with `auto_open = false` under `[table]`); `:table-close` always gives you the raw
text of the same file, so the grid and the text are two views on one file.

The view is **read-only** — edit commands and `:w` are refused rather than applied
to the buffer behind the grid. Commands that read the *text* of the current
document (`ga` code actions, `gd`/`gr`, `gw` jump, `gs` symbols, `f`/`t`, `v`,
folds) are refused too, and say which they are: the buffer behind the grid is
empty, so they would otherwise answer about nothing at all. `/` search says it
isn't built for the grid yet. Everything not specific to the text — `:q`, the
command palette, `:theme`, buffer switching, the toggles — works as it does
everywhere else.

`:42` goes to row 42, the grid's equivalent of a line number.

Navigation uses the ordinary motions, reinterpreted against the grid:

| Key | Moves |
|-----|-------|
| `h` / `j` / `k` / `l` | one cell left / down / up / right |
| `w` / `b` | next / previous column |
| `0` / `$` | first / last column |
| `gg` / `G` | first / last row (keeping the column) |
| `J` / `Ctrl+u` | half a screen of rows down / up |

(`K` is not bound in the table override map, so it keeps whatever it means in
Normal mode — by default `show-documentation`, which the table view routes to the
cell peek. If you have rebound `K` in `[keys.normal]` your binding still wins;
`gk` always peeks. PageUp is `Ctrl+u` / `PgUp`.)

Long values are **truncated to their column** with a `…` and never wrap, so a
column of paragraph-length text can't swallow the grid; multi-line values show a
`↵` where the line breaks were. Numeric columns (detected by sampling
`table.sample_rows` rows) are right-aligned so their digits line up.

### Reading a cell in full

The grid deliberately shows a clipped one-line rendering of every value, so
there are two ways to see one whole:

- **`Enter`** opens the cell's untruncated text in its own buffer, named
  `*cell <row>:<column>*` — an ordinary buffer, so `/` search, motions and
  word-wrap all work on it. Word-wrap is forced on while it is open and put back
  as it was when you leave. **`q`** (or `:bd`) returns to the grid, on the same
  cell you left — the table is stashed, not re-parsed.
- **`gk`** (or `K`, unless you have rebound it) peeks the same text in a float
  beside the grid. The float works exactly like the LSP completion popup: it is
  a passive overlay you read at a glance and any key dismisses, **`Tab`** engages
  with it, and once focused `j`/`k` scroll a line, `J`/`K` (or `Ctrl+d`/`Ctrl+u`)
  half a float, `g`/`G` jump to the ends, `Tab` disengages and `Esc` closes.
  Hover docs (`K` in a text buffer) behave the same way.

The cell buffer is read-only in the sense that the table view is: it is a virtual
buffer with no path, so `:w` has nothing to save to and edits never reach the
data file.

Configuration lives under `[table]` in `config.toml`: `auto_open`,
`max_col_width`, `min_col_width`, `row_numbers`, `max_rows`, `sample_rows`,
`null_display`. Colours come from `[table]` in the theme
(`header`, `header_background`, `grid`, `row_highlight`, `cursor`, `truncation`,
`numeric`, `null`), and the status line uses the `[statusline.table]` layout with
the `table_position`, `table_column`, and `table_shape` modules.

Large files load on a background thread — the status spinner runs while the parse
is in flight — and stop at `table.max_rows`, reporting that they did.

## Notebooks

Opening a `.ipynb` file shows its cells as a vertical stack. **There is no separate
notebook mode** — the focused cell is edited in place with the ordinary Normal /
Insert / Select modes, exactly like a plain buffer. A few extra bindings apply while
a notebook is open (they shadow the normal bindings):

- `J` / `K` scroll half a page down / up — flowing across cells and through output
  blocks, so they page through the whole notebook rather than one cell.
- `N` / `M` move to the next / previous cell. (`N` shadows `search-prev` while a
  notebook is open; `ctrl+p` still works for that.)
- A plain `j` past a cell's last source line steps into that cell's **output block**
  — so long errors and streams scroll into view — and then into the next cell
  (column preserved). `k` is the exact inverse, so vertical motion flows
  continuously through the whole notebook.
- Long output is capped at `notebook.max_output_lines` (tracebacks at
  `max_traceback_lines`) and ends in a `... (N more lines — zO to expand)` row.
  `zO` (or `:expand-output`) lifts the cap for that cell so `j`/`k` scroll through
  all of it; `zO` again re-collapses it. (Capitalised because `zo` is fold-open
  in every view, including inside a notebook cell.)
- `Ctrl+E` executes the focused cell (works on any terminal). `Shift+Enter` /
  `Ctrl+Enter` also execute it, but only on terminals that support keyboard-enhancement
  reporting (kitty protocol) — otherwise a modified Enter is indistinguishable from a
  plain Enter and never arrives.

Cell management (new/delete cell, clear outputs, cell-type conversion, structural
undo) has no default key — use the command palette (`Space`) or the `:` command line.

### Navigation & editing

| Command | Default Key | Alias | Description |
|---------|-------------|-------|-------------|
| `enter-notebook` | — | `:nb`, `:notebook` | Open the current buffer's `.ipynb` as a notebook (no-op if already open) |
| `notebook-next-cell` | `N` | — | Focus the next cell |
| `notebook-prev-cell` | `M` | — | Focus the previous cell |
| `notebook-scroll-down` | — | — | Scroll the cell viewport down (snaps back to the focused cell) |
| `notebook-scroll-up` | — | — | Scroll the cell viewport up (snaps back to the focused cell) |
| `notebook-open-cell-edit` | — | `:open-cell`, `:edit-cell` | Open the focused cell in a full-screen edit overlay |
| `notebook-close-cell-edit` | `ctrl+Enter` | `:close-cell`, `:discard-cell` | Save the cell and close the overlay |

### Cell management

| Command | Default Key | Alias | Description |
|---------|-------------|-------|-------------|
| `notebook-new-cell-below` | — | `:new-cell` | Insert a new code cell below the focused cell |
| `notebook-new-cell-above` | — | — | Insert a new code cell above the focused cell |
| `notebook-delete-cell` | — | — | Delete the focused cell |
| `notebook-clear-outputs` | — | — | Clear the focused cell's outputs |
| `notebook-cell-to-markdown` | — | `:cell-md`, `:to-markdown` | Convert the focused cell to a Markdown cell |
| `notebook-cell-to-code` | — | `:cell-code`, `:to-code` | Convert the focused cell to a code cell |
| `notebook-undo-structural` | — | — | Undo the last add/delete-cell change |
| `notebook-redo-structural` | — | — | Redo the last undone structural change |
| `notebook-goto-error` | — | `:goto-error`, `:error` | Jump the cursor to the source line of the focused cell's error (the innermost traceback frame) |
| `notebook-follow-error` | `Enter` (on a traceback `File …` line) | — | Jump to the exact cell + line named by the traceback frame the output cursor is on |

### Execution & kernel

| Command | Default Key | Alias | Description |
|---------|-------------|-------|-------------|
| `notebook-execute-cell` | `Ctrl+E`, `Shift+Enter`, `ctrl+Enter` | `:run` | Execute the focused cell. Code cells run in the kernel (queued if one is already running); Markdown cells render to their formatted view |
| `notebook-execute-and-advance` | — | `:run-next` | Execute the focused cell, then focus the next |
| `notebook-execute-all-cells` | — | `:run-all`, `:execute-all-cells` | Queue every cell for execution in order (markdown cells render) |
| `notebook-execute-cells-below` | — | `:run-all-below`, `:execute-all-cells-below` | Queue the focused cell and every cell below it, in order |
| `notebook-restart-kernel` | — | `:restart-kernel`, `:kernel-restart` | Kill and restart the kernel (clears all state and the execution queue) |
| `notebook-interrupt-kernel` | — | `:interrupt-kernel`, `:kernel-interrupt` | Send SIGINT to the running kernel and drop any queued cells |
| `export` | — | `:export <fmt>`, `:quarto` | Render the notebook (or a markdown buffer) via Quarto, in the background. Format defaults to `pdf`; any `quarto render --to` value works (`html`, `docx`, `revealjs`, …) |

The kernel boots **asynchronously**: the first `:run` starts it in the background
(`[starting]` in the status line) and queues the cell; queued cells start as soon
as the kernel reports ready. Cell completion, kernel lifecycle events (ready /
restarted / died), and queue progress are all logged to the `*Messages*` buffer
with timings.

`Ctrl+E` is the portable execute key. `Shift+Enter` / `ctrl+Enter` additionally
require a terminal that supports keyboard-enhancement reporting (the kitty protocol);
on terminals that don't, a modified Enter collapses to a bare Enter and won't trigger
execution. (Set `SV_DEBUG_KEYS=1` to log received key events to `/tmp/sv-keys.log`
and see what your terminal actually reports.) All notebook commands also work from the
command palette and `:` line. Normal editing keys, `g`-prefixed LSP bindings (`gd`,
`gr`, `gk`, `ga`, …), `:`, and `ctrl+s` (save notebook to disk) behave exactly as in a
plain buffer.
