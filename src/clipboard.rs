//! The system clipboard, through whichever command-line tool the platform has.

use std::sync::OnceLock;

/// A clipboard tool: `(write command, write args, read command, read args)`.
type Tool = (&'static str, &'static [&'static str], &'static str, &'static [&'static str]);

const TOOLS: &[Tool] = &[
    ("pbcopy", &[], "pbpaste", &[]),
    ("wl-copy", &[], "wl-paste", &["--no-newline"]),
    ("xclip", &["-selection", "clipboard"], "xclip", &["-selection", "clipboard", "-o"]),
    ("xsel", &["--clipboard", "--input"], "xsel", &["--clipboard", "--output"]),
];

/// The tool that worked last time, so a yank is one process rather than a
/// failed spawn for every tool listed ahead of it.
static TOOL: OnceLock<Tool> = OnceLock::new();

/// The known-good tool first, else every candidate.
fn candidates() -> impl Iterator<Item = &'static Tool> {
    TOOL.get().into_iter().chain(TOOLS.iter().filter(|_| TOOL.get().is_none()))
}

/// Write `text` to the system clipboard. Silent failure if no clipboard tool is available.
pub fn write(text: &str) {
    // Tests share one real clipboard; they use the editor's own register.
    if cfg!(test) {
        return;
    }
    for tool in candidates() {
        if try_write_cmd(tool.0, tool.1, text) {
            let _ = TOOL.set(*tool);
            return;
        }
    }
}

/// Read text from the system clipboard. Returns `None` if unavailable or empty.
pub fn read() -> Option<String> {
    if cfg!(test) {
        return None;
    }
    for tool in candidates() {
        if let Some(text) = try_read_cmd(tool.2, tool.3) {
            let _ = TOOL.set(*tool);
            return text;
        }
    }
    None
}

fn try_write_cmd(cmd: &str, args: &[&str], text: &str) -> bool {
    use std::io::Write;
    let child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    drop(child.stdin.take());
    child.wait().is_ok_and(|status| status.success())
}

/// `None` when the tool is missing or failed; `Some(None)` for an empty clipboard.
fn try_read_cmd(cmd: &str, args: &[&str]) -> Option<Option<String>> {
    let output = std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).to_string();
    Some((!s.is_empty()).then_some(s))
}
