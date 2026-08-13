# Data layer — implementation plan

Turning the notebook and the CSV grid into one dataset-exploration tool: a
shared compute session, a source-agnostic dataframe layer over DuckDB and the
Python kernel, and terminal-native plots.

Line references are against `f905f0a`.

## Shape of the work

```
                    ┌──────────────────────┐
                    │ D1 column intelligence│  no dependencies — ship first
                    └──────────────────────┘

┌────────────┐   ┌──────────────┐   ┌────────────┐   ┌──────────┐
│ D0         │──▶│ D2 DuckDB    │──▶│ D3         │──▶│ D5       │
│ foundations│   │    backend   │   │ transforms │   │ plotting │
└─────┬──────┘   └──────┬───────┘   └─────▲──────┘   └──────────┘
      │                 │                 │
      └────────────┬────┘                 │
                   ▼                       │
            ┌──────────────┐               │
            │ D4 kernel    │───────────────┘
            │    bridge    │
            └──────────────┘
```

D3 waits for D2 on purpose: pushdown needs a second backend to validate
against, or the abstraction is guesswork. D5 waits for D3's typed cell values so
one plot command works on every source.

---

## Read-only by construction

**Every viewer in this plan is read-only. The only channel that writes data is
user code in a notebook or a script** — because that is the channel that gets
reviewed, versioned, and re-run. A grid, a query, a summary, and a plot are ways
of looking; none of them may mutate what they look at.

This is not just a documented intention. It has to be enforced, because `:sql`
against a live database is the one place in the plan where a keystroke could
destroy data.

### Three layers, in order of trust

1. **The connection is never opened read-write.** This is the guarantee. The
   editor attaches every database with `ATTACH '…' (READ_ONLY)`, and file-backed
   DuckDB connections use `Config::access_mode(AccessMode::ReadOnly)`. Even if
   layers 2 and 3 are defeated by input nobody anticipated, the engine refuses
   the write. Postgres/MySQL via DuckDB's scanner extensions honour read-only
   attach the same way.
2. **A statement gate on everything the user types.** This exists for the *error
   message*, not the guarantee — it catches mistakes early and explains them.
   See D2.
3. **The view holds no writable handle.** Already true, and already tested: the
   table view detaches `app.buffer` (`Buffer::new_empty()`, `path = None`) and
   `exec::table::is_text_mutation` refuses the write commands. Every new source
   inherits this.

### What "read-only" does not forbid

- **Deriving a new source.** A sort, a filter, a groupby, a pivot — these
  produce a *new* `TableSource` and never touch the parent. That is the whole
  design of D3's `derive`.
- **Handing data to the kernel.** `TableSendToKernel` binds a variable in the
  user's own namespace. That is not a write to data at rest; it is a handoff
  *into* the auditable channel, which is exactly the principle above. It still
  gets a clobber guard (D4).

### Relaxing it later

When writes are eventually wanted, they arrive as a new capability on the
source — not by loosening any of the three layers above. Sketch, so the door is
left in a sensible place:

```rust
/// Implemented only by sources that are explicitly writable. Absent by default,
/// so a source is read-only unless it opts in.
trait TableSink: TableSource {
    fn set_cell(&mut self, row: usize, col: usize, v: Value) -> Result<()>;
}
```

A view checks for `TableSink` and stays read-only when it is absent. Nothing in
D0–D5 implements it.

---

## D0 — Foundations

**Status: done** (D0.4, D0.1+D0.2, D0.3). D0.5 remains deliberately deferred to its
trigger condition.

Four changes with no user-visible effect. All four are cheap now and
structurally expensive later, because each is a thing currently owned by the
wrong module.

### D0.1 Hoist the kernel out of `Notebook`

`notebook.rs:440` holds `pub kernel: Option<KernelSession>`, so Python is
reachable only when a notebook is open. Move it to `App` as a session-level
engine that views borrow.

```rust
// new: src/compute/mod.rs
pub struct ComputeSession {
    kernel: KernelSession,          // moved verbatim from notebook.rs
    root: PathBuf,                  // venv root it was resolved against
    pending: HashMap<u64, Consumer>,
}

pub enum Consumer {                 // who is waiting on a reply
    NotebookCell(String),           // stable cell id
    ColumnSummary { col: usize },
    ArrowWindow { req: WindowReq },
    VariableList,
}

// App gains:
pub compute: Option<ComputeSession>,
```

- `Notebook::start_kernel` and `find_python_executable` move to `compute`;
  `notebook::venv_python_up` is already the shared discovery and stays put.
- `exec/notebook.rs` switches from `nb.kernel` to `app.compute`.
  `process_kernel_events` becomes `process_compute_events` and routes by
  `Consumer` instead of reading `state.executing_cell`.
- `Ctrl+R` / `:restart-kernel` must now invalidate *every* consumer — notebook
  outputs, live `ArrowSource` windows, the variable list — not just notebook
  state.

**New invariant:** the compute session is owned by `App` and only ever borrowed
by a view. Every consumer must tolerate it being absent, busy, or restarted
between frames — a view may not cache a handle to it across frames.

**Regression gate:** `async_kernel_executes_queued_cells_in_order` must pass
unchanged. If it needs editing, the refactor changed behaviour.

### D0.2 Protocol v2 — request ids

Today the framing is `__KI_META__<tag>` + code + `__KI_CODE_END__`, and replies
carry no id — the editor infers the target from `state.executing_cell`. That
breaks the moment the editor issues requests of its own.

```
__KI_REQ__{"id":7,"kind":"exec","tag":"cell-a3f"}
<code lines>
__KI_CODE_END__

{"id":7,"t":"stream","name":"stdout","text":"…"}
{"id":7,"t":"done"}
```

New kinds beyond `exec`: `vars` (list globals), `arrow` (export a variable or
row window to Arrow IPC), `describe` (column statistics computed kernel-side).
`KernelMessage` gains `id: u64`; both sides migrate at once since there are no
external clients.

**Constraint to design around, not fix:** the runner is a single-threaded loop
that reads stdin only between executions. An editor request sent while a user
cell is running sits in the pipe until that cell finishes. This is *correct* —
it is what prevents concurrent access to a namespace mid-execution — but it
means the variable explorer can go stale during a long cell. Do not solve it
with a thread inside the runner. Solve it in the UI: consumers render a "kernel
busy" state.

### D0.3 Source identity that isn't a path

Buffer identity is a `PathBuf` everywhere: `open_buffers: Vec<PathBuf>`,
`table_buffers: HashMap<PathBuf, Session>`, `Session.path`, and
`navigate_buffer` reconstructs "where am I" by canonicalising paths. A SQL
result, a pivot output, and a kernel dataframe are none of these.

```rust
pub enum SourceId {
    File(PathBuf),
    Virtual(String),   // "*query 1*", "*pivot of sales.csv*", "*df*"
}
```

Thread it through the three stash maps, `open_buffers`, and `Session`.
`is_special_path`'s existing `*…*` shape convention already gives virtual
sources the right behaviour for saving, LSP sync, and crash recovery — this
extends the same idea to the table stash.

### D0.4 Move the shared render primitives

Pure moves, no behaviour change, one sitting. **Do this first.**

| Item | From | To | Why |
|---|---|---|---|
| `ImageRequest` | `notebook_ui.rs:268` | `kitty.rs` | `ImageCrop` already lives there; D5 needs it from a non-notebook view |
| `wrap_segments` | `notebook_ui.rs` | `render_util.rs` | already called from `exec/table.rs:280`, `exec/mod.rs:1162,1198`, `motion.rs:773` |
| `graphics.pending` fill | notebook branch only (`app.rs:1042`) | any view's render | plots need to emit images too |

### D0.5 A jobs registry, on a trigger

The run loop has four bespoke pollers (`app.rs:844–848`) plus a hand-enumerated
`background_active` expression for the spinner. D2 and D4 each add one.

**Don't build this yet** — it would be speculative. Build it when the poll list
reaches six: a `Vec<Box<dyn Job>>` polled once per frame, each returning
"anything applied?", with `background_active` derived from the registry rather
than restated.

---

## D1 — Column intelligence

*Independent of D0. Ship first.*

Summary statistics, a frequency table, and an in-header sparkline — all in pure
Rust over the existing `TableSource`. No kernel, no DuckDB.

```rust
// new: src/table/summary.rs
pub struct ColumnSummary {
    pub rows: usize, pub nulls: usize, pub distinct: usize,
    pub min: Option<f64>, pub max: Option<f64>,
    pub quantiles: Option<[f64; 5]>,   // p0 p25 p50 p75 p100
    pub hist: Vec<u32>,                // fixed bin count, drives the sparkline
    pub top: Vec<(String, usize)>,     // frequency table for Text/Bool columns
}

pub fn summarize(src: &dyn TableSource, col: usize, bins: usize) -> ColumnSummary;
```

| Deliverable | Surface | Notes |
|---|---|---|
| Summary panel | `Command::TableColumnSummary` → `PopupContent::Text` | Reuses the focused-float scrolling built in T2 |
| Frequency table | `Command::TableFrequency` | Opens as its own grid over a `SourceId::Virtual` — the first derived table, and the natural rehearsal for D3's groupby |
| Header sparkline | `[table] column_sparkline` | Second header row; theme gains `table.sparkline` |

**Touches the geometry invariant.** The sparkline makes the header two rows
tall, and `layout::HEADER_ROWS` feeds `layout::visible_rows`. The height must
stay a function of config inside `layout` — never computed independently in
`table_ui`. Extend `layout_contains_cursor_after_scroll` to run with the
sparkline on and off, so the renderer and the scroll math cannot drift over the
taller header.

**Open behaviour.** For a windowed source, statistics over `loaded_rows()` are
not statistics over the dataset. Until D3 can push a summary down to the
backend, label the panel with the row count it actually covered rather than
implying it is complete.

**Read-only:** `summarize` takes `&dyn TableSource` and only reads. When D3 later
pushes a summary down to a backend, it must go through the same read-only path as
any other derived query — a `SUMMARIZE`/aggregate select, never a temp table.

**Tests:** `summarize` against the existing `VecSource` fixture (quantile edges,
all-null column, single-value column); the extended layout test above.

---

## D2 — DuckDB backend

*Needs D0.3.*

A second `TableSource` that reads parquet, JSON, and huge CSVs without loading
them into memory, plus SQL as a first-class surface. This is what validates that
the trait's `ensure_rows` windowing design actually works.

```rust
// new: src/table/duck.rs
pub struct DuckDbSource {
    conn: duckdb::Connection,
    sql: String,                 // the query this source is a window onto
    columns: Vec<Column>,
    window: Range<usize>,        // rows currently materialised
    cells: Vec<Vec<String>>,     // formatted window, ~100 rows
    total: Option<usize>,        // None until the background COUNT(*) lands
}
// ensure_rows(r) → SELECT * FROM (<sql>) LIMIT n OFFSET k, via stmt.query_arrow()
```

- **Cargo:** `duckdb = { version = "1", features = ["bundled"] }` behind a
  `dataframe` feature, default-on. Gating it from day one is what makes
  "optional plugin later" a config change rather than a refactor.
- **Extension dispatch** in `exec::open_path`: `.parquet`, `.jsonl`, `.ndjson`,
  `.arrow`. Add `[table] engine = "builtin" | "duckdb"` so a large CSV can be
  routed through DuckDB instead of `CsvSource`.
- **`:sql`** opens a `*sql*` scratch buffer; `Ctrl+E` — the same execute key as
  a notebook cell — runs it and opens the result as a `DuckDbSource` under a
  `SourceId::Virtual`.
- **Schema browser** is just a grid over `information_schema`, with Enter on a
  row opening that table. No new view required.

### Read-only enforcement

This is the one phase where read-only needs code, not just a convention.

**Layer 1 — the connection.** Every attach the editor issues carries
`READ_ONLY`; a file-backed connection is opened with
`Config::access_mode(AccessMode::ReadOnly)`. The editor never constructs a
read-write handle to a database, so a mutating statement fails in the engine even
if it gets past the gate.

```rust
// src/table/duck.rs — the only place a connection is created.
fn open_readonly(path: Option<&Path>) -> Result<Connection> { … }
// ATTACH always: ATTACH '<path>' AS <alias> (READ_ONLY)
```

**Layer 2 — the statement gate**, in `table/duck/gate.rs`. Runs on anything the
user typed before it reaches the connection. Allowlist by leading keyword:

| Allowed | Rejected |
|---|---|
| `SELECT`, `WITH`, `FROM` (DuckDB's FROM-first syntax), `DESCRIBE`, `SUMMARIZE`, `SHOW`, `EXPLAIN`, `VALUES`, `TABLE` | all DML/DDL (`INSERT`, `UPDATE`, `DELETE`, `DROP`, `CREATE`, `ALTER`, `TRUNCATE`, `MERGE`) |
| read-only `PRAGMA` / `CALL` on an explicit sub-allowlist | `COPY … TO`, `EXPORT DATABASE` — the only ways to write a *file* from a read-only connection |
| | `ATTACH` (the editor issues these itself, always `READ_ONLY`), `DETACH`, `INSTALL`, `LOAD`, `SET` |

Two details that a naive gate gets wrong:

- **`FROM` must be allowed.** DuckDB accepts `FROM tbl SELECT …` and bare
  `FROM tbl`. A gate that only knows `SELECT` rejects idiomatic DuckDB.
- **One statement per run.** `SELECT 1; DROP TABLE t` must not slip through on
  its leading keyword. Reject input containing a statement separator followed by
  anything but whitespace or a comment.

A leading-keyword allowlist is a heuristic, and it is deliberately the *second*
layer for that reason. The robust upgrade — worth taking if the gate ever gets
fiddly — is to ask DuckDB's own parser for the statement type via
`json_serialize_sql('…')`, which parses without executing.

**Do not reach for safe mode.** `enable_external_access(false)` (and the CLI's
`-safe` equivalent) does block writes — and also blocks `read_parquet`,
`read_csv`, and `read_json`, which is the entire purpose of this backend.
Likewise `SET disabled_filesystems = 'LocalFileSystem'` blocks local reads. And
note there has been at least one advisory where a table function reached the
filesystem despite external access being disabled, which is a further reason to
treat engine file-gating as a bonus rather than the guarantee.

**Turn the refusal into a workflow.** When the gate rejects a mutating
statement, don't just refuse — say that writes go through code, and offer to
drop the statement into a notebook cell (or a `*sql*` script buffer) where it can
be reviewed, committed, and run deliberately. The restriction should route the
user to the auditable path rather than dead-end them.

**Tests:** a corpus test over the gate — every statement in a list of mutating
forms is rejected, every statement in a list of legitimate analytical queries
(including `FROM`-first and CTEs) is allowed, and `SELECT 1; DROP TABLE t` is
rejected. Plus one integration test asserting that a mutating statement forced
past the gate still fails at the connection, so layer 1 is proven to be doing
its job independently.

**Risk:** `features = ["bundled"]` compiles DuckDB's C++ from source: a long
first build and a much larger binary, against a project whose release profile is
currently tuned for size. Measure both before committing and record the numbers
in the PR — this is the one dependency in the plan that changes the
`cargo install` experience.

**Known wart:** `TableSource::cell` returns `&str`, so an Arrow-backed source
must format strings it doesn't natively hold. At window size that is cheap and
fine — but it is exactly why D3 adds a typed accessor rather than layering more
on top of the string one.

**Tests:** a small committed parquet fixture opens with correct column types; a
query-counting wrapper asserts that scrolling fetches only the drawn window, not
the whole table.

---

## D3 — Transforms with pushdown

*Needs D2.*

Sort, filter, hide, groupby and pivot — expressed once in the UI, executed three
different ways depending on what the source can do natively.

```rust
pub enum Transform {
    Sort   { col: usize, desc: bool },
    Filter { col: usize, pred: Predicate },
    GroupBy{ keys: Vec<usize>, aggs: Vec<(usize, Agg)> },
    Pivot  { index: Vec<usize>, columns: usize, values: usize, agg: Agg },
}

trait TableSource {
    /// Satisfy `op` natively. `None` → the caller wraps it locally.
    fn derive(&self, op: &Transform) -> Option<Box<dyn TableSource>> { None }
    /// Typed access, so comparison and aggregation don't go through display strings.
    fn value(&self, row: usize, col: usize) -> Value;
}
```

| Source | `derive` strategy |
|---|---|
| `DuckDbSource` | wrap `sql` in a subquery — everything pushes down |
| `ArrowSource` (D4) | send a polars expression to the kernel; the frame never crosses the pipe |
| `CsvSource` | `None` → generic `LocalView { parent, order: Vec<usize>, mask }` in `table/transform.rs` |

- `Session` gains `transforms: Vec<Transform>` as a **stack**, so `u` pops the
  last one. That is the natural undo for a read-only view and reuses existing
  muscle memory rather than inventing a key.
- New status-line module `table_transforms` renders the active stack
  (`sort:price↓ · filter:qty>0`). Without it, a filtered grid is
  indistinguishable from a small dataset.
- Column *freeze* and *resize* are `layout` concerns, not transforms — keep them
  out of this enum.

**Read-only:** `derive` takes `&self` on purpose — a transform produces a *new*
source and never mutates its parent. Pushdown implementations may only generate
reading statements: `DuckDbSource::derive` wraps `sql` in a subquery and its
output goes through the same D2 gate, so a `Predicate` carrying injected SQL
cannot escalate into a write. `ArrowSource::derive` sends a polars expression
that operates on a frame, never a rebind of the user's variable.

**The test that makes this real:** apply the same `Transform` to the same data
via both paths — `VecSource` (local wrapper) and `DuckDbSource` (pushdown) — and
assert cell-for-cell identical output. Without this, pushdown silently diverges
from local execution and the grid quietly lies about a filtered dataset.

---

## D4 — Kernel bridge

*Needs D0, D2.*

The variable explorer, and a grid over any dataframe living in the kernel. This
is the phase that stops the notebook and the grid from being two tools in one
binary.

### Transport

Arrow IPC, **not** base64 over the JSON line protocol. Images already go base64
and that is fine because they are small; a hundred-row window of a wide frame is
not. The kernel writes IPC to a temp file under `config::state_dir()`, sends
`{"id":9,"t":"arrow","path":"…","rows":100}`, and the editor reads and unlinks
it.

```rust
// new: src/table/arrow_src.rs
pub struct ArrowSource {
    var: String,                 // kernel-side variable name
    columns: Vec<Column>,
    window: Range<usize>,
    batch: RecordBatch,          // via duckdb::arrow — one arrow version in the tree
    total: Option<usize>,
    stale: bool,                 // the frame may have been reassigned under us
}
```

Use `duckdb::arrow` rather than a direct `arrow` dependency, so there is exactly
one Arrow version in the tree and no skew between what DuckDB produces and what
the reader parses.

### Python side

| Object | Export | Cost |
|---|---|---|
| polars `DataFrame` | `df.write_ipc(path)` | native, no extra dependency |
| pandas (pyarrow-backed) | `pa.Table.from_pandas` | cheap; needs pyarrow |
| pandas (numpy-backed) | same | real linear conversion — surface it in the status line for big frames |
| DuckDB relation | `rel.arrow()` | native |

### Surfaces

- **Variable explorer** (`gv`): a `vars` request returns name, type, shape,
  memory, and whether it is viewable; renders as `PopupContent::List`; Enter
  opens an `ArrowSource`.
- **`:view df`** — open a named variable directly.
- **`Command::TableSendToKernel`** — push the current grid, transforms included,
  back into the namespace as a variable. This is the reverse arrow that closes
  the loop.

### Read-only, and the one deliberate exception

The kernel bridge reads dataframes; it never writes to a file, a database, or an
existing binding. `TableSendToKernel` is the single command in the plan that
mutates anything, and what it mutates is a name in the user's own REPL — a
handoff *into* the auditable channel, not a write to data at rest.

Two guards on it:

- **Never clobber silently.** If the target name is already bound, prompt with
  the existing value's type and shape rather than overwriting. Default the
  suggested name to something unlikely to collide.
- **Never `eval` user-typed expressions.** `vars`, `describe`, and `arrow`
  requests take a *bound variable name*, validated as an identifier, and operate
  on it — they do not evaluate arbitrary text from the editor. Introspecting an
  object can still run user code through a property or `__getattr__`, which is
  unavoidable when inspecting a live namespace; keep introspection to type,
  shape, and `nbytes` rather than anything that materialises values.

**Staleness.** A kernel frame can be reassigned while a grid is open on it. Do
not poll for that. Mark the source `stale` whenever a cell finishes executing,
show a chip in the status line, and let `:refresh` re-fetch — cheap, honest, and
no background traffic.

**Inherited from D0.2:** every request here queues behind a running user cell.
The explorer must render "kernel busy" rather than appearing empty or hanging.

---

## D5 — Plotting

*Needs D3, D0.4.*

Histograms, scatter, line, and 2D heatmaps drawn from any `TableSource` — so one
command works identically on a CSV, a parquet file, and a kernel dataframe.

`ratatui 0.29` already ships `widgets::canvas::Canvas` with `Marker::Braille`,
so scatter and line are close to free. Half-block characters give heatmaps 2×
vertical resolution; histograms are block runs. The existing Kitty pipeline
(`ImageRequest`, relocated in D0.4) is the fallback for a true raster —
`imshow` of an actual image array.

```rust
// new: src/plot/mod.rs, src/plot/render.rs
pub struct PlotSpec {
    pub kind: PlotKind,          // Hist | Scatter | Line | Heatmap | Imshow
    pub x: usize,                // column indices into the source
    pub y: Option<usize>,
    pub bins: usize,
    pub scale: Scale,            // Linear | Log
}
// :hist price · :scatter x y · :heatmap a b · :plot col
```

- Adds `View::Plot` — the fourth arm in `app::draw_frame`,
  `exec::update_scroll`, and the `input` keymap layer.
- Bin edges, axis ticks, and the data→cell mapping are one geometry module that
  the renderer and any cursor readout both derive from. Same discipline as
  `table::layout` and `nb_cell_height`; do not let the renderer compute its own
  bins.
- Measure with `unicode_width`, following `table_ui` — not the notebook
  renderer's width-1 assumption.

**Read-only:** a plot never writes. There is deliberately no `:plot-save` in this
phase — saving a figure is `matplotlib` in a notebook cell, which is the
auditable path and already works. If a PNG export is wanted later it is an
explicit command writing an explicit new path, never an implicit side effect of
drawing.

**Ordering decision.** `App::view()` asserts that exactly one view owns the
screen. D5 makes that four views. If split panes are wanted — and for
script-left / grid-right they will be — the pane refactor is *the same work* as
collapsing the three parallel stash maps, and it is markedly cheaper before D5
than after.

---

## Sequencing

| Order | Work | Can run in parallel with |
|---|---|---|
| 1 | D0.4 — pure moves | anything; one sitting, no behaviour change |
| 2 | D0.1 + D0.2 together | D1, D0.3 — treat the hoist and the protocol as one refactor, since routing by `Consumer` is what the hoist is *for* |
| 3 | D0.3 — `SourceId` | D0.1/0.2, D1 — touches different files |
| 4 | D1 — column intelligence | all of D0; depends on none of it |
| 5 | D2 — DuckDB | — |
| 6 | D3 — transforms | — |
| 7 | D4 / D5 | each other, once D3 lands |

Each phase keeps `./scripts/check.sh` green — clippy with `-D warnings` plus
tests — and the toolchain stays pinned in `rust-toolchain.toml`. Every new
`Command` is one row in the `commands!` table, one arm in `exec::execute`, one
row in `docs/commands.md`, and — for anything the table view cannot do — one
entry in `exec::table::refusal`.

**Per-phase read-only checklist.** Before a phase merges: no new type holds a
read-write handle to a data source; no new command mutates one; every new source
is registered in `exec::table::refusal` for the write commands; and any new SQL
path goes through the D2 gate rather than round it.

---

## Decisions to make before starting

**Does the DuckDB feature default on?** Default-on gives every user parquet and
SQL out of the box but taxes the `cargo install` build with a C++ compile.
Default-off keeps the lean binary but means the headline features need a flag to
discover. Gate it either way in D2; only the default is in question.

**Pane refactor before D5, or accept four single-owner views?** The refactor and
the stash-map consolidation are one job. Doing it between D3 and D5 costs a
detour; doing it after means retrofitting four views instead of three.

**Does a re-run `:sql` query keep its identity?** Replacing the source under a
stable `SourceId::Virtual` preserves the cursor and the transform stack across
edits to the query. Minting a new id each run gives a history you can cycle back
through with `H`/`L`. The first is better for iterating on one query; the second
is better for comparing results.

**How do credentials for a remote database get supplied?** Read-only removes the
worst outcome but not the secret-handling problem. The options are a DSN in
`config.toml` (convenient, but a connection string in a dotfile), an environment
variable read at attach time, or requiring the user to `ATTACH` from a notebook
cell so the credential lives in their own code. The last is most consistent with
"writes go through code" and needs no new secret storage; it is also the least
convenient. Not blocking for D2 against local files — decide before the first
remote backend.

**Summary statistics on windowed sources: push down or label?** D1 ships before
pushdown exists, so it must label its row coverage. The question is whether D3
then upgrades it to a pushed-down exact summary, or whether "statistics over
what is loaded" is the permanent, clearly-labelled contract.
