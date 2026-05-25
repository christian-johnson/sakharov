/// Write `text` to the system clipboard. Silent failure if no clipboard tool is available.
pub fn write(text: &str) {
    if try_write_cmd("wl-copy", &[], text) {
        return;
    }
    if try_write_cmd("xclip", &["-selection", "clipboard"], text) {
        return;
    }
    if try_write_cmd("xsel", &["--clipboard", "--input"], text) {
        return;
    }
    let _ = try_write_cmd("pbcopy", &[], text);
}

/// Read text from the system clipboard. Returns `None` if unavailable or empty.
pub fn read() -> Option<String> {
    try_read_cmd("wl-paste", &["--no-newline"])
        .or_else(|| try_read_cmd("xclip", &["-selection", "clipboard", "-o"]))
        .or_else(|| try_read_cmd("xsel", &["--clipboard", "--output"]))
        .or_else(|| try_read_cmd("pbpaste", &[]))
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
    let _ = child.wait();
    true
}

fn try_read_cmd(cmd: &str, args: &[&str]) -> Option<String> {
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
    if s.is_empty() { None } else { Some(s) }
}
