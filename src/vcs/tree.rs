//! The changes tree: every changed path as a collapsible directory tree.

use std::collections::{BTreeMap, HashSet};

use super::{store::Store, Mark, Repo};

/// One row of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub depth: usize,
    /// The last path component.
    pub name: String,
    /// Repository-relative path (a directory's has no trailing slash).
    pub path: String,
    pub dir: bool,
    /// A directory's mark summarises its files.
    pub mark: Mark,
    pub collapsed: bool,
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    files: Vec<(String, Mark)>,
}

impl Node {
    fn marks(&self, out: &mut Vec<Mark>) {
        out.extend(self.files.iter().map(|(_, m)| *m));
        for child in self.dirs.values() {
            child.marks(out);
        }
    }
}

/// What a set of files adds up to.
pub fn summarise(marks: &[Mark]) -> Mark {
    if marks.contains(&Mark::Conflicted) {
        Mark::Conflicted
    } else if marks.iter().all(|m| *m == Mark::Untracked) {
        Mark::Untracked
    } else if marks.iter().all(|m| *m == Mark::Staged) {
        Mark::Staged
    } else if marks.iter().all(|m| matches!(m, Mark::Unstaged | Mark::Untracked)) {
        Mark::Unstaged
    } else {
        Mark::Partly
    }
}

/// The visible rows: directories first, then files with conflicts first.
pub fn rows(repo: &Repo, store: &Store, collapsed: &HashSet<String>) -> Vec<Row> {
    let mut root = Node::default();
    for (path, state) in &repo.files {
        let mut node = &mut root;
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap_or_default();
        for part in parts {
            node = node.dirs.entry(part.to_string()).or_default();
        }
        node.files.push((name.to_string(), store.mark(state)));
    }
    let mut out = Vec::new();
    walk(&root, "", 0, collapsed, &mut out);
    out
}

fn walk(node: &Node, prefix: &str, depth: usize, collapsed: &HashSet<String>, out: &mut Vec<Row>) {
    for (name, child) in &node.dirs {
        let path = format!("{prefix}{name}");
        let mut marks = Vec::new();
        child.marks(&mut marks);
        let folded = collapsed.contains(&path);
        out.push(Row { depth, name: name.clone(), path: path.clone(), dir: true, mark: summarise(&marks), collapsed: folded });
        if !folded {
            walk(child, &format!("{path}/"), depth + 1, collapsed, out);
        }
    }
    let mut files: Vec<&(String, Mark)> = node.files.iter().collect();
    files.sort_by_key(|(name, mark)| (*mark != Mark::Conflicted, name.clone()));
    for (name, mark) in files {
        out.push(Row {
            depth,
            name: name.clone(),
            path: format!("{prefix}{name}"),
            dir: false,
            mark: *mark,
            collapsed: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{Entry, FileState, Index, Oid, MODE_FILE};

    fn repo(paths: &[&str]) -> Repo {
        let mut repo = Repo::default();
        for p in paths {
            let e = Some(Entry::object(MODE_FILE, Oid::new(format!("{p}-x"))));
            repo.files.insert(
                p.to_string(),
                FileState { head: None, index: Index::Entry(None), work: e },
            );
        }
        repo
    }

    #[test]
    fn directories_come_first_and_fold_away_their_files() {
        let repo = repo(&["z.txt", "src/b.rs", "src/a.rs", "src/ui/w.rs"]);
        let store = Store::new(std::env::temp_dir());
        let names: Vec<String> = rows(&repo, &store, &HashSet::new()).iter().map(|r| r.path.clone()).collect();
        assert_eq!(names, vec!["src", "src/ui", "src/ui/w.rs", "src/a.rs", "src/b.rs", "z.txt"]);

        let folded: HashSet<String> = ["src".to_string()].into();
        let rows = rows(&repo, &store, &folded);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].collapsed);
        assert_eq!(rows[0].mark, Mark::Untracked);
    }

    #[test]
    fn a_directory_mark_is_mixed_unless_its_files_agree() {
        assert_eq!(summarise(&[Mark::Staged, Mark::Staged]), Mark::Staged);
        assert_eq!(summarise(&[Mark::Staged, Mark::Unstaged]), Mark::Partly);
        assert_eq!(summarise(&[Mark::Untracked, Mark::Unstaged]), Mark::Unstaged);
        assert_eq!(summarise(&[Mark::Staged, Mark::Conflicted]), Mark::Conflicted);
    }
}
