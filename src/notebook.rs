use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use ropey::Rope;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

pub struct Notebook {
    pub path: PathBuf,
    pub metadata: NotebookMeta,
    pub cells: Vec<Cell>,
    pub modified: bool,
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
        /// Navigable traceback frames — the `File "Cell [N]", line L` lines that
        /// map to a source line the cursor can jump to. Runtime-only (derived
        /// from the traceback, not part of nbformat): rebuilt on load from the
        /// displayed labels, and at kernel time from the cells' compile ids.
        frames: Vec<ErrorFrame>,
    },
}

/// One navigable frame inside an error traceback: a `File "…", line L` line that
/// resolves to a concrete `(cell, line)` the cursor can jump to.
#[derive(Clone)]
pub struct ErrorFrame {
    /// Index into the owning `Output::Error`'s `traceback` Vec of the `File`
    /// line this frame styles / navigates from.
    pub tb_index: usize,
    /// The target cell's stable id when known (frames built at kernel time);
    /// `None` for frames rebuilt from a reloaded notebook, which only carry the
    /// 1-based `cell_number` printed in the label.
    pub cell_id: Option<String>,
    /// 1-based cell number, as shown in the `Cell [N]` label.
    pub cell_number: usize,
    /// 0-based line within the target cell.
    pub line: usize,
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
            let traceback: Vec<String> = obj
                .get("traceback")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|s| s.as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default();
            let frames = frames_from_display(&traceback);
            Some(Output::Error { ename, evalue, traceback, frames })
        }
        _ => None,
    }
}

/// Parse a CPython traceback frame line — `  File "NAME", line N, in …` —
/// returning `(name, line_number)`. Returns `None` for non-frame lines.
fn parse_frame_line(line: &str) -> Option<(String, usize)> {
    let rest = line.trim_start().strip_prefix("File \"")?;
    let close = rest.find('"')?;
    let name = rest[..close].to_string();
    let after = rest[close + 1..].strip_prefix(", line ")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    Some((name, digits.parse().ok()?))
}

/// `"Cell [3]"` → `3`. The friendly label `build_error_output` writes for a
/// notebook cell frame (and that `frames_from_display` reads back on reload).
fn cell_number_from_label(name: &str) -> Option<usize> {
    name.strip_prefix("Cell [")?.strip_suffix(']')?.parse().ok()
}

/// Rebuild navigable frames from an already-displayed traceback (`Cell [N]`
/// labels) — the reload path, where the original compile ids are gone.
fn frames_from_display(traceback: &[String]) -> Vec<ErrorFrame> {
    traceback
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let (name, lineno) = parse_frame_line(line)?;
            let number = cell_number_from_label(&name)?;
            Some(ErrorFrame {
                tb_index: i,
                cell_id: None,
                cell_number: number,
                line: lineno.saturating_sub(1),
            })
        })
        .collect()
}

/// Build an `Output::Error` from a raw kernel traceback.
///
/// The kernel compiles each cell under its stable id as the filename, so a
/// traceback frame `File "<id>", line L` names the exact cell + line that
/// raised. We resolve those frames against `cells`, record them as navigable
/// [`ErrorFrame`]s, and rewrite the filename to a friendly `Cell [N]` label for
/// display (frames from library files are left untouched and non-navigable).
pub fn build_error_output(traceback: &str, cells: &[Cell]) -> Output {
    let mut lines: Vec<String> = traceback.lines().map(str::to_owned).collect();
    let mut frames = Vec::new();
    for (i, line) in lines.iter_mut().enumerate() {
        let Some((name, lineno)) = parse_frame_line(line) else { continue };
        let Some(cidx) = cells.iter().position(|c| c.id == name) else { continue };
        let number = cidx + 1;
        frames.push(ErrorFrame {
            tb_index: i,
            cell_id: Some(cells[cidx].id.clone()),
            cell_number: number,
            line: lineno.saturating_sub(1),
        });
        *line = line.replacen(&format!("\"{name}\""), &format!("\"Cell [{number}]\""), 1);
    }
    let (ename, evalue) = split_error_headline(&lines);
    Output::Error { ename, evalue, traceback: lines, frames }
}

/// Split a traceback's last non-empty line — typically `ExceptionType: message`
/// — into `(ename, evalue)`.
fn split_error_headline(lines: &[String]) -> (String, String) {
    let last = lines
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .cloned()
        .unwrap_or_default();
    let (ename, evalue) = last.split_once(": ").unwrap_or((&last, ""));
    (ename.to_owned(), evalue.to_owned())
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
    /// file).  The returned notebook has `modified = false`; the kernel is the
    /// editor's (`app.compute`), not the notebook's.
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
        })
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
                Output::Error { ename, evalue, traceback, .. } => serde_json::json!({
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
        }
    }

    fn cell_with_id(id: &str, src: &str) -> Cell {
        Cell {
            id: id.into(),
            cell_type: CellType::Code,
            source: Rope::from_str(src),
            outputs: vec![],
            execution_count: None,
            rendered: false,
        }
    }

    #[test]
    fn build_error_output_resolves_and_relabels_cell_frame() {
        // Two cells; the error's in-cell frame names cell "bbb" (the 2nd cell).
        let cells = vec![cell_with_id("aaa", "x = 1"), cell_with_id("bbb", "data = []\nprint(data[10])")];
        let tb = "Traceback (most recent call last):\n  \
                  File \"bbb\", line 2, in <module>\n    \
                  print(data[10])\nIndexError: list index out of range";
        let out = build_error_output(tb, &cells);
        let Output::Error { ename, traceback, frames, .. } = out else { panic!("not an error") };
        assert_eq!(ename, "IndexError");
        // Exactly one navigable frame → cell index 1 (number 2), line 1 (0-based).
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].cell_id.as_deref(), Some("bbb"));
        assert_eq!(frames[0].cell_number, 2);
        assert_eq!(frames[0].line, 1);
        // The frame's traceback row was relabelled for display.
        assert!(traceback[frames[0].tb_index].contains("\"Cell [2]\""));
        assert!(!traceback[frames[0].tb_index].contains("\"bbb\""));
    }

    #[test]
    fn build_error_output_ignores_library_frames() {
        let cells = vec![cell_with_id("aaa", "raise ValueError('x')")];
        // A frame from a real file must not be treated as navigable.
        let tb = "Traceback (most recent call last):\n  \
                  File \"/usr/lib/python3.11/foo.py\", line 42, in bar\n\
                  ValueError: x";
        let Output::Error { frames, traceback, .. } = build_error_output(tb, &cells) else {
            panic!("not an error")
        };
        assert!(frames.is_empty());
        assert!(traceback.iter().any(|l| l.contains("/usr/lib/python3.11/foo.py")));
    }

    #[test]
    fn frames_rebuild_from_displayed_labels_on_reload() {
        // The reload path only has the friendly `Cell [N]` labels to work from.
        let traceback = vec![
            "Traceback (most recent call last):".to_string(),
            "  File \"Cell [3]\", line 5, in <module>".to_string(),
            "ZeroDivisionError: division by zero".to_string(),
        ];
        let frames = frames_from_display(&traceback);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].tb_index, 1);
        assert_eq!(frames[0].cell_id, None);
        assert_eq!(frames[0].cell_number, 3);
        assert_eq!(frames[0].line, 4);
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
