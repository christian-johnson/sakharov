//! The version-control view's data model.
//!
//! A [`Repo`] describes a repository: where HEAD is, the branches, tags and
//! stashes, any operation in progress, and every path whose HEAD, index and
//! work-tree versions are not all the same.  [`load`] reads one from git; a
//! [`plan::Plan`] folds its steps over one to produce another.  Everything
//! that draws takes a `Repo` and never knows which of the two it was given.
//!
//! Only [`apply`] changes the repository.  See `docs/version-control-plan.md`.

pub mod apply;
pub mod derive;
pub mod diff;
pub mod layout;
pub mod load;
pub mod plan;
pub mod state;
pub mod store;
pub mod tree;

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::conflict;

/// A git object id, always stored in full.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(String);

/// How many hex digits an abbreviated hash shows.
pub const SHORT_LEN: usize = 7;

impl Oid {
    pub fn new(hex: impl Into<String>) -> Self {
        Oid(hex.into())
    }

    /// A commit the plan will create but that does not exist yet; `n` indexes
    /// [`Repo::pending`].  The `~` prefix cannot collide with hex.
    pub fn pending(n: usize) -> Self {
        Oid(format!("~{n}"))
    }

    /// The index into [`Repo::pending`] for a planned commit.
    pub fn pending_index(&self) -> Option<usize> {
        self.0.strip_prefix('~')?.parse().ok()
    }

    pub fn is_pending(&self) -> bool {
        self.0.starts_with('~')
    }

    /// The abbreviated form; a planned commit has no hash yet.
    pub fn short(&self) -> &str {
        if self.is_pending() {
            return "new";
        }
        &self.0[..self.0.len().min(SHORT_LEN)]
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a version of a file's content lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Blob {
    /// An object git already has.
    Object(Oid),
    /// The work-tree file at this repository path, as it was when loaded.
    File(String),
    /// Content the plan produced (a partly staged file, a resolution).
    Text(Arc<[u8]>),
}

/// Regular file, the git mode almost everything has.
pub const MODE_FILE: u32 = 0o100_644;
/// Executable file.
pub const MODE_EXEC: u32 = 0o100_755;
/// Symbolic link.
pub const MODE_LINK: u32 = 0o120_000;
/// A submodule.  Shown, never edited.
pub const MODE_GITLINK: u32 = 0o160_000;

/// One version of a path: its mode and its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: u32,
    pub blob: Blob,
}

impl Entry {
    pub fn object(mode: u32, oid: Oid) -> Self {
        Entry { mode, blob: Blob::Object(oid) }
    }

    pub fn text(mode: u32, bytes: impl Into<Arc<[u8]>>) -> Self {
        Entry { mode, blob: Blob::Text(bytes.into()) }
    }
}

/// The three versions git could not reconcile.  Any may be absent: an
/// add/add conflict has no base, a modify/delete has no stage on one side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub base: Option<Entry>,
    pub left: Option<Entry>,
    pub right: Option<Entry>,
}

/// What the index holds for a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Index {
    /// One entry, or `None` when the path is not in the index.
    Entry(Option<Entry>),
    /// Unmerged: the three stages of a conflict.
    Conflict(Box<Conflict>),
}

impl Index {
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Index::Entry(e) => e.as_ref(),
            Index::Conflict(_) => None,
        }
    }

    pub fn is_conflict(&self) -> bool {
        matches!(self, Index::Conflict(_))
    }
}

/// A path's three versions.  Absent from [`Repo::files`] means all three are
/// the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub head: Option<Entry>,
    pub index: Index,
    pub work: Option<Entry>,
}

/// How far a path is from HEAD, as the changes tree marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Conflicted,
    /// Not in HEAD or the index; only on disk.
    Untracked,
    /// The index matches the work tree, and both differ from HEAD.
    Staged,
    /// The index still matches HEAD.
    Unstaged,
    /// The index differs from both.
    Partly,
}

/// Where HEAD is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Head {
    /// The branch HEAD is on, or `None` when detached.
    pub branch: Option<String>,
    /// The commit HEAD resolves to; `None` before the first commit.
    pub commit: Option<Oid>,
}

/// A commit's tree: a real tree-ish with planned changes laid over it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// A commit or tree git has; `None` is the empty tree.
    pub base: Option<Oid>,
    /// Paths whose entry differs from `base` (`None` = removed).
    pub overrides: BTreeMap<String, Option<Entry>>,
}

impl Tree {
    pub fn of(base: Option<Oid>) -> Self {
        Tree { base, overrides: BTreeMap::new() }
    }
}

/// A commit the plan will create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCommit {
    pub parents: Vec<Oid>,
    pub message: String,
    pub tree: Tree,
}

impl PendingCommit {
    /// The message's first line.
    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

/// One `git stash` entry, newest first in [`Repo::stashes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    /// The stash commit, or a planned one (pending id, no object yet).
    pub commit: Oid,
    pub message: String,
    /// For a planned stash: the paths it took away, to give back on pop.
    pub saved: Option<BTreeMap<String, FileState>>,
}

/// An operation stopped part-way, waiting for conflicts to be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProgress {
    pub kind: conflict::Operation,
    /// The commit being merged, picked or reverted.
    pub commit: Option<Oid>,
    /// What to call the incoming side ("feature", a commit subject).
    pub label: String,
    /// The message the finishing commit will carry.
    pub message: String,
    /// Commits still to replay after this one, oldest first.
    pub rest: Vec<Oid>,
    /// The branch a rebase will move when it finishes.
    pub branch: Option<String>,
    /// Where a rebase is landing, for the labels.
    pub onto: Option<String>,
}

/// A repository, real or projected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repo {
    pub head: Head,
    /// Local branches.
    pub branches: BTreeMap<String, Oid>,
    pub tags: BTreeMap<String, Oid>,
    pub stashes: Vec<StashEntry>,
    pub operation: Option<InProgress>,
    /// Every path whose three versions are not all equal.
    pub files: BTreeMap<String, FileState>,
    /// Commits the plan created, addressed by [`Oid::pending`].
    pub pending: Vec<PendingCommit>,
    /// The loaded state, compared again before anything is written.
    pub fingerprint: load::Fingerprint,
}

impl Repo {
    /// HEAD's tree: the commit it points at, with any planned changes.
    pub fn head_tree(&self) -> Tree {
        self.tree_of(self.head.commit.as_ref())
    }

    /// The tree of `commit`, real or planned.
    pub fn tree_of(&self, commit: Option<&Oid>) -> Tree {
        match commit {
            None => Tree::of(None),
            Some(oid) => match oid.pending_index().and_then(|i| self.pending.get(i)) {
                Some(p) => p.tree.clone(),
                None => Tree::of(Some(oid.clone())),
            },
        }
    }

    /// A short name for HEAD: its branch, or where it is detached.
    pub fn head_label(&self) -> String {
        match (&self.head.branch, &self.head.commit) {
            (Some(b), _) => b.clone(),
            (None, Some(c)) => format!("detached at {}", c.short()),
            (None, None) => "no commits".to_string(),
        }
    }

    /// The commit a branch name or a commit id refers to.
    pub fn resolve(&self, target: &Target) -> Option<Oid> {
        match target {
            Target::Branch(name) => self.branches.get(name).cloned(),
            Target::Commit(oid) => Some(oid.clone()),
        }
    }

    /// `(staged, unstaged, conflicted)` path counts, for the status line.
    pub fn counts(&self, store: &store::Store) -> (usize, usize, usize) {
        let mut out = (0, 0, 0);
        for state in self.files.values() {
            match store.mark(state) {
                Mark::Conflicted => out.2 += 1,
                Mark::Staged => out.0 += 1,
                Mark::Partly => {
                    out.0 += 1;
                    out.1 += 1;
                }
                Mark::Unstaged | Mark::Untracked => out.1 += 1,
            }
        }
        out
    }
}

/// What a step names as its subject: a branch (resolved when the step runs)
/// or a fixed commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Branch(String),
    Commit(Oid),
}

impl Target {
    /// How the target reads in a plan row and on git's command line.
    pub fn name(&self) -> String {
        match self {
            Target::Branch(b) => b.clone(),
            Target::Commit(c) => c.short().to_string(),
        }
    }
}

/// Render `when` as an age relative to `now`, both unix seconds.
pub fn relative_time(when: i64, now: i64) -> String {
    let secs = (now - when).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    match secs {
        s if s < MINUTE => "just now".to_string(),
        s if s < HOUR => format!("{}m ago", s / MINUTE),
        s if s < DAY => format!("{}h ago", s / HOUR),
        s if s < MONTH => format!("{}d ago", s / DAY),
        s if s < YEAR => format!("{}mo ago", s / MONTH),
        s => format!("{}y ago", s / YEAR),
    }
}

/// Seconds since the unix epoch, for [`relative_time`]'s `now`.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A throwaway repository on disk, for tests.
#[cfg(test)]
pub mod testrepo {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::load::git;

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    pub struct TestRepo {
        pub root: PathBuf,
    }

    impl TestRepo {
        pub fn new() -> Option<Self> {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!("sv-vcs-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).ok()?;
            let repo = TestRepo { root };
            repo.git(&["init", "-q", "-b", "main"])?;
            repo.git(&["config", "user.email", "test@example.com"])?;
            repo.git(&["config", "user.name", "Test"])?;
            repo.git(&["config", "commit.gpgsign", "false"])?;
            Some(repo)
        }

        pub fn git(&self, args: &[&str]) -> Option<String> {
            git(&self.root, args).ok()
        }

        pub fn write(&self, file: &str, contents: &str) {
            let path = self.root.join(file);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(path, contents).unwrap();
        }

        pub fn read(&self, file: &str) -> Option<String> {
            std::fs::read_to_string(self.root.join(file)).ok()
        }

        pub fn commit(&self, file: &str, contents: &str, message: &str) {
            self.write(file, contents);
            self.git(&["add", file]).unwrap();
            self.git(&["commit", "-q", "-m", message]).unwrap();
        }

        pub fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_id_is_never_mistaken_for_a_hash() {
        let p = Oid::pending(3);
        assert!(p.is_pending());
        assert_eq!(p.pending_index(), Some(3));
        assert_eq!(p.short(), "new");
        assert!(!Oid::new("a3f91c2").is_pending());
        assert_eq!(Oid::new("a3f91c2b7d4e").short(), "a3f91c2");
        assert_eq!(Oid::new("abc").short(), "abc");
    }

    #[test]
    fn ages_read_as_ages() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - 3 * 3600, now), "3h ago");
        assert_eq!(relative_time(now + 5000, now), "just now");
    }
}
