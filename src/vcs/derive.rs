//! Turning a projected plan into the git commands that make it real.
//!
//! Working-tree steps become **target state**: the index entry and work-tree
//! file each path should end up with, worked out by comparing the projection
//! before and after the step.  Everything else is the git porcelain the step
//! names.  [`Op::describe`] is what the confirmation shows and [`Op::run`] is
//! what runs, so the two cannot disagree.

use std::collections::HashSet;
use std::sync::Arc;

use super::{plan::{ResetMode, Step}, store::Store, Blob, Entry, Index, Oid, Repo, Target};
use crate::conflict::Operation;

/// Content for an index entry.
#[derive(Debug, Clone)]
pub enum Data {
    /// A blob git already has.
    Object(Oid),
    /// Bytes the plan produced.
    Bytes(Arc<[u8]>),
    /// The work-tree file as it is now, staged with `git add` so filters run.
    WorkFile,
}

/// One unmerged index entry: `(path, stage, blob)`.
pub type Stage = (String, u8, Oid);

/// What a git command is expected to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    Success,
    /// Stop with exactly these conflicts, which later steps resolve.
    Conflicts(Vec<Stage>),
}

#[derive(Debug, Clone)]
pub enum Op {
    SetIndex { path: String, entry: Option<(u32, Data)> },
    WriteFile { path: String, entry: Option<(u32, Arc<[u8]>)> },
    Git { args: Vec<String>, expect: Expect },
}

impl Op {
    /// How the op reads in the confirmation list.
    pub fn describe(&self) -> String {
        match self {
            Op::SetIndex { path, entry: None } => format!("git update-index --force-remove {path}"),
            Op::SetIndex { path, entry: Some((_, Data::WorkFile)) } => format!("git add {path}"),
            Op::SetIndex { path, entry: Some(_) } => format!("git update-index --cacheinfo … {path}"),
            Op::WriteFile { path, entry: None } => format!("delete {path}"),
            Op::WriteFile { path, entry: Some(_) } => format!("write {path}"),
            Op::Git { args, expect } => {
                // `-c key=value` pairs are how git is told to stay quiet, not
                // what it is being asked to do.
                let mut shown: Vec<&str> = Vec::new();
                let mut words = args.iter().map(String::as_str);
                while let Some(word) = words.next() {
                    if word == "-c" {
                        words.next();
                    } else {
                        shown.push(word);
                    }
                }
                match expect {
                    Expect::Success => format!("git {}", shown.join(" ")),
                    Expect::Conflicts(c) => {
                        let paths: HashSet<&str> = c.iter().map(|s| s.0.as_str()).collect();
                        format!("git {}   (stops: {} conflicted)", shown.join(" "), paths.len())
                    }
                }
            }
        }
    }
}

/// The ops for every step of a plan, in order.
pub fn derive(store: &Store, snapshot: &Repo, steps: &[Step], states: &[Repo]) -> Result<Vec<Op>, String> {
    let mut ops = Vec::new();
    let mut written: HashSet<String> = HashSet::new();
    let mut prev = snapshot;
    for (step, next) in steps.iter().zip(states) {
        if step.is_working_tree() {
            working_tree(store, prev, next, &mut written, &mut ops);
        } else {
            ops.extend(porcelain(prev, step, next)?);
        }
        prev = next;
    }
    Ok(ops)
}

/// Index and work-tree writes that turn `prev` into `next`.
fn working_tree(store: &Store, prev: &Repo, next: &Repo, written: &mut HashSet<String>, ops: &mut Vec<Op>) {
    let paths: std::collections::BTreeSet<&String> = prev.files.keys().chain(next.files.keys()).collect();
    for path in paths {
        let (before, after) = (prev.files.get(path), next.files.get(path));
        // A path missing on one side is at HEAD there; HEAD does not move here.
        let head = before.or(after).and_then(|f| f.head.as_ref());
        let index = |f: Option<&super::FileState>| match f {
            Some(f) => f.index.clone(),
            None => Index::Entry(head.cloned()),
        };
        let work = |f: Option<&super::FileState>| match f {
            Some(f) => f.work.clone(),
            None => head.cloned(),
        };
        let (work_before, work_after) = (work(before), work(after));
        if !store.same(work_before.as_ref(), work_after.as_ref()) {
            ops.push(Op::WriteFile {
                path: path.clone(),
                entry: work_after.as_ref().map(|e| (e.mode, store.bytes(&e.blob))),
            });
            written.insert(path.clone());
        }
        let (index_before, index_after) = (index(before), index(after));
        let changed = match (&index_before, &index_after) {
            (Index::Entry(a), Index::Entry(b)) => !store.same(a.as_ref(), b.as_ref()),
            _ => true,
        };
        if let (true, Index::Entry(entry)) = (changed, index_after) {
            ops.push(Op::SetIndex { path: path.clone(), entry: entry.map(|e| data(store, path, e, written)) });
        }
    }
}

fn data(store: &Store, path: &str, entry: Entry, written: &HashSet<String>) -> (u32, Data) {
    let data = match &entry.blob {
        Blob::Object(oid) => Data::Object(oid.clone()),
        Blob::File(p) if p == path && !written.contains(path) => Data::WorkFile,
        blob => Data::Bytes(store.bytes(blob)),
    };
    (entry.mode, data)
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

fn target(t: &Target) -> String {
    match t {
        Target::Branch(b) => b.clone(),
        Target::Commit(c) => c.to_string(),
    }
}

/// The conflicts `next` is stopped at, if it is stopped.
fn expect(next: &Repo) -> Expect {
    let mut stages = Vec::new();
    for (path, state) in &next.files {
        if let Index::Conflict(c) = &state.index {
            for (n, entry) in [(1, &c.base), (2, &c.left), (3, &c.right)] {
                if let Some(Entry { blob: Blob::Object(oid), .. }) = entry {
                    stages.push((path.clone(), n, oid.clone()));
                }
            }
        }
    }
    if stages.is_empty() { Expect::Success } else { Expect::Conflicts(stages) }
}

fn porcelain(prev: &Repo, step: &Step, next: &Repo) -> Result<Vec<Op>, String> {
    let git = |parts: Vec<String>| Op::Git { args: parts, expect: Expect::Success };
    // rerere off: a remembered resolution would settle conflicts the plan
    // answers explicitly, and git's conflicts would no longer be the ones
    // the projection predicted.
    let stops = |parts: Vec<String>| {
        let mut args = args(&["-c", "rerere.enabled=false"]);
        args.extend(parts);
        Op::Git { args, expect: expect(next) }
    };
    let op = match step {
        Step::Commit { message, amend } => {
            let mut a = args(&["commit", "-q", "-m", message]);
            if *amend {
                a.insert(1, "--amend".into());
            }
            git(a)
        }
        Step::Switch { branch } => git(args(&["switch", branch])),
        Step::CreateBranch { name } => git(args(&["switch", "-c", name])),
        Step::DeleteBranch { name } => git(args(&["branch", "-D", name])),
        Step::RenameBranch { from, to } => git(args(&["branch", "-m", from, to])),
        Step::Tag { name, at } => git(args(&["tag", name, &target(at)])),
        Step::Merge { from } => stops(args(&["merge", "--no-edit", &target(from)])),
        Step::Rebase { onto } => stops(args(&["-c", "core.editor=true", "rebase", &target(onto)])),
        Step::CherryPick { commit } => stops(args(&["cherry-pick", commit.as_str()])),
        Step::Revert { commit } => stops(args(&["revert", "--no-edit", commit.as_str()])),
        Step::Reset { to, mode } => {
            let flag = if *mode == ResetMode::Soft { "--soft" } else { "--mixed" };
            git(args(&["reset", "-q", flag, &target(to)]))
        }
        Step::StashPush { message } if message.trim().is_empty() => git(args(&["stash", "push"])),
        Step::StashPush { message } => git(args(&["stash", "push", "-m", message.trim()])),
        Step::StashPop { index } => git(args(&["stash", "pop", &format!("stash@{{{index}}}")])),
        Step::StashApply { index } => git(args(&["stash", "apply", &format!("stash@{{{index}}}")])),
        Step::StashDrop { index } => git(args(&["stash", "drop", &format!("stash@{{{index}}}")])),
        Step::Continue => {
            let kind = prev.operation.as_ref().map(|o| o.kind).ok_or("nothing to continue")?;
            match kind {
                Operation::Merge => stops(args(&["commit", "--no-edit"])),
                Operation::Rebase => stops(args(&["-c", "core.editor=true", "rebase", "--continue"])),
                Operation::CherryPick => stops(args(&["-c", "core.editor=true", "cherry-pick", "--continue"])),
                Operation::Revert => stops(args(&["-c", "core.editor=true", "revert", "--continue"])),
                Operation::Unknown => return Ok(Vec::new()),
            }
        }
        Step::Abort => {
            let kind = prev.operation.as_ref().map(|o| o.kind).ok_or("nothing to abort")?;
            match kind {
                Operation::Merge => git(args(&["merge", "--abort"])),
                Operation::Rebase => git(args(&["rebase", "--abort"])),
                Operation::CherryPick => git(args(&["cherry-pick", "--abort"])),
                Operation::Revert => git(args(&["revert", "--abort"])),
                Operation::Unknown => git(args(&["reset", "-q", "--merge"])),
            }
        }
        Step::Stage { .. } | Step::Unstage { .. } | Step::Discard { .. } | Step::Resolve { .. } => {
            unreachable!("working-tree steps are derived from state")
        }
    };
    Ok(vec![op])
}
