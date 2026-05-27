# majorana 

`majorana` (pronounced __my-your-on-uh__) is a lightweight text editor written in Rust, heavily inspired by Helix.
It steals many of Helix's features (noun->verb modal editing; minimal configuration), while adding a few additional components for data science support, namely:
- Native Jupyter notebook interface
- LSP support in notebooks
- Kitty graphics protocol integration

The goal is to use Jupyter notebooks with the full customizability and power of a TUI text editor. 
This project is focused on Python notebooks - other kernels (e.g. R) could theoretically be added in the future, but it's not a high priority. 

There are other projects in the same vein out there (e.g. Euporie, ...) but I have never been fully happy with any of them.
Then along came LLMs and the ability to build a custom text editor, so here we are.

__AI disclosure: This application was entirely vibe-coded, mostly with Claude Sonnet 4.6. It is almost certain to have numerous bugs and missing features as a result.__
__Until a version 1.0 is released, breaking changes and instability are to be expected.__

## Screenshots

[Screencast_20260527_121149.webm](https://github.com/user-attachments/assets/a767a1f7-6535-4f34-a0d3-0d5594fc00ba)

This screenshot shows some of `majorana`'s features: a command palette with fuzzy matching, LSP documentation lookup & code actions, format-on-save, etc.

## Etymology
Ettore Majorana was a renowned Italian physicist who was the first to propose that fermions could be their own antiparticles. 
Neat stuff! 
I decided to name this repo after him. 

## Installation

To compile and run `majorana`, make sure you have the following:

1. **Rust Toolchain**: `cargo` and `rustc` (Edition 2021).
2. **Git**: for cloning this repo
3. **Terminal with Kitty graphics support (optional)**: Needed if you want to view rich graphical outputs (like plots and images) inside notebooks.
I've tested this with WezTerm, but Kitty and Ghostty should work. Alacritty doesn't support images so it won't work.  
4. **Language Servers (optional but recommended)**:
   - Python: `ruff` and `pylsp`. Recommend installing `pylsp` via `uv tool install python-language-server`. 
   - Rust: `rust-analyzer`
   - JavaScript/TypeScript: `typescript-language-server`

To build and install `majorana` from source:

1. Clone the repository:
   ```bash
   git clone https://github.com/christian-johnson/text-editor.git
   cd text-editor
   ```
2. Build the release binary:
   ```bash
   cargo build --release
   ```
3. Copy the compiled binary to your path:
   ```bash
   cp target/release/mj ~/.local/bin/  # Or any directory on your $PATH
   ```

To run `majorana`, simply pass a file path:
```bash
mj my_script.py
mj my_notebook.ipynb
```

---

## Modes & Keybindings

`majorana` uses Helix-style selection-first editing. Most actions operate on the current selection. In Normal mode, the selection is collapsed to a single-character "point" cursor.

### Normal & Select Modes

| Default Key | Normal Mode Action | Select Mode Action |
|-------------|--------------------|--------------------|
| `h` / `←` | Move cursor left (`move-left`) | Extend selection left |
| `l` / `→` | Move cursor right (`move-right`) | Extend selection right |
| `k` / `↑` | Move cursor up (`move-up`) | Extend selection up |
| `j` / `↓` | Move cursor down (`move-down`) | Extend selection down |
| `w` | Move word forward (`move-word-forward`) | Extend selection to next word start |
| `b` | Move word backward (`move-word-backward`) | Extend selection to previous word start |
| `e` | Move word end (`move-word-end`) | Extend selection to current word end |
| `W` | Move big WORD forward | Extend selection to next WORD start |
| `B` | Move big WORD backward | Extend selection to previous WORD start |
| `E` | Move big WORD end | Extend selection to current WORD end |
| `0` | Move to line start | Extend selection to line start |
| `^` | Move to line first non-whitespace | Extend selection to first non-whitespace |
| `$` | Move to line end | Extend selection to line end |
| `G` | Move to file end (`goto-file-end`) | Extend selection to file end |
| `g` | Enter Goto Mode (press `g` again for file start) | Extend selection to file start (via Goto) |
| `f <char>` | Find character forward | Extend selection to character forward |
| `t <char>` | Till character forward | Extend selection till character forward |
| `F <char>` | Find character backward | Extend selection to character backward |
| `T <char>` | Till character backward | Extend selection till character backward |
| `x` | Select whole line (`select-line`) | Select current line |
| `%` | Select entire file (`select-all`) | Select entire file |
| `PgDn` / `Ctrl+d` | Scroll half-page down (`page-down`) | Scroll half-page down |
| `PgUp` / `Ctrl+u` | Scroll half-page up (`page-up`) | Scroll half-page up |
| `d` | Delete selection (`delete-selection`) | Delete selection |
| `c` | Change selection (`change-selection`) | Change selection (enters Insert) |
| `y` | Yank selection to clipboard | Yank selection to clipboard |
| `p` | Paste after cursor | Paste after cursor |
| `P` | Paste before cursor | Paste before cursor |
| `u` | Undo last change | Undo last change |
| `U` | Redo last change | Redo last change |
| `i` | Enter Insert mode | — |
| `a` | Enter Insert after cursor | — |
| `I` | Enter Insert at line start | — |
| `A` | Enter Insert at line end | — |
| `o` | Open line below & Insert | — |
| `O` | Open line above & Insert | — |
| `v` | Enter Select mode | — |
| `:` | Enter Command mode | Enter Command mode |
| `Esc` | — | Return to Normal mode |
| `/` | Live Search forward | Live Search forward |
| `?` | Live Search backward | Live Search backward |
| `n` | Enter Notebook Mode | Enter Notebook Mode |
| `N` | Jump to previous search match | Jump to previous search match |
| `Ctrl+n` | Jump to next search match | Jump to next search match |
| `Ctrl+p` | Jump to previous search match | Jump to previous search match |
| `Space` | Open Command Palette | — |
| `K` | LSP Hover Doc | — |
| `Ctrl+s` | Save file | Save file |

### Notebook Mode

Activated automatically for `.ipynb` files, or by pressing `ESC` from Normal mode. In Notebook mode, there are two distinct sub-modes: **Navigate Mode** (operating on cells) and **Edit Mode** (editing code inside a cell).

#### Cell Navigate Mode (Keybindings)

| Key | Action | Command |
|-----|--------|---------|
| `j` / `↓` | Move to next cell | `notebook-next-cell` |
| `k` / `↑` | Move to previous cell | `notebook-prev-cell` |
| `i` / `Enter` | Edit focused cell in-place | (Enters cell edit mode) |
| `Esc` | Return to standard Normal mode | `enter-normal` |
| `o` | Create new cell below | `notebook-new-cell-below` |
| `O` | Create new cell above | `notebook-new-cell-above` |
| `d` | Delete focused cell | `notebook-delete-cell` |
| `x` | Clear cell outputs | `notebook-clear-outputs` |
| `e` | Execute focused cell | `notebook-execute-cell` |
| `E` | Execute cell and advance | `notebook-execute-and-advance` |
| `u` | Undo cell action (add/delete) | `notebook-undo-structural` |
| `U` | Redo cell action | `notebook-redo-structural` |
| `Ctrl+r` | Restart kernel session | `notebook-restart-kernel` |
| `Ctrl+s` | Save notebook back to JSON | `save` |
| `:` | Open Command mode | `enter-command-mode` |

---

## Configuration

`majorana` reads configuration from `~/.config/majorana/config.toml`, which supersede the built-in defaults.

### Default Options

```toml
[theme]
background = "#1e1e2e"
foreground = "#cdd6f4"
cursor = "#f5e0dc"
selection = "#313244"
line_numbers = "#45475a"

[editor]
tab_width = 4
line_numbers = true
relative_line_numbers = false
scroll_off = 5
git_gutter = true
```

### Configurable Sections

1. **`[theme]`**: Colors specified as CSS-like `#RRGGBB` hex strings.
2. **`[editor]`**:
   - `tab_width`: Number of columns for a tab character.
   - `line_numbers`: Show or hide line numbers column.
   - `relative_line_numbers`: Enable relative line number drawing (useful for motions).
   - `scroll_off`: Number of screen lines to keep as padding above and below the cursor.
   - `git_gutter`: Show green/blue/red diff line indicators.
3. **`[keys.normal]` & `[keys.select]`**: Custom key maps matching key shortcuts to editor commands.
4. **`[language_servers.<lang_id>]`**: LSP server configurations:
   - `command`: LSP executable binary.
   - `args`: Arguments list.
   - `init_options`: Initialization JSON structure passed to server.

### Example Keybinding Configuration

If you want to customize your keys (for example, mapping `J` and `K` to page down/page up instead of cell navigation or default bindings), add `[keys.normal]` and `[keys.select]` sections to `~/.config/majorana/config.toml`:

```toml
# ~/.config/majorana/config.toml

[theme]
# Use custom theme colors
background = "#0f1419"
foreground = "#e6b450"

[editor]
relative_line_numbers = true
# If you want to use, say, yazi instead of the built-in file picker
file_picker = "yazi --chooser-file=$MJ_PICKER_FILE"

# Custom key bindings
[keys.normal]
"J" = "page-down"
"K" = "page-up"
"ctrl+t" = "open-command-palette"

[keys.select]
"J" = "page-down"
"K" = "page-up"
```

### Example Language Server Configuration

To enable Python LSP support with [ruff](https://docs.astral.sh/ruff/), add a `[language_servers.python]` section. Run `majorana` from your project root so it can find your virtualenv automatically.

```toml
# ~/.config/majorana/config.toml

[language_servers.python]
command = "pylsp"

[[language_servers.python.extra_servers]]
command = "ruff"
args = ["server"]
features = ["format", "code-actions", "diagnostics"]   # ruff wins for ga / code-actions requests

[formatters.python]
command = "ruff"
args=["format"]

```

For Rust and JavaScript/TypeScript:

```toml
[language_servers.rust]
command = "rust-analyzer"
args = []

[language_servers.javascript]
command = "typescript-language-server"
args = ["--stdio"]
```

---

## Command Reference

All commands can be run from the Normal/Select modes via the command line `:` (e.g. `:save`, `:quit`, `:goto-line 42`).

### Motions
- `move-left`: Move cursor left.
- `move-right`: Move cursor right.
- `move-up`: Move cursor up.
- `move-down`: Move cursor down.
- `move-word-forward`: Jump to next word.
- `move-word-backward`: Jump to previous word.
- `move-word-end`: Jump to end of word.
- `move-big-word-forward`: Jump to next WORD.
- `move-big-word-backward`: Jump to previous WORD.
- `move-big-word-end`: Jump to end of WORD.
- `move-line-start`: Jump to start of line.
- `move-line-first-non-ws`: Jump to first non-whitespace char.
- `move-line-end`: Jump to end of line.
- `goto-file-start`: Go to start of document.
- `goto-file-end`: Go to end of document.
- `goto-line <n>`: Jump to line number `<n>`.

### Selection & Editing
- `select-line`: Select current line.
- `select-all`: Select everything.
- `delete-selection`: Cut/delete selected characters.
- `change-selection`: Delete selection and enter Insert mode.
- `yank-selection`: Copy selection to clipboard.
- `paste-after`: Paste after cursor.
- `paste-before`: Paste before cursor.
- `undo`: Undo last action.
- `redo`: Redo last action.
- `open-line-below`: Add line below and enter Insert mode.
- `open-line-above`: Add line above and enter Insert mode.

### Mode Transitions
- `enter-insert`: Switch to Insert mode.
- `enter-insert-after`: Switch to Insert mode after cursor.
- `enter-insert-at-line-start`: Insert at line start.
- `enter-insert-at-line-end`: Insert at line end.
- `enter-normal`: Switch to Normal mode.
- `enter-select`: Switch to Select mode.
- `enter-command-mode`: Switch to Command line mode.

### File Operations
- `save` (alias `:w`): Save current file.
- `save-as <path>` (alias `:w <path>`): Save file to a new path.
- `quit` (alias `:q`): Exit editor.
- `force-quit` (alias `:q!`): Exit editor disregarding unsaved modifications.
- `write-quit` (alias `:wq` / `:x`): Save and exit.

### Search
- `search-forward`: Search forward in file.
- `search-backward`: Search backward in file.
- `search-next`: Jump to next match.
- `search-prev`: Jump to previous match.

### Scrolling
- `page-down`: Scroll down.
- `page-up`: Scroll up.

### UI & Popups
- `open-command-palette`: Open command fuzzy finder.
- `open-buffer-picker`: Open buffer lists.
- `open-symbol-picker`: Open symbol fuzzy finder.
- `open-diagnostic-picker`: Open LSP diagnostics explorer.
- `toggle-git-gutter`: Toggle git indicators.

### Jupyter Notebooks
- `notebook-next-cell`: Move selection to next cell.
- `notebook-prev-cell`: Move selection to previous cell.
- `notebook-scroll-down`: Scroll notebook view down.
- `notebook-scroll-up`: Scroll notebook view up.
- `notebook-enter-edit`: Enter in-cell edit mode.
- `notebook-exit-edit`: Exit in-cell edit mode.
- `notebook-execute-cell`: Execute focused cell.
- `notebook-execute-and-advance`: Execute cell and go to next.
- `notebook-new-cell-below`: Create new cell below.
- `notebook-new-cell-above`: Create new cell above.
- `notebook-delete-cell`: Delete cell.
- `notebook-clear-outputs`: Clear outputs.
- `notebook-restart-kernel`: Restart persistent Python kernel.
- `notebook-interrupt-kernel`: Send SIGINT to executing cell.
- `notebook-undo-structural`: Undo structural action (add/delete cell).
- `notebook-redo-structural`: Redo structural action.

### Scripting & Shell
- `shell <cmd>` (alias `:sh <cmd>`): Run a shell command and print outputs to status bar.
