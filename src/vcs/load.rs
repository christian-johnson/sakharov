//! Reading a repository into a [`Repo`].
//!
//! Everything here shells out to `git` (see `docs/version-control-plan.md`).
//! The parsers are pure functions of git's output so the tests can exercise
//! them without a repository; [`read`] is the only part that runs anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};

use super::{
    Conflict, Entry, FileState, Head, InProgress, Index, Oid, Repo, StashEntry, MODE_EXEC,
    MODE_FILE, MODE_LINK,
};
use crate::conflict::Operation;

/// Field separator for the `--format` strings below.
const FS: char = '\x1f';

/// A `git` invocation anchored at `root` and insulated from an inherited git
/// environment.
///
/// `GIT_DIR`, `GIT_INDEX_FILE` and their relatives override `-C` outright, and
/// git exports them to hooks — this repository runs its test suite from
/// `.githooks/pre-commit`, where an inherited `GIT_DIR` once sent the test
/// fixtures' commits into the developer's own repository.  The identity
/// variables are cleared for the same reason: they outrank `user.name` in
/// config, so a commit made from inside a hook would be stamped with the outer
/// commit's author.
pub(crate) fn git_command(root: &Path) -> Command {
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
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
    ] {
        cmd.env_remove(var);
    }
    cmd.arg("-C").arg(root);
    cmd
}

/// Run `git` in `root` and return its stdout, or its stderr as the error.
pub fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_command(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if err.is_empty() { format!("git {} failed", args.join(" ")) } else { err })
    }
}

/// The work tree containing `from`, or `None` outside a repository.
pub fn discover_root(from: &Path) -> Option<PathBuf> {
    let dir = if from.is_dir() { from } else { from.parent()? };
    let out = git_command(dir).args(["rev-parse", "--show-toplevel"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// The repository's git directory (`.git`, or a worktree's own).
pub fn git_dir(root: &Path) -> PathBuf {
    match git(root, &["rev-parse", "--absolute-git-dir"]) {
        Ok(dir) => PathBuf::from(dir.trim()),
        Err(_) => root.join(".git"),
    }
}

/// Everything about a loaded repository that would change if something other
/// than this view touched it.  Compared again just before applying.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprint {
    pub head: Head,
    pub branches: BTreeMap<String, Oid>,
    pub stashes: Vec<Oid>,
    pub operation: Option<Operation>,
    /// `git status --porcelain=v2` verbatim: covers HEAD and the index.
    pub status: String,
    /// Size and modification time of every changed work-tree file, since a
    /// modified file edited again does not change its status line.
    pub stats: BTreeMap<String, (u64, i128)>,
}

/// An in-flight read, polled once per frame by the run loop.
pub struct RepoLoad {
    pub root: PathBuf,
    rx: Receiver<Result<Repo, String>>,
}

impl RepoLoad {
    pub fn poll(&self) -> Option<Result<Repo, String>> {
        self.rx.try_recv().ok()
    }
}

/// Start reading `root` on a background thread.
pub fn start(root: PathBuf) -> RepoLoad {
    let (tx, rx) = mpsc::channel();
    let thread_root = root.clone();
    std::thread::spawn(move || {
        let _ = tx.send(read(&thread_root));
    });
    RepoLoad { root, rx }
}

/// Blocking read of the whole snapshot.
pub fn read(root: &Path) -> Result<Repo, String> {
    let status = git(
        root,
        &["status", "--porcelain=v2", "-z", "--no-renames", "--untracked-files=all"],
    )?;
    let (refs, head) = (read_refs(root), read_head(root));
    let mut files = parse_status(&status);
    fill_conflict_heads(root, &mut files, head.commit.is_some());
    let stashes = read_stashes(root);
    let operation = read_operation(root, files.values().any(|f| f.index.is_conflict()));
    let fingerprint = Fingerprint {
        head: head.clone(),
        branches: refs.0.clone(),
        stashes: stashes.iter().map(|s| s.commit.clone()).collect(),
        operation: operation.as_ref().map(|o| o.kind),
        stats: stats(root, files.keys()),
        status,
    };
    Ok(Repo {
        head,
        branches: refs.0,
        tags: refs.1,
        stashes,
        operation,
        files,
        pending: Vec::new(),
        fingerprint,
    })
}

/// What the repository looks like now, for the staleness check.
pub fn fingerprint(root: &Path) -> Result<Fingerprint, String> {
    read(root).map(|repo| repo.fingerprint)
}

fn stats<'a>(root: &Path, paths: impl Iterator<Item = &'a String>) -> BTreeMap<String, (u64, i128)> {
    paths
        .filter_map(|p| {
            let meta = std::fs::symlink_metadata(root.join(p)).ok()?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as i128);
            Some((p.clone(), (meta.len(), mtime)))
        })
        .collect()
}

fn read_head(root: &Path) -> Head {
    let branch = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let commit = git(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .map(Oid::new);
    Head { branch, commit }
}

type Refs = (BTreeMap<String, Oid>, BTreeMap<String, Oid>);

fn read_refs(root: &Path) -> Refs {
    let out = git(
        root,
        &["for-each-ref", "--format=%(refname)\x1f%(objectname)\x1f%(*objectname)", "refs/heads", "refs/tags"],
    )
    .unwrap_or_default();
    parse_refs(&out)
}

/// `for-each-ref` output → `(branches, tags)`.  An annotated tag resolves to
/// the commit it labels.
pub fn parse_refs(out: &str) -> Refs {
    let (mut branches, mut tags) = (BTreeMap::new(), BTreeMap::new());
    for line in out.lines() {
        let mut fields = line.split(FS);
        let (Some(name), Some(object)) = (fields.next(), fields.next()) else { continue };
        let deref = fields.next().unwrap_or("").trim();
        let target = Oid::new(if deref.is_empty() { object.trim() } else { deref });
        if let Some(b) = name.strip_prefix("refs/heads/") {
            branches.insert(b.to_string(), target);
        } else if let Some(t) = name.strip_prefix("refs/tags/") {
            tags.insert(t.to_string(), target);
        }
    }
    (branches, tags)
}

fn read_stashes(root: &Path) -> Vec<StashEntry> {
    git(root, &["stash", "list", "--format=%H\x1f%gs"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (oid, msg) = line.split_once(FS)?;
            Some(StashEntry { commit: Oid::new(oid), message: msg.to_string(), saved: None })
        })
        .collect()
}

/// Parse `git status --porcelain=v2 -z --no-renames` into per-path states.
///
/// A column of `.` means that version equals the one before it, so the object
/// id is reused rather than pointing at the work-tree file.
pub fn parse_status(out: &str) -> BTreeMap<String, FileState> {
    let mut files = BTreeMap::new();
    for record in out.split('\0').filter(|r| !r.is_empty()) {
        let fields: Vec<&str> = record.splitn(11, ' ').collect();
        match fields.first().copied() {
            Some("1") if fields.len() >= 9 => {
                let path = record.splitn(9, ' ').nth(8).unwrap_or_default().to_string();
                let xy = fields[1].as_bytes();
                let (mh, mi, mw) = (mode(fields[3]), mode(fields[4]), mode(fields[5]));
                let head = mh.map(|m| Entry::object(m, Oid::new(fields[6])));
                let index = mi.map(|m| Entry::object(m, Oid::new(fields[7])));
                let work = match (mw, xy.get(1)) {
                    (None, _) => None,
                    (Some(_), Some(b'.')) => index.clone(),
                    (Some(m), _) => Some(Entry { mode: m, blob: super::Blob::File(path.clone()) }),
                };
                files.insert(path, FileState { head, index: Index::Entry(index), work });
            }
            Some("u") if fields.len() >= 11 => {
                let path = fields[10].to_string();
                let stage = |m: &str, h: &str| mode(m).map(|m| Entry::object(m, Oid::new(h)));
                let conflict = Conflict {
                    base: stage(fields[3], fields[7]),
                    left: stage(fields[4], fields[8]),
                    right: stage(fields[5], fields[9]),
                };
                let work = mode(fields[6]).map(|m| Entry { mode: m, blob: super::Blob::File(path.clone()) });
                // HEAD's version is filled in by `fill_conflict_heads`.
                files.insert(path, FileState { head: None, index: Index::Conflict(Box::new(conflict)), work });
            }
            Some("?") => {
                let path = record.get(2..).unwrap_or_default().to_string();
                let work = Some(Entry { mode: MODE_FILE, blob: super::Blob::File(path.clone()) });
                files.insert(path, FileState { head: None, index: Index::Entry(None), work });
            }
            _ => {}
        }
    }
    files
}

/// Octal mode → `None` for git's "absent" (`000000`).
fn mode(field: &str) -> Option<u32> {
    u32::from_str_radix(field, 8).ok().filter(|m| *m != 0)
}

/// Fix up untracked files' modes from disk, and read HEAD's version of every
/// conflicted path (`status` does not report it for unmerged entries).
fn fill_conflict_heads(root: &Path, files: &mut BTreeMap<String, FileState>, has_head: bool) {
    for (path, state) in files.iter_mut() {
        if !matches!(state.index, Index::Entry(None)) || state.head.is_some() {
            continue;
        }
        if let (Some(work), Ok(meta)) = (state.work.as_mut(), std::fs::symlink_metadata(root.join(path))) {
            work.mode = file_mode(&meta);
        }
    }
    let conflicted: Vec<String> =
        files.iter().filter(|(_, f)| f.index.is_conflict()).map(|(p, _)| p.clone()).collect();
    if conflicted.is_empty() || !has_head {
        return;
    }
    let mut args = vec!["ls-tree", "-z", "HEAD", "--"];
    args.extend(conflicted.iter().map(String::as_str));
    for (path, entry) in parse_ls_tree(&git(root, &args).unwrap_or_default()) {
        if let Some(state) = files.get_mut(&path) {
            state.head = Some(entry);
        }
    }
}

/// The git mode a file on disk would be added with.
pub fn file_mode(meta: &std::fs::Metadata) -> u32 {
    if meta.file_type().is_symlink() {
        return MODE_LINK;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return MODE_EXEC;
        }
    }
    MODE_FILE
}

/// `git ls-tree -z` output → `(path, entry)` for every blob or gitlink.
pub fn parse_ls_tree(out: &str) -> Vec<(String, Entry)> {
    out.split('\0')
        .filter_map(|record| {
            let (meta, path) = record.split_once('\t')?;
            let mut parts = meta.split(' ');
            let mode = mode(parts.next()?)?;
            let kind = parts.next()?;
            let oid = parts.next()?;
            (kind != "tree").then(|| (path.to_string(), Entry::object(mode, Oid::new(oid))))
        })
        .collect()
}

/// The operation git is in the middle of, if any.
fn read_operation(root: &Path, conflicted: bool) -> Option<InProgress> {
    let dir = git_dir(root);
    let kind = crate::conflict::load::detect_operation(&dir);
    let text = |rel: &str| std::fs::read_to_string(dir.join(rel)).ok().map(|s| s.trim().to_string());
    let rev = |name: &str| {
        git(root, &["rev-parse", "--verify", "--quiet", name]).ok().map(|s| Oid::new(s.trim()))
    };
    let message_of = |oid: &Option<Oid>| {
        oid.as_ref()
            .and_then(|o| git(root, &["log", "-1", "--format=%B", o.as_str()]).ok())
            .unwrap_or_default()
            .trim_end()
            .to_string()
    };
    let subject = |msg: &str| msg.lines().next().unwrap_or("").to_string();
    let mut op = InProgress {
        kind,
        commit: None,
        label: String::new(),
        message: String::new(),
        rest: Vec::new(),
        branch: None,
        onto: None,
    };
    match kind {
        Operation::Merge => {
            op.commit = rev("MERGE_HEAD");
            op.message = text("MERGE_MSG").unwrap_or_default();
            op.label = op
                .message
                .lines()
                .next()
                .and_then(crate::conflict::load::parse_merge_msg)
                .unwrap_or_else(|| op.commit.as_ref().map_or_else(String::new, |c| c.short().to_string()));
        }
        Operation::CherryPick | Operation::Revert => {
            op.commit = rev(if kind == Operation::CherryPick { "CHERRY_PICK_HEAD" } else { "REVERT_HEAD" });
            op.message = text("MERGE_MSG").unwrap_or_else(|| message_of(&op.commit));
            op.label = subject(&message_of(&op.commit));
            op.rest = parse_todo(&text("sequencer/todo").unwrap_or_default());
        }
        Operation::Rebase => {
            op.commit = rev("REBASE_HEAD");
            op.message = text("rebase-merge/message").unwrap_or_else(|| message_of(&op.commit));
            op.label = subject(&message_of(&op.commit));
            op.rest = parse_todo(&text("rebase-merge/git-rebase-todo").unwrap_or_default());
            op.branch = text("rebase-merge/head-name").map(|b| b.trim_start_matches("refs/heads/").to_string());
            op.onto = text("rebase-merge/onto").map(|o| Oid::new(o).short().to_string());
        }
        Operation::Unknown if conflicted => op.label = "the conflicted index".to_string(),
        Operation::Unknown => return None,
    }
    Some(op)
}

/// The commits a sequencer todo list will still pick, oldest first.  Stops at
/// the first line that is not a plain pick, since nothing past it can be
/// predicted.
pub fn parse_todo(todo: &str) -> Vec<Oid> {
    let mut out = Vec::new();
    for line in todo.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("pick" | "p"), Some(oid)) => out.push(Oid::new(oid)),
            _ => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{testrepo::TestRepo, Blob};

    #[test]
    fn an_inherited_git_environment_cannot_redirect_a_command_or_sign_it() {
        let cmd = git_command(Path::new("/tmp/somewhere"));
        let cleared: Vec<&str> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .filter_map(|(k, _)| k.to_str())
            .collect();
        for var in ["GIT_DIR", "GIT_INDEX_FILE", "GIT_WORK_TREE", "GIT_AUTHOR_NAME", "GIT_COMMITTER_DATE"] {
            assert!(cleared.contains(&var), "{var} is inherited: {cleared:?}");
        }
    }

    #[test]
    fn status_records_become_three_versions_per_path() {
        let out = [
            "1 M. N... 100644 100644 100644 aaaa bbbb staged.rs",
            "1 .M N... 100644 100644 100644 aaaa aaaa edited.rs",
            "1 A. N... 000000 100644 100644 0000 cccc new file.rs",
            "1 .D N... 100644 100644 000000 aaaa aaaa gone.rs",
            "u UU N... 100644 100644 100644 100644 b1 b2 b3 both.rs",
            "? scratch.txt",
            "",
        ]
        .join("\0");
        let files = parse_status(&out);

        let staged = &files["staged.rs"];
        assert_eq!(staged.index.entry().map(|e| &e.blob), Some(&Blob::Object(Oid::new("bbbb"))));
        assert_eq!(staged.work, staged.index.entry().cloned(), "`.` reuses the index object");

        let edited = &files["edited.rs"];
        assert_eq!(edited.work.as_ref().map(|e| &e.blob), Some(&Blob::File("edited.rs".into())));

        assert!(files["new file.rs"].head.is_none(), "a path with a space survives");
        assert!(files["gone.rs"].work.is_none());
        assert!(files["both.rs"].index.is_conflict());
        assert!(matches!(files["scratch.txt"].index, Index::Entry(None)));
    }

    #[test]
    fn a_todo_list_is_read_up_to_the_first_step_that_is_not_a_pick() {
        let todo = "# comment\npick aaa one\np bbb two\nedit ccc three\npick ddd four\n";
        assert_eq!(parse_todo(todo), vec![Oid::new("aaa"), Oid::new("bbb")]);
    }

    #[test]
    fn reading_a_real_repository_finds_branches_changes_and_stashes() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("a.txt", "one\n", "first");
        repo.write("a.txt", "two\n");
        repo.git(&["stash"]).unwrap();
        repo.git(&["branch", "side"]).unwrap();
        repo.write("a.txt", "three\n");
        repo.write("dir/new.txt", "new\n");

        let snap = read(repo.path()).unwrap();
        assert_eq!(snap.head.branch.as_deref(), Some("main"));
        assert_eq!(snap.branches.keys().collect::<Vec<_>>(), vec!["main", "side"]);
        assert_eq!(snap.stashes.len(), 1);
        assert_eq!(snap.files.keys().collect::<Vec<_>>(), vec!["a.txt", "dir/new.txt"]);
        assert!(snap.operation.is_none());
        assert_eq!(fingerprint(repo.path()).unwrap(), snap.fingerprint, "nothing moved");

        repo.write("a.txt", "four, and longer\n");
        assert_ne!(fingerprint(repo.path()).unwrap(), snap.fingerprint, "an edit to a modified file shows");
    }
}
