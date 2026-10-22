//! The plan: an ordered stack of steps, and what the repository looks like
//! after each one.
//!
//! [`apply_step`] is the projection: it takes a [`Repo`] and returns the repo
//! that step would leave behind, or says why git would refuse it.  It never
//! writes anything but unreferenced objects (see [`super::store`]).

use std::collections::BTreeMap;

use super::{
    diff::{self, ChangeKey},
    load::git,
    store::{Merged, Store},
    Entry, FileState, Head, InProgress, Index, Mark, Oid, PendingCommit, Repo, StashEntry, Target, Tree,
};
use crate::conflict::Operation;

/// What part of a path a working-tree step acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The whole file.
    File,
    /// Every changed path under a directory.
    Dir,
    /// One change in a text file.
    Change(ChangeKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    /// Move HEAD; keep the index and the work tree.
    Soft,
    /// Move HEAD and reset the index; keep the work tree.
    Mixed,
}

/// One planned action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Stage { path: String, scope: Scope },
    Unstage { path: String, scope: Scope },
    /// Throw a change away: index and work tree go back to HEAD.
    Discard { path: String, scope: Scope },
    /// Settle a conflicted path (`None` = the file is deleted).
    Resolve { path: String, result: Option<Entry> },
    Commit { message: String, amend: bool },
    Switch { branch: String },
    /// Create a branch at HEAD and switch to it.
    CreateBranch { name: String },
    DeleteBranch { name: String },
    RenameBranch { from: String, to: String },
    Tag { name: String, at: Target },
    Merge { from: Target },
    Rebase { onto: Target },
    CherryPick { commit: Oid },
    Revert { commit: Oid },
    Reset { to: Target, mode: ResetMode },
    StashPush { message: String },
    StashPop { index: usize },
    StashApply { index: usize },
    StashDrop { index: usize },
    /// Finish the operation in progress once its conflicts are resolved.
    Continue,
    /// Back out of the operation in progress.
    Abort,
}

impl Step {
    /// How the step reads in the plan panel.
    pub fn label(&self) -> String {
        let scoped = |verb: &str, path: &str, scope: &Scope| match scope {
            Scope::File => format!("{verb} {path}"),
            Scope::Dir => format!("{verb} {path}/"),
            Scope::Change(k) => format!("{verb} a change in {path} (line {})", k.old.start + 1),
        };
        match self {
            Step::Stage { path, scope } => scoped("stage", path, scope),
            Step::Unstage { path, scope } => scoped("unstage", path, scope),
            Step::Discard { path, scope } => scoped("discard", path, scope),
            Step::Resolve { path, result: Some(_) } => format!("resolve {path}"),
            Step::Resolve { path, result: None } => format!("resolve {path} (delete it)"),
            Step::Commit { message, amend } => {
                let verb = if *amend { "amend" } else { "commit" };
                format!("{verb} \"{}\"", message.lines().next().unwrap_or(""))
            }
            Step::Switch { branch } => format!("switch to {branch}"),
            Step::CreateBranch { name } => format!("new branch {name}"),
            Step::DeleteBranch { name } => format!("delete branch {name}"),
            Step::RenameBranch { from, to } => format!("rename {from} to {to}"),
            Step::Tag { name, at } => format!("tag {} as {name}", at.name()),
            Step::Merge { from } => format!("merge {}", from.name()),
            Step::Rebase { onto } => format!("rebase onto {}", onto.name()),
            Step::CherryPick { commit } => format!("cherry-pick {}", commit.short()),
            Step::Revert { commit } => format!("revert {}", commit.short()),
            Step::Reset { to, mode } => {
                let mode = if *mode == ResetMode::Soft { "soft" } else { "mixed" };
                format!("reset ({mode}) to {}", to.name())
            }
            Step::StashPush { message } if message.is_empty() => "stash changes".to_string(),
            Step::StashPush { message } => format!("stash changes \"{message}\""),
            Step::StashPop { index } => format!("pop stash@{{{index}}}"),
            Step::StashApply { index } => format!("apply stash@{{{index}}}"),
            Step::StashDrop { index } => format!("drop stash@{{{index}}}"),
            Step::Continue => "continue".to_string(),
            Step::Abort => "abort".to_string(),
        }
    }

    /// True for steps that only move content between HEAD, the index and the
    /// work tree.
    pub fn is_working_tree(&self) -> bool {
        matches!(
            self,
            Step::Stage { .. } | Step::Unstage { .. } | Step::Discard { .. } | Step::Resolve { .. }
        )
    }
}

/// The stack of steps, and the repository after each one.
#[derive(Default)]
pub struct Plan {
    steps: Vec<Step>,
    /// `states[i]` is the repository after `steps[i]`.
    states: Vec<Repo>,
}

impl Plan {
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn states(&self) -> &[Repo] {
        &self.states
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// The repository after every step.
    pub fn current<'a>(&'a self, snapshot: &'a Repo) -> &'a Repo {
        self.states.last().unwrap_or(snapshot)
    }

    /// Add a step, if git would allow it at this point.
    pub fn push(&mut self, store: &Store, snapshot: &Repo, step: Step) -> Result<(), String> {
        let next = apply_step(store, self.current(snapshot), &step)?;
        self.steps.push(step);
        self.states.push(next);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Step> {
        self.states.pop();
        self.steps.pop()
    }

    pub fn clear(&mut self) {
        self.steps.clear();
        self.states.clear();
    }

    /// Take out the `i`th step, refusing if a later step depended on it.
    pub fn remove(&mut self, store: &Store, snapshot: &Repo, i: usize) -> Result<Step, String> {
        if i >= self.steps.len() {
            return Err("no such step".to_string());
        }
        let mut repo = if i == 0 { snapshot.clone() } else { self.states[i - 1].clone() };
        let mut states = self.states[..i].to_vec();
        for (n, step) in self.steps.iter().enumerate().skip(i + 1) {
            repo = apply_step(store, &repo, step)
                .map_err(|why| format!("step {} ({}) needs it: {why}", n + 1, step.label()))?;
            states.push(repo.clone());
        }
        self.states = states;
        Ok(self.steps.remove(i))
    }

    /// Re-project every step over a fresh snapshot, dropping the ones that no
    /// longer apply.  Returns what was dropped, and why.
    pub fn replay(&mut self, store: &Store, snapshot: &Repo) -> Vec<(Step, String)> {
        let steps = std::mem::take(&mut self.steps);
        self.states.clear();
        let mut dropped = Vec::new();
        for step in steps {
            if let Err(why) = self.push(store, snapshot, step.clone()) {
                dropped.push((step, why));
            }
        }
        dropped
    }
}

// ---------------------------------------------------------------------------
// The projection
// ---------------------------------------------------------------------------

/// The repository `step` would leave behind.
pub fn apply_step(store: &Store, repo: &Repo, step: &Step) -> Result<Repo, String> {
    let mut next = repo.clone();
    match step {
        Step::Stage { path, scope } => each_path(store, &mut next, path, scope, stage)?,
        Step::Unstage { path, scope } => each_path(store, &mut next, path, scope, unstage)?,
        Step::Discard { path, scope } => each_path(store, &mut next, path, scope, discard)?,
        Step::Resolve { path, result } => resolve(&mut next, path, result.clone())?,
        Step::Commit { message, amend } => commit(store, &mut next, message, *amend)?,
        Step::Switch { branch } => switch(store, &mut next, branch)?,
        Step::CreateBranch { name } => {
            free_name(&next, name)?;
            let at = next.head.commit.clone().ok_or("there is no commit to start a branch from yet")?;
            next.branches.insert(name.clone(), at);
            next.head.branch = Some(name.clone());
        }
        Step::DeleteBranch { name } => {
            if next.head.branch.as_ref() == Some(name) {
                return Err(format!("{name} is checked out — switch to another branch first"));
            }
            next.branches.remove(name).ok_or_else(|| format!("there is no branch {name}"))?;
        }
        Step::RenameBranch { from, to } => {
            free_name(&next, to)?;
            let at = next.branches.remove(from).ok_or_else(|| format!("there is no branch {from}"))?;
            next.branches.insert(to.clone(), at);
            if next.head.branch.as_ref() == Some(from) {
                next.head.branch = Some(to.clone());
            }
        }
        Step::Tag { name, at } => {
            if next.tags.contains_key(name) {
                return Err(format!("there is already a tag {name}"));
            }
            let commit = next.resolve(at).ok_or_else(|| format!("{} does not exist", at.name()))?;
            next.tags.insert(name.clone(), commit);
        }
        Step::Merge { from } => merge(store, &mut next, from)?,
        Step::Rebase { onto } => rebase(store, &mut next, onto)?,
        Step::CherryPick { commit } => pick(store, &mut next, commit, false)?,
        Step::Revert { commit } => pick(store, &mut next, commit, true)?,
        Step::Reset { to, mode } => reset(store, &mut next, to, *mode)?,
        Step::StashPush { message } => stash_push(store, &mut next, message)?,
        Step::StashPop { index } => stash_apply(store, &mut next, *index, true)?,
        Step::StashApply { index } => stash_apply(store, &mut next, *index, false)?,
        Step::StashDrop { index } => {
            if *index >= next.stashes.len() {
                return Err(format!("there is no stash@{{{index}}}"));
            }
            next.stashes.remove(*index);
        }
        Step::Continue => continue_op(store, &mut next)?,
        Step::Abort => abort(store, &mut next)?,
    }
    store.normalise(&mut next);
    Ok(next)
}

fn free_name(repo: &Repo, name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.contains(char::is_whitespace) || name.starts_with('-') {
        return Err(format!("{name:?} is not a usable branch name"));
    }
    if repo.branches.contains_key(name) {
        return Err(format!("there is already a branch {name}"));
    }
    Ok(())
}

type PathOp = fn(&Store, &mut FileState, &str, Option<&ChangeKey>) -> Result<(), String>;

/// Run `op` on one path, or on every changed path under a directory.
fn each_path(store: &Store, repo: &mut Repo, path: &str, scope: &Scope, op: PathOp) -> Result<(), String> {
    let paths: Vec<String> = match scope {
        Scope::Dir => {
            let prefix = format!("{path}/");
            repo.files.keys().filter(|p| p.starts_with(&prefix)).cloned().collect()
        }
        _ => vec![path.to_string()],
    };
    if paths.is_empty() {
        return Err(format!("nothing has changed under {path}/"));
    }
    let key = match scope {
        Scope::Change(k) => Some(k),
        _ => None,
    };
    for p in paths {
        let state = repo.files.get_mut(&p).ok_or_else(|| format!("{p} has no changes"))?;
        if state.index.is_conflict() {
            if matches!(scope, Scope::Dir) {
                continue;
            }
            return Err(format!("{p} is conflicted — resolve it first"));
        }
        op(store, state, &p, key)?;
    }
    Ok(())
}

/// The three versions of a text file, and the change `key` names.
fn texts(store: &Store, state: &FileState, path: &str, key: &ChangeKey) -> Result<(String, String, String, diff::Change), String> {
    let index = state.index.entry();
    for e in [state.head.as_ref(), index, state.work.as_ref()].into_iter().flatten() {
        if diff::is_binary(&store.bytes(&e.blob)) {
            return Err(format!("{path} is binary — stage it whole"));
        }
    }
    let head = store.text(state.head.as_ref()).unwrap_or_default();
    let index = store.text(index).unwrap_or_default();
    let work = store.text(state.work.as_ref()).unwrap_or_default();
    let change = diff::find(&diff::changes(&head, &work), key)
        .ok_or_else(|| format!("that change in {path} is no longer there"))?;
    Ok((head, index, work, change))
}

fn mode_of(state: &FileState) -> u32 {
    [state.work.as_ref(), state.index.entry(), state.head.as_ref()]
        .into_iter()
        .flatten()
        .map(|e| e.mode)
        .next()
        .unwrap_or(super::MODE_FILE)
}

/// An entry for `text`, or no entry when it would be an empty new file that
/// HEAD does not have either.
fn text_entry(mode: u32, text: String, head: Option<&Entry>) -> Option<Entry> {
    (head.is_some() || !text.is_empty()).then(|| Entry::text(mode, text.into_bytes()))
}

fn stage(store: &Store, state: &mut FileState, path: &str, key: Option<&ChangeKey>) -> Result<(), String> {
    match key {
        None => state.index = Index::Entry(state.work.clone()),
        Some(key) => {
            let (head, index, _, change) = texts(store, state, path, key)?;
            let text = diff::restage(&head, &index, &change, true);
            state.index = Index::Entry(Some(Entry::text(mode_of(state), text.into_bytes())));
        }
    }
    Ok(())
}

fn unstage(store: &Store, state: &mut FileState, path: &str, key: Option<&ChangeKey>) -> Result<(), String> {
    match key {
        None => state.index = Index::Entry(state.head.clone()),
        Some(key) => {
            let (head, index, _, change) = texts(store, state, path, key)?;
            let text = diff::restage(&head, &index, &change, false);
            state.index = Index::Entry(text_entry(mode_of(state), text, state.head.as_ref()));
        }
    }
    Ok(())
}

fn discard(store: &Store, state: &mut FileState, path: &str, key: Option<&ChangeKey>) -> Result<(), String> {
    match key {
        None => {
            state.index = Index::Entry(state.head.clone());
            state.work = state.head.clone();
        }
        Some(key) => {
            let (head, index, work, change) = texts(store, state, path, key)?;
            let mode = mode_of(state);
            state.index = Index::Entry(text_entry(mode, diff::restage(&head, &index, &change, false), state.head.as_ref()));
            state.work = text_entry(mode, diff::revert(&head, &work, &change), state.head.as_ref());
        }
    }
    Ok(())
}

fn resolve(repo: &mut Repo, path: &str, result: Option<Entry>) -> Result<(), String> {
    let state = repo.files.get_mut(path).ok_or_else(|| format!("{path} is not conflicted"))?;
    if !state.index.is_conflict() {
        return Err(format!("{path} is not conflicted"));
    }
    state.index = Index::Entry(result.clone());
    state.work = result;
    let settled = !repo.files.values().any(|f| f.index.is_conflict());
    if settled && repo.operation.as_ref().is_some_and(|o| o.kind == Operation::Unknown) {
        repo.operation = None;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Commits
// ---------------------------------------------------------------------------

fn no_conflicts(repo: &Repo) -> Result<(), String> {
    match repo.files.iter().find(|(_, f)| f.index.is_conflict()) {
        Some((path, _)) => Err(format!("{path} is still conflicted")),
        None => Ok(()),
    }
}

fn not_mid_operation(repo: &Repo) -> Result<(), String> {
    match &repo.operation {
        Some(op) => Err(format!("a {} is in progress — continue or abort it first", op.kind.describe())),
        None => Ok(()),
    }
}

/// Refuse when anything tracked has uncommitted changes.
fn clean_tree(store: &Store, repo: &Repo, doing: &str) -> Result<(), String> {
    match repo.files.iter().find(|(_, f)| store.mark(f) != Mark::Untracked) {
        Some((path, _)) => Err(format!("{path} has uncommitted changes — commit or stash them before {doing}")),
        None => Ok(()),
    }
}

/// HEAD's tree with the index laid over it.
fn index_tree(repo: &Repo) -> Tree {
    let mut tree = repo.head_tree();
    for (path, state) in &repo.files {
        if let Index::Entry(e) = &state.index {
            if e != &state.head {
                tree.overrides.insert(path.clone(), e.clone());
            }
        }
    }
    tree
}

/// Record a new commit at HEAD whose tree is `tree`, and move HEAD (and its
/// branch) onto it.
fn land(repo: &mut Repo, parents: Vec<Oid>, message: &str, tree: Tree) -> Oid {
    repo.pending.push(PendingCommit { parents, message: message.to_string(), tree });
    let id = Oid::pending(repo.pending.len() - 1);
    if let Some(branch) = &repo.head.branch {
        repo.branches.insert(branch.clone(), id.clone());
    }
    repo.head.commit = Some(id.clone());
    id
}

/// After committing the index: HEAD's version of every path is its index
/// version.
fn index_becomes_head(repo: &mut Repo) {
    for state in repo.files.values_mut() {
        if let Index::Entry(e) = &state.index {
            state.head = e.clone();
        }
    }
}

fn parents_of(store: &Store, repo: &Repo, commit: &Oid) -> Result<Vec<Oid>, String> {
    if let Some(p) = commit.pending_index().and_then(|i| repo.pending.get(i)) {
        return Ok(p.parents.clone());
    }
    let out = git(store.root(), &["rev-list", "--parents", "-n", "1", commit.as_str()])?;
    Ok(out.split_whitespace().skip(1).map(Oid::new).collect())
}

fn has_staged(store: &Store, repo: &Repo) -> bool {
    repo.files.values().any(|f| matches!(store.mark(f), Mark::Staged | Mark::Partly))
}

fn commit(store: &Store, repo: &mut Repo, message: &str, amend: bool) -> Result<(), String> {
    not_mid_operation(repo)?;
    no_conflicts(repo)?;
    if message.trim().is_empty() {
        return Err("the commit message is empty".to_string());
    }
    let parents = match (&repo.head.commit, amend) {
        (None, true) => return Err("there is no commit to amend".to_string()),
        (Some(head), true) => parents_of(store, repo, head)?,
        (Some(head), false) => vec![head.clone()],
        (None, false) => Vec::new(),
    };
    if !amend && !has_staged(store, repo) {
        return Err("nothing is staged".to_string());
    }
    let tree = index_tree(repo);
    land(repo, parents, message, tree);
    index_becomes_head(repo);
    Ok(())
}

// ---------------------------------------------------------------------------
// Branches
// ---------------------------------------------------------------------------

fn switch(store: &Store, repo: &mut Repo, branch: &str) -> Result<(), String> {
    not_mid_operation(repo)?;
    if repo.head.branch.as_deref() == Some(branch) {
        return Err(format!("already on {branch}"));
    }
    let target = repo.branches.get(branch).cloned().ok_or_else(|| format!("there is no branch {branch}"))?;
    let from = repo.head_tree();
    let to = repo.tree_of(Some(&target));
    for (path, _, new) in store.diff(repo, &from, &to)? {
        if let Some(state) = repo.files.get(&path) {
            if store.mark(state) == Mark::Untracked && new.is_none() {
                continue;
            }
            return Err(format!("your changes to {path} would be overwritten — commit or stash them first"));
        }
    }
    repo.head = Head { branch: Some(branch.to_string()), commit: Some(target) };
    Ok(())
}

// ---------------------------------------------------------------------------
// Merging and replaying
// ---------------------------------------------------------------------------

fn is_ancestor(store: &Store, a: &Oid, b: &Oid) -> bool {
    git(store.root(), &["merge-base", "--is-ancestor", a.as_str(), b.as_str()]).is_ok()
}

fn first_parent(store: &Store, commit: &Oid) -> Result<Oid, String> {
    let out = git(store.root(), &["rev-list", "--parents", "-n", "1", commit.as_str()])?;
    let parents: Vec<&str> = out.split_whitespace().skip(1).collect();
    match parents.as_slice() {
        [one] => Ok(Oid::new(*one)),
        [] => Err(format!("{} is a root commit", commit.short())),
        _ => Err(format!("{} is a merge commit and cannot be replayed", commit.short())),
    }
}

fn message_of(store: &Store, repo: &Repo, commit: &Oid) -> String {
    if let Some(p) = commit.pending_index().and_then(|i| repo.pending.get(i)) {
        return p.message.clone();
    }
    git(store.root(), &["log", "-1", "--format=%B", commit.as_str()])
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

/// The paths a merge result differs from HEAD in, as the index and work tree
/// hold them after git stops: clean merges staged, conflicts unmerged.
fn stop(store: &Store, repo: &mut Repo, merged: &Merged, op: InProgress) -> Result<(), String> {
    let result = Tree::of(Some(merged.tree.clone()));
    for (path, old, new) in store.diff(repo, &repo.head_tree(), &result)? {
        let index = match merged.conflicts.get(&path) {
            Some(c) => Index::Conflict(Box::new(c.clone())),
            None => Index::Entry(new.clone()),
        };
        repo.files.insert(path, FileState { head: old, index, work: new });
    }
    repo.operation = Some(op);
    Ok(())
}

fn merge(store: &Store, repo: &mut Repo, from: &Target) -> Result<(), String> {
    not_mid_operation(repo)?;
    clean_tree(store, repo, "merging")?;
    let head = repo.head.commit.clone().ok_or("there is nothing to merge into yet")?;
    let theirs = repo.resolve(from).ok_or_else(|| format!("{} does not exist", from.name()))?;
    let (ours_real, theirs_real) = (store.real(repo, &head)?, store.real(repo, &theirs)?);
    if is_ancestor(store, &theirs_real, &ours_real) {
        return Err(format!("{} is already merged", from.name()));
    }
    if is_ancestor(store, &ours_real, &theirs_real) {
        // A fast-forward: the branch moves, nothing is created.
        if let Some(branch) = &repo.head.branch {
            repo.branches.insert(branch.clone(), theirs.clone());
        }
        repo.head.commit = Some(theirs);
        return Ok(());
    }
    let message = match from {
        Target::Branch(b) => format!("Merge branch '{b}'"),
        Target::Commit(c) => format!("Merge commit '{}'", c.short()),
    };
    let merged = store.merge(None, &ours_real, &theirs_real)?;
    if merged.conflicts.is_empty() {
        land(repo, vec![head, theirs], &message, Tree::of(Some(merged.tree)));
        return Ok(());
    }
    let op = InProgress {
        kind: Operation::Merge,
        commit: Some(theirs),
        label: from.name(),
        message,
        rest: Vec::new(),
        branch: None,
        onto: None,
    };
    stop(store, repo, &merged, op)
}

/// Replay `commits` one at a time on top of HEAD, stopping at the first
/// conflict.  `kind` is what the stop is reported as.
fn replay(store: &Store, repo: &mut Repo, commits: &[Oid], kind: Operation, revert: bool) -> Result<(), String> {
    for (i, commit) in commits.iter().enumerate() {
        let head = repo.head.commit.clone().ok_or("there is no commit to replay onto")?;
        let parent = first_parent(store, commit)?;
        let (base, theirs) = if revert { (commit.clone(), parent) } else { (parent, commit.clone()) };
        let ours = store.real(repo, &head)?;
        let merged = store.merge(Some(&base), &ours, &theirs)?;
        let original = message_of(store, repo, commit);
        let subject = original.lines().next().unwrap_or("").to_string();
        let message = if revert {
            format!("Revert \"{subject}\"\n\nThis reverts commit {commit}.")
        } else {
            original
        };
        if !merged.conflicts.is_empty() {
            let op = InProgress {
                kind,
                commit: Some(commit.clone()),
                label: subject,
                message,
                rest: commits[i + 1..].to_vec(),
                branch: repo.operation.as_ref().and_then(|o| o.branch.clone()),
                onto: repo.operation.as_ref().and_then(|o| o.onto.clone()),
            };
            return stop(store, repo, &merged, op);
        }
        let ours_tree = git(store.root(), &["rev-parse", &format!("{ours}^{{tree}}")])?;
        if ours_tree.trim() == merged.tree.as_str() {
            // git drops a commit that would change nothing.
            continue;
        }
        land(repo, vec![head], &message, Tree::of(Some(merged.tree)));
    }
    Ok(())
}

fn pick(store: &Store, repo: &mut Repo, commit: &Oid, revert: bool) -> Result<(), String> {
    not_mid_operation(repo)?;
    clean_tree(store, repo, if revert { "reverting" } else { "cherry-picking" })?;
    if commit.is_pending() {
        return Err("that commit does not exist yet".to_string());
    }
    let before = repo.pending.len();
    let kind = if revert { Operation::Revert } else { Operation::CherryPick };
    replay(store, repo, std::slice::from_ref(commit), kind, revert)?;
    if repo.operation.is_none() && repo.pending.len() == before {
        return Err(format!("{} would change nothing here", commit.short()));
    }
    Ok(())
}

fn rebase(store: &Store, repo: &mut Repo, onto: &Target) -> Result<(), String> {
    not_mid_operation(repo)?;
    clean_tree(store, repo, "rebasing")?;
    let branch = repo.head.branch.clone().ok_or("HEAD is detached — switch to a branch to rebase it")?;
    let head = repo.head.commit.clone().ok_or("there is nothing to rebase yet")?;
    let target = repo.resolve(onto).ok_or_else(|| format!("{} does not exist", onto.name()))?;
    let (head_real, target_real) = (store.real(repo, &head)?, store.real(repo, &target)?);
    if is_ancestor(store, &target_real, &head_real) {
        return Err(format!("{branch} is already based on {}", onto.name()));
    }
    let range = format!("{target_real}..{head_real}");
    let commits: Vec<Oid> = git(store.root(), &["rev-list", "--reverse", "--no-merges", &range])?
        .lines()
        .map(Oid::new)
        .collect();
    let original = repo.branches.get(&branch).cloned();
    repo.head = Head { branch: None, commit: Some(target.clone()) };
    repo.operation = Some(InProgress {
        kind: Operation::Rebase,
        commit: None,
        label: String::new(),
        message: String::new(),
        rest: Vec::new(),
        branch: Some(branch.clone()),
        onto: Some(onto.name()),
    });
    // The branch keeps its old commit until the rebase finishes.
    if let Some(original) = original {
        repo.branches.insert(branch.clone(), original);
    }
    replay(store, repo, &commits, Operation::Rebase, false)?;
    finish_rebase(repo);
    Ok(())
}

/// When a rebase has no more conflicts to stop at: the branch moves to where
/// the replay ended and HEAD goes back onto it.
fn finish_rebase(repo: &mut Repo) {
    let Some(op) = &repo.operation else { return };
    if op.kind != Operation::Rebase || op.commit.is_some() {
        return;
    }
    if let (Some(branch), Some(tip)) = (op.branch.clone(), repo.head.commit.clone()) {
        repo.branches.insert(branch.clone(), tip.clone());
        repo.head = Head { branch: Some(branch), commit: Some(tip) };
    }
    repo.operation = None;
}

fn continue_op(store: &Store, repo: &mut Repo) -> Result<(), String> {
    let op = repo.operation.clone().ok_or("nothing is in progress")?;
    no_conflicts(repo)?;
    let head = repo.head.commit.clone().ok_or("there is no commit to continue from")?;
    match op.kind {
        Operation::Merge => {
            let theirs = op.commit.clone().ok_or("the merge does not say what it is merging")?;
            let tree = index_tree(repo);
            land(repo, vec![head, theirs], &op.message, tree);
            index_becomes_head(repo);
            repo.operation = None;
        }
        Operation::CherryPick | Operation::Revert | Operation::Rebase => {
            let tree = index_tree(repo);
            let changed = has_staged(store, repo);
            if changed {
                land(repo, vec![head], &op.message, tree);
                index_becomes_head(repo);
            } else if op.kind != Operation::Rebase {
                return Err("the result is empty — abort instead, or change something".to_string());
            }
            repo.operation = Some(InProgress { commit: None, ..op.clone() });
            replay(store, repo, &op.rest, op.kind, op.kind == Operation::Revert)?;
            if repo.operation.as_ref().is_some_and(|o| o.commit.is_none()) {
                if op.kind == Operation::Rebase {
                    finish_rebase(repo);
                } else {
                    repo.operation = None;
                }
            }
        }
        Operation::Unknown => repo.operation = None,
    }
    Ok(())
}

fn abort(store: &Store, repo: &mut Repo) -> Result<(), String> {
    let op = repo.operation.take().ok_or("nothing is in progress")?;
    // What survives an abort is only what git never touched.
    repo.files.retain(|_, f| store.mark(f) == Mark::Untracked);
    if op.kind == Operation::Rebase {
        if let Some(branch) = op.branch {
            let tip = repo.branches.get(&branch).cloned();
            repo.head = Head { branch: Some(branch), commit: tip };
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reset and stash
// ---------------------------------------------------------------------------

fn reset(store: &Store, repo: &mut Repo, to: &Target, mode: ResetMode) -> Result<(), String> {
    not_mid_operation(repo)?;
    no_conflicts(repo)?;
    let target = repo.resolve(to).ok_or_else(|| format!("{} does not exist", to.name()))?;
    let (from, onto) = (repo.head_tree(), repo.tree_of(Some(&target)));
    for (path, old, new) in store.diff(repo, &from, &onto)? {
        let state = repo
            .files
            .entry(path)
            .or_insert_with(|| FileState { head: old.clone(), index: Index::Entry(old.clone()), work: old });
        state.head = new;
    }
    if mode == ResetMode::Mixed {
        for state in repo.files.values_mut() {
            state.index = Index::Entry(state.head.clone());
        }
    }
    if let Some(branch) = &repo.head.branch {
        repo.branches.insert(branch.clone(), target.clone());
    }
    repo.head.commit = Some(target);
    Ok(())
}

fn stash_push(store: &Store, repo: &mut Repo, message: &str) -> Result<(), String> {
    not_mid_operation(repo)?;
    no_conflicts(repo)?;
    let saved: BTreeMap<String, FileState> = repo
        .files
        .iter()
        .filter(|(_, f)| store.mark(f) != Mark::Untracked)
        .map(|(p, f)| (p.clone(), f.clone()))
        .collect();
    if saved.is_empty() {
        return Err("there are no local changes to stash".to_string());
    }
    for path in saved.keys() {
        repo.files.remove(path);
    }
    let message = match (&repo.head.branch, message.trim()) {
        (Some(b), "") => format!("WIP on {b}"),
        (Some(b), m) => format!("On {b}: {m}"),
        (None, "") => "WIP".to_string(),
        (None, m) => m.to_string(),
    };
    repo.stashes.insert(0, StashEntry { commit: Oid::new("~stash"), message, saved: Some(saved) });
    Ok(())
}

fn stash_apply(store: &Store, repo: &mut Repo, index: usize, pop: bool) -> Result<(), String> {
    not_mid_operation(repo)?;
    no_conflicts(repo)?;
    let entry = repo.stashes.get(index).cloned().ok_or_else(|| format!("there is no stash@{{{index}}}"))?;
    let touched: Vec<(String, FileState)> = match &entry.saved {
        Some(saved) => {
            let paths: Vec<String> = saved.keys().cloned().collect();
            let now = store.entries(repo, &repo.head_tree(), &paths)?;
            let mut out = Vec::new();
            for (path, state) in saved {
                if !store.same(now.get(path), state.head.as_ref()) {
                    return Err(format!("{path} changed since that stash was made — apply it after reloading"));
                }
                out.push((path.clone(), unstaged_restore(state.head.clone(), state.work.clone())));
            }
            out
        }
        None => {
            let head = repo.head.commit.clone().ok_or("there is no commit to apply a stash to")?;
            let base = first_parent(store, &entry.commit)?;
            let merged = store.merge(Some(&base), &store.real(repo, &head)?, &entry.commit)?;
            if !merged.conflicts.is_empty() {
                let paths: Vec<&str> = merged.conflicts.keys().map(String::as_str).collect();
                return Err(format!("that stash would conflict in {} — apply it after reloading", paths.join(", ")));
            }
            store
                .diff(repo, &repo.head_tree(), &Tree::of(Some(merged.tree)))?
                .into_iter()
                .map(|(path, old, new)| (path, unstaged_restore(old, new)))
                .collect()
        }
    };
    for (path, state) in touched {
        if repo.files.get(&path).is_some_and(|f| store.mark(f) != Mark::Untracked || state.work.is_some()) {
            return Err(format!("your changes to {path} would be overwritten by the stash"));
        }
        repo.files.insert(path, state);
    }
    if pop {
        repo.stashes.remove(index);
    }
    Ok(())
}

/// How `git stash apply` leaves a path: changed but not staged, except that a
/// file the stash added is staged.
fn unstaged_restore(head: Option<Entry>, work: Option<Entry>) -> FileState {
    let index = if head.is_none() { work.clone() } else { head.clone() };
    FileState { head, index: Index::Entry(index), work }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{load, testrepo::TestRepo, Blob};

    fn setup() -> Option<(TestRepo, Store, Repo)> {
        let repo = TestRepo::new()?;
        repo.commit("a.txt", "a\nb\nc\nd\ne\n", "base");
        repo.write("a.txt", "a\nB\nc\nD\ne\n");
        repo.write("new.txt", "new\n");
        let snap = load::read(repo.path()).ok()?;
        let store = Store::new(repo.root.clone());
        Some((repo, store, snap))
    }

    fn change_key(store: &Store, repo: &Repo, path: &str, nth: usize) -> ChangeKey {
        let state = &repo.files[path];
        let head = store.text(state.head.as_ref()).unwrap_or_default();
        let work = store.text(state.work.as_ref()).unwrap_or_default();
        diff::changes(&head, &work)[nth].key()
    }

    #[test]
    fn staging_one_change_then_committing_leaves_the_other_unstaged() {
        let Some((_repo, store, snap)) = setup() else { return };
        let mut plan = Plan::default();
        let key = change_key(&store, &snap, "a.txt", 1);
        plan.push(&store, &snap, Step::Stage { path: "a.txt".into(), scope: Scope::Change(key) }).unwrap();
        assert_eq!(store.mark(&plan.current(&snap).files["a.txt"]), Mark::Partly);

        plan.push(&store, &snap, Step::Commit { message: "one change".into(), amend: false }).unwrap();
        let after = plan.current(&snap);
        assert!(after.head.commit.as_ref().unwrap().is_pending());
        assert_eq!(after.branches["main"], after.head.commit.clone().unwrap(), "the branch moved with HEAD");
        let a = &after.files["a.txt"];
        assert_eq!(store.text(a.head.as_ref()).unwrap(), "a\nb\nc\nD\ne\n");
        assert_eq!(store.mark(a), Mark::Unstaged);
        assert_eq!(store.mark(&after.files["new.txt"]), Mark::Untracked, "untracked stays out of it");
    }

    #[test]
    fn a_commit_with_nothing_staged_is_refused() {
        let Some((_repo, store, snap)) = setup() else { return };
        let err = apply_step(&store, &snap, &Step::Commit { message: "m".into(), amend: false }).unwrap_err();
        assert!(err.contains("nothing is staged"), "{err}");
    }

    #[test]
    fn removing_a_step_another_depends_on_is_refused() {
        let Some((_repo, store, snap)) = setup() else { return };
        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::Stage { path: "new.txt".into(), scope: Scope::File }).unwrap();
        plan.push(&store, &snap, Step::Commit { message: "add".into(), amend: false }).unwrap();
        assert!(plan.remove(&store, &snap, 0).unwrap_err().contains("nothing is staged"));
        assert_eq!(plan.len(), 2, "nothing was removed");
        plan.remove(&store, &snap, 1).unwrap();
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn discarding_an_untracked_file_deletes_it_and_the_path_goes_clean() {
        let Some((_repo, store, snap)) = setup() else { return };
        let next = apply_step(&store, &snap, &Step::Discard { path: "new.txt".into(), scope: Scope::File }).unwrap();
        assert!(!next.files.contains_key("new.txt"));
    }

    #[test]
    fn switching_with_changes_that_would_be_overwritten_is_refused() {
        let Some((repo, store, _)) = setup() else { return };
        repo.git(&["stash", "-u"]).unwrap();
        repo.git(&["checkout", "-q", "-b", "other"]).unwrap();
        repo.commit("a.txt", "other\n", "other");
        repo.git(&["checkout", "-q", "main"]).unwrap();
        repo.write("a.txt", "dirty\n");
        let snap = load::read(repo.path()).unwrap();
        let err = apply_step(&store, &snap, &Step::Switch { branch: "other".into() }).unwrap_err();
        assert!(err.contains("would be overwritten"), "{err}");
    }

    #[test]
    fn a_merge_that_will_conflict_stops_with_the_conflict_in_the_index() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("f.txt", "a\nb\nc\n", "base");
        repo.git(&["checkout", "-q", "-b", "feature"]).unwrap();
        repo.commit("f.txt", "a\nX\nc\n", "feature");
        repo.git(&["checkout", "-q", "main"]).unwrap();
        repo.commit("f.txt", "a\nY\nc\n", "main");
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());

        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::Merge { from: Target::Branch("feature".into()) }).unwrap();
        let stopped = plan.current(&snap);
        assert_eq!(stopped.operation.as_ref().map(|o| o.kind), Some(Operation::Merge));
        assert!(stopped.files["f.txt"].index.is_conflict());
        assert!(apply_step(&store, stopped, &Step::Continue).unwrap_err().contains("conflicted"));

        let resolved = Entry { mode: crate::vcs::MODE_FILE, blob: Blob::Text(b"a\nXY\nc\n".to_vec().into()) };
        plan.push(&store, &snap, Step::Resolve { path: "f.txt".into(), result: Some(resolved) }).unwrap();
        plan.push(&store, &snap, Step::Continue).unwrap();
        let merged = plan.current(&snap);
        assert!(merged.operation.is_none());
        let tip = merged.head.commit.clone().unwrap();
        assert_eq!(merged.pending[tip.pending_index().unwrap()].parents.len(), 2);
        assert!(merged.files.is_empty(), "everything is committed");
    }

    #[test]
    fn a_clean_rebase_moves_the_branch_onto_replayed_commits() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("a.txt", "a\n", "base");
        repo.git(&["checkout", "-q", "-b", "feature"]).unwrap();
        repo.commit("b.txt", "b\n", "feature work");
        repo.git(&["checkout", "-q", "main"]).unwrap();
        repo.commit("c.txt", "c\n", "main work");
        repo.git(&["checkout", "-q", "feature"]).unwrap();
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());

        let next = apply_step(&store, &snap, &Step::Rebase { onto: Target::Branch("main".into()) }).unwrap();
        assert!(next.operation.is_none());
        assert_eq!(next.head.branch.as_deref(), Some("feature"));
        let tip = next.branches["feature"].clone();
        let commit = &next.pending[tip.pending_index().unwrap()];
        assert_eq!(commit.message, "feature work");
        assert_eq!(commit.parents, vec![snap.branches["main"].clone()]);
    }

    #[test]
    fn a_stash_round_trip_restores_the_changes_unstaged() {
        let Some((_repo, store, snap)) = setup() else { return };
        let mut plan = Plan::default();
        plan.push(&store, &snap, Step::StashPush { message: String::new() }).unwrap();
        assert!(!plan.current(&snap).files.contains_key("a.txt"));
        assert_eq!(plan.current(&snap).stashes[0].message, "WIP on main");
        plan.push(&store, &snap, Step::StashPop { index: 0 }).unwrap();
        let back = plan.current(&snap);
        assert_eq!(store.mark(&back.files["a.txt"]), Mark::Unstaged);
        assert!(back.stashes.is_empty());
    }
}
