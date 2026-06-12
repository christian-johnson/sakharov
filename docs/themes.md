# Themes

sakharov ships with a set of built-in color themes and loads user themes from
`~/.config/sakharov/themes/*.toml`.

## Choosing a theme

* **Config (persistent):**

  ```toml
  [theme]
  name = "tokyonight"
  ```

* **At runtime:** `:theme` opens a fuzzy picker over every available theme
  (built-in + user files); `:theme <name>` switches directly. Runtime switches
  last for the session — set `[theme] name` in `config.toml` to persist.

The default (`name = "default"`) is the classic look: no background painting,
ANSI colors inherited from the terminal's own palette.

## Built-in themes

| Family | Names |
|--------|-------|
| Tokyo Night | `tokyonight`, `tokyonight-storm`, `tokyonight-moon`, `tokyonight-day` (light) |
| Catppuccin | `catppuccin-mocha`, `catppuccin-macchiato`, `catppuccin-frappe`, `catppuccin-latte` (light) |
| Nord | `nord`, `nord-darker` |
| Rosé Pine | `rose-pine`, `rose-pine-moon`, `rose-pine-dawn` (light) |
| Dracula | `dracula` |
| Gruvbox | `gruvbox`, `gruvbox-light` |
| One Dark | `onedark` |
| Solarized | `solarized`, `solarized-light` |
| Kanagawa | `kanagawa` |
| Everforest | `everforest` |
| Monokai | `monokai` |

## Writing a theme

Copy [`config/themes/example.toml`](../config/themes/example.toml) — the fully
commented schema reference — to `~/.config/sakharov/themes/<name>.toml` and
edit. A user theme with the same name as a built-in shadows it.

Every key is optional. Unset keys are derived:

* **Syntax fallback chains** — e.g. `number` → `constant`, `namespace` →
  `type`, `property` → `variable` → the theme foreground. Eight colors
  (`comment`, `keyword`, `function`, `string`, `type`, `constant`, plus
  `ui.background`/`ui.foreground`) make a complete theme.
* **Chrome derivation** — once `ui.background` is set, unset chrome (status
  line, popups, selection, line numbers, notebook cell backgrounds) is blended
  automatically from background/foreground.
* **Terminal defaults** — anything still unset falls back to the classic
  ANSI/terminal-inherited colors.

Color values: `"#rrggbb"` hex, ANSI names (`"blue"`, `"light-magenta"`, …,
which track the terminal palette), a `[palette]` key, or `"none"`.

Sections: `[palette]` (reusable named colors), `[ui]` (chrome), `[modes]`
(status-line chip / cursor color per editor mode), `[syntax]` (tree-sitter
highlighting), `[markdown]` (headings, code, links, …), `[notebook]` (cell
backgrounds + execution-state border colors).

## Overriding individual colors

Any theme-file key can be set under `[theme]` in `config.toml`; it is
deep-merged **over** the chosen theme (and survives `:theme` switches):

```toml
[theme]
name = "catppuccin-mocha"

[theme.ui]
accent = "#fab387"

[theme.syntax]
comment = "#7f849c"

[theme.modes]
insert = "#a6e3a1"
```

`:reload-config` re-applies the whole stack after editing.
