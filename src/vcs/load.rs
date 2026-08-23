//! Reading the repository into a [`Dag`].
//!
//! Everything here shells out to `git`.  See `docs/version-control-plan.md`
//! for why: applying a plan has to run the user's own git (hooks, config,
//! credential helper, reflog), and once apply does, having the *reads* go
//! through a linked library too would mean two different implementations of
//! "what does this repository look like" that could disagree.
//!
//! The parsing is deliberately split from the running.  A repository is
//! awkward to construct in a unit test and impossible to construct for the
//! interesting cases (an octopus merge, a detached HEAD, a ref past the
//! horizon), so every parser here is a pure function from git's output, and
//! that is what the tests exercise.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};

use super::{Commit, Dag, Head, Oid, Ref, RefKind, WorkTree};

/// ASCII record / field separators.  Used rather than a printable delimiter
/// because a commit summary can contain anything a human can type — including
/// every punctuation character anyone would otherwise reach for.
const RS: char = '\x1e';
const FS: char = '\x1f';

/// The `--format` string the log walk uses.  Field order must match
/// [`parse_log`]; the summary is last so a stray separator inside it cannot
/// shift the other fields.
const LOG_FORMAT: &str = "--format=%x1e%H%x1f%P%x1f%an%x1f%ct%x1f%s";

// ---------------------------------------------------------------------------
// Running git
// ---------------------------------------------------------------------------

/// Run `git` in `root` and return its stdout, or its stderr as the error.
///
/// Blocking.  Every caller in this module runs on the loader thread.
/// A `git` invocation anchored at `root` and insulated from an inherited git
/// environment.
///
/// `-C` sets the working directory, which is *not* enough: `GIT_DIR`,
/// `GIT_INDEX_FILE` and their relatives override repository discovery
/// outright, so with them set `git -C <somewhere>` operates on whatever they
/// name and ignores `<somewhere>` entirely.  Git exports exactly those
/// variables to hooks, and this repository runs its whole test suite from
/// `.githooks/pre-commit` — so under that hook the `apply.rs` fixtures stopped
/// creating branches and commits in their temp directories and started
/// creating them in the developer's own repository, alongside re-initialising
/// it and overwriting its `user.email`.  Clearing them makes `-C` mean what it
/// reads as.
fn git_command(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    for var in [
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd.arg("-C").arg(root);
    cmd
}

pub fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_command(root)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if err.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            err
        })
    }
}

/// The work tree containing `from`, or `None` when it is not in a repository.
///
/// Takes a directory hint rather than using the process working directory so
/// the view opens on the repository of the *file being edited*, which is not
/// always the one the editor was launched in.
pub fn discover_root(from: &Path) -> Option<PathBuf> {
    let dir = if from.is_dir() { from } else { from.parent()? };
    let out = git_command(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

// ---------------------------------------------------------------------------
// The background load
// ---------------------------------------------------------------------------

/// An in-flight repository read, polled once per frame by the run loop.
pub struct RepoLoad {
    pub root: PathBuf,
    rx: Receiver<Result<Dag, String>>,
}

impl RepoLoad {
    /// Non-blocking: `Some` once the background git commands finish.
    pub fn poll(&self) -> Option<Result<Dag, String>> {
        self.rx.try_recv().ok()
    }
}

/// Start reading `root` into a [`Dag`] on a background thread.
///
/// Never blocks the UI: a repository with a hundred thousand commits, or one
/// on a cold network filesystem, would otherwise stall a frame for as long as
/// the walk takes.
pub fn start(root: PathBuf, max_commits: usize) -> RepoLoad {
    let (tx, rx) = mpsc::channel();
    let thread_root = root.clone();
    std::thread::spawn(move || {
        let _ = tx.send(read(&thread_root, max_commits));
    });
    RepoLoad { root, rx }
}

/// Blocking read of the whole snapshot.  Loader thread only.
pub(super) fn read(root: &Path, max_commits: usize) -> Result<Dag, String> {
    let refs = parse_refs(&git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)\x1f%(objectname)\x1f%(*objectname)\x1f%(upstream:short)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?);
    let head = read_head(root);
    let work = parse_status(&git(root, &["status", "--porcelain"]).unwrap_or_default());

    // Roots: every local branch, plus HEAD (which covers a detached one), plus
    // the upstream of each local branch.  The upstreams are what make an
    // ahead/behind readable at all — a dim `origin/main` label pointing at a
    // commit that was never loaded says nothing about how far ahead it is.
    let mut roots: Vec<String> = vec!["--branches".to_string(), "HEAD".to_string()];
    roots.extend(
        refs.iter()
            .filter(|r| r.kind == RefKind::Local)
            .filter_map(|r| r.upstream.clone()),
    );

    let limit = format!("-{}", max_commits.max(1));
    let mut args: Vec<&str> = vec!["log", "--topo-order", "--numstat", &limit, LOG_FORMAT];
    args.extend(roots.iter().map(String::as_str));
    // A repository with no commits yet makes `git log` fail; that is an empty
    // graph, not an error the user needs to see.
    let log = git(root, &args).unwrap_or_default();

    let commits = parse_log(&log);
    let truncated = commits.len() >= max_commits.max(1);
    Ok(Dag::new(commits, refs, head, work, truncated))
}

/// HEAD's branch and commit.  Both queries are allowed to fail: a detached
/// HEAD has no branch, and a fresh repository has no commit.
fn read_head(root: &Path) -> Head {
    let branch = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let target = git(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .map(Oid::new);
    Head { branch, target }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse `git log --numstat` output in [`LOG_FORMAT`].
///
/// Each record starts with the record separator, so the header line and the
/// numstat lines that follow it belong together without having to track state
/// across a blank line.
pub(super) fn parse_log(out: &str) -> Vec<Commit> {
    out.split(RS)
        .filter(|record| !record.trim().is_empty())
        .filter_map(parse_log_record)
        .collect()
}

fn parse_log_record(record: &str) -> Option<Commit> {
    let mut lines = record.lines();
    let header = lines.next()?;
    // The summary is last and takes everything remaining, so a separator
    // character inside a commit message cannot shift the fields before it.
    let mut fields = header.splitn(5, FS);
    let id = Oid::new(fields.next()?.trim());
    let parents = fields
        .next()?
        .split_whitespace()
        .map(Oid::new)
        .collect::<Vec<_>>();
    let author = fields.next()?.to_owned();
    let when = fields.next()?.trim().parse().unwrap_or(0);
    let summary = fields.next().unwrap_or("").to_owned();

    if id.as_str().is_empty() {
        return None;
    }

    // Remaining lines are `added\tremoved\tpath`.  A binary file reports `-`
    // for both counts, which contributes nothing rather than failing the parse.
    let (mut insertions, mut deletions) = (0u32, 0u32);
    for line in lines {
        let mut cols = line.split('\t');
        let (Some(add), Some(del)) = (cols.next(), cols.next()) else {
            continue;
        };
        insertions += add.trim().parse::<u32>().unwrap_or(0);
        deletions += del.trim().parse::<u32>().unwrap_or(0);
    }

    Some(Commit { id, parents, summary, author, when, insertions, deletions })
}

/// Parse `git for-each-ref` output: full refname, object, dereferenced object,
/// upstream.
pub(super) fn parse_refs(out: &str) -> Vec<Ref> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let mut fields = line.split(FS);
            let refname = fields.next()?;
            let object = fields.next().unwrap_or("").trim();
            let deref = fields.next().unwrap_or("").trim();
            let upstream = fields.next().unwrap_or("").trim();

            let (kind, name) = classify_ref(refname)?;
            // An annotated tag's own object is the tag, not the commit it
            // labels; `*objectname` dereferences it and is empty otherwise.
            let target = if deref.is_empty() { object } else { deref };
            if target.is_empty() {
                return None;
            }
            Some(Ref {
                name: name.to_owned(),
                kind,
                target: Oid::new(target),
                upstream: (!upstream.is_empty()).then(|| upstream.to_owned()),
            })
        })
        .collect()
}

/// Full refname → what kind of ref it is and its short name.
///
/// `refs/remotes/<remote>/HEAD` is skipped: it is a symbolic ref that
/// duplicates whichever branch it points at, and drawing both puts two labels
/// on one commit that always move together.
fn classify_ref(refname: &str) -> Option<(RefKind, &str)> {
    if let Some(name) = refname.strip_prefix("refs/heads/") {
        Some((RefKind::Local, name))
    } else if let Some(name) = refname.strip_prefix("refs/remotes/") {
        (!name.ends_with("/HEAD")).then_some((RefKind::Remote, name))
    } else if let Some(name) = refname.strip_prefix("refs/tags/") {
        Some((RefKind::Tag, name))
    } else {
        None
    }
}

/// Parse `git status --porcelain` into counts.
///
/// The two status columns are the index and the work tree, so one file can be
/// both staged and unstaged (edited after `git add`) and is counted in both.
pub(super) fn parse_status(out: &str) -> WorkTree {
    let mut work = WorkTree::default();
    for line in out.lines() {
        let mut chars = line.chars();
        let (Some(x), Some(y)) = (chars.next(), chars.next()) else {
            continue;
        };
        match (x, y) {
            ('?', '?') => work.untracked += 1,
            // The unmerged states, from `git status`'s own table.  Both
            // columns matter: `AA` and `DD` are conflicts despite looking
            // like ordinary staged changes.
            ('D', 'D') | ('A', 'A') | ('U', _) | (_, 'U') => work.conflicted += 1,
            (x, y) => {
                if x != ' ' {
                    work.staged += 1;
                }
                if y != ' ' {
                    work.unstaged += 1;
                }
            }
        }
    }
    work
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the log text git would produce, so the fixtures read as data
    /// rather than as escape sequences.
    fn record(header: &str, numstat: &[&str]) -> String {
        let mut s = format!("{RS}{header}\n");
        for line in numstat {
            s.push_str(line);
            s.push('\n');
        }
        s
    }

    #[test]
    fn a_log_record_carries_its_fields_and_its_line_counts() {
        let out = record(
            &format!("aaaa1{FS}bbbb2 cccc3{FS}Christian{FS}1700000000{FS}Fix the thing"),
            &["10\t2\tsrc/a.rs", "3\t0\tsrc/b.rs"],
        );
        let commits = parse_log(&out);
        assert_eq!(commits.len(), 1);
        let c = &commits[0];
        assert_eq!(c.id, Oid::new("aaaa1"));
        assert_eq!(c.parents, vec![Oid::new("bbbb2"), Oid::new("cccc3")]);
        assert_eq!(c.author, "Christian");
        assert_eq!(c.when, 1_700_000_000);
        assert_eq!(c.summary, "Fix the thing");
        assert_eq!((c.insertions, c.deletions), (13, 2));
    }

    /// The summary is the last field precisely so this cannot shift the
    /// fields before it — a commit message can contain anything.
    #[test]
    fn a_separator_inside_a_commit_message_does_not_shift_the_fields() {
        let header = format!("aaaa{FS}{FS}A{FS}5{FS}subject with {FS} and {RS} inside");
        // The record separator really does split, so only test the field one.
        let header = header.replace(RS, "~");
        let commits = parse_log(&record(&header, &[]));
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].author, "A");
        assert_eq!(commits[0].when, 5);
        assert!(commits[0].summary.starts_with("subject with"));
    }

    #[test]
    fn a_root_commit_has_no_parents_and_a_binary_diff_counts_nothing() {
        let out = record(
            &format!("aaaa{FS}{FS}A{FS}1{FS}Initial"),
            &["-\t-\timage.png", "4\t1\tREADME"],
        );
        let commits = parse_log(&out);
        assert!(commits[0].parents.is_empty());
        assert_eq!((commits[0].insertions, commits[0].deletions), (4, 1));
    }

    /// A merge commit gets no numstat from `git log` without `-m`, which is
    /// why the block reports `+0 -0` for one rather than a number taken from
    /// whichever side happened to be diffed.
    #[test]
    fn a_merge_with_no_diff_lines_reports_no_change_rather_than_failing() {
        let out = record(&format!("mmmm{FS}aaaa bbbb{FS}A{FS}1{FS}Merge"), &[]);
        let commits = parse_log(&out);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].parents.len(), 2);
        assert_eq!((commits[0].insertions, commits[0].deletions), (0, 0));
    }

    #[test]
    fn an_empty_log_is_an_empty_graph_not_a_failure() {
        assert!(parse_log("").is_empty());
        assert!(parse_log("\n\n").is_empty());
    }

    #[test]
    fn refs_are_classified_by_their_full_name_and_shortened() {
        let out = [
            format!("refs/heads/main{FS}aaaa{FS}{FS}origin/main"),
            format!("refs/remotes/origin/main{FS}bbbb{FS}{FS}"),
            format!("refs/tags/v1.0{FS}tagobj{FS}cccc{FS}"),
            format!("refs/tags/light{FS}dddd{FS}{FS}"),
        ]
        .join("\n");
        let refs = parse_refs(&out);
        assert_eq!(refs.len(), 4);

        assert_eq!(refs[0].name, "main");
        assert_eq!(refs[0].kind, RefKind::Local);
        assert_eq!(refs[0].upstream.as_deref(), Some("origin/main"));

        assert_eq!(refs[1].name, "origin/main");
        assert_eq!(refs[1].kind, RefKind::Remote);
        assert!(refs[1].upstream.is_none());

        // An annotated tag resolves to the commit it labels, not to the tag
        // object — otherwise it points at an oid no commit block carries.
        assert_eq!(refs[2].target, Oid::new("cccc"));
        // A lightweight tag has no dereferenced object and keeps its own.
        assert_eq!(refs[3].target, Oid::new("dddd"));
    }

    /// `refs/remotes/origin/HEAD` is symbolic: it always sits on whichever
    /// branch it points at, so drawing it puts two labels on one commit that
    /// can never be moved apart.
    #[test]
    fn the_remote_head_symlink_is_not_a_ref_of_its_own() {
        let out = format!("refs/remotes/origin/HEAD{FS}aaaa{FS}{FS}");
        assert!(parse_refs(&out).is_empty());
        // A branch actually named HEAD-something is not skipped.
        let out = format!("refs/remotes/origin/HEADROOM{FS}aaaa{FS}{FS}");
        assert_eq!(parse_refs(&out).len(), 1);
    }

    #[test]
    fn status_counts_the_index_and_the_worktree_separately() {
        let work = parse_status(
            "M  staged.rs\n\
             \x20M unstaged.rs\n\
             MM both.rs\n\
             ?? new.rs\n\
             A  added.rs\n",
        );
        // `MM` is one file counted in both columns: staged, then edited again.
        assert_eq!(work.staged, 3);
        assert_eq!(work.unstaged, 2);
        assert_eq!(work.untracked, 1);
        assert_eq!(work.conflicted, 0);
        assert!(work.is_dirty());
    }

    /// `AA` and `DD` look like ordinary staged changes but are unmerged
    /// states — counting them as staged would let an apply start on top of a
    /// conflicted tree.
    #[test]
    fn every_unmerged_state_counts_as_a_conflict() {
        for code in ["DD", "AU", "UD", "UA", "DU", "AA", "UU"] {
            let work = parse_status(&format!("{code} f.rs\n"));
            assert_eq!(work.conflicted, 1, "{code} should be a conflict");
            assert_eq!(work.staged, 0, "{code} must not read as merely staged");
        }
    }

    #[test]
    fn a_clean_tree_is_not_dirty() {
        let work = parse_status("");
        assert_eq!(work, WorkTree::default());
        assert!(!work.is_dirty());
    }
}
