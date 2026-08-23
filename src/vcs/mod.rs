//! The version-control view's data model.
//!
//! A [`Dag`] is an **immutable snapshot** of what git said the repository
//! looked like at one moment: the commits reachable from the local branches,
//! the refs pointing into them, where HEAD is, and what the worktree looks
//! like.  Nothing here talks to git — [`load`] does that, on a background
//! thread — and nothing here can write.
//!
//! That split is the architecture, not tidiness.  The view lets the user
//! rearrange history freely, and the only thing standing between "I dragged an
//! arrow" and "my branch was rewritten" is that the layers below this one
//! ([`plan`], [`derive`]) are pure functions and only [`apply`] runs a command.
//! See `docs/version-control-plan.md`.

pub mod apply;
pub mod derive;
pub mod layout;
pub mod load;
pub mod plan;
pub mod state;

use std::collections::HashMap;

/// A git object id, stored in full.
///
/// A newtype rather than a `String` because the hazard this module is full of
/// is passing the wrong hex string somewhere — a *short* hash where a full one
/// belongs, a ref name where an oid belongs.  Shortening is a display concern
/// and lives in [`Oid::short`]; the value itself is always complete, so it can
/// always be handed to git.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(String);

/// How many hex digits an abbreviated hash shows.  Seven is git's own default
/// for repositories of ordinary size.
pub const SHORT_LEN: usize = 7;

impl Oid {
    pub fn new(hex: impl Into<String>) -> Self {
        Oid(hex.into())
    }

    /// The identity of a commit the plan *will create* but that does not exist
    /// yet — the merge commit a pending merge would produce.
    ///
    /// A merge cannot be modelled as another parent on an existing commit:
    /// git never adds a parent to a commit, it makes a new one whose parents
    /// are the two tips.  So the projection needs a node with no object behind
    /// it, and this is what identifies one.  The `~` prefix cannot collide
    /// with a hash, which is hex.
    pub fn pending(n: usize) -> Self {
        Oid(format!("~{n}"))
    }

    /// True for an id [`Oid::pending`] made up — a commit that does not exist
    /// in the repository and must never be handed to git as an argument.
    pub fn is_pending(&self) -> bool {
        self.0.starts_with('~')
    }

    /// The abbreviated form shown in a commit block.
    ///
    /// A pending commit has no hash to abbreviate, and showing its internal
    /// placeholder would read as a real (and wrong) object id.
    pub fn short(&self) -> &str {
        if self.is_pending() {
            return "new";
        }
        let n = self.0.len().min(SHORT_LEN);
        &self.0[..n]
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

/// One commit, with everything a block on screen needs to label itself.
#[derive(Debug, Clone)]
pub struct Commit {
    pub id: Oid,
    /// Parents in git's own order: `parents[0]` is the first parent, which is
    /// the one a linear history follows and the one a merge came *from*.
    pub parents: Vec<Oid>,
    /// First line of the commit message.
    pub summary: String,
    pub author: String,
    /// Committer date, unix seconds.
    pub when: i64,
    /// Lines added / removed, summed over the diff against the first parent.
    ///
    /// A merge commit has no such diff, so it reports `(0, 0)` — honest rather
    /// than a number picked from one side.
    pub insertions: u32,
    pub deletions: u32,
}

/// What kind of thing points at a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// A local branch — the only kind this view can move.
    Local,
    /// A remote-tracking branch.  Drawn dim: it records where the remote was
    /// at the last fetch, and moving it locally would only lie about that.
    Remote,
    Tag,
}

/// A named pointer into the graph.
#[derive(Debug, Clone)]
pub struct Ref {
    /// Short name: `main`, `origin/main`, `v1.2`.
    pub name: String,
    pub kind: RefKind,
    pub target: Oid,
    /// The remote-tracking branch this one is configured to track, if any —
    /// what makes an ahead/behind count meaningful.
    pub upstream: Option<String>,
}

/// Where HEAD is.
#[derive(Debug, Clone, Default)]
pub struct Head {
    /// The branch HEAD is on, or `None` when detached.
    pub branch: Option<String>,
    /// The commit HEAD resolves to.  `None` in a repository with no commits
    /// yet, where the branch exists but points at nothing.
    pub target: Option<Oid>,
}

impl Head {
    pub fn detached(&self) -> bool {
        self.branch.is_none() && self.target.is_some()
    }
}

/// One path `git status` reported, and what has happened to it.
///
/// The two columns are git's own: the first is the index, the second the work
/// tree, so one file can be both staged and unstaged (edited after `git add`)
/// and an untracked file is `??`.  Kept verbatim rather than reduced to a
/// category, because the pair *is* the state and any reduction loses a case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub index: char,
    pub work: char,
}

impl Change {
    /// Never added to the repository at all — the scrap notebooks and stray
    /// output files that accumulate in a working tree.
    pub fn is_untracked(&self) -> bool {
        self.index == '?'
    }

    /// The unmerged states, from `git status`'s own table.  Both columns
    /// matter: `AA` and `DD` are conflicts despite looking like ordinary
    /// staged changes.
    pub fn is_conflicted(&self) -> bool {
        matches!((self.index, self.work), ('D', 'D') | ('A', 'A') | ('U', _) | (_, 'U'))
    }

    pub fn is_staged(&self) -> bool {
        !self.is_untracked() && !self.is_conflicted() && self.index != ' '
    }

    pub fn is_unstaged(&self) -> bool {
        !self.is_untracked() && !self.is_conflicted() && self.work != ' '
    }

    /// A word for what happened, for a list the user reads.
    ///
    /// Says both halves when both apply ("staged, edited since"), because a
    /// file that was added and then edited again is exactly the case someone
    /// is surprised by at commit time.
    pub fn describe(&self) -> String {
        if self.is_conflicted() {
            return "conflicted".to_string();
        }
        if self.is_untracked() {
            return "untracked".to_string();
        }
        let word = |c: char| match c {
            'A' => "added",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            'T' => "type changed",
            _ => "modified",
        };
        match (self.index, self.work) {
            (' ', w) => word(w).to_string(),
            (i, ' ') => format!("{}, staged", word(i)),
            (i, _) => format!("{}, staged — edited since", word(i)),
        }
    }
}

/// What `git status` says about the working tree.
///
/// The paths, not just the counts: a repository quietly fills up with
/// untracked scratch files, and "3 untracked" is the number you can see
/// without being told, while *which three* is the thing you actually need.
/// The counts are derived from the list rather than stored beside it, so the
/// summary and the list can never disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkTree {
    pub entries: Vec<Change>,
}

impl WorkTree {
    pub fn new(entries: Vec<Change>) -> Self {
        WorkTree { entries }
    }

    fn count(&self, which: fn(&Change) -> bool) -> usize {
        self.entries.iter().filter(|c| which(c)).count()
    }

    pub fn staged(&self) -> usize {
        self.count(Change::is_staged)
    }

    pub fn unstaged(&self) -> usize {
        self.count(Change::is_unstaged)
    }

    pub fn untracked(&self) -> usize {
        self.count(Change::is_untracked)
    }

    pub fn conflicted(&self) -> usize {
        self.count(Change::is_conflicted)
    }

    /// True when there is anything at all uncommitted.
    ///
    /// This is the gate on applying a plan: replaying commits over uncommitted
    /// work is the reliable way to lose it.  Untracked files are excluded —
    /// git itself lets a checkout proceed past them, and refusing on a stray
    /// build artefact would make the feature unusable in a real tree.
    pub fn is_dirty(&self) -> bool {
        self.entries
            .iter()
            .any(|c| c.is_staged() || c.is_unstaged() || c.is_conflicted())
    }

    /// The two lines the HEAD block shows: what would go into the next commit,
    /// and what git is not tracking at all.
    ///
    /// Built here rather than in the renderer because the layout has to size a
    /// block against them before anything is drawn, and a block sized from one
    /// string and filled with another clips the half that matters.
    pub fn summary_lines(&self) -> [String; 2] {
        let mut tracked = Vec::new();
        for (n, word) in [
            (self.conflicted(), "conflicted"),
            (self.staged(), "staged"),
            (self.unstaged(), "unstaged"),
        ] {
            if n > 0 {
                tracked.push(format!("{n} {word}"));
            }
        }
        let first = if tracked.is_empty() {
            "clean".to_string()
        } else {
            tracked.join(" · ")
        };
        let second = match self.untracked() {
            0 if self.is_dirty() => "w to list them".to_string(),
            0 => String::new(),
            n => format!("{n} untracked  (w)"),
        };
        [first, second]
    }
}

/// An immutable snapshot of the repository's history.
pub struct Dag {
    /// Commits in git's topological order — newest first, and every commit
    /// listed before any of its ancestors.  The layout depends on that
    /// ordering, so it is a property of the type rather than of the loader.
    commits: Vec<Commit>,
    index: HashMap<Oid, usize>,
    pub refs: Vec<Ref>,
    pub head: Head,
    pub work: WorkTree,
    /// True when the walk stopped at the configured commit limit, so the
    /// oldest blocks on screen have parents that were never loaded.
    pub truncated: bool,
}

impl Dag {
    pub fn new(
        commits: Vec<Commit>,
        refs: Vec<Ref>,
        head: Head,
        work: WorkTree,
        truncated: bool,
    ) -> Self {
        let index = commits
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id.clone(), i))
            .collect();
        Dag { commits, index, refs, head, work, truncated }
    }

    pub fn commits(&self) -> &[Commit] {
        &self.commits
    }

    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }

    pub fn get(&self, id: &Oid) -> Option<&Commit> {
        self.index.get(id).map(|&i| &self.commits[i])
    }

    pub fn find_ref(&self, name: &str) -> Option<&Ref> {
        self.refs.iter().find(|r| r.name == name)
    }

    /// The local branches, which are the only refs the view can move.
    pub fn local_branches(&self) -> impl Iterator<Item = &Ref> {
        self.refs.iter().filter(|r| r.kind == RefKind::Local)
    }
}

/// Render `when` as an age relative to `now`, both unix seconds.
///
/// Coarse on purpose: a commit block has one short field for this, and "3
/// days ago" answers the question a timestamp would make the reader compute.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: Oid::new(id),
            parents: parents.iter().map(|p| Oid::new(*p)).collect(),
            summary: format!("commit {id}"),
            author: "Test".into(),
            when: 0,
            insertions: 1,
            deletions: 0,
        }
    }

    fn dag() -> Dag {
        Dag::new(
            vec![commit("cccc", &["bbbb"]), commit("bbbb", &["aaaa"]), commit("aaaa", &[])],
            vec![
                Ref { name: "main".into(), kind: RefKind::Local, target: Oid::new("cccc"), upstream: Some("origin/main".into()) },
                Ref { name: "origin/main".into(), kind: RefKind::Remote, target: Oid::new("bbbb"), upstream: None },
            ],
            Head { branch: Some("main".into()), target: Some(Oid::new("cccc")) },
            WorkTree::default(),
            false,
        )
    }

    #[test]
    fn a_short_hash_is_a_display_form_and_never_the_stored_value() {
        let id = Oid::new("a3f91c2b7d4e5f60718293a4b5c6d7e8f9012345");
        assert_eq!(id.short(), "a3f91c2");
        assert_eq!(id.short().len(), SHORT_LEN);
        // The full value survives, so it can always be handed to git.
        assert_eq!(id.as_str().len(), 40);
    }

    /// A hash shorter than the abbreviation (a test fixture, or a repository
    /// using a shorter one) must not panic on the slice.
    #[test]
    fn shortening_a_hash_that_is_already_short_is_not_a_panic() {
        assert_eq!(Oid::new("abc").short(), "abc");
        assert_eq!(Oid::new("").short(), "");
    }

    /// A pending id must be impossible to mistake for a real one: it would
    /// otherwise be handed to git as an argument and fail, or worse, resolve.
    #[test]
    fn a_pending_commit_is_not_a_hash_and_never_looks_like_one() {
        let pending = Oid::pending(3);
        assert!(pending.is_pending());
        assert_eq!(pending.short(), "new");
        // Hashes are hex, so the marker cannot collide with one.
        assert!(!Oid::new("a3f91c2").is_pending());
        assert!(Oid::pending(0) != Oid::pending(1));
    }

    #[test]
    fn the_index_answers_lookups_and_tolerates_a_ref_past_the_horizon() {
        let dag = dag();
        assert_eq!(dag.get(&Oid::new("bbbb")).map(|c| c.summary.as_str()), Some("commit bbbb"));
        // A ref pointing past the loaded window is normal, not an error.
        assert!(dag.get(&Oid::new("dddd")).is_none());
        // Newest first, and every commit before its ancestors.
        let order: Vec<_> = dag.commits().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, vec!["cccc", "bbbb", "aaaa"]);
    }

    #[test]
    fn refs_are_found_by_name_and_by_what_they_point_at() {
        let dag = dag();
        assert_eq!(dag.find_ref("origin/main").map(|r| r.kind), Some(RefKind::Remote));
        // Only local branches are movable, so only they are offered.
        let local: Vec<_> = dag.local_branches().map(|r| r.name.as_str()).collect();
        assert_eq!(local, vec!["main"]);
    }

    /// Untracked files deliberately do not count: git lets a checkout proceed
    /// past them, and a stray build artefact must not block every apply.
    #[test]
    fn only_tracked_changes_make_the_tree_dirty() {
        assert!(!WorkTree::default().is_dirty());
        let tree = |index: char, work: char| {
            WorkTree::new(vec![Change { path: "f.rs".into(), index, work }])
        };
        assert!(!tree('?', '?').is_dirty());
        assert!(tree('M', ' ').is_dirty());
        assert!(tree(' ', 'M').is_dirty());
        assert!(tree('U', 'U').is_dirty());
    }

    #[test]
    fn head_is_detached_only_when_it_has_a_commit_but_no_branch() {
        assert!(Head { branch: None, target: Some(Oid::new("a")) }.detached());
        assert!(!Head { branch: Some("main".into()), target: Some(Oid::new("a")) }.detached());
        // An unborn branch in a fresh repository is not "detached".
        assert!(!Head { branch: Some("main".into()), target: None }.detached());
        assert!(!Head::default().detached());
    }

    #[test]
    fn ages_read_as_ages_rather_than_timestamps() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - 90, now), "1m ago");
        assert_eq!(relative_time(now - 3 * 3600, now), "3h ago");
        assert_eq!(relative_time(now - 3 * 86400, now), "3d ago");
        assert_eq!(relative_time(now - 60 * 86400, now), "2mo ago");
        assert_eq!(relative_time(now - 800 * 86400, now), "2y ago");
        // Clock skew (a commit dated in the future) must not underflow.
        assert_eq!(relative_time(now + 5000, now), "just now");
    }
}
