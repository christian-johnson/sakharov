use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GutterMark {
    Added,
    Modified,
}

/// Result of a background git refresh: current branch + per-line diff marks.
pub struct GitInfo {
    pub branch: Option<String>,
    pub diff: HashMap<usize, GutterMark>,
}

/// An in-flight background git refresh.  Poll with [`GitRefresh::poll`] from
/// the run loop; the result arrives without ever blocking the UI thread.
pub struct GitRefresh {
    rx: Receiver<GitInfo>,
}

impl GitRefresh {
    /// Non-blocking: `Some(info)` once the background git commands finish.
    pub fn poll(&self) -> Option<GitInfo> {
        self.rx.try_recv().ok()
    }
}

/// Start a background refresh of the git branch and (when `path` is given)
/// the per-line diff marks for that file.  Never blocks: a slow or absent
/// git simply means the result arrives late or reads as "no repo".
pub fn refresh(path: Option<PathBuf>) -> GitRefresh {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let info = GitInfo {
            branch: query_branch(),
            diff: path.as_deref().map(query_diff_marks).unwrap_or_default(),
        };
        let _ = tx.send(info);
    });
    GitRefresh { rx }
}

/// Blocking: current git branch name, or `None` when git is unavailable or
/// the working directory is not inside a repository.  Run on the refresh
/// thread only — never call from the UI thread.
fn query_branch() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Blocking: per-line git diff marks for `path` (empty when untracked /
/// unavailable / unchanged).  Run on the refresh thread only.
fn query_diff_marks(path: &std::path::Path) -> HashMap<usize, GutterMark> {
    let Some(path_str) = path.to_str() else {
        return HashMap::new();
    };
    let output = match std::process::Command::new("git")
        .args(["diff", "--no-color", "--unified=0", "HEAD", "--", path_str])
        .output()
    {
        Ok(o) => o,
        Err(_) => return HashMap::new(),
    };
    if output.stdout.is_empty() {
        return HashMap::new();
    }
    parse_diff(&String::from_utf8_lossy(&output.stdout))
}

fn parse_diff(diff: &str) -> HashMap<usize, GutterMark> {
    let mut marks: HashMap<usize, GutterMark> = HashMap::new();
    // new_start is 1-indexed (from the hunk header "+N[,M]").
    let mut new_start = 0usize;
    // How many `-` lines have been seen since the last `+` within a hunk.
    let mut pending_del = 0usize;
    // How many `+` lines have been emitted in the current hunk.
    let mut new_off = 0usize;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            pending_del = 0;
            new_off = 0;
            // Extract "+N[,M]" → new_start = N.
            if let Some(plus_part) = rest.split('+').nth(1) {
                let token = plus_part.split_whitespace().next().unwrap_or("1");
                new_start = token
                    .split(',')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            // Convert 1-indexed new_start to 0-indexed.
            let line_no = new_start.saturating_sub(1) + new_off;
            let mark = if pending_del > 0 {
                pending_del -= 1;
                GutterMark::Modified
            } else {
                GutterMark::Added
            };
            marks.insert(line_no, mark);
            new_off += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            pending_del += 1;
        }
        // With --unified=0 there are no context lines to skip.
    }

    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_diff() {
        let diff = "@@ -10,0 +11,3 @@\n+added 1\n+added 2\n+added 3\n@@ -20,2 +23,2 @@\n-old 1\n-old 2\n+new 1\n+new 2";
        let marks = parse_diff(diff);
        assert_eq!(marks.get(&10), Some(&GutterMark::Added));
        assert_eq!(marks.get(&11), Some(&GutterMark::Added));
        assert_eq!(marks.get(&12), Some(&GutterMark::Added));
        assert_eq!(marks.get(&22), Some(&GutterMark::Modified));
        assert_eq!(marks.get(&23), Some(&GutterMark::Modified));
        assert_eq!(marks.len(), 5);
    }
}
