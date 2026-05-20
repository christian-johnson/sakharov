use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GutterMark {
    Added,
    Modified,
}

/// Compute per-line git diff marks for `path`.
///
/// Returns an empty map when the file is untracked, git is unavailable,
/// or no changes exist relative to HEAD.  Capped at 2 s so a slow git
/// invocation can't block the UI.
pub fn diff_marks(path: &Path) -> HashMap<usize, GutterMark> {
    use std::sync::mpsc;

    let path_str = match path.to_str() {
        Some(s) => s.to_owned(),
        None => return HashMap::new(),
    };

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new("git")
            .args(["diff", "--no-color", "--unified=0", "HEAD", "--", &path_str])
            .output();
        let _ = tx.send(out);
    });

    let output = match rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Ok(o)) => o,
        _ => return HashMap::new(),
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
