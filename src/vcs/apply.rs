//! Running a derived plan — the **only** module here that writes.
//!
//! Everything above this ([`super::load`], [`super::plan`], [`super::derive`])
//! is a pure function over a snapshot.  That is what makes "nothing happens
//! until `:vc-apply`" a property of the architecture instead of a promise: no
//! other path can reach a `git` invocation that mutates.
//!
//! Two safeguards sit in front of the first write:
//!
//! * **A dirty work tree refuses.**  Replaying commits over uncommitted work
//!   is the reliable way to lose it, and the editor stashing on the user's
//!   behalf is a decision they should make.
//! * **Every branch that will move is saved first**, under
//!   `refs/sakharov/undo/<stamp>/<branch>`.  A real ref, not just the reflog:
//!   it survives `gc`, it survives 90 days, and it covers branches that were
//!   never checked out (which have no reflog worth the name).

use std::path::{Path, PathBuf};

use super::{derive::Op, load::git, Dag, Oid};

/// The ref namespace backups live under.
const UNDO_NS: &str = "refs/sakharov/undo";

/// What running a plan did.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Ops that completed.
    pub ran: usize,
    /// How many there were in total.
    pub total: usize,
    /// The backup namespace, when anything was saved.
    pub backup: Option<String>,
    /// Where it stopped, if it did.
    pub failure: Option<Failure>,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.failure.is_none()
    }
}

/// A stopped run.
#[derive(Debug, Clone)]
pub struct Failure {
    pub op: Op,
    pub message: String,
    /// Paths git reported as conflicted, when that is why it stopped.
    ///
    /// Separated from the message because a conflict is not an error to
    /// report and move past — it is a state the repository is now *in*, and
    /// the user has to be told which files to open.
    pub conflicts: Vec<PathBuf>,
}

/// Why a plan cannot be started.  Distinct from a [`Failure`], which happens
/// once git is already halfway through.
pub fn preflight(dag: &Dag) -> Result<(), String> {
    if dag.work.conflicted() > 0 {
        return Err(format!(
            "{} file(s) are still conflicted — resolve them and commit first",
            dag.work.conflicted()
        ));
    }
    if dag.work.is_dirty() {
        return Err(format!(
            "the work tree has uncommitted changes ({} staged, {} unstaged) — \
             commit or stash them first",
            dag.work.staged(), dag.work.unstaged()
        ));
    }
    Ok(())
}

/// Save `branches`, then run `ops`.
///
/// Takes the derived list rather than the plan so the caller can derive on the
/// UI thread — where the snapshot and the plan live — and hand only this part
/// to a background thread, which needs neither and must not borrow them.
///
/// [`preflight`] is the caller's to check: by the time this runs the decision
/// has been confirmed, and re-reading the work tree here would be a second,
/// different answer to a question already asked.
pub fn run(
    root: &Path,
    branches: &[(String, Oid)],
    ops: &[Op],
    stamp: &str,
) -> Result<Outcome, String> {
    if ops.is_empty() {
        return Err("nothing to apply".to_string());
    }
    let backup = save_backup(root, branches, stamp)?;

    let total = ops.len();
    for (ran, op) in ops.iter().enumerate() {
        let owned = op.args();
        let args: Vec<&str> = owned.iter().map(String::as_str).collect();
        if let Err(message) = git(root, &args) {
            return Ok(Outcome {
                ran,
                total,
                backup,
                failure: Some(Failure {
                    op: op.clone(),
                    message,
                    conflicts: conflicted_paths(root),
                }),
            });
        }
    }
    Ok(Outcome { ran: total, total, backup, failure: None })
}

/// Record every branch the plan will move, plus HEAD.
///
/// Returns the namespace, or `None` when the plan moves no branch at all —
/// which is possible: rewriting the commits *under* a tip leaves the ref name
/// pointing at the same place until the replay runs.
fn save_backup(
    root: &Path,
    branches: &[(String, Oid)],
    stamp: &str,
) -> Result<Option<String>, String> {
    // *Every* local branch is saved, not only the ones the projection moved: a
    // replay changes where a branch points without the projection ever having
    // moved that ref, and a backup that missed those would restore the ref
    // names while leaving the old commits unreachable.
    if branches.is_empty() {
        return Ok(None);
    }
    let ns = format!("{UNDO_NS}/{stamp}");
    for (name, target) in branches {
        git(root, &["update-ref", &format!("{ns}/{name}"), target.as_str()])?;
    }
    Ok(Some(ns))
}

/// Files git currently reports as unmerged.
fn conflicted_paths(root: &Path) -> Vec<PathBuf> {
    git(root, &["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| root.join(l))
        .collect()
}

/// Every saved backup, newest first.
///
/// Newest first because the stamp sorts lexicographically as a timestamp,
/// which is the one property the caller's format has to have.
pub fn backups(root: &Path) -> Vec<String> {
    let mut stamps: Vec<String> = git(
        root,
        &["for-each-ref", "--format=%(refname)", UNDO_NS],
    )
    .unwrap_or_default()
    .lines()
    .filter_map(|refname| refname.strip_prefix(&format!("{UNDO_NS}/")))
    // `<stamp>/<branch>`, and a branch name may itself contain slashes.
    .filter_map(|rest| rest.split('/').next())
    .map(str::to_owned)
    .collect();
    stamps.sort();
    stamps.dedup();
    stamps.reverse();
    stamps
}

/// Put every branch in `stamp` back where it was, and drop the backup.
///
/// The backup is deleted rather than kept: once the branches point at those
/// commits again they are reachable on their own, and a namespace that only
/// ever grows is worse than one that cleans up after itself.
pub fn undo(root: &Path, stamp: &str) -> Result<Vec<String>, String> {
    let ns = format!("{UNDO_NS}/{stamp}");
    let listing = git(root, &["for-each-ref", "--format=%(refname)\x1f%(objectname)", &ns])?;

    let saved: Vec<(String, String)> = listing
        .lines()
        .filter_map(|line| {
            let (refname, oid) = line.split_once('\x1f')?;
            let branch = refname.strip_prefix(&format!("{ns}/"))?;
            (!branch.is_empty()).then(|| (branch.to_owned(), oid.trim().to_owned()))
        })
        .collect();
    if saved.is_empty() {
        return Err(format!("no backup called {stamp}"));
    }

    // Detach first: a checked-out branch cannot be force-moved.
    let head = git(root, &["rev-parse", "HEAD"]).map(|s| s.trim().to_owned());
    let on_branch = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_owned());
    if let Ok(ref oid) = head {
        git(root, &["checkout", "--detach", oid])?;
    }

    let mut restored = Vec::new();
    for (branch, oid) in &saved {
        git(root, &["branch", "--force", branch, oid])?;
        restored.push(branch.clone());
    }

    if let Some(branch) = on_branch {
        // The branch we were on may itself have just moved; checking it out
        // lands on its restored position, which is the point.
        git(root, &["checkout", &branch])?;
    }

    // `-d` over the namespace: `update-ref --stdin` would be one process, but
    // the branch count here is small and the failure mode of a partial delete
    // is a stale backup, which is harmless.
    for (branch, oid) in &saved {
        let _ = git(root, &["update-ref", "-d", &format!("{ns}/{branch}"), oid]);
    }

    restored.sort();
    Ok(restored)
}

/// Back out of a stopped run: `git <verb> --abort` for whichever operation is
/// in progress.
///
/// Which one is in progress is read from the presence of git's own state
/// directories rather than remembered, because the user may have run
/// something in a shell in between.
pub fn abort(root: &Path) -> Result<&'static str, String> {
    for (marker, verb) in [
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REBASE_HEAD", "rebase"),
        ("MERGE_HEAD", "merge"),
    ] {
        if git(root, &["rev-parse", "--verify", "--quiet", marker]).is_ok() {
            git(root, &[verb, "--abort"])?;
            return Ok(verb);
        }
    }
    Err("nothing is in progress".to_string())
}

/// Resume a stopped run once the conflicts are resolved and staged.
pub fn resume(root: &Path) -> Result<&'static str, String> {
    for (marker, verb) in [
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REBASE_HEAD", "rebase"),
        ("MERGE_HEAD", "merge"),
    ] {
        if git(root, &["rev-parse", "--verify", "--quiet", marker]).is_ok() {
            git(root, &[verb, "--continue"])?;
            return Ok(verb);
        }
    }
    Err("nothing is in progress".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{derive, load, plan::Edit, plan::Plan};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    /// A throwaway repository on disk.
    ///
    /// Real git, not a mock.  The whole point of this layer is that it drives
    /// the user's git, and a mock would test the mock: `cherry-pick` refusing
    /// to replay a merge, `branch -f` refusing a checked-out branch, and the
    /// exact shape of a conflicted status are all behaviours worth pinning
    /// against the real thing.
    struct Repo {
        root: PathBuf,
    }

    impl Repo {
        fn new() -> Option<Self> {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir()
                .join(format!("sv-vcs-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).ok()?;
            let repo = Repo { root };
            // `-b main` so the default branch name is not whatever the
            // machine's git is configured for, and identity + signing are
            // pinned locally so a developer's global config cannot fail the
            // suite.
            repo.run(&["init", "-b", "main"])?;
            repo.run(&["config", "user.email", "test@example.com"])?;
            repo.run(&["config", "user.name", "Test"])?;
            repo.run(&["config", "commit.gpgsign", "false"])?;
            Some(repo)
        }

        fn run(&self, args: &[&str]) -> Option<String> {
            git(&self.root, args).ok()
        }

        fn commit(&self, file: &str, contents: &str, message: &str) -> Oid {
            std::fs::write(self.root.join(file), contents).unwrap();
            self.run(&["add", file]).unwrap();
            self.run(&["commit", "-m", message]).unwrap();
            self.head()
        }

        fn head(&self) -> Oid {
            Oid::new(self.run(&["rev-parse", "HEAD"]).unwrap().trim())
        }

        fn dag(&self) -> Dag {
            load::read(&self.root, 100).unwrap()
        }

        fn branch_at(&self, name: &str) -> Oid {
            Oid::new(self.run(&["rev-parse", name]).unwrap().trim())
        }

        fn log(&self, refname: &str) -> Vec<String> {
            self.run(&["log", "--format=%s", refname])
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Derive and run, exactly as `exec::vcs` does: preflight on the UI side,
    /// derive from the snapshot, then hand the op list to `run`.  Going
    /// through the same two steps is what keeps these tests about the real
    /// path rather than about a convenience wrapper only they use.
    fn apply(root: &Path, dag: &Dag, plan: &Plan, stamp: &str) -> Result<Outcome, String> {
        preflight(dag)?;
        let ops = derive::derive(dag, plan)?;
        let branches: Vec<(String, Oid)> = dag
            .local_branches()
            .map(|r| (r.name.clone(), r.target.clone()))
            .collect();
        run(root, &branches, &ops, stamp)
    }

    /// `main: base → one`, `feature: base → side`.
    fn diverged() -> Option<(Repo, Oid, Oid, Oid)> {
        let repo = Repo::new()?;
        let base = repo.commit("base.txt", "base\n", "base");
        repo.run(&["checkout", "-b", "feature"])?;
        let side = repo.commit("side.txt", "side\n", "side");
        repo.run(&["checkout", "main"])?;
        let one = repo.commit("one.txt", "one\n", "one");
        Some((repo, base, side, one))
    }

    /// The end-to-end case, through every layer: the user drags `feature`'s
    /// base arrow from `base` onto `main`'s tip, and the repository ends up
    /// with `feature` replayed on top — without the word "rebase" appearing
    /// anywhere between the gesture and the result.
    #[test]
    fn dragging_a_branch_onto_another_replays_it_there() {
        let Some((repo, _base, side, one)) = diverged() else {
            return;
        };
        let dag = repo.dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Reparent { child: side.clone(), slot: 0, new_parent: Some(one.clone()) })
            .unwrap();

        let outcome = apply(&repo.root, &dag, &plan, "test-rebase").unwrap();
        assert!(outcome.succeeded(), "{:?}", outcome.failure);

        // `feature` now contains main's commit, and its own on top.
        assert_eq!(repo.log("feature"), vec!["side", "one", "base"]);
        // `main` is untouched, and HEAD is back on it.
        assert_eq!(repo.branch_at("main"), one);
        assert_eq!(
            repo.run(&["symbolic-ref", "--short", "HEAD"]).unwrap().trim(),
            "main"
        );
        // The replayed commit is a *new* object: the original is not reused.
        assert_ne!(repo.branch_at("feature"), side);
    }

    /// The backup is what makes the destructive operation reversible, and it
    /// has to restore the exact prior oids — not merely the branch names.
    #[test]
    fn undo_puts_every_branch_back_where_it_was() {
        let Some((repo, _base, side, one)) = diverged() else {
            return;
        };
        let dag = repo.dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Reparent { child: side.clone(), slot: 0, new_parent: Some(one.clone()) })
            .unwrap();

        let outcome = apply(&repo.root, &dag, &plan, "stamp-1").unwrap();
        assert!(outcome.succeeded());
        assert_eq!(backups(&repo.root), vec!["stamp-1".to_string()]);
        assert_ne!(repo.branch_at("feature"), side);

        let restored = undo(&repo.root, "stamp-1").unwrap();
        assert_eq!(restored, vec!["feature".to_string(), "main".to_string()]);
        assert_eq!(repo.branch_at("feature"), side, "the exact prior commit");
        assert_eq!(repo.branch_at("main"), one);
        // The backup cleans up after itself: the commits are reachable again
        // from the branches, so keeping it would only accumulate refs.
        assert!(backups(&repo.root).is_empty());
    }

    /// Replaying over uncommitted work is how people lose it, so the run is
    /// refused before the backup is even written.
    #[test]
    fn a_dirty_work_tree_refuses_before_anything_is_written() {
        let Some((repo, _base, side, one)) = diverged() else {
            return;
        };
        std::fs::write(repo.root.join("one.txt"), "edited\n").unwrap();

        let dag = repo.dag();
        assert!(dag.work.is_dirty());
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Reparent { child: side, slot: 0, new_parent: Some(one) })
            .unwrap();

        let err = apply(&repo.root, &dag, &plan, "nope").expect_err("dirty tree");
        assert!(err.contains("uncommitted"), "{err}");
        assert!(backups(&repo.root).is_empty(), "nothing was written");
    }

    /// A conflict is a state the repository is now in, not an error to report
    /// and forget: the run stops where it stopped, and the conflicted paths
    /// are named so the user can open them.
    #[test]
    fn a_conflicting_replay_stops_and_names_the_files() {
        let Some(repo) = Repo::new() else { return };
        repo.commit("shared.txt", "original\n", "base");
        repo.run(&["checkout", "-b", "feature"]).unwrap();
        let side = repo.commit("shared.txt", "from feature\n", "side");
        repo.run(&["checkout", "main"]).unwrap();
        let one = repo.commit("shared.txt", "from main\n", "one");

        let dag = repo.dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Reparent { child: side, slot: 0, new_parent: Some(one) })
            .unwrap();

        let outcome = apply(&repo.root, &dag, &plan, "conflict").unwrap();
        let failure = outcome.failure.expect("the replay conflicts");
        assert!(
            failure.conflicts.iter().any(|p| p.ends_with("shared.txt")),
            "the conflicted file is named: {:?}",
            failure.conflicts
        );
        assert!(outcome.ran < outcome.total, "it stopped part way");

        // And the user can back out of it in one command.
        assert_eq!(abort(&repo.root).unwrap(), "cherry-pick");
        assert!(!repo.dag().work.is_dirty());
    }

    #[test]
    fn aborting_when_nothing_is_in_progress_says_so() {
        let Some(repo) = Repo::new() else { return };
        repo.commit("a.txt", "a\n", "a");
        assert!(abort(&repo.root).is_err());
        assert!(resume(&repo.root).is_err());
    }

    /// Moving a branch forward onto a descendant rewrites nothing — the case
    /// every other tool calls a fast-forward, arrived at without naming it.
    #[test]
    fn moving_a_branch_forward_reuses_the_existing_commits() {
        let Some(repo) = Repo::new() else { return };
        repo.commit("a.txt", "a\n", "base");
        repo.run(&["branch", "old"]).unwrap();
        let tip = repo.commit("b.txt", "b\n", "tip");

        let dag = repo.dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::MoveRef { name: "old".into(), new_target: tip.clone() })
            .unwrap();

        let outcome = apply(&repo.root, &dag, &plan, "ff").unwrap();
        assert!(outcome.succeeded(), "{:?}", outcome.failure);
        // The very same object, not a replayed copy.
        assert_eq!(repo.branch_at("old"), tip);
    }

    #[test]
    fn undoing_a_backup_that_does_not_exist_says_so() {
        let Some(repo) = Repo::new() else { return };
        repo.commit("a.txt", "a\n", "a");
        assert!(undo(&repo.root, "never-happened").is_err());
    }

    /// A merge is emitted as a merge, and produces a commit with two parents
    /// rather than a replay.
    #[test]
    fn merging_produces_a_real_merge_commit() {
        let Some((repo, _base, side, _one)) = diverged() else {
            return;
        };
        let dag = repo.dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Merge { into: "main".into(), from: side.clone() })
            .unwrap();

        let outcome = apply(&repo.root, &dag, &plan, "merge").unwrap();
        assert!(outcome.succeeded(), "{:?}", outcome.failure);

        let parents = repo.run(&["rev-list", "--parents", "-n", "1", "main"]).unwrap();
        assert_eq!(
            parents.split_whitespace().count(),
            3,
            "the tip is a commit with two parents: {parents}"
        );
        assert!(repo.log("main").contains(&"side".to_string()));
    }
}
