# sakharov — personal TUI text editor

A from-scratch TUI editor in Rust, for personal use. Invoked as `sv [file]`
(`target/debug/sv` / `target/release/sv`). Helix-style selection-first modal
editing, plus views for Jupyter notebooks, tabular data (CSV/parquet/SQL via
DuckDB), a git commit graph, and a merge-conflict resolver.

Reference docs: `docs/commands.md` (every command + key — keep in sync with
`command.rs`), `docs/themes.md`, `docs/data-layer-plan.md`,
`docs/version-control-plan.md`, `docs/merge-conflict-plan.md`.
Config defaults: `config/default.toml` (user file at
`~/.config/sakharov/config.toml`, deep-merged; loading is infallible).

## Checks (local == CI)

`./scripts/check.sh` is the one definition of "passes": clippy `-D warnings`
over all targets, then tests. CI runs `./scripts/check.sh --full` (adds the
release build); the pre-commit hook runs it bare
(`git config core.hooksPath .githooks`). Toolchain pinned in
`rust-toolchain.toml`. **No `cargo fmt`** — parts of the source are
hand-aligned; don't add it.

The `dataframe` cargo feature (default on) bundles DuckDB; building with
`--no-default-features` must keep working.

## Architecture

```
src/
  main.rs, app.rs     entry point; App struct, terminal setup, run loop (dirty-flag
                      rendering), draw_frame, flush_images
  view.rs             View enum, Chrome (status + command rows), Refusal
  source.rs           SourceId { File(canonical) | Virtual("*name*") } — buffer identity
  stash.rs            per-view state left behind on buffer switch, keyed by SourceId
  command.rs          Command enum; parse/name/palette generated from one `commands!` table
  keymap.rs, input.rs key bindings + dispatch (per-view override layers)
  exec/               execute(app, cmd) — the ONLY place App is mutated by commands
                      (buffers, scroll, text, search, lsp, doctor, pickers, notebook,
                      table, sql, attach, bridge, export, format, vcs, conflict)
  buffer.rs           rope buffer, undo/redo, atomic file I/O
  motion.rs, indent.rs, fold.rs, jump.rs, selection.rs, mode.rs
  textobject.rs       `mi`/`mo` text objects (pure: rope + cursor -> char range)
  highlight.rs        tree-sitter highlighting; markdown.rs + sql_highlight.rs are
                      hand-written highlighters producing the same spans
  theme.rs            all renderer colours (theme::active()); config/themes/*.toml
  statusline.rs       config-driven status line modules
  ui.rs, popup*.rs, render_util.rs, table_ui.rs, notebook_ui.rs, vcs_ui.rs,
  conflict_ui.rs      renderers
  lsp.rs, lsp_manager.rs   JSON-RPC client; multi-server feature routing
  notebook.rs, notebook_state.rs, compute/ (Python kernel + runner.py), kitty.rs
  table/              TableSource trait, layout (geometry), transform, summary,
                      csv, duck/ (DuckDB source, connect, statement gate)
  vcs/                load → plan → derive → apply (only apply writes)
  conflict/           load → hunk → choice fold → write (only write writes)
  config.rs, recovery.rs, history.rs, git.rs, git_highlight.rs, clipboard.rs, spinner.rs
```

## Key invariants

**State and dispatch**
- `exec/` is the only place that mutates `App` in response to commands.
- Per-view decisions are an **exhaustive `match` on `View`** — never
  `if view == X` with an implicit else. There is no `dyn View` trait.
- Opening a file by path goes through `exec::open_path` (picks the view by
  extension). Don't call `lsp::open_file_at` from a "user picked a file" site.
- Identity is `source::SourceId`, never a bare `PathBuf`. Canonicalisation
  happens only in `source.rs`. Anything that writes goes through `as_path()`
  and handles `None`. `*…*` names are virtual (no save/LSP/recovery).
- Minibuffer messages go through `app.messages.show(...)`.

**Adding things**
- New command: row in the `commands!` table → arm in `exec::execute()` → row
  in `docs/commands.md`.
- New view: add the `view::View` variant and follow the compiler errors
  (`App::view`, `current_source_id`, `draw_frame`, `update_scroll`,
  `input::keymap_layer`, `exec::execute`, `goto_hints` + `goto_command`,
  `ui::status_ctx`, `StatuslineConfig::layout_for`, `stash::Stash`,
  `teardown_current_buffer`, `open_path`). Use `view::Chrome::split` for the
  bottom rows and `view::refusal(cmd)` for commands the view can't do.
- Which-key hint lists (`goto_hints`, `fold_hints`, vcs `key_sheet`) are
  pinned by tests to the keys actually dispatched — update both together.

**Rendering**
- Every colour comes from `theme::active()`; no colour literals in renderers.
  New coloured elements get a `Theme` field with a fallback in `theme::resolve`.
  The `"default"` theme must reproduce the classic terminal look.
- Renderer and scroll math share one geometry model per view and must agree
  row-for-row: `render_util::scan_wrap_rows`/`wrap_segments` (text),
  `nb_cell_height`/`cell_output_rows`/`OutputLimits` (notebook),
  `table::layout` (grid), `vcs::layout`, `conflict::layout`.
- Text the editor didn't write must be sanitised before it becomes a cell
  symbol (`popup::sanitize_lines`, `table::layout::sanitize`) — raw control
  chars are emitted verbatim by the backend and corrupt the screen.
- Widths are display columns (`unicode_width`), never `str::len`.
- Renderers emit `kitty::ImageRequest`s into `app.graphics.pending`; only
  `app::flush_images` talks to the terminal. Anything that clears images must
  also clear `last_placed`.
- Nothing blocking between entering the alternate screen and the first frame.

**Editing / LSP**
- Insert-mode edits use `insert_raw`/`remove_raw` (one undo snapshot per
  Insert session).
- Insert keystrokes sync LSP incrementally (`lsp_did_change_insert/_remove`);
  any other edit path should call `exec::lsp_did_change` (full text).
- Diagnostics lookups key with `lsp::diagnostic_key(path)`. Notebook cells are
  addressed by `notebook::cell_virtual_path` (index-based — resync with
  `notebook_lsp_reopen` after structural edits). Markup cells are never sent
  to the LSP.
- Python LSP requires a venv (`compute::venv_python_up`); no venv → no server.

**Kernel**
- Compute sessions are owned by `App` (`ComputePool`, one per notebook) and
  only borrowed by views; never cache a handle across frames. Replies route by
  (session, request id); unknown ids are dropped.
- The runner serves requests strictly in order; don't work around a busy
  kernel.

**Data / tables**
- Tables are read-only: `app.buffer` is detached and path-less while a table
  is open; write commands are `Refusal::ReadOnly`.
- Read table data only via `Session::source()` (top of the transform stack).
  A new `Transform` variant must extend `to_sql`, `apply_local`, **and**
  `pushdown_and_local_execution_agree`.
- A windowed `TableSource` must say so (`is_windowed`); local computation over
  it is refused.
- The editor never handles credentials. `:attach` is local files, always
  `READ_ONLY`; remote data goes through the kernel bridge (`gv` / `:view`).
- All DuckDB connections go through `duck::connect` / `open_readonly`; user
  SQL passes `duck::gate::check`. The editor never runs `INSTALL`/`LOAD`.

**Version control**
- Only `vcs/apply.rs` and `conflict/write.rs` write to the repository. Every
  git process is built by `vcs::load::git_command` (clears inherited git env).
- Shell out to `git`, not libgit2. Apply writes backup refs under
  `refs/sakharov/undo/` for every local branch first.
- Conflict regions come from our own `git merge-file --diff3` over the index
  stages — never parse the working file's markers. The ours/theirs inversion
  is stated only in `Operation::left_is_yours`.
- Tests for `vcs/apply.rs` and `conflict/` drive real temp repositories.

## Known gaps
No split panes; no user-defined `[commands]`; highlighting reparses the whole
buffer per edit; table view has no search, no pivot, no column hide/resize;
notebook cells assume width-1 chars and have no horizontal scroll; vcs graph
and kernel-backed tables are snapshots (refresh manually).
