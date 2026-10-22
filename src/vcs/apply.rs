//! Running a derived plan — the **only** module that writes to the
//! repository.
//!
//! Before the first write: the repository must still be exactly as it was
//! loaded ([`preflight`]), and a backup is taken ([`save_backup`]) that
//! [`undo`] can restore — every local branch and HEAD as refs under
//! `refs/sakharov/undo/<stamp>/`, and a copy of the index and of every changed
//! work-tree file under `<git dir>/sakharov/undo/<stamp>/`.

use std::path::{Path, PathBuf};

use super::{
    derive::{Data, Expect, Op, Stage},
    load::{self, git, git_command},
    Oid, Repo,
};

/// The ref namespace backups live under.
const UNDO_NS: &str = "refs/sakharov/undo";

/// What running a plan did.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Ops that completed.
    pub ran: usize,
    pub total: usize,
    pub failure: Option<Failure>,
}

/// Where a run stopped.
#[derive(Debug, Clone)]
pub struct Failure {
    pub op: String,
    pub message: String,
    /// Paths git now reports as conflicted.
    pub conflicts: Vec<String>,
}

/// Refuse when the repository changed since `snapshot` was loaded.
pub fn preflight(root: &Path, snapshot: &Repo) -> Result<(), String> {
    let now = load::fingerprint(root)?;
    let then = &snapshot.fingerprint;
    if now == *then {
        return Ok(());
    }
    let what = if now.head != then.head || now.branches != then.branches {
        "the branches moved".to_string()
    } else if now.stashes != then.stashes {
        "the stash list changed".to_string()
    } else if now.operation != then.operation {
        "an operation started or finished".to_string()
    } else {
        let changed: Vec<&String> = now
            .stats
            .keys()
            .chain(then.stats.keys())
            .filter(|p| now.stats.get(*p) != then.stats.get(*p))
            .collect();
        match changed.first() {
            Some(path) => format!("{path} changed on disk"),
            None => "the work tree changed".to_string(),
        }
    };
    Err(format!("{what} since the view loaded — r reloads (the plan is kept)"))
}

fn backup_dir(root: &Path, stamp: &str) -> PathBuf {
    load::git_dir(root).join("sakharov").join("undo").join(stamp)
}

/// Save the branches, HEAD, the index and every changed work-tree file.
pub fn save_backup(root: &Path, snapshot: &Repo, stamp: &str) -> Result<(), String> {
    let dir = backup_dir(root, stamp);
    let files = dir.join("files");
    std::fs::create_dir_all(&files).map_err(|e| format!("cannot write the backup: {e}"))?;
    let ns = format!("{UNDO_NS}/{stamp}");
    for (name, oid) in &snapshot.branches {
        git(root, &["update-ref", &format!("{ns}/heads/{name}"), oid.as_str()])?;
    }
    for (n, stash) in snapshot.stashes.iter().enumerate() {
        git(root, &["update-ref", &format!("{ns}/stash/{n}"), stash.commit.as_str()])?;
    }
    let mut manifest = String::new();
    if let Some(commit) = &snapshot.head.commit {
        git(root, &["update-ref", &format!("{ns}/HEAD"), commit.as_str()])?;
    }
    if let Some(branch) = &snapshot.head.branch {
        manifest.push_str(&format!("branch\t{branch}\n"));
    }
    for (n, stash) in snapshot.stashes.iter().enumerate() {
        manifest.push_str(&format!("stash\t{n}\t{}\t{}\n", stash.commit, stash.message));
    }
    let index = load::git_dir(root).join("index");
    if index.exists() {
        std::fs::copy(&index, dir.join("index")).map_err(|e| format!("cannot back up the index: {e}"))?;
    }
    for path in snapshot.files.keys() {
        let from = root.join(path);
        if std::fs::symlink_metadata(&from).is_ok() {
            let to = files.join(path);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::copy(&from, &to).map_err(|e| format!("cannot back up {path}: {e}"))?;
            manifest.push_str(&format!("present\t{path}\n"));
        } else {
            manifest.push_str(&format!("absent\t{path}\n"));
        }
    }
    std::fs::write(dir.join("manifest"), manifest).map_err(|e| e.to_string())
}

/// Back up, then run `ops` in order, stopping at the first that fails.
pub fn run(root: &Path, snapshot: &Repo, ops: &[Op], stamp: &str) -> Result<Outcome, String> {
    if ops.is_empty() {
        return Err("nothing to apply".to_string());
    }
    preflight(root, snapshot)?;
    save_backup(root, snapshot, stamp)?;
    let total = ops.len();
    for (ran, op) in ops.iter().enumerate() {
        if let Err((message, conflicts)) = run_op(root, op) {
            let failure = Failure { op: op.describe(), message, conflicts };
            return Ok(Outcome { ran, total, failure: Some(failure) });
        }
    }
    Ok(Outcome { ran: total, total, failure: None })
}

type OpError = (String, Vec<String>);

fn run_op(root: &Path, op: &Op) -> Result<(), OpError> {
    let plain = |e: String| (e, Vec::new());
    match op {
        Op::WriteFile { path, entry } => write_file(&root.join(path), entry.as_ref()).map_err(plain),
        Op::SetIndex { path, entry: None } => {
            git(root, &["update-index", "--force-remove", "--", path]).map(|_| ()).map_err(plain)
        }
        Op::SetIndex { path, entry: Some((_, Data::WorkFile)) } => {
            git(root, &["add", "--force", "--", path]).map(|_| ()).map_err(plain)
        }
        Op::SetIndex { path, entry: Some((mode, data)) } => {
            let oid = match data {
                Data::Object(oid) => oid.clone(),
                Data::Bytes(bytes) => hash_object(root, bytes).map_err(plain)?,
                Data::WorkFile => unreachable!("matched above"),
            };
            let info = format!("{mode:06o},{oid},{path}");
            git(root, &["update-index", "--add", "--cacheinfo", &info]).map(|_| ()).map_err(plain)
        }
        Op::Git { args, expect } => {
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = git(root, &argv);
            match (expect, result) {
                (Expect::Success, Ok(_)) => Ok(()),
                (Expect::Success, Err(e)) => Err((e, unmerged_paths(root))),
                (Expect::Conflicts(_), Ok(_)) => Err((
                    "git finished without the conflicts the plan expected; reload to see what happened".to_string(),
                    Vec::new(),
                )),
                (Expect::Conflicts(want), Err(e)) => {
                    let got = unmerged(root);
                    if sorted(got.clone()) == sorted(want.clone()) {
                        Ok(())
                    } else {
                        let paths = unmerged_paths(root);
                        Err((format!("{e}\n(the conflicts are not the ones the plan resolved)"), paths))
                    }
                }
            }
        }
    }
}

fn sorted(mut stages: Vec<Stage>) -> Vec<Stage> {
    stages.sort();
    stages
}

fn hash_object(root: &Path, bytes: &[u8]) -> Result<Oid, String> {
    use std::io::Write;
    let mut child = git_command(root)
        .args(["hash-object", "-w", "--no-filters", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run git: {e}"))?;
    child.stdin.take().ok_or("no stdin")?.write_all(bytes).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(Oid::new(String::from_utf8_lossy(&out.stdout).trim()))
}

fn write_file(path: &Path, entry: Option<&(u32, std::sync::Arc<[u8]>)>) -> Result<(), String> {
    let Some((mode, bytes)) = entry else {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("cannot delete {}: {e}", path.display())),
            _ => Ok(()),
        };
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if *mode == super::MODE_LINK {
        let _ = std::fs::remove_file(path);
        #[cfg(unix)]
        return std::os::unix::fs::symlink(String::from_utf8_lossy(bytes).as_ref(), path).map_err(|e| e.to_string());
    }
    let text = String::from_utf8_lossy(bytes);
    if text.as_bytes() == &bytes[..] {
        crate::buffer::atomic_write(path, &text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    } else {
        std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = if *mode == super::MODE_EXEC { 0o755 } else { 0o644 };
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(perm));
    }
    Ok(())
}

/// The unmerged index entries, as `(path, stage, blob)`.
fn unmerged(root: &Path) -> Vec<Stage> {
    git(root, &["ls-files", "-u", "-z"])
        .unwrap_or_default()
        .split('\0')
        .filter_map(|r| {
            let (meta, path) = r.split_once('\t')?;
            let mut parts = meta.split(' ');
            let (_, oid, stage) = (parts.next()?, parts.next()?, parts.next()?);
            Some((path.to_string(), stage.parse().ok()?, Oid::new(oid)))
        })
        .collect()
}

fn unmerged_paths(root: &Path) -> Vec<String> {
    let mut paths: Vec<String> = unmerged(root).into_iter().map(|s| s.0).collect();
    paths.dedup();
    paths
}

/// Every saved backup, newest first.
pub fn backups(root: &Path) -> Vec<String> {
    let dir = load::git_dir(root).join("sakharov").join("undo");
    let mut stamps: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    stamps.sort();
    stamps.reverse();
    stamps
}

/// Put the repository back the way `stamp` saved it, and drop the backup.
///
/// Anything done since that apply — by the plan or by hand — to the files the
/// backup covers is overwritten.
pub fn undo(root: &Path, stamp: &str) -> Result<(), String> {
    let dir = backup_dir(root, stamp);
    let manifest = std::fs::read_to_string(dir.join("manifest")).map_err(|_| format!("no backup called {stamp}"))?;
    let ns = format!("{UNDO_NS}/{stamp}");

    for verb in ["merge", "rebase", "cherry-pick", "revert"] {
        let _ = git(root, &[verb, "--abort"]);
    }
    let refs = git(root, &["for-each-ref", "--format=%(refname)\x1f%(objectname)", &ns]).unwrap_or_default();
    let mut head = None;
    for line in refs.lines() {
        let Some((name, oid)) = line.split_once('\x1f') else { continue };
        let rel = name.strip_prefix(&format!("{ns}/")).unwrap_or_default();
        if let Some(branch) = rel.strip_prefix("heads/") {
            git(root, &["update-ref", &format!("refs/heads/{branch}"), oid])?;
        } else if rel == "HEAD" {
            head = Some(oid.to_string());
        }
    }
    let branch = manifest.lines().find_map(|l| l.strip_prefix("branch\t"));
    match (branch, &head) {
        (Some(b), _) => git(root, &["symbolic-ref", "HEAD", &format!("refs/heads/{b}")]).map(|_| ())?,
        (None, Some(oid)) => git(root, &["update-ref", "--no-deref", "HEAD", oid]).map(|_| ())?,
        (None, None) => {}
    }
    if head.is_some() {
        git(root, &["reset", "-q", "--hard", "HEAD"])?;
    }
    for line in manifest.lines() {
        let mut parts = line.splitn(2, '\t');
        match (parts.next(), parts.next()) {
            (Some("present"), Some(path)) => {
                let to = root.join(path);
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let _ = std::fs::remove_file(&to);
                std::fs::copy(dir.join("files").join(path), &to).map_err(|e| format!("cannot restore {path}: {e}"))?;
            }
            (Some("absent"), Some(path)) => {
                let _ = std::fs::remove_file(root.join(path));
            }
            _ => {}
        }
    }
    if dir.join("index").exists() {
        std::fs::copy(dir.join("index"), load::git_dir(root).join("index")).map_err(|e| e.to_string())?;
    }
    let current: Vec<String> = git(root, &["stash", "list", "--format=%H"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let mut saved: Vec<(usize, &str, &str)> = manifest
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(4, '\t');
            (p.next()? == "stash").then_some(())?;
            Some((p.next()?.parse().ok()?, p.next()?, p.next().unwrap_or("")))
        })
        .collect();
    saved.sort_by_key(|s| std::cmp::Reverse(s.0));
    for (_, oid, message) in saved {
        if !current.iter().any(|c| c == oid) {
            git(root, &["stash", "store", "-m", message, oid])?;
        }
    }
    for line in refs.lines() {
        if let Some((name, _)) = line.split_once('\x1f') {
            let _ = git(root, &["update-ref", "-d", name]);
        }
    }
    std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{
        derive,
        diff,
        plan::{Plan, Scope, Step},
        store::Store,
        testrepo::TestRepo,
        Blob, Entry, Index, Target, MODE_FILE,
    };

    /// Derive and run exactly as `exec::vcs` does.
    fn apply(repo: &TestRepo, snap: &Repo, store: &Store, plan: &Plan, stamp: &str) -> Outcome {
        let ops = derive::derive(store, snap, plan.steps(), plan.states()).unwrap();
        run(repo.path(), snap, &ops, stamp).unwrap()
    }

    /// The invariant the whole design rests on: what the projection says the
    /// repository will look like is what applying the plan produces.
    fn assert_agrees(repo: &TestRepo, store: &Store, projected: &Repo) {
        let real = load::read(repo.path()).unwrap();
        let fresh = Store::new(repo.root.clone());
        assert_eq!(real.head.branch, projected.head.branch, "HEAD's branch");
        assert_eq!(real.branches.keys().collect::<Vec<_>>(), projected.branches.keys().collect::<Vec<_>>());
        assert_eq!(real.stashes.len(), projected.stashes.len(), "stash count");
        assert_eq!(real.operation.as_ref().map(|o| o.kind), projected.operation.as_ref().map(|o| o.kind));
        let paths: std::collections::BTreeSet<&String> = real.files.keys().chain(projected.files.keys()).collect();
        for path in paths {
            let (r, p) = (real.files.get(path), projected.files.get(path));
            let (Some(r), Some(p)) = (r, p) else {
                panic!("{path}: real {r:?} vs projected {p:?}");
            };
            match (&r.index, &p.index) {
                (Index::Entry(a), Index::Entry(b)) => {
                    assert_eq!(fresh.text(a.as_ref()), store.text(b.as_ref()), "{path}: index");
                    assert_eq!(fresh.text(r.work.as_ref()), store.text(p.work.as_ref()), "{path}: work tree");
                }
                (Index::Conflict(a), Index::Conflict(b)) => assert_eq!(a, b, "{path}: conflict stages"),
                (a, b) => panic!("{path}: index {a:?} vs {b:?}"),
            }
            assert_eq!(fresh.text(r.head.as_ref()), store.text(p.head.as_ref()), "{path}: HEAD");
        }
        // And every commit the plan made has the tree it was projected to.
        if let (Some(commit), Some(projected_head)) = (&real.head.commit, &projected.head.commit) {
            let tree = git(repo.path(), &["rev-parse", &format!("{commit}^{{tree}}")]).unwrap();
            let want = store.write_tree(projected, &projected.tree_of(Some(projected_head))).unwrap();
            assert_eq!(tree.trim(), want.as_str(), "HEAD's tree");
        }
    }

    fn key(store: &Store, repo: &Repo, path: &str, nth: usize) -> diff::ChangeKey {
        let f = &repo.files[path];
        let (h, w) = (store.text(f.head.as_ref()).unwrap_or_default(), store.text(f.work.as_ref()).unwrap_or_default());
        diff::changes(&h, &w)[nth].key()
    }

    #[test]
    fn staging_part_of_a_file_committing_and_discarding_agree_with_the_projection() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("a.txt", "a\nb\nc\nd\ne\n", "base");
        repo.commit("gone.txt", "bye\n", "gone");
        repo.write("a.txt", "a\nB\nc\nD\ne\n");
        repo.write("new.txt", "new\n");
        repo.write("junk.txt", "junk\n");
        std::fs::remove_file(repo.root.join("gone.txt")).unwrap();
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());

        let mut plan = Plan::default();
        let k = key(&store, &snap, "a.txt", 0);
        plan.push(&store, &snap, Step::Stage { path: "a.txt".into(), scope: Scope::Change(k) }).unwrap();
        plan.push(&store, &snap, Step::Stage { path: "new.txt".into(), scope: Scope::File }).unwrap();
        plan.push(&store, &snap, Step::Stage { path: "gone.txt".into(), scope: Scope::File }).unwrap();
        plan.push(&store, &snap, Step::Commit { message: "part".into(), amend: false }).unwrap();
        plan.push(&store, &snap, Step::Discard { path: "junk.txt".into(), scope: Scope::File }).unwrap();

        let outcome = apply(&repo, &snap, &store, &plan, "0001");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_agrees(&repo, &store, plan.current(&snap));
        assert_eq!(repo.read("a.txt").unwrap(), "a\nB\nc\nD\ne\n", "the unstaged change is still on disk");
        assert!(repo.read("junk.txt").is_none());
    }

    #[test]
    fn a_planned_conflicting_merge_with_its_resolution_agrees_with_the_projection() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("f.txt", "a\nb\nc\n", "base");
        repo.git(&["checkout", "-q", "-b", "feature"]).unwrap();
        repo.commit("f.txt", "a\nX\nc\n", "feature");
        repo.commit("g.txt", "only on feature\n", "feature 2");
        repo.git(&["checkout", "-q", "main"]).unwrap();
        repo.commit("f.txt", "a\nY\nc\n", "main");
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());

        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::Merge { from: Target::Branch("feature".into()) }).unwrap();
        let result = Entry { mode: MODE_FILE, blob: Blob::Text(b"a\nX and Y\nc\n".to_vec().into()) };
        plan.push(&store, &snap, Step::Resolve { path: "f.txt".into(), result: Some(result) }).unwrap();
        plan.push(&store, &snap, Step::Continue).unwrap();

        let outcome = apply(&repo, &snap, &store, &plan, "0002");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_agrees(&repo, &store, plan.current(&snap));
        assert_eq!(repo.read("f.txt").unwrap(), "a\nX and Y\nc\n");
    }

    #[test]
    fn a_rebase_that_stops_and_is_resolved_agrees_with_the_projection() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("f.txt", "a\nb\nc\n", "base");
        repo.git(&["checkout", "-q", "-b", "feature"]).unwrap();
        repo.commit("f.txt", "a\nX\nc\n", "feature edits f");
        repo.commit("h.txt", "h\n", "feature adds h");
        repo.git(&["checkout", "-q", "main"]).unwrap();
        repo.commit("f.txt", "a\nY\nc\n", "main edits f");
        repo.git(&["checkout", "-q", "feature"]).unwrap();
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());

        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::Rebase { onto: Target::Branch("main".into()) }).unwrap();
        assert!(plan.current(&snap).files["f.txt"].index.is_conflict());
        let result = Entry { mode: MODE_FILE, blob: Blob::Text(b"a\nXY\nc\n".to_vec().into()) };
        plan.push(&store, &snap, Step::Resolve { path: "f.txt".into(), result: Some(result) }).unwrap();
        plan.push(&store, &snap, Step::Continue).unwrap();
        assert!(plan.current(&snap).operation.is_none(), "the rest replays cleanly");

        let outcome = apply(&repo, &snap, &store, &plan, "0003");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_agrees(&repo, &store, plan.current(&snap));
        let log = git(repo.path(), &["log", "--format=%s", "feature"]).unwrap();
        assert_eq!(log, "feature adds h\nfeature edits f\nmain edits f\nbase\n");
    }

    #[test]
    fn branches_tags_and_stashes_agree_with_the_projection() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("a.txt", "a\n", "base");
        repo.write("a.txt", "edited\n");
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());

        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::StashPush { message: "wip".into() }).unwrap();
        plan.push(&store, &snap, Step::CreateBranch { name: "topic".into() }).unwrap();
        plan.push(&store, &snap, Step::Tag { name: "v1".into(), at: Target::Branch("main".into()) }).unwrap();
        plan.push(&store, &snap, Step::StashPop { index: 0 }).unwrap();
        plan.push(&store, &snap, Step::Stage { path: "a.txt".into(), scope: Scope::File }).unwrap();
        plan.push(&store, &snap, Step::Commit { message: "on topic".into(), amend: false }).unwrap();
        plan.push(&store, &snap, Step::Switch { branch: "main".into() }).unwrap();
        plan.push(&store, &snap, Step::Merge { from: Target::Branch("topic".into()) }).unwrap();
        plan.push(&store, &snap, Step::DeleteBranch { name: "topic".into() }).unwrap();

        let outcome = apply(&repo, &snap, &store, &plan, "0004");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_agrees(&repo, &store, plan.current(&snap));
        assert_eq!(git(repo.path(), &["tag"]).unwrap(), "v1\n");
    }

    /// `main` and `feature` both change line 2 of `f.txt`; `feature` also
    /// adds `h.txt` in a second commit.
    fn diverged() -> Option<TestRepo> {
        let repo = TestRepo::new()?;
        repo.commit("f.txt", "a\nb\nc\n", "base");
        repo.git(&["checkout", "-q", "-b", "feature"])?;
        repo.commit("f.txt", "a\nX\nc\n", "feature edits f");
        repo.commit("h.txt", "h\n", "feature adds h");
        repo.git(&["checkout", "-q", "main"])?;
        repo.commit("f.txt", "a\nY\nc\n", "main edits f");
        Some(repo)
    }

    fn resolve_and_continue(repo: &TestRepo) -> (Repo, Store, Plan) {
        let snap = load::read(repo.path()).unwrap();
        assert!(snap.operation.is_some(), "git left an operation in progress");
        assert!(snap.files["f.txt"].index.is_conflict());
        let store = Store::new(repo.root.clone());
        let mut plan = Plan::default();
        let result = Entry { mode: MODE_FILE, blob: Blob::Text(b"a\nresolved\nc\n".to_vec().into()) };
        plan.push(&store, &snap, Step::Resolve { path: "f.txt".into(), result: Some(result) }).unwrap();
        plan.push(&store, &snap, Step::Continue).unwrap();
        (snap, store, plan)
    }

    #[test]
    fn a_merge_git_left_conflicted_is_resolved_and_finished_through_the_plan() {
        let Some(repo) = diverged() else { return };
        assert!(repo.git(&["merge", "feature"]).is_none(), "the merge conflicts");
        let (snap, store, plan) = resolve_and_continue(&repo);
        assert!(plan.current(&snap).operation.is_none());

        let outcome = apply(&repo, &snap, &store, &plan, "0007");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_agrees(&repo, &store, plan.current(&snap));
        let parents = git(repo.path(), &["rev-list", "--parents", "-n", "1", "HEAD"]).unwrap();
        assert_eq!(parents.split_whitespace().count(), 3, "a merge commit");
    }

    #[test]
    fn a_rebase_git_left_conflicted_replays_the_rest_after_continuing() {
        let Some(repo) = diverged() else { return };
        repo.git(&["checkout", "-q", "feature"]).unwrap();
        assert!(repo.git(&["-c", "core.editor=true", "rebase", "main"]).is_none(), "the rebase stops");
        let (snap, store, plan) = resolve_and_continue(&repo);
        let done = plan.current(&snap);
        assert!(done.operation.is_none(), "the remaining pick replays cleanly");
        assert_eq!(done.head.branch.as_deref(), Some("feature"), "and the branch is checked out again");

        let outcome = apply(&repo, &snap, &store, &plan, "0008");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_agrees(&repo, &store, plan.current(&snap));
        let log = git(repo.path(), &["log", "--format=%s", "feature"]).unwrap();
        assert_eq!(log, "feature adds h\nfeature edits f\nmain edits f\nbase\n");
    }

    #[test]
    fn cherry_pick_revert_reset_and_rename_agree_with_the_projection() {
        let Some(repo) = diverged() else { return };
        let added_h = crate::vcs::Oid::new(git(repo.path(), &["rev-parse", "feature"]).unwrap().trim());
        let main_edit = crate::vcs::Oid::new(git(repo.path(), &["rev-parse", "main"]).unwrap().trim());
        let base = crate::vcs::Oid::new(git(repo.path(), &["rev-parse", "main~1"]).unwrap().trim());
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());

        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::CherryPick { commit: added_h }).unwrap();
        plan.push(&store, &snap, Step::Revert { commit: main_edit }).unwrap();
        plan.push(&store, &snap, Step::RenameBranch { from: "main".into(), to: "trunk".into() }).unwrap();
        plan.push(&store, &snap, Step::Reset { to: Target::Commit(base), mode: crate::vcs::plan::ResetMode::Mixed }).unwrap();

        let outcome = apply(&repo, &snap, &store, &plan, "0009");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_agrees(&repo, &store, plan.current(&snap));
        assert_eq!(repo.read("h.txt").unwrap(), "h\n", "a mixed reset keeps the files");
    }

    /// With rerere on, git would replay a remembered resolution and the
    /// conflicts would not be the ones the plan answers.
    #[test]
    fn a_remembered_resolution_does_not_override_the_plans_answer() {
        let Some(repo) = diverged() else { return };
        repo.git(&["config", "rerere.enabled", "true"]).unwrap();
        repo.git(&["config", "rerere.autoUpdate", "true"]).unwrap();
        let _ = repo.git(&["merge", "feature"]);
        repo.write("f.txt", "a\nremembered\nc\n");
        repo.git(&["add", "f.txt"]).unwrap();
        repo.git(&["commit", "-q", "--no-edit"]).unwrap();
        repo.git(&["reset", "-q", "--hard", "HEAD~1"]).unwrap();

        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());
        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::Merge { from: Target::Branch("feature".into()) }).unwrap();
        let result = Entry { mode: MODE_FILE, blob: Blob::Text(b"a\nplanned\nc\n".to_vec().into()) };
        plan.push(&store, &snap, Step::Resolve { path: "f.txt".into(), result: Some(result) }).unwrap();
        plan.push(&store, &snap, Step::Continue).unwrap();

        let outcome = apply(&repo, &snap, &store, &plan, "0010");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_eq!(repo.read("f.txt").unwrap(), "a\nplanned\nc\n");
        assert_agrees(&repo, &store, plan.current(&snap));
    }

    #[test]
    fn a_repository_changed_since_loading_is_refused_before_anything_is_written() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("a.txt", "a\n", "base");
        repo.write("a.txt", "b\n");
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());
        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::Stage { path: "a.txt".into(), scope: Scope::File }).unwrap();

        repo.write("a.txt", "changed behind our back\n");
        let ops = derive::derive(&store, &snap, plan.steps(), plan.states()).unwrap();
        let err = run(repo.path(), &snap, &ops, "0005").unwrap_err();
        assert!(err.contains("a.txt changed on disk"), "{err}");
        assert!(backups(repo.path()).is_empty(), "nothing was written");
    }

    #[test]
    fn undo_puts_branches_index_files_and_stashes_back() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("a.txt", "a\n", "base");
        repo.write("other.txt", "x\n");
        repo.git(&["stash", "-u"]).unwrap();
        repo.write("a.txt", "staged\n");
        repo.git(&["add", "a.txt"]).unwrap();
        repo.write("a.txt", "staged then edited\n");
        repo.write("scratch.txt", "untracked\n");
        let before_status = git(repo.path(), &["status", "--porcelain"]).unwrap();
        let before_head = git(repo.path(), &["rev-parse", "HEAD"]).unwrap();
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());

        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::Stage { path: "a.txt".into(), scope: Scope::File }).unwrap();
        plan.push(&store, &snap, Step::Commit { message: "c".into(), amend: false }).unwrap();
        plan.push(&store, &snap, Step::Discard { path: "scratch.txt".into(), scope: Scope::File }).unwrap();
        plan.push(&store, &snap, Step::StashDrop { index: 0 }).unwrap();
        let outcome = apply(&repo, &snap, &store, &plan, "0006");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_eq!(backups(repo.path()), vec!["0006".to_string()]);

        undo(repo.path(), "0006").unwrap();
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]).unwrap(), before_head);
        assert_eq!(git(repo.path(), &["status", "--porcelain"]).unwrap(), before_status);
        assert_eq!(repo.read("a.txt").unwrap(), "staged then edited\n");
        assert_eq!(repo.read("scratch.txt").unwrap(), "untracked\n");
        assert_eq!(git(repo.path(), &["stash", "list"]).unwrap().lines().count(), 1);
        assert!(backups(repo.path()).is_empty());
    }
}
