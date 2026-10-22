//! Building the resolver's view of a repository's conflicts, and working out
//! who each side is.
//!
//! The conflicts come from a [`Repo`] — the real one, or a projection that
//! stopped at a planned merge — so a conflict the plan will cause is resolved
//! with the same view as one git already left behind.

use std::path::Path;

use super::{hunk, Choice, ConflictFile, ConflictSet, Operation, SideLabel};
use crate::vcs::{load::git, store::Store, Blob, Entry, Index, Oid, Repo};

/// Every conflicted file in `repo`, with its regions and the side labels.
pub fn build(store: &Store, repo: &Repo) -> ConflictSet {
    let operation = repo.operation.as_ref().map_or(Operation::Unknown, |o| o.kind);
    let (left, right) = labels(store, repo);
    let mut files = Vec::new();
    for (path, state) in &repo.files {
        let Index::Conflict(c) = &state.index else { continue };
        let text = |e: &Option<Entry>| store.text(e.as_ref());
        let oid = |e: &Option<Entry>| match e {
            Some(Entry { blob: Blob::Object(oid), .. }) => Some(oid.clone()),
            _ => None,
        };
        let mut file = ConflictFile {
            path: path.clone(),
            base: text(&c.base),
            left: text(&c.left),
            right: text(&c.right),
            regions: Vec::new(),
            choices: Vec::new(),
            history: Vec::new(),
        };
        let stages = (oid(&c.base), oid(&c.left), oid(&c.right));
        // A file whose three-way diff cannot be computed is one whole-file
        // conflict rather than dropped: a conflicted path the resolver omits
        // is one the operation cannot finish without.
        if hunk::fill(store.root(), &mut file, stages).is_err() {
            file.regions = vec![hunk::Region::Conflict(hunk::Versions {
                base: file.base.clone().unwrap_or_default(),
                left: file.left.clone().unwrap_or_default(),
                right: file.right.clone().unwrap_or_default(),
            })];
            file.choices = vec![Choice::default()];
        }
        files.push(file);
    }
    ConflictSet { operation, left, right, files }
}

/// What git is in the middle of, from the state files in `git_dir`.
///
/// Rebase first: some git versions also write `CHERRY_PICK_HEAD` during a
/// rebase, and the rebase is the more specific answer.
pub fn detect_operation(git_dir: &Path) -> Operation {
    if git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir() {
        return Operation::Rebase;
    }
    for (file, op) in [
        ("MERGE_HEAD", Operation::Merge),
        ("CHERRY_PICK_HEAD", Operation::CherryPick),
        ("REVERT_HEAD", Operation::Revert),
    ] {
        if git_dir.join(file).exists() {
            return op;
        }
    }
    Operation::Unknown
}

/// What each side *is*, in words and in commits — the one place the
/// ours/theirs inversion becomes language.  Never "ours" or "theirs".
fn labels(store: &Store, repo: &Repo) -> (SideLabel, SideLabel) {
    let head = repo.head.commit.as_ref();
    let Some(op) = repo.operation.as_ref() else {
        return (
            describe(store, repo, head, "Index stage 2".to_string()),
            SideLabel { role: "Index stage 3".to_string(), ..SideLabel::default() },
        );
    };
    let here = repo.head.branch.clone().or_else(|| op.branch.clone());
    let incoming = op.commit.as_ref();
    let (left, right) = match op.kind {
        Operation::Merge => (
            match &here {
                Some(b) => format!("On {b} — your branch"),
                None => "Where you are — your work".to_string(),
            },
            match op.label.as_str() {
                "" => "Coming in".to_string(),
                name => format!("Merging in {name}"),
            },
        ),
        Operation::Rebase => (
            match &op.onto {
                Some(onto) => format!("On {onto} — landing here"),
                None => "The branch you are landing on".to_string(),
            },
            match &op.branch {
                Some(b) => format!("Your commit from {b}, being replayed"),
                None => "Your commit, being replayed".to_string(),
            },
        ),
        Operation::CherryPick => (
            match &here {
                Some(b) => format!("On {b} — landing here"),
                None => "Where you are — landing here".to_string(),
            },
            "The commit being replayed".to_string(),
        ),
        Operation::Revert => (
            match &here {
                Some(b) => format!("On {b} — your branch"),
                None => "Where you are".to_string(),
            },
            "The commit being undone".to_string(),
        ),
        Operation::Unknown => ("Index stage 2".to_string(), "Index stage 3".to_string()),
    };
    (describe(store, repo, head, left), describe(store, repo, incoming, right))
}

/// A side's commit facts, keeping the role wording whatever happens.
fn describe(store: &Store, repo: &Repo, commit: Option<&Oid>, role: String) -> SideLabel {
    let mut label = SideLabel { role, ..SideLabel::default() };
    let Some(oid) = commit else { return label };
    if let Some(p) = oid.pending_index().and_then(|i| repo.pending.get(i)) {
        label.commit = "planned".to_string();
        label.summary = p.summary().to_string();
        return label;
    }
    let Ok(out) = git(store.root(), &["log", "-1", "--format=%h\x1f%an\x1f%ct\x1f%s", oid.as_str()]) else {
        return label;
    };
    let mut parts = out.trim_end_matches('\n').split('\x1f');
    label.commit = parts.next().unwrap_or_default().to_string();
    label.author = parts.next().unwrap_or_default().to_string();
    label.when = parts.next().unwrap_or_default().parse().unwrap_or(0);
    label.summary = parts.next().unwrap_or_default().to_string();
    label
}

/// Pull the branch name out of git's own merge message.
pub fn parse_merge_msg(line: &str) -> Option<String> {
    let rest = line.strip_prefix("Merge ")?;
    let rest = rest
        .strip_prefix("branch ")
        .or_else(|| rest.strip_prefix("remote-tracking branch "))
        .or_else(|| rest.strip_prefix("commit "))
        .unwrap_or(rest);
    let name = rest.trim().trim_matches('\'').trim_matches('"');
    let name = name.split(" into ").next().unwrap_or(name).trim_matches('\'');
    (!name.is_empty()).then(|| name.to_string())
}

/// The conflicts in a real repository, as the view would build them.
#[cfg(test)]
pub fn read(root: &Path) -> Result<ConflictSet, String> {
    let repo = crate::vcs::load::read(root)?;
    Ok(build(&Store::new(root.to_path_buf()), &repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::testrepo::{diverged, Repo};

    /// The whole read, against a real conflicted merge: three stages, the
    /// three-way diff, and the labels.
    #[test]
    fn a_conflicted_merge_reads_as_one_file_with_one_region_and_two_names() {
        let Some(repo) = diverged() else { return };
        repo.try_run(&["merge", "feature"]);

        let set = read(&repo.root).expect("the read");
        assert_eq!(set.operation, Operation::Merge);
        assert_eq!(set.files.len(), 1);
        let file = &set.files[0];
        assert_eq!(file.path, "f.txt");

        // The base is present — which is a third of the reason this view
        // exists, and is *not* what git leaves in the working file by default.
        assert_eq!(file.base.as_deref(), Some("a\ntimeout = 30\nz\n"));
        assert_eq!(file.left.as_deref(), Some("a\ntimeout = 45\nz\n"));
        assert_eq!(file.right.as_deref(), Some("a\ntimeout = 60\nz\n"));

        assert_eq!(file.conflict_count(), 1, "one disagreement, one region");
        let super::hunk::Region::Conflict(ref v) = file.regions[1] else {
            panic!("the middle region is the conflict: {:?}", file.regions)
        };
        assert_eq!(v.left, "timeout = 45\n");
        assert_eq!(v.right, "timeout = 60\n");
        assert_eq!(v.base, "timeout = 30\n", "the ancestor says who changed what");

        // In a merge, the left pane is your branch.
        assert!(set.operation.left_is_yours());
        assert!(set.left.role.contains("main"), "{}", set.left.role);
        assert!(set.right.role.contains("feature"), "{}", set.right.role);
        assert!(!set.left.commit.is_empty() && !set.left.author.is_empty());
    }

    /// The inversion, against real git.  On `git rebase main` from `feature`,
    /// stage 2 holds **main's** version and stage 3 holds the user's own
    /// commit — the opposite of what "ours" and "theirs" suggest, and the
    /// single most confusing thing about resolving a rebase.
    #[test]
    fn a_rebase_puts_your_own_commit_on_the_right_and_says_so() {
        let Some(repo) = diverged() else { return };
        repo.run(&["checkout", "feature"]).expect("checkout");
        repo.try_run(&["rebase", "main"]);

        let set = read(&repo.root).expect("the read");
        assert_eq!(set.operation, Operation::Rebase);
        assert!(!set.operation.left_is_yours(), "stage 2 is not yours in a replay");

        let file = &set.files[0];
        assert_eq!(
            file.left.as_deref(),
            Some("a\ntimeout = 45\nz\n"),
            "stage 2 is main's version — the branch being landed on"
        );
        assert_eq!(
            file.right.as_deref(),
            Some("a\ntimeout = 60\nz\n"),
            "stage 3 is the user's own commit, being replayed"
        );

        // And the header says it in words rather than leaving it to be worked
        // out — which is the whole reason this case is handled at all.
        assert!(set.left.role.contains("landing"), "{}", set.left.role);
        assert!(
            set.right.role.contains("Your commit") && set.right.role.contains("feature"),
            "{}",
            set.right.role
        );
    }

    /// An add/add conflict has no stage 1.  It is a real conflict, not a
    /// malformed read, and the base side is simply empty.
    #[test]
    fn a_file_added_on_both_sides_has_no_ancestor_and_still_resolves() {
        let Some(repo) = Repo::new() else { return };
        repo.commit("seed.txt", "seed\n", "seed");
        repo.run(&["checkout", "-b", "feature"]).expect("branch");
        repo.commit("new.txt", "from feature\n", "add on feature");
        repo.run(&["checkout", "main"]).expect("checkout");
        repo.commit("new.txt", "from main\n", "add on main");
        repo.try_run(&["merge", "feature"]);

        let set = read(&repo.root).expect("the read");
        let file = set.files.iter().find(|f| f.path == "new.txt").expect("new.txt");
        assert_eq!(file.base, None, "there is no common ancestor");
        // The three-way diff itself ran, rather than the whole-file fallback.
                assert_eq!(file.conflict_count(), 1);
        assert!(file.left.is_some() && file.right.is_some());
    }

    /// The read must not depend on the user's `merge.conflictStyle`: the
    /// regions come from our own three-way diff, not from the markers git left
    /// in the working file.  Under `merge` style git writes **no** base at
    /// all — so a resolver that parsed the file would have nothing to show in
    /// the ancestor pane, and this is what proves it does not.
    #[test]
    fn the_regions_do_not_come_from_the_working_files_markers() {
        let Some(repo) = diverged() else { return };
        repo.try_run(&["merge", "feature"]);

        let on_disk = std::fs::read_to_string(repo.root.join("f.txt")).unwrap();
        assert!(on_disk.contains("<<<<<<<"), "git did leave markers");
        assert!(
            !on_disk.contains("|||||||"),
            "under `merge` style git writes no ancestor — which is the point"
        );

        let set = read(&repo.root).expect("the read");
        let super::hunk::Region::Conflict(ref v) = set.files[0].regions[1] else {
            panic!("a conflict")
        };
        assert_eq!(v.base, "timeout = 30\n", "we computed the ancestor ourselves");
    }

    /// A clean repository reads as no conflicts rather than as an error.
    #[test]
    fn a_repository_with_nothing_conflicted_reads_as_empty() {
        let Some(repo) = Repo::new() else { return };
        repo.commit("f.txt", "fine\n", "fine");
        let set = read(&repo.root).expect("the read");
        assert!(set.files.is_empty());
        assert_eq!(set.operation, Operation::Unknown);
    }

    /// The merged branch's *name* survives only in the merge message; the
    /// header would otherwise have an oid where a name belongs.
    #[test]
    fn the_merged_branch_name_comes_out_of_gits_own_message() {
        assert_eq!(parse_merge_msg("Merge branch 'feature'"), Some("feature".into()));
        assert_eq!(
            parse_merge_msg("Merge branch 'feature' into main"),
            Some("feature".into())
        );
        assert_eq!(
            parse_merge_msg("Merge remote-tracking branch 'origin/main'"),
            Some("origin/main".into())
        );
        assert_eq!(parse_merge_msg("Fixed a bug"), None);
    }
}
