use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use ropey::Rope;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Kernel runner script
// ---------------------------------------------------------------------------

const RUNNER_SCRIPT: &str = r#"
import sys, json, io, traceback, base64

_ns = {'__name__': '__main__'}
_inline_matplotlib = False

sys.stdout.write('__KI_READY__\n')
sys.stdout.flush()

while True:
    lines = []
    for line in sys.stdin:
        s = line.rstrip('\n')
        if s == '__KI_CODE_END__':
            break
        lines.append(s)

    # Handle IPython-style line magics. Only %matplotlib is processed;
    # everything else starting with % or ! is silently dropped for now.
    code_lines = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith('%matplotlib'):
            parts = stripped.split()
            backend = parts[1] if len(parts) > 1 else 'inline'
            if backend == 'inline':
                _inline_matplotlib = True
                try:
                    import matplotlib as _mpl
                    try:
                        _mpl.use('Agg', force=True)
                    except TypeError:
                        _mpl.use('Agg')
                except Exception:
                    pass
            else:
                _inline_matplotlib = False
                try:
                    import matplotlib as _mpl
                    try:
                        _mpl.use(backend, force=True)
                    except TypeError:
                        _mpl.use(backend)
                except Exception:
                    pass
        elif stripped.startswith('%') or stripped.startswith('!'):
            pass  # other magics/shell escapes ignored
        else:
            code_lines.append(line)

    code = '\n'.join(code_lines)
    _out, _err, _exc = io.StringIO(), io.StringIO(), None
    images = []

    sys.stdout, sys.stderr = _out, _err
    try:
        if code.strip():
            exec(compile(code, '<cell>', 'exec'), _ns)
        if _inline_matplotlib:
            try:
                import matplotlib.pyplot as _plt
                for _num in _plt.get_fignums():
                    _fig = _plt.figure(_num)
                    _buf = io.BytesIO()
                    _fig.savefig(_buf, format='png', bbox_inches='tight', dpi=150)
                    _buf.seek(0)
                    images.append(base64.b64encode(_buf.read()).decode('ascii'))
                _plt.close('all')
            except Exception:
                pass
    except SystemExit:
        pass
    except BaseException:
        _exc = traceback.format_exc()
    finally:
        sys.stdout, sys.stderr = sys.__stdout__, sys.__stderr__

    sys.stdout.write(json.dumps({
        'stdout': _out.getvalue(),
        'stderr': _err.getvalue(),
        'error': _exc,
        'images': images,
    }) + '\n')
    sys.stdout.write('__KI_OUTPUT_END__\n')
    sys.stdout.flush()
"#;

// ---------------------------------------------------------------------------
// Kernel session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelStatus {
    Idle,
    Busy,
    Dead,
}

pub struct KernelOutput {
    pub stdout: String,
    pub stderr: String,
    /// Full traceback string if an exception occurred.
    pub error: Option<String>,
    /// PNG images captured from matplotlib figures (only when %matplotlib inline is active).
    pub images: Vec<Vec<u8>>,
}

pub struct KernelSession {
    child: std::process::Child,
    stdin: std::io::BufWriter<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    pub execution_count: u32,
    pub status: KernelStatus,
    #[allow(dead_code)]
    pub python_executable: String,
}

impl KernelSession {
    /// Spawn a persistent Python kernel running the runner script.
    ///
    /// Blocks until the kernel prints `__KI_READY__` on stdout.
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
        let mut stdout = std::io::BufReader::new(stdout_raw);

        // Wait for __KI_READY__
        let mut buf = String::new();
        let mut attempts = 0usize;
        loop {
            if attempts >= 200 {
                anyhow::bail!("kernel did not send __KI_READY__ within 200 lines");
            }
            buf.clear();
            let n = stdout
                .read_line(&mut buf)
                .context("reading kernel startup output")?;
            if n == 0 {
                anyhow::bail!("kernel process exited before sending __KI_READY__");
            }
            if buf.trim() == "__KI_READY__" {
                break;
            }
            attempts += 1;
        }

        Ok(Self {
            child,
            stdin,
            stdout,
            execution_count: 0,
            status: KernelStatus::Idle,
            python_executable: python.to_owned(),
        })
    }

    /// Send `code` to the kernel and collect its output.
    pub fn execute_code(&mut self, code: &str) -> Result<KernelOutput> {
        self.status = KernelStatus::Busy;

        // Write each line, then the sentinel.
        for line in code.lines() {
            self.stdin.write_all(line.as_bytes())?;
            self.stdin.write_all(b"\n")?;
        }
        self.stdin.write_all(b"__KI_CODE_END__\n")?;
        self.stdin.flush()?;

        // Collect output lines until __KI_OUTPUT_END__.
        let mut payload_line = String::new();
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = self
                .stdout
                .read_line(&mut buf)
                .context("reading kernel output")?;
            if n == 0 {
                self.status = KernelStatus::Dead;
                anyhow::bail!("kernel process closed stdout unexpectedly");
            }
            let trimmed = buf.trim_end_matches('\n').trim_end_matches('\r');
            if trimmed == "__KI_OUTPUT_END__" {
                break;
            }
            payload_line = trimmed.to_owned();
        }

        self.execution_count += 1;
        self.status = KernelStatus::Idle;

        // Parse the JSON payload.
        let v: serde_json::Value =
            serde_json::from_str(&payload_line).context("parsing kernel output JSON")?;

        let stdout = v
            .get("stdout")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_owned();
        let stderr = v
            .get("stderr")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_owned();
        let error = v
            .get("error")
            .and_then(|s| s.as_str())
            .map(str::to_owned);
        let images: Vec<Vec<u8>> = v
            .get("images")
            .and_then(|arr| arr.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|img| {
                        img.as_str().and_then(|s| {
                            base64::engine::general_purpose::STANDARD.decode(s).ok()
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(KernelOutput { stdout, stderr, error, images })
    }

    /// Send SIGINT to the child process (Unix/macOS).
    pub fn interrupt(&self) {
        let _ = std::process::Command::new("kill")
            .args(["-2", &self.child.id().to_string()])
            .output();
    }

    /// Returns `true` if the kernel process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for KernelSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    #[allow(dead_code)]
    pub id: String,
    pub cell_type: CellType,
    pub source: Rope,
    pub outputs: Vec<Output>,
    pub execution_count: Option<u32>,
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
    /// Decoded from base64.
    pub image_png: Option<Vec<u8>>,
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
    let image_png = data.get("image/png").and_then(decode_base64);
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

/// Generate a pseudo-random hex ID without an external crate.
fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix in some extra bits to reduce collision chance between cells.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:016x}{:016x}", t as u64, n)
}

// ---------------------------------------------------------------------------
// Python kernel resolution
// ---------------------------------------------------------------------------

/// Find the best Python executable for the given directory.
/// Checks common virtual-environment layouts (.venv, venv, .env, env) in
/// the notebook's directory and the current working directory, then falls
/// back to the system `python3`.
///
/// Returns `(python_path, found_venv)`. When `found_venv` is false the
/// caller should warn the user that the system python3 is being used.
pub fn find_python_executable(base: &Path) -> (String, bool) {
    let venv_names = [".venv", "venv", ".env", "env"];

    let mut search = vec![base.to_path_buf()];
    if let Ok(cwd) = std::env::current_dir() {
        if cwd != base {
            search.push(cwd);
        }
    }

    for dir in &search {
        for name in &venv_names {
            let candidate = dir.join(name).join("bin").join("python");
            if candidate.is_file() {
                return (candidate.to_string_lossy().into_owned(), true);
            }
            // Windows layout (bin → Scripts)
            let candidate_win = dir.join(name).join("Scripts").join("python.exe");
            if candidate_win.is_file() {
                return (candidate_win.to_string_lossy().into_owned(), true);
            }
        }
    }

    ("python3".to_string(), false)
}

// ---------------------------------------------------------------------------
// Notebook impl
// ---------------------------------------------------------------------------

impl Notebook {
    /// Parse a `.ipynb` file (nbformat 4).
    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading notebook {}", path.display()))?;
        let json: Value =
            serde_json::from_str(&raw).context("parsing notebook JSON")?;

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
                .unwrap_or_else(gen_id);

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

            cells.push(Cell {
                id,
                cell_type,
                source,
                outputs,
                execution_count,
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
    pub fn save(&mut self) -> Result<()> {
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
                            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
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
                            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
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
                    cell.outputs.iter().map(|o| serialise_output(o)).collect(),
                );
            }
            obj
        }).collect();

        json["cells"] = Value::Array(new_cells);

        let serialised =
            serde_json::to_string_pretty(&json).context("serialising notebook JSON")?;
        std::fs::write(&self.path, serialised)
            .with_context(|| format!("writing notebook {}", self.path.display()))?;
        self.modified = false;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Cell execution
// ---------------------------------------------------------------------------

impl Cell {
    /// Execute the cell source using a persistent kernel session and populate outputs.
    pub fn execute(&mut self, session: &mut KernelSession) -> Result<()> {
        self.outputs.clear();
        let source = self.source.to_string();
        let out = session.execute_code(&source)?;
        self.execution_count = Some(session.execution_count);
        if !out.stdout.is_empty() {
            self.outputs.push(Output::Stream {
                name: "stdout".into(),
                text: out.stdout,
            });
        }
        if !out.stderr.is_empty() {
            self.outputs.push(Output::Stream {
                name: "stderr".into(),
                text: out.stderr,
            });
        }
        if let Some(tb) = out.error {
            let lines: Vec<String> = tb.lines().map(str::to_owned).collect();
            // Last non-empty line is typically "ExceptionType: message".
            let last = lines
                .iter()
                .rev()
                .find(|l| !l.trim().is_empty())
                .cloned()
                .unwrap_or_default();
            let (ename, evalue) = last
                .split_once(": ")
                .unwrap_or((&last, ""));
            self.outputs.push(Output::Error {
                ename: ename.to_owned(),
                evalue: evalue.to_owned(),
                traceback: lines,
            });
        }
        for image_bytes in out.images {
            self.outputs.push(Output::DisplayData {
                data: MimeData {
                    text_plain: None,
                    image_png: Some(image_bytes),
                },
            });
        }
        Ok(())
    }
}
