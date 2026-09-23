//! Colouring git's own output, and the output of whatever git runs.
//!
//! Three buffers show text this module wrote nothing of: a commit's diff
//! (`*commit …*`), the transcript of a streamed command (`*git output*`, see
//! [`crate::vcs::run`]), and `git status` in a float.  All three used to be
//! flat foreground text, which for a diff means the one distinction that
//! matters — this line was added, that one was taken away — is left to a `+`
//! and a `-` in column zero.
//!
//! ## Why not just pass the terminal's colours through
//!
//! git can colour its own output, and a hook usually can too.  But both of
//! them check whether they are talking to a terminal, and here they are
//! talking to a pipe — so most of the time there is nothing to pass through,
//! and what does arrive is an escape sequence that has to be stripped before
//! it reaches a rope anyway (`run::clean`).  Colouring the *text* works
//! whether or not the program that printed it felt like colouring it, and it
//! comes out in the user's theme like everything else the editor draws.
//!
//! Emits the same [`Span`] list every other highlighter produces, so nothing
//! downstream knows the difference — the same arrangement `crate::markdown`
//! and `crate::sql_highlight` have.

use std::path::Path;

use ropey::Rope;

use crate::highlight::{Span, GIT_ADDED, GIT_HASH, GIT_HUNK, GIT_META, GIT_REMOVED, GIT_WARNING};

/// Which part of `git status` the scan is inside.
///
/// The same line (`\tmodified:   src/main.rs`) is green under "Changes to be
/// committed" and red under "Changes not staged", and nothing about the line
/// itself says which — so the heading above it has to be remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    None,
    Staged,
    Unstaged,
}

/// True for the buffers git's output is read in.
pub fn is_git_output(path: Option<&Path>) -> bool {
    let Some(path) = path else { return false };
    if let Some(name) = path.to_str() {
        if name == crate::app::GIT_OUTPUT_BUFFER
            || name.starts_with(crate::app::COMMIT_BUFFER_PREFIX)
        {
            return true;
        }
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("diff") | Some("patch")
    )
}

/// Spans for a whole buffer of git output.
pub fn highlight(rope: &Rope) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut section = Section::default();
    let mut start = 0usize;
    for line in rope.lines() {
        let text = line.to_string();
        let trimmed = text.trim_end_matches(['\n', '\r']);
        for (from, to, index) in line_spans(trimmed, &mut section) {
            spans.push((start + from, start + to, index));
        }
        start += line.len_chars();
    }
    spans
}

/// How one line of git output is coloured, as char ranges within the line.
///
/// `section` is carried between lines for `git status`; a caller with a single
/// line to colour (a diff pane, which only ever holds a diff) can pass a fresh
/// one.
pub fn line_spans(line: &str, section: &mut Section) -> Vec<(usize, usize, usize)> {
    let len = line.chars().count();
    let all = |index: usize| vec![(0, len, index)];
    if line.is_empty() {
        return Vec::new();
    }

    // --- a diff ---
    if line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("old mode")
        || line.starts_with("new mode")
        || line.starts_with("similarity ")
        || line.starts_with("rename ")
        || line.starts_with("Binary files")
        || line.starts_with(r"\ No newline")
    {
        return all(GIT_META);
    }
    if line.starts_with("@@") {
        // The range is the useful part; the function context git appends after
        // it is ordinary code, and colouring it as a hunk header claims it is
        // part of the address.
        let end = line
            .char_indices()
            .skip(2)
            .collect::<Vec<_>>()
            .windows(2)
            .find(|w| w[0].1 == '@' && w[1].1 == '@')
            .map_or(len, |w| line[..w[1].0].chars().count() + 1);
        return vec![(0, end.min(len), GIT_HUNK)];
    }
    if line.starts_with('+') {
        return all(GIT_ADDED);
    }
    if line.starts_with('-') {
        return all(GIT_REMOVED);
    }

    // --- a commit header ---
    if let Some(hash) = line.strip_prefix("commit ") {
        let hash_len = hash.chars().take_while(char::is_ascii_hexdigit).count();
        return vec![(0, 6, GIT_META), (7, 7 + hash_len, GIT_HASH)];
    }
    for label in ["Author:", "AuthorDate:", "Commit:", "CommitDate:", "Date:", "Merge:"] {
        if line.starts_with(label) {
            return vec![(0, label.chars().count(), GIT_META)];
        }
    }

    // --- git status ---
    if line.starts_with("Changes to be committed") {
        *section = Section::Staged;
        return all(GIT_META);
    }
    if line.starts_with("Changes not staged")
        || line.starts_with("Untracked files")
        || line.starts_with("Unmerged paths")
    {
        *section = Section::Unstaged;
        return all(GIT_META);
    }
    if line.starts_with("On branch ")
        || line.starts_with("Your branch")
        || line.starts_with("HEAD detached")
        || line.starts_with("No commits yet")
    {
        return all(GIT_META);
    }
    // git indents the files themselves with a tab and its own advice with
    // spaces, which is the only thing that tells them apart.
    if line.starts_with('\t') {
        return match section {
            Section::Staged => all(GIT_ADDED),
            Section::Unstaged => all(GIT_REMOVED),
            Section::None => Vec::new(),
        };
    }
    if line.starts_with("  (use ") || line.starts_with("nothing to commit") {
        return all(GIT_META);
    }
    // The transcript's own first line: what was run.
    if line.starts_with("$ ") {
        return all(GIT_META);
    }

    // --- anything else: a hook's own output ---
    verdict_words(line)
}

/// Words worth spotting in arbitrary tool output, in the colour of what they
/// mean.
///
/// A hook prints a linter's or a test runner's output, which has no syntax to
/// parse — but "passed", "failed" and "warning" carry almost all of what a
/// reader is scanning for, and they are the words every one of those tools
/// happens to use.
const GOOD: &[&str] = &[
    "passed", "pass", "ok", "success", "succeeded", "committed", "clean", "up-to-date",
];
const BAD: &[&str] = &[
    "failed", "fail", "failure", "error", "errors", "fatal", "panicked", "aborted", "denied",
    "rejected", "conflict",
];
const WARN: &[&str] = &["warning", "warnings", "skipped", "ignored"];

fn verdict_words(line: &str) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    let mut previous: Option<&str> = None;
    for (start, word) in words(line) {
        // The word itself (`ok.`), or — since pre-commit writes
        // `black....................Passed` — whatever follows the rule of
        // dots it drew, which is punctuation to a tokeniser and a heading to a
        // reader.
        let bare = word.trim_matches(|c: char| c.is_ascii_punctuation());
        let after_dots = word.rfind('.').map_or(bare, |i| {
            word[i + 1..].trim_matches(|c: char| c.is_ascii_punctuation())
        });
        let tail = if verdict_word(bare) || !verdict_word(after_dots) { bare } else { after_dots };
        let lower = tail.to_ascii_lowercase();
        let index = if GOOD.contains(&lower.as_str()) {
            GIT_ADDED
        } else if BAD.contains(&lower.as_str()) {
            // "0 failed" is the good news, and painting it red is the one way
            // this could actively mislead — a green line with a red word in it
            // reads as a failure at a glance.
            if previous == Some("0") {
                previous = Some(word);
                continue;
            }
            GIT_REMOVED
        } else if WARN.contains(&lower.as_str()) {
            GIT_WARNING
        } else {
            previous = Some(word);
            continue;
        };
        // The span covers the word the reader is scanning for, not the rule of
        // dots leading up to it.
        let offset = word.rfind(tail).unwrap_or(0);
        let from = start + word[..offset].chars().count();
        spans.push((from, from + tail.chars().count(), index));
        previous = Some(word);
    }
    spans
}

/// True when `word` is one of the words worth colouring.
fn verdict_word(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    [GOOD, BAD, WARN].iter().any(|set| set.contains(&lower.as_str()))
}

/// The line's words, with the char index each starts at.
fn words(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    let mut byte_start = 0usize;
    for (chars, (byte, ch)) in line.char_indices().enumerate() {
        let boundary = ch.is_whitespace();
        match (boundary, start) {
            (false, None) => {
                start = Some(chars);
                byte_start = byte;
            }
            (true, Some(from)) => {
                out.push((from, &line[byte_start..byte]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        out.push((from, &line[byte_start..]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colours(line: &str) -> Vec<usize> {
        let mut section = Section::default();
        line_spans(line, &mut section)
            .into_iter()
            .map(|(_, _, index)| index)
            .collect()
    }

    #[test]
    fn a_diff_reads_as_added_and_removed() {
        assert_eq!(colours("+    let x = 1;"), vec![GIT_ADDED]);
        assert_eq!(colours("-    let x = 0;"), vec![GIT_REMOVED]);
        // The file headers are not the first added and removed lines of the
        // diff, however much they look like it.
        assert_eq!(colours("+++ b/src/main.rs"), vec![GIT_META]);
        assert_eq!(colours("--- a/src/main.rs"), vec![GIT_META]);
        assert_eq!(colours("diff --git a/x b/x"), vec![GIT_META]);
        assert!(colours("     unchanged context").is_empty());
    }

    /// A hunk header addresses a place; the function name git appends is
    /// ordinary code and colouring it claims it is part of the address.
    #[test]
    fn a_hunk_header_colours_its_range_and_not_the_context() {
        let mut section = Section::default();
        let line = "@@ -1,7 +1,9 @@ fn main() {";
        let spans = line_spans(line, &mut section);
        assert_eq!(spans.len(), 1);
        let (start, end, index) = spans[0];
        assert_eq!((start, index), (0, GIT_HUNK));
        assert_eq!(&line[..end], "@@ -1,7 +1,9 @@");
    }

    #[test]
    fn a_commit_header_picks_out_the_hash() {
        let mut section = Section::default();
        let spans = line_spans("commit d2b3187f0a1", &mut section);
        assert_eq!(spans, vec![(0, 6, GIT_META), (7, 18, GIT_HASH)]);
        assert_eq!(colours("Author: Someone <a@b.c>"), vec![GIT_META]);
    }

    /// The same line means opposite things under two different headings, and
    /// only the heading says which.
    #[test]
    fn a_status_file_takes_the_colour_of_the_section_it_is_under() {
        let mut section = Section::default();
        line_spans("Changes to be committed:", &mut section);
        assert_eq!(
            line_spans("\tmodified:   src/main.rs", &mut section)
                .into_iter()
                .map(|(_, _, i)| i)
                .collect::<Vec<_>>(),
            vec![GIT_ADDED]
        );
        line_spans("Changes not staged for commit:", &mut section);
        assert_eq!(
            line_spans("\tmodified:   src/main.rs", &mut section)
                .into_iter()
                .map(|(_, _, i)| i)
                .collect::<Vec<_>>(),
            vec![GIT_REMOVED]
        );
    }

    #[test]
    fn a_hooks_verdict_is_spotted_in_its_own_words() {
        assert_eq!(colours("black....................Passed"), vec![GIT_ADDED]);
        assert_eq!(colours("cargo clippy.............Passed"), vec![GIT_ADDED]);
        assert_eq!(colours("ruff.....................Failed"), vec![GIT_REMOVED]);
        assert_eq!(colours("warning: unused variable"), vec![GIT_WARNING]);
        // …and a count of nothing is not bad news.
        assert_eq!(colours("test result: ok. 470 passed; 0 failed"), vec![GIT_ADDED, GIT_ADDED]);
    }

    /// The span offsets have to land on the words themselves, or the colour
    /// appears somewhere near them.
    #[test]
    fn a_words_span_covers_the_word() {
        let mut section = Section::default();
        let line = "  cargo test .... Passed";
        let (start, end, _) = line_spans(line, &mut section)[0];
        assert_eq!(line.chars().skip(start).take(end - start).collect::<String>(), "Passed");
    }

    #[test]
    fn spans_of_a_buffer_are_offset_by_the_lines_before_them() {
        let rope = Rope::from_str("commit abc123\n+added\n-removed\n");
        let spans = highlight(&rope);
        let added = spans.iter().find(|(_, _, i)| *i == GIT_ADDED).expect("the added line");
        assert_eq!(rope.char_to_line(added.0), 1);
        let removed = spans.iter().find(|(_, _, i)| *i == GIT_REMOVED).expect("the removed line");
        assert_eq!(rope.char_to_line(removed.0), 2);
    }

    /// The wiring, not just the classifier: a commit's diff opens in a buffer
    /// whose highlighter is this module.
    #[test]
    fn a_commit_buffer_is_highlighted_as_git_output() {
        let mut hl = crate::highlight::Highlighter::new(Some(Path::new("*commit 7e5120c*")));
        assert!(hl.git);
        let rope = Rope::from_str("diff --git a/x b/x\n+added\n-removed\n");
        let spans = hl.highlight(&rope);
        assert!(spans.iter().any(|(_, _, i)| *i == GIT_ADDED));
        assert!(spans.iter().any(|(_, _, i)| *i == GIT_REMOVED));
    }

    /// The transcript's verdict line is an em dash, not a diff's minus.
    #[test]
    fn the_transcripts_own_lines_are_not_read_as_a_diff() {
        assert_eq!(colours("$ git commit -m wip"), vec![GIT_META]);
        assert_eq!(colours("— Committed  (q to go back)"), vec![GIT_ADDED]);
    }

    #[test]
    fn only_the_buffers_git_writes_are_treated_as_git_output() {
        assert!(is_git_output(Some(Path::new(crate::app::GIT_OUTPUT_BUFFER))));
        assert!(is_git_output(Some(Path::new("*commit 7e5120c*"))));
        assert!(is_git_output(Some(Path::new("fix.patch"))));
        assert!(!is_git_output(Some(Path::new("src/main.rs"))));
        assert!(!is_git_output(None));
    }
}
