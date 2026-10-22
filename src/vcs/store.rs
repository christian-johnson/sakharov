//! Reading and writing git content for the projection.
//!
//! The projection reads blobs and trees through here, and caches what it
//! reads.  The only writes are **unreferenced objects** (blobs, trees,
//! commits) needed to ask `git merge-tree` about a tree that exists only in
//! the plan; no ref, index or work-tree file is ever touched.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use super::{
    load::{git, git_command, parse_ls_tree},
    Blob, Conflict, Entry, FileState, Index, Mark, Oid, Repo, Tree,
};

/// The outcome of a three-way merge of trees.
pub struct Merged {
    /// The merged tree; conflicted paths hold git's marker text.
    pub tree: Oid,
    /// Each conflicted path's stages.
    pub conflicts: BTreeMap<String, Conflict>,
}

/// One path that differs between two trees.
pub type TreeChange = (String, Option<Entry>, Option<Entry>);

pub struct Store {
    root: PathBuf,
    blobs: RefCell<HashMap<Blob, Arc<[u8]>>>,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Store { root, blobs: RefCell::new(HashMap::new()) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bytes of a blob.  A missing work-tree file reads as empty.
    pub fn bytes(&self, blob: &Blob) -> Arc<[u8]> {
        if let Blob::Text(bytes) = blob {
            return bytes.clone();
        }
        if let Some(bytes) = self.blobs.borrow().get(blob) {
            return bytes.clone();
        }
        let bytes: Arc<[u8]> = match blob {
            Blob::Object(oid) => self.cat(oid).unwrap_or_default().into(),
            Blob::File(path) => read_file(&self.root.join(path)).into(),
            Blob::Text(_) => unreachable!("handled above"),
        };
        self.blobs.borrow_mut().insert(blob.clone(), bytes.clone());
        bytes
    }

    /// An entry's content as text (lossy), or `None` for an absent entry.
    pub fn text(&self, entry: Option<&Entry>) -> Option<String> {
        entry.map(|e| String::from_utf8_lossy(&self.bytes(&e.blob)).into_owned())
    }

    fn cat(&self, oid: &Oid) -> Option<Vec<u8>> {
        let out = git_command(&self.root).args(["cat-file", "blob", oid.as_str()]).output().ok()?;
        out.status.success().then_some(out.stdout)
    }

    /// Whether two versions are the same: mode and content.
    pub fn same(&self, a: Option<&Entry>, b: Option<&Entry>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => a.mode == b.mode && (a.blob == b.blob || self.bytes(&a.blob) == self.bytes(&b.blob)),
            _ => false,
        }
    }

    /// How a path differs from HEAD.
    pub fn mark(&self, state: &FileState) -> Mark {
        let index = match &state.index {
            Index::Conflict(_) => return Mark::Conflicted,
            Index::Entry(e) => e.as_ref(),
        };
        if state.head.is_none() && index.is_none() {
            return Mark::Untracked;
        }
        let staged = !self.same(state.head.as_ref(), index);
        let unstaged = !self.same(index, state.work.as_ref());
        match (staged, unstaged) {
            (true, false) => Mark::Staged,
            (false, _) => Mark::Unstaged,
            (true, true) => Mark::Partly,
        }
    }

    /// True when a path's three versions agree, so it can leave the map.
    pub fn is_clean(&self, state: &FileState) -> bool {
        match &state.index {
            Index::Conflict(_) => false,
            Index::Entry(e) => {
                self.same(state.head.as_ref(), e.as_ref()) && self.same(e.as_ref(), state.work.as_ref())
            }
        }
    }

    /// Drop every path whose versions now agree.
    pub fn normalise(&self, repo: &mut Repo) {
        repo.files.retain(|_, state| !self.is_clean(state));
    }

    // -----------------------------------------------------------------------
    // Trees
    // -----------------------------------------------------------------------

    /// The entries `tree` has at `paths` (absent paths are left out).
    pub fn entries(&self, repo: &Repo, tree: &Tree, paths: &[String]) -> Result<BTreeMap<String, Entry>, String> {
        let mut out = BTreeMap::new();
        let mut ask = Vec::new();
        for path in paths {
            match tree.overrides.get(path) {
                Some(Some(e)) => {
                    out.insert(path.clone(), e.clone());
                }
                Some(None) => {}
                None => ask.push(path.as_str()),
            }
        }
        if let (Some(base), false) = (&tree.base, ask.is_empty()) {
            let base = self.real(repo, base)?;
            for chunk in ask.chunks(500) {
                let mut args = vec!["ls-tree", "-r", "-z", base.as_str(), "--"];
                args.extend(chunk.iter().copied());
                out.extend(parse_ls_tree(&git(&self.root, &args)?));
            }
        }
        Ok(out)
    }

    /// Every path that differs between two trees.
    pub fn diff(&self, repo: &Repo, a: &Tree, b: &Tree) -> Result<Vec<TreeChange>, String> {
        let mut changed: BTreeMap<String, ()> = BTreeMap::new();
        if a.base != b.base {
            let (x, y) = (self.tree_ish(repo, a.base.as_ref())?, self.tree_ish(repo, b.base.as_ref())?);
            let out = git(&self.root, &["diff-tree", "-r", "-z", "--no-renames", "--name-only", &x, &y])?;
            changed.extend(out.split('\0').filter(|p| !p.is_empty()).map(|p| (p.to_string(), ())));
        }
        changed.extend(a.overrides.keys().chain(b.overrides.keys()).map(|p| (p.clone(), ())));
        let paths: Vec<String> = changed.into_keys().collect();
        let (left, right) = (self.entries(repo, a, &paths)?, self.entries(repo, b, &paths)?);
        Ok(paths
            .into_iter()
            .filter_map(|p| {
                let (l, r) = (left.get(&p).cloned(), right.get(&p).cloned());
                (!self.same(l.as_ref(), r.as_ref())).then_some((p, l, r))
            })
            .collect())
    }

    /// A tree-ish git can read for `base`, realising planned commits.
    fn tree_ish(&self, repo: &Repo, base: Option<&Oid>) -> Result<String, String> {
        match base {
            Some(oid) => Ok(self.real(repo, oid)?.to_string()),
            None => Ok(self.empty_tree()?.to_string()),
        }
    }

    fn empty_tree(&self) -> Result<Oid, String> {
        self.hash(&["-t", "tree", "--stdin"], &[], false)
    }

    /// `oid` itself if git has it, or the object a planned commit becomes.
    pub fn real(&self, repo: &Repo, oid: &Oid) -> Result<Oid, String> {
        let Some(n) = oid.pending_index() else { return Ok(oid.clone()) };
        let pending = repo.pending.get(n).ok_or("a planned commit went missing")?;
        let tree = self.write_tree(repo, &pending.tree)?;
        let parents = pending
            .parents
            .iter()
            .map(|p| self.real(repo, p))
            .collect::<Result<Vec<_>, _>>()?;
        self.write_commit(&tree, &parents, &pending.message)
    }

    // -----------------------------------------------------------------------
    // Writing unreferenced objects
    // -----------------------------------------------------------------------

    fn hash(&self, args: &[&str], input: &[u8], write: bool) -> Result<Oid, String> {
        let mut cmd = git_command(&self.root);
        cmd.arg("hash-object");
        if write {
            cmd.arg("-w");
        }
        let mut child = cmd
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run git: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("no stdin")?
            .write_all(input)
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(Oid::new(String::from_utf8_lossy(&out.stdout).trim()))
    }

    /// The object id of a blob, writing it to the object store.
    pub fn write_blob(&self, blob: &Blob) -> Result<Oid, String> {
        match blob {
            Blob::Object(oid) => Ok(oid.clone()),
            _ => self.hash(&["--no-filters", "--stdin"], &self.bytes(blob), true),
        }
    }

    /// Build `tree` as a real tree object, through a scratch index file.
    pub fn write_tree(&self, repo: &Repo, tree: &Tree) -> Result<Oid, String> {
        if tree.overrides.is_empty() {
            if let Some(base) = &tree.base {
                return Ok(Oid::new(git(&self.root, &["rev-parse", &format!("{}^{{tree}}", self.real(repo, base)?)])?.trim()));
            }
            return self.empty_tree();
        }
        let scratch = tempfile_path("index");
        let with_index = |args: &[&str], input: Option<&str>| -> Result<String, String> {
            let mut cmd = git_command(&self.root);
            cmd.env("GIT_INDEX_FILE", &scratch).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
            cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
            let mut child = cmd.spawn().map_err(|e| format!("could not run git: {e}"))?;
            if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
                stdin.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            }
            let out = child.wait_with_output().map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let result = (|| {
            match &tree.base {
                Some(base) => with_index(&["read-tree", self.real(repo, base)?.as_str()], None)?,
                None => with_index(&["read-tree", "--empty"], None)?,
            };
            let mut info = String::new();
            for (path, entry) in &tree.overrides {
                match entry {
                    Some(e) => info.push_str(&format!("{:06o} {}\t{path}\0", e.mode, self.write_blob(&e.blob)?)),
                    None => info.push_str(&format!("0 {}\t{path}\0", "0".repeat(40))),
                }
            }
            with_index(&["update-index", "-z", "--index-info"], Some(&info))?;
            Ok(Oid::new(with_index(&["write-tree"], None)?.trim()))
        })();
        let _ = std::fs::remove_file(&scratch);
        result
    }

    pub fn write_commit(&self, tree: &Oid, parents: &[Oid], message: &str) -> Result<Oid, String> {
        let mut args = vec!["commit-tree".to_string(), tree.to_string(), "-m".into(), message.to_string()];
        for p in parents {
            args.push("-p".into());
            args.push(p.to_string());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        Ok(Oid::new(git(&self.root, &args)?.trim()))
    }

    /// Three-way merge `ours` and `theirs`, with an explicit base or git's own.
    pub fn merge(&self, base: Option<&Oid>, ours: &Oid, theirs: &Oid) -> Result<Merged, String> {
        let mut cmd = git_command(&self.root);
        cmd.args(["merge-tree", "--write-tree", "-z", "--no-messages"]);
        if let Some(base) = base {
            cmd.arg(format!("--merge-base={base}"));
        }
        let out = cmd.arg(ours.as_str()).arg(theirs.as_str()).output().map_err(|e| e.to_string())?;
        // 0 is clean, 1 is conflicted; anything else is a real failure.
        if out.status.code().unwrap_or(2) > 1 {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        parse_merge(&String::from_utf8_lossy(&out.stdout))
    }

}

/// `merge-tree -z --no-messages` output: the tree, then `mode oid stage\tpath`
/// records for conflicted paths.
pub fn parse_merge(out: &str) -> Result<Merged, String> {
    let mut records = out.split('\0');
    let tree = Oid::new(records.next().ok_or("merge-tree printed nothing")?.trim());
    let mut conflicts: BTreeMap<String, Conflict> = BTreeMap::new();
    for record in records.filter(|r| !r.is_empty()) {
        let Some((meta, path)) = record.split_once('\t') else { continue };
        let mut parts = meta.split(' ');
        let (Some(mode), Some(oid), Some(stage)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Ok(mode) = u32::from_str_radix(mode, 8) else { continue };
        let entry = Some(Entry::object(mode, Oid::new(oid)));
        let slot = conflicts
            .entry(path.to_string())
            .or_insert(Conflict { base: None, left: None, right: None });
        match stage {
            "1" => slot.base = entry,
            "2" => slot.left = entry,
            "3" => slot.right = entry,
            _ => {}
        }
    }
    Ok(Merged { tree, conflicts })
}

fn read_file(path: &Path) -> Vec<u8> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::read_link(path)
            .map(|t| t.to_string_lossy().into_owned().into_bytes())
            .unwrap_or_default(),
        _ => std::fs::read(path).unwrap_or_default(),
    }
}

/// A unique path under the system temp directory.
fn tempfile_path(what: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("sv-{what}-{}-{n}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{load, testrepo::TestRepo, MODE_FILE};

    #[test]
    fn merge_tree_output_becomes_conflict_stages() {
        let out = ["tree1", "100644 b 1\tf", "100644 l 2\tf", "100644 r 3\tf", "100644 l2 2\tnew", "100644 r2 3\tnew", "", ""]
            .join("\0");
        let merged = parse_merge(&out).unwrap();
        assert_eq!(merged.tree, Oid::new("tree1"));
        assert_eq!(merged.conflicts.len(), 2);
        assert!(merged.conflicts["new"].base.is_none(), "an add/add has no base");
        assert_eq!(merged.conflicts["f"].right, Some(Entry::object(MODE_FILE, Oid::new("r"))));
    }

    #[test]
    fn a_planned_tree_can_be_written_diffed_and_merged() {
        let Some(repo) = TestRepo::new() else { return };
        repo.commit("a.txt", "a\nb\nc\n", "base");
        let snap = load::read(repo.path()).unwrap();
        let store = Store::new(repo.root.clone());

        let mut tree = snap.head_tree();
        tree.overrides.insert("a.txt".into(), Some(Entry::text(MODE_FILE, b"a\nX\nc\n".to_vec())));
        tree.overrides.insert("b.txt".into(), Some(Entry::text(MODE_FILE, b"new\n".to_vec())));

        let changes = store.diff(&snap, &snap.head_tree(), &tree).unwrap();
        assert_eq!(changes.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["a.txt", "b.txt"]);

        let written = store.write_tree(&snap, &tree).unwrap();
        let listing = git(repo.path(), &["ls-tree", "--name-only", written.as_str()]).unwrap();
        assert_eq!(listing, "a.txt\nb.txt\n");
        assert!(git(repo.path(), &["status", "--porcelain"]).unwrap().is_empty(), "no index or file was touched");

        let head = snap.head.commit.clone().unwrap();
        let ours = store.write_commit(&written, std::slice::from_ref(&head), "ours").unwrap();
        let clean = store.merge(None, &ours, &head).unwrap();
        assert!(clean.conflicts.is_empty());
    }
}
