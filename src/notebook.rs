use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use anyhow::{Context, Result};
use base64::Engine as _;
use ropey::Rope;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Kernel runner script
// ---------------------------------------------------------------------------

// The runner streams output as it is produced: stdout/stderr are replaced with
// forwarders that emit one JSON message per write, so the editor can render
// progress bars (tqdm etc.) live instead of waiting for the cell to finish.
// Message shapes (one compact JSON object per line on real stdout):
//   {"t":"stream","name":"stdout"|"stderr","text":...}
//   {"t":"image","data":<base64 png>}
//   {"t":"error","text":<traceback>}
//   {"t":"done"}
const RUNNER_SCRIPT: &str = r#"
import sys, json, io, traceback, base64, ast

_ns = {'__name__': '__main__'}
_real_stdout = sys.stdout

def _emit(obj):
    _real_stdout.write(json.dumps(obj) + '\n')
    _real_stdout.flush()

def _emit_png_bytes(raw):
    _emit({'t': 'image', 'data': base64.b64encode(raw).decode('ascii')})

# Rasterise a LaTeX string (e.g. SymPy's _repr_latex_) to a PNG via matplotlib's
# mathtext engine, then emit it through the normal image channel.  mathtext
# supports a wide subset of LaTeX (fractions, powers, sqrt, greek, sums…), which
# covers typical SymPy output.  Returns True on success; the caller falls back
# to a plain-text repr when this returns False (no matplotlib, or unsupported
# markup).
def _emit_latex(latex):
    if not _capture_matplotlib:
        return False
    try:
        import matplotlib.pyplot as _plt
        s = latex.strip()
        # Strip surrounding $ / $$ delimiters and \displaystyle (unsupported).
        while s.startswith('$'):
            s = s[1:]
        while s.endswith('$'):
            s = s[:-1]
        s = s.replace('\\displaystyle', '').strip()
        if not s:
            return False
        expr = '$' + s + '$'
        _fig = _plt.figure()
        _fig.patch.set_facecolor('white')
        _fig.text(0.5, 0.5, expr, fontsize=18, ha='center', va='center', color='black')
        _buf = io.BytesIO()
        _fig.savefig(_buf, format='png', dpi=150, bbox_inches='tight', facecolor='white')
        _plt.close(_fig)
        _buf.seek(0)
        _emit_png_bytes(_buf.read())
        return True
    except Exception:
        return False

# Display the value of a cell's trailing expression, preferring rich reprs
# (LaTeX → PNG, then _repr_png_) and falling back to repr().
def _display_result(obj):
    try:
        m = getattr(obj, '_repr_latex_', None)
        latex = m() if callable(m) else None
    except Exception:
        latex = None
    if latex and _emit_latex(latex):
        return
    try:
        m = getattr(obj, '_repr_png_', None)
        png = m() if callable(m) else None
        if png:
            raw = png if isinstance(png, (bytes, bytearray)) else base64.b64decode(png)
            _emit_png_bytes(raw)
            return
    except Exception:
        pass
    _emit({'t': 'stream', 'name': 'stdout', 'text': repr(obj) + '\n'})

# At startup, try to configure matplotlib with the Agg (non-interactive) backend
# so that plt.show() captures figures without requiring %matplotlib inline.
_capture_matplotlib = False
try:
    import matplotlib as _mpl
    try:
        _mpl.use('Agg', force=True)
    except TypeError:
        _mpl.use('Agg')
    import matplotlib.pyplot as _plt_global
    _capture_matplotlib = True
    # We capture figures via savefig(); make show() a no-op so it doesn't
    # emit "FigureCanvasAgg is non-interactive" warnings on every plt.show() call.
    _plt_global.show = lambda **kw: None
except Exception:
    pass

class _Fwd(io.TextIOBase):
    def __init__(self, name):
        self._name = name
    def writable(self):
        return True
    def write(self, s):
        if s:
            _emit({'t': 'stream', 'name': self._name, 'text': s})
        return len(s)
    def flush(self):
        pass

_real_stdout.write('__KI_READY__\n')
_real_stdout.flush()

while True:
    lines = []
    for line in sys.stdin:
        s = line.rstrip('\n')
        if s == '__KI_CODE_END__':
            break
        lines.append(s)
    else:
        break  # stdin closed — kernel shutting down

    # Handle IPython-style line magics. Only %matplotlib is processed;
    # everything else starting with % or ! is silently dropped for now.
    code_lines = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith('%matplotlib'):
            parts = stripped.split()
            backend = parts[1] if len(parts) > 1 else 'inline'
            try:
                import matplotlib as _mpl
                if backend in ('inline', 'agg', 'Agg'):
                    try:
                        _mpl.use('Agg', force=True)
                    except TypeError:
                        _mpl.use('Agg')
                    _capture_matplotlib = True
                else:
                    try:
                        _mpl.use(backend, force=True)
                    except TypeError:
                        _mpl.use(backend)
                    _capture_matplotlib = False
            except Exception:
                pass
        elif stripped.startswith('%') or stripped.startswith('!'):
            pass  # other magics/shell escapes ignored
        else:
            code_lines.append(line)

    code = '\n'.join(code_lines)

    sys.stdout, sys.stderr = _Fwd('stdout'), _Fwd('stderr')
    try:
        if code.strip():
            # Split off a trailing bare expression so its value can be displayed
            # (rich repr if available), mirroring Jupyter's execute_result.
            _parsed = ast.parse(code, '<cell>', 'exec')
            _last_expr = None
            if _parsed.body and isinstance(_parsed.body[-1], ast.Expr):
                _last_expr = _parsed.body.pop()
            if _parsed.body:
                exec(compile(_parsed, '<cell>', 'exec'), _ns)
            if _last_expr is not None:
                _expr_ast = ast.fix_missing_locations(ast.Expression(_last_expr.value))
                _value = eval(compile(_expr_ast, '<cell>', 'eval'), _ns)
                if _value is not None:
                    _display_result(_value)
        if _capture_matplotlib:
            try:
                import matplotlib.pyplot as _plt
                fignums = _plt.get_fignums()
                for _num in fignums:
                    _fig = _plt.figure(_num)
                    _buf = io.BytesIO()
                    # No bbox_inches='tight' — preserve the figsize aspect ratio exactly.
                    _fig.savefig(_buf, format='png', dpi=150)
                    _buf.seek(0)
                    _emit({'t': 'image', 'data': base64.b64encode(_buf.read()).decode('ascii')})
                if fignums:
                    _plt.close('all')
            except Exception:
                pass
    except SystemExit:
        pass
    except BaseException:
        _emit({'t': 'error', 'text': traceback.format_exc()})
    finally:
        sys.stdout, sys.stderr = _real_stdout, sys.__stderr__

    _emit({'t': 'done'})
"#;

// ---------------------------------------------------------------------------
// Kernel session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelStatus {
    /// Spawned but still booting — waiting for the `__KI_READY__` handshake.
    Starting,
    Idle,
    Busy,
    Dead,
}

/// One incremental message from a running cell, produced by the kernel reader
/// thread and drained on the main thread via [`KernelSession::poll`].
pub enum KernelMessage {
    /// The kernel finished booting and sent `__KI_READY__`.
    Ready,
    /// A chunk of stdout/stderr text, emitted as the cell produces it.
    Stream { name: String, text: String },
    /// A captured matplotlib figure (decoded PNG bytes).
    Image { png: Vec<u8> },
    /// An uncaught exception traceback.
    Error { traceback: String },
    /// The cell finished executing.
    Done,
    /// The kernel process exited / closed its stdout.
    Dead,
}

pub struct KernelSession {
    child: std::process::Child,
    stdin: std::io::BufWriter<std::process::ChildStdin>,
    /// Messages from the background reader thread (drained by `poll`).
    rx: Receiver<KernelMessage>,
    pub execution_count: u32,
    pub status: KernelStatus,
    /// The interpreter this kernel runs (for status/log messages).
    pub python: String,
}

impl KernelSession {
    /// Spawn a persistent Python kernel running the runner script.
    ///
    /// Returns immediately with status [`KernelStatus::Starting`]; the
    /// background reader thread sends [`KernelMessage::Ready`] once the kernel
    /// prints `__KI_READY__`, so a slow Python boot (venv, matplotlib import)
    /// never blocks the UI.
    pub fn new(python: &str, notebook_dir: &Path) -> Result<Self> {
        use std::process::{Command, Stdio};

        let mut child = Command::new(python)
            .arg("-c")
            .arg(RUNNER_SCRIPT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(notebook_dir)
            .spawn()
            .with_context(|| format!("spawning kernel python executable '{python}'"))?;

        let stdin = child
            .stdin
            .take()
            .context("kernel child process has no stdin")?;
        let stdout_raw = child
            .stdout
            .take()
            .context("kernel child process has no stdout")?;

        let stdin = std::io::BufWriter::new(stdin);
        let stdout = std::io::BufReader::new(stdout_raw);

        // The reader thread performs the __KI_READY__ handshake and then
        // parses the JSON message stream; nothing here blocks on the child.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || reader_thread(stdout, tx));

        Ok(Self {
            child,
            stdin,
            rx,
            execution_count: 0,
            status: KernelStatus::Starting,
            python: python.to_owned(),
        })
    }

    /// Send `code` to the kernel and return immediately. Output arrives later
    /// as [`KernelMessage`]s via [`poll`](Self::poll); the kernel marks itself
    /// busy until a `Done` message is observed.
    pub fn start_execution(&mut self, code: &str) -> Result<()> {
        for line in code.lines() {
            self.stdin.write_all(line.as_bytes())?;
            self.stdin.write_all(b"\n")?;
        }
        self.stdin.write_all(b"__KI_CODE_END__\n")?;
        self.stdin.flush()?;
        self.status = KernelStatus::Busy;
        Ok(())
    }

    /// Non-blocking drain of all messages the reader thread has queued.
    pub fn poll(&mut self) -> Vec<KernelMessage> {
        let mut msgs = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }

    /// Send SIGINT to the child process (Unix/macOS).
    pub fn interrupt(&self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGINT);
        }
    }

    /// Returns `true` if the kernel process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for KernelSession {
    fn drop(&mut self) {
        // Killing the child closes its stdout, so the reader thread sees EOF
        // and exits on its own.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Background thread: wait for the `__KI_READY__` startup handshake (sending
/// `Ready`), then parse one JSON message per line from the kernel and forward
/// it to the session. Exits (sending `Dead`) when stdout closes.
fn reader_thread(
    mut reader: std::io::BufReader<std::process::ChildStdout>,
    tx: std::sync::mpsc::Sender<KernelMessage>,
) {
    let mut line = String::new();
    // Handshake phase: scan for the ready marker, skipping any noise the
    // interpreter prints while booting.
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send(KernelMessage::Dead);
                return;
            }
            Ok(_) => {}
        }
        if line.trim() == "__KI_READY__" {
            if tx.send(KernelMessage::Ready).is_err() {
                return; // session dropped
            }
            break;
        }
    }
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send(KernelMessage::Dead);
                return;
            }
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        // Lines that aren't our framed JSON (e.g. raw output from a subprocess
        // the cell spawned) are skipped rather than crashing the stream.
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let msg = match v.get("t").and_then(|t| t.as_str()) {
            Some("stream") => KernelMessage::Stream {
                name: v.get("name").and_then(|n| n.as_str()).unwrap_or("stdout").to_owned(),
                text: v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_owned(),
            },
            Some("image") => {
                match v.get("data").and_then(|d| d.as_str())
                    .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                {
                    Some(png) => KernelMessage::Image { png },
                    None => continue,
                }
            }
            Some("error") => KernelMessage::Error {
                traceback: v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_owned(),
            },
            Some("done") => KernelMessage::Done,
            _ => continue,
        };
        if tx.send(msg).is_err() {
            return; // session dropped
        }
    }
}

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

pub struct Notebook {
    pub path: PathBuf,
    pub metadata: NotebookMeta,
    pub cells: Vec<Cell>,
    pub modified: bool,
    pub kernel: Option<KernelSession>,
}

pub struct NotebookMeta {
    /// Kernel language, e.g. "python", "rust" — used for syntax highlighting.
    pub kernel_language: String,
}

#[derive(Clone)]
pub struct Cell {
    pub id: String,
    pub cell_type: CellType,
    pub source: Rope,
    pub outputs: Vec<Output>,
    pub execution_count: Option<u32>,
    /// Runtime-only display state for Markdown cells: `true` shows the formatted
    /// (highlighted) view, `false` shows the editable source.  Toggled by
    /// "executing" a markdown cell (render) vs. entering edit (source).  Not
    /// serialised — nbformat has no equivalent field.
    pub rendered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellType {
    Code,
    Markdown,
    Raw,
}

#[derive(Clone)]
pub enum Output {
    Stream {
        name: String,
        text: String,
    },
    DisplayData {
        data: MimeData,
    },
    ExecuteResult {
        execution_count: u32,
        data: MimeData,
    },
    Error {
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
}

#[derive(Clone)]
pub struct MimeData {
    pub text_plain: Option<String>,
    /// Decoded from base64.  Wrapped in Arc so passing it through each render
    /// frame is O(1) (ref-count bump) rather than O(n) (full copy).
    pub image_png: Option<std::sync::Arc<Vec<u8>>>,
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Join a JSON value that is either a string or an array of strings.
fn join_source(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .map(|s| s.as_str().unwrap_or(""))
            .collect::<String>(),
        _ => String::new(),
    }
}

/// Decode a JSON value that is a base64 string into bytes.
fn decode_base64(v: &Value) -> Option<Vec<u8>> {
    let s = v.as_str()?;
    // Strip whitespace / newlines that sometimes appear in notebook base64 blobs
    let cleaned: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .ok()
}

fn parse_mime_data(data: &Value) -> MimeData {
    let text_plain = data.get("text/plain").map(join_source).filter(|s| !s.is_empty());
    let image_png = data.get("image/png").and_then(decode_base64).map(std::sync::Arc::new);
    MimeData { text_plain, image_png }
}

fn parse_output(obj: &Value) -> Option<Output> {
    let output_type = obj.get("output_type")?.as_str()?;
    match output_type {
        "stream" => {
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("stdout")
                .to_string();
            let text = obj.get("text").map(join_source).unwrap_or_default();
            Some(Output::Stream { name, text })
        }
        "display_data" => {
            let data = obj.get("data").unwrap_or(&Value::Null);
            Some(Output::DisplayData {
                data: parse_mime_data(data),
            })
        }
        "execute_result" => {
            let execution_count = obj
                .get("execution_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let data = obj.get("data").unwrap_or(&Value::Null);
            Some(Output::ExecuteResult {
                execution_count,
                data: parse_mime_data(data),
            })
        }
        "error" => {
            let ename = obj
                .get("ename")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let evalue = obj
                .get("evalue")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let traceback = obj
                .get("traceback")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|s| s.as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default();
            Some(Output::Error { ename, evalue, traceback })
        }
        _ => None,
    }
}

/// Generate a unique cell ID without an external crate: nanosecond timestamp
/// mixed with a process-wide counter to avoid collisions between cells.
pub fn new_cell_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:016x}{:016x}", t as u64, n)
}

/// Produce the JSON text for a fresh, empty nbformat-4 notebook with a single
/// empty Python code cell.  Used by the `:new-notebook` command; the result
/// round-trips cleanly through `Notebook::from_path` and `Notebook::save`.
pub fn empty_notebook_json() -> String {
    let json = serde_json::json!({
        "cells": [
            {
                "cell_type": "code",
                "execution_count": null,
                "id": new_cell_id(),
                "metadata": {},
                "outputs": [],
                "source": []
            }
        ],
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3"
            },
            "language_info": {
                "name": "python"
            }
        },
        "nbformat": 4,
        "nbformat_minor": 5
    });
    serde_json::to_string_pretty(&json).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Virtual path / directory helpers (shared with exec and notebook_ui)
// ---------------------------------------------------------------------------

/// Resolve the parent directory of a notebook path, falling back to cwd.
pub fn notebook_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Build the virtual file path for a single notebook cell.
/// Used for LSP document identity and for looking up diagnostics.
pub fn cell_virtual_path(nb_path: &Path, lang: &str, idx: usize) -> PathBuf {
    let ext = crate::lang::lang_to_ext(lang);
    let stem = nb_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "notebook".into());
    let dir = notebook_dir(nb_path);
    dir.join(format!("{stem}__cell{idx}.{ext}"))
}

/// If `path` is one of this notebook's virtual cell paths, return its cell index.
///
/// LSP responses (go-to-definition / references / etc.) for a notebook come back
/// keyed by these virtual cell paths, which don't exist on disk. Callers use this
/// to jump to the cell in-place rather than trying to open the (nonexistent) file.
/// Comparison goes through `lsp::diagnostic_key` so relative/absolute forms agree.
pub fn cell_index_for_virtual_path(nb: &Notebook, path: &Path) -> Option<usize> {
    let target = crate::lsp::diagnostic_key(path);
    let lang = &nb.metadata.kernel_language;
    (0..nb.cells.len()).find(|&idx| {
        crate::lsp::diagnostic_key(&cell_virtual_path(&nb.path, lang, idx)) == target
    })
}

/// Virtual path of the notebook's shadow concatenated document.
///
/// Hover / signature-help / references requests are answered with full
/// cross-cell context by syncing all code cells, joined into one plain text
/// document, under this path (a URI only — nothing is ever written to disk)
/// and querying it instead of the single-cell virtual doc. See
/// `LspManager::request_via_shadow_doc`.
pub fn concat_virtual_path(nb_path: &Path, lang: &str) -> PathBuf {
    let ext = crate::lang::lang_to_ext(lang);
    let stem = nb_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "notebook".into());
    notebook_dir(nb_path).join(format!("{stem}__concat.{ext}"))
}

/// Join all code cells into one source string (cells separated by a newline,
/// matching how pylsp concatenates notebooks internally). Returns the text and
/// a `(cell_idx, start_line)` entry per code cell, in cell order.
///
/// `focused_override` substitutes the given rope for one cell's stored source —
/// while editing, `app.buffer` is ahead of `nb.cells[focused].source`, and the
/// shadow document must reflect what the user actually sees.
pub fn concat_source(
    nb: &Notebook,
    focused_override: Option<(usize, &Rope)>,
) -> (String, Vec<(usize, usize)>) {
    let mut text = String::new();
    let mut map = Vec::new();
    let mut line = 0usize;
    for (idx, cell) in nb.cells.iter().enumerate() {
        if cell.cell_type != CellType::Code {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        map.push((idx, line));
        let src = match focused_override {
            Some((focus_idx, rope)) if focus_idx == idx => rope.to_string(),
            _ => cell.source.to_string(),
        };
        line += src.matches('\n').count() + 1;
        text.push_str(&src);
    }
    (text, map)
}

/// Map a line in the shadow concatenated document back to
/// `(cell_idx, cell-relative line)`.
pub fn cell_for_concat_line(
    nb: &Notebook,
    focused_override: Option<(usize, &Rope)>,
    line: usize,
) -> Option<(usize, usize)> {
    let (_, map) = concat_source(nb, focused_override);
    map.iter()
        .rev()
        .find(|&&(_, start)| start <= line)
        .map(|&(idx, start)| (idx, line - start))
}

// ---------------------------------------------------------------------------
// Python kernel resolution
// ---------------------------------------------------------------------------

/// Find the best Python executable for the given directory.
/// Checks common virtual-environment layouts (.venv, venv, .env, env) by
/// walking up the directory tree from the notebook's directory (and, as a
/// fallback, from the current working directory), then falls back to the
/// system `python3`. Walking up matters because a notebook commonly lives in
/// a subdirectory of the project whose venv is at the project root — this is
/// the same ancestor search the LSP uses (`lsp_manager::detect_python_venv`),
/// so the kernel and LSP agree on which interpreter the project uses.
///
/// Returns `(python_path, found_venv)`. When `found_venv` is false the
/// caller should warn the user that the system python3 is being used.
pub fn find_python_executable(base: &Path) -> (String, bool) {
    // Search `base` and its ancestors first (most specific to the notebook),
    // then the cwd and its ancestors as a fallback.
    let mut roots = vec![base.to_path_buf()];
    if let Ok(cwd) = std::env::current_dir() {
        if cwd != base {
            roots.push(cwd);
        }
    }

    for root in &roots {
        if let Some(python) = venv_python_up(root) {
            return (python.to_string_lossy().into_owned(), true);
        }
    }

    ("python3".to_string(), false)
}

/// Walk up the directory tree from `start` looking for a project virtualenv
/// (`.venv`/`venv`/`.env`/`env`); return the path to its python interpreter.
///
/// This is the single venv discovery used by **both** the notebook kernel
/// (`find_python_executable`) and the Python language server
/// (`lsp_manager::ensure_server`), so the code the user runs and the
/// environment jedi resolves against are always the same interpreter.
pub fn venv_python_up(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if let Some(python) = venv_python_in(d) {
            return Some(python);
        }
        dir = d.parent();
    }
    None
}

/// If `dir` directly contains a recognised virtualenv layout, return the path
/// to its python interpreter.
fn venv_python_in(dir: &Path) -> Option<PathBuf> {
    for name in [".venv", "venv", ".env", "env"] {
        let venv = dir.join(name);
        // Unix layout (python3 preferred), then Windows (bin → Scripts).
        for rel in ["bin/python3", "bin/python", "Scripts/python.exe"] {
            let candidate = venv.join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Notebook impl
// ---------------------------------------------------------------------------

impl Notebook {
    /// Parse a `.ipynb` file (nbformat 4).
    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading notebook {}", path.display()))?;
        Self::from_json_str(path, &raw)
    }

    /// Parse a notebook from an in-memory nbformat JSON string, associating it
    /// with `path`.  Shared by `from_path` (disk) and crash recovery (recovery
    /// file).  The returned notebook has `modified = false` and no kernel.
    pub fn from_json_str(path: &Path, raw: &str) -> Result<Self> {
        let json: Value =
            serde_json::from_str(raw).context("parsing notebook JSON")?;

        // Kernel language
        let kernel_language = json
            .pointer("/metadata/kernelspec/language")
            .and_then(|v| v.as_str())
            .unwrap_or("python")
            .to_string();

        let cells_json = json
            .get("cells")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut cells = Vec::with_capacity(cells_json.len());
        for cell_obj in &cells_json {
            let cell_type = match cell_obj
                .get("cell_type")
                .and_then(|v| v.as_str())
                .unwrap_or("raw")
            {
                "code" => CellType::Code,
                "markdown" => CellType::Markdown,
                _ => CellType::Raw,
            };

            let id = cell_obj
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(new_cell_id);

            let source_str = cell_obj
                .get("source")
                .map(join_source)
                .unwrap_or_default();
            let source = Rope::from_str(&source_str);

            let execution_count = cell_obj
                .get("execution_count")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);

            let outputs = cell_obj
                .get("outputs")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(parse_output).collect())
                .unwrap_or_default();

            // Markdown cells open in their formatted (rendered) view, like a
            // freshly-opened notebook in Jupyter.
            let rendered = cell_type == CellType::Markdown;
            cells.push(Cell {
                id,
                cell_type,
                source,
                outputs,
                execution_count,
                rendered,
            });
        }

        Ok(Self {
            path: path.to_path_buf(),
            metadata: NotebookMeta { kernel_language },
            cells,
            modified: false,
            kernel: None,
        })
    }

    /// Start (or restart) the persistent Python kernel for this notebook.
    /// Returns `Ok(true)` when a venv was found, `Ok(false)` when falling
    /// back to system python3 (the caller should surface a warning).
    pub fn start_kernel(&mut self, notebook_dir: &Path) -> Result<bool> {
        let (python, found_venv) = find_python_executable(notebook_dir);
        self.kernel = Some(KernelSession::new(&python, notebook_dir)?);
        Ok(found_venv)
    }

    /// Serialise the notebook back to `self.path` as valid nbformat 4 JSON.
    /// The write is atomic (temp + rename) so a crash can't truncate the file.
    pub fn save(&mut self) -> Result<()> {
        let serialised = self.to_nbformat_string()?;
        crate::buffer::atomic_write(&self.path, &serialised)
            .with_context(|| format!("writing notebook {}", self.path.display()))?;
        self.modified = false;
        Ok(())
    }

    /// Serialise the in-memory notebook to an nbformat-4 JSON string, preserving
    /// the on-disk notebook-level metadata (nbformat, kernelspec, …).  Shared by
    /// `save` (writes to disk) and crash recovery (writes to a recovery file).
    pub fn to_nbformat_string(&self) -> Result<String> {
        // Serialise source as array of lines; each line ends with '\n' except the last.
        let serialise_source = |rope: &Rope| -> Value {
            let text = rope.to_string();
            if text.is_empty() {
                return Value::Array(vec![]);
            }
            let lines: Vec<&str> = text.split('\n').collect();
            let n = lines.len();
            let mut arr: Vec<Value> = lines
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    if i + 1 < n {
                        Value::String(format!("{line}\n"))
                    } else {
                        Value::String((*line).to_string())
                    }
                })
                .collect();
            // Drop trailing empty string produced by a trailing newline.
            if let Some(Value::String(last)) = arr.last() {
                if last.is_empty() {
                    arr.pop();
                }
            }
            Value::Array(arr)
        };

        let serialise_output = |o: &Output| -> Value {
            match o {
                Output::Stream { name, text } => {
                    let lines: Vec<Value> = text
                        .split('\n')
                        .enumerate()
                        .map(|(i, line)| {
                            let s = if i + 1 < text.split('\n').count() {
                                format!("{line}\n")
                            } else {
                                line.to_string()
                            };
                            Value::String(s)
                        })
                        .collect();
                    serde_json::json!({
                        "output_type": "stream",
                        "name": name,
                        "text": lines,
                    })
                }
                Output::DisplayData { data } => {
                    let mut d = serde_json::Map::new();
                    if let Some(t) = &data.text_plain {
                        d.insert("text/plain".into(), Value::String(t.clone()));
                    }
                    if let Some(bytes) = &data.image_png {
                        d.insert(
                            "image/png".into(),
                            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes.as_slice())),
                        );
                    }
                    serde_json::json!({ "output_type": "display_data", "data": d, "metadata": {} })
                }
                Output::ExecuteResult { execution_count, data } => {
                    let mut d = serde_json::Map::new();
                    if let Some(t) = &data.text_plain {
                        d.insert("text/plain".into(), Value::String(t.clone()));
                    }
                    if let Some(bytes) = &data.image_png {
                        d.insert(
                            "image/png".into(),
                            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes.as_slice())),
                        );
                    }
                    serde_json::json!({
                        "output_type": "execute_result",
                        "execution_count": execution_count,
                        "data": d,
                        "metadata": {},
                    })
                }
                Output::Error { ename, evalue, traceback } => serde_json::json!({
                    "output_type": "error",
                    "ename": ename,
                    "evalue": evalue,
                    "traceback": traceback,
                }),
            }
        };

        // Read existing JSON so we preserve notebook-level metadata (nbformat,
        // kernelspec, etc.) without having to round-trip it through our structs.
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading notebook {}", self.path.display()))?;
        let mut json: Value = serde_json::from_str(&raw).context("parsing notebook JSON")?;

        // Rebuild the cells array completely from self.cells.  Patching by index
        // (the old approach) silently dropped any cells that were added or deleted.
        let new_cells: Vec<Value> = self.cells.iter().map(|cell| {
            let cell_type_str = match cell.cell_type {
                CellType::Code => "code",
                CellType::Markdown => "markdown",
                CellType::Raw => "raw",
            };
            let mut obj = serde_json::json!({
                "id": cell.id,
                "cell_type": cell_type_str,
                "metadata": {},
                "source": serialise_source(&cell.source),
            });
            // Only code cells carry outputs and execution_count.
            if matches!(cell.cell_type, CellType::Code) {
                obj["execution_count"] = match cell.execution_count {
                    Some(n) => Value::Number(n.into()),
                    None => Value::Null,
                };
                obj["outputs"] = Value::Array(
                    cell.outputs.iter().map(&serialise_output).collect(),
                );
            }
            obj
        }).collect();

        json["cells"] = Value::Array(new_cells);

        serde_json::to_string_pretty(&json).context("serialising notebook JSON")
    }
}

// ---------------------------------------------------------------------------
// Output helpers (used by the async streaming-execution handler)
// ---------------------------------------------------------------------------

/// Append a streamed stdout/stderr chunk to `outputs`, merging into the
/// trailing stream of the same name and honouring carriage returns so that
/// in-place progress bars (tqdm) render as a single updating line.
pub fn append_stream(outputs: &mut Vec<Output>, name: &str, chunk: &str) {
    let merge = matches!(outputs.last(), Some(Output::Stream { name: n, .. }) if n == name);
    if !merge {
        outputs.push(Output::Stream { name: name.to_owned(), text: String::new() });
    }
    if let Some(Output::Stream { text, .. }) = outputs.last_mut() {
        let mut chars = chunk.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                // CR not part of CRLF: return to start of the current line so
                // the next writes overwrite it.
                '\r' if chars.peek() != Some(&'\n') => {
                    let line_start = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
                    text.truncate(line_start);
                }
                '\r' => {} // CRLF — drop the CR, the '\n' handles the newline
                c => text.push(c),
            }
        }
    }
}

/// Push an `Error` output parsed from a Python traceback string.
pub fn push_error_output(outputs: &mut Vec<Output>, traceback: &str) {
    let lines: Vec<String> = traceback.lines().map(str::to_owned).collect();
    // Last non-empty line is typically "ExceptionType: message".
    let last = lines
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .cloned()
        .unwrap_or_default();
    let (ename, evalue) = last.split_once(": ").unwrap_or((&last, ""));
    outputs.push(Output::Error {
        ename: ename.to_owned(),
        evalue: evalue.to_owned(),
        traceback: lines,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nb_with(cells: Vec<(CellType, &str)>) -> Notebook {
        Notebook {
            path: PathBuf::from("/tmp/test.ipynb"),
            metadata: NotebookMeta { kernel_language: "python".into() },
            cells: cells
                .into_iter()
                .map(|(cell_type, src)| Cell {
                    id: String::new(),
                    cell_type,
                    source: Rope::from_str(src),
                    outputs: vec![],
                    execution_count: None,
                    rendered: false,
                })
                .collect(),
            modified: false,
            kernel: None,
        }
    }

    #[test]
    fn concat_skips_markdown_and_tracks_offsets() {
        let nb = nb_with(vec![
            (CellType::Code, "import numpy as np\n"), // lines 0-1 (trailing \n -> empty line 1)
            (CellType::Markdown, "# heading\n"),      // excluded
            (CellType::Code, "x = 1\ny = 2"),         // starts at line 2
        ]);
        let (text, map) = concat_source(&nb, None);
        assert_eq!(text, "import numpy as np\n\nx = 1\ny = 2");
        assert_eq!(map, vec![(0, 0), (2, 2)]);
    }

    #[test]
    fn concat_focused_override_replaces_cell_source() {
        let nb = nb_with(vec![
            (CellType::Code, "a = 1"),
            (CellType::Code, "stale"),
        ]);
        let fresh = Rope::from_str("fresh = True\nmore = 2");
        let (text, map) = concat_source(&nb, Some((1, &fresh)));
        assert_eq!(text, "a = 1\nfresh = True\nmore = 2");
        assert_eq!(map, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn concat_line_maps_back_to_cell() {
        let nb = nb_with(vec![
            (CellType::Code, "import numpy as np\n"), // concat lines 0-1
            (CellType::Markdown, "skip"),
            (CellType::Code, "x = 1\ny = 2"),         // concat lines 2-3
        ]);
        assert_eq!(cell_for_concat_line(&nb, None, 0), Some((0, 0)));
        assert_eq!(cell_for_concat_line(&nb, None, 2), Some((2, 0)));
        assert_eq!(cell_for_concat_line(&nb, None, 3), Some((2, 1)));
    }

    #[test]
    fn concat_round_trips_cell_starts() {
        let nb = nb_with(vec![
            (CellType::Code, "def foo(a, b):\n    return a + b\n"),
            (CellType::Code, "foo(\n"),
            (CellType::Code, "z = 3"),
        ]);
        let (_, map) = concat_source(&nb, None);
        for &(idx, start) in &map {
            assert_eq!(cell_for_concat_line(&nb, None, start), Some((idx, 0)));
        }
    }
}
