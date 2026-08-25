//! Running a git command whose *output* is the point.
//!
//! Most of what this module's neighbours ask git is a question with an answer:
//! run it, parse stdout, done ([`load::git`]).  A handful of commands are not
//! like that.  A commit runs the repository's `pre-commit` hook, which can be
//! a minute of linters and test runs printing as it goes; a push talks to a
//! network and counts objects.  Waiting for those with `Command::output()`
//! blocks the frame, so the editor looks hung for exactly as long as the
//! interesting part takes — with no spinner, because nothing knew it was busy.
//!
//! So these run as a child process whose two pipes are drained by threads onto
//! a channel, one line at a time, and the run loop empties that channel into a
//! read-only buffer the user is already looking at.  The same shape as the
//! notebook kernel's streaming output, one layer down.
//!
//! What crosses the channel is *already cleaned* ([`clean`]): a hook's output
//! is arbitrary program output, and a stray escape sequence written into a
//! rope is emitted verbatim by the terminal backend, moving the real cursor
//! mid-flush and garbling every cell drawn after it.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

/// One thing that happened to a running git command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A line of its combined stdout/stderr, cleaned for display.
    Line(String),
    /// It exited.  `Err` carries something worth showing the user.
    Finished(Result<(), String>),
}

/// A git command running off the UI thread, read a line at a time.
pub struct Stream {
    rx: Receiver<Event>,
    running: bool,
}

impl Stream {
    /// Start `git <args>` in `root`.
    ///
    /// Fails only when the process could not be spawned; a command that runs
    /// and *fails* is an [`Event::Finished`] with the reason, because by then
    /// its own output is the explanation and the caller has somewhere to show
    /// it.
    pub fn start(root: &Path, args: &[&str]) -> Result<Stream, String> {
        let mut child = super::load::git_command(root)
            .args(args)
            // A hook that reads stdin would otherwise inherit the terminal the
            // editor is drawing on and block forever on input nobody can type.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run git: {e}"))?;

        let (tx, rx) = mpsc::channel();
        let out = child.stdout.take().expect("stdout is piped");
        let err = child.stderr.take().expect("stderr is piped");
        let readers = [pump(out, tx.clone()), pump(err, tx.clone())];

        std::thread::spawn(move || {
            // Both pipes are drained to EOF before the exit is reported, so
            // "Finished" really is the last event: a caller that stops reading
            // on it cannot lose the failure's own explanation.
            for reader in readers {
                let _ = reader.join();
            }
            let done = match child.wait() {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(match status.code() {
                    Some(code) => format!("git exited with status {code}"),
                    None => "git was killed by a signal".to_string(),
                }),
                Err(e) => Err(format!("could not wait for git: {e}")),
            };
            let _ = tx.send(Event::Finished(done));
        });

        Ok(Stream { rx, running: true })
    }

    /// Everything that has arrived since the last call.
    ///
    /// Never blocks: an empty vector means the command is still working, which
    /// is the normal answer on most frames.
    pub fn drain(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(event) => {
                    if matches!(event, Event::Finished(_)) {
                        self.running = false;
                    }
                    events.push(event);
                }
                Err(TryRecvError::Empty) => break,
                // The sending side is gone without a verdict — only possible
                // if the collector thread itself died.  Say so rather than
                // spinning forever on a stream that will never finish.
                Err(TryRecvError::Disconnected) => {
                    if self.running {
                        self.running = false;
                        events.push(Event::Finished(Err("git stopped unexpectedly".into())));
                    }
                    break;
                }
            }
        }
        events
    }
}

/// Drain one pipe onto the channel, a line at a time.
fn pump<R: Read + Send + 'static>(pipe: R, tx: Sender<Event>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut raw = Vec::new();
        // Bytes, not `lines()`: a hook is free to print anything, and one
        // invalid UTF-8 byte must not end the stream.
        while reader.read_until(b'\n', &mut raw).unwrap_or(0) > 0 {
            let text = String::from_utf8_lossy(&raw);
            if tx.send(Event::Line(clean(&text))).is_err() {
                return;
            }
            raw.clear();
        }
    })
}

/// What one line of a hook's output may safely become in a rope.
///
/// Three things happen here, in order:
///
/// * **Carriage-return discipline** — a progress bar rewrites one line by
///   returning to its start, so only the text after the last `\r` was ever
///   meant to be visible.  Keeping the rest would render every intermediate
///   frame of the bar as though it were history.
/// * **Escape sequences are dropped.**  Hooks colour their output whenever
///   they think a terminal is watching, and the editor paints its own colours;
///   an ANSI sequence sitting in a rope is emitted verbatim by the backend and
///   takes the cursor with it.
/// * **Every remaining control character becomes a space**, for the same
///   reason the minibuffer flattens what it prints.
pub fn clean(line: &str) -> String {
    let visible = line.trim_end_matches(['\n', '\r']);
    let visible = visible.rsplit('\r').next().unwrap_or(visible);

    let mut out = String::with_capacity(visible.len());
    let mut chars = visible.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            skip_escape(&mut chars);
        } else if c == '\t' {
            // A tab is the one control character that means something here:
            // hook output is aligned with them.  Four columns, not eight —
            // this is program output in a code editor's buffer.
            out.push_str("    ");
        } else if c.is_control() {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// Consume the rest of an escape sequence, having just read the `ESC`.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars>) {
    match chars.next() {
        // CSI: parameters, then one final byte in `@`..=`~`.
        Some('[') => {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        // OSC: runs to a BEL or an ESC-terminated string.
        Some(']') => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    chars.next();
                    break;
                }
            }
        }
        // Anything else is a two-character sequence, already consumed.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_progress_bar_leaves_only_its_last_frame() {
        assert_eq!(clean("10%\r50%\r100% done\n"), "100% done");
    }

    #[test]
    fn colour_is_stripped_rather_than_written_into_the_rope() {
        // An escape written into a buffer is emitted verbatim by the backend,
        // which moves the real cursor mid-flush.
        assert_eq!(clean("\u{1b}[32mblack\u{1b}[0m...Passed"), "black...Passed");
        assert_eq!(clean("\u{1b}]0;title\u{7}ruff"), "ruff");
        assert!(!clean("\u{1b}[1;31mfail").contains('\u{1b}'));
    }

    #[test]
    fn stray_control_characters_become_spaces() {
        assert_eq!(clean("a\u{7}b"), "a b");
        assert_eq!(clean("a\tb"), "a    b");
    }

    #[test]
    fn a_command_streams_its_output_and_then_its_exit() {
        let dir = std::env::temp_dir().join(format!("sv-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // `git --help`-free: any repository-less command that prints and
        // succeeds does, and `version` is the one every git has.
        let mut stream = Stream::start(&dir, &["version"]).expect("spawn git");

        let mut lines = Vec::new();
        let mut verdict = None;
        while verdict.is_none() {
            for event in stream.drain() {
                match event {
                    Event::Line(line) => lines.push(line),
                    Event::Finished(result) => verdict = Some(result),
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(verdict, Some(Ok(())));
        assert!(
            lines.iter().any(|l| l.starts_with("git version")),
            "stdout did not reach the channel: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_command_reports_why_after_its_output() {
        let dir = std::env::temp_dir().join(format!("sv-run-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut stream = Stream::start(&dir, &["rev-parse", "--verify", "nope"]).expect("spawn");

        let mut events = Vec::new();
        while !matches!(events.last(), Some(Event::Finished(_))) {
            events.extend(stream.drain());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        match events.last() {
            Some(Event::Finished(Err(why))) => assert!(why.contains("status"), "{why}"),
            other => panic!("expected a failure last, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
