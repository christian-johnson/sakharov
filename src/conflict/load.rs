//! Reading the conflicted index, and working out who each side is.
//!
//! Split the way [`crate::vcs::load`] is: every parser is a pure function of
//! git's output, and only [`read`] spawns anything.  That split is not
//! tidiness — the interesting cases here (a rebase stopped halfway, an add/add
//! conflict with no base, a modify/delete) are impossible to build as fixtures
//! any other way, and they are exactly the cases a resolver has to get right.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use super::{hunk, Choice, ConflictFile, ConflictSet, Operation, SideLabel};
use crate::vcs::load::git;

/// An in-flight read, polled once per frame by the run loop.
pub struct ConflictLoad {
    rx: Receiver<Result<ConflictSet, String>>,
}

impl ConflictLoad {
    pub fn poll(&self) -> Option<Result<ConflictSet, String>> {
        self.rx.try_recv().ok()
    }
}

/// Start reading `root`'s conflicts on a background thread.
///
/// Off the UI thread for the same reason the graph's load is: one
/// `git merge-file` runs per conflicted file, and a merge that went badly can
/// leave dozens.
pub fn start(root: PathBuf) -> ConflictLoad {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read(&root));
    });
    ConflictLoad { rx }
}

/// Blocking read of the whole snapshot.  Loader thread only.
pub fn read(root: &Path) -> Result<ConflictSet, String> {
    let operation = detect_operation(root);
    let (left, right) = resolve_labels(root, operation);

    let listing = git(root, &["ls-files", "-u", "-z"]).unwrap_or_default();
    let mut files = Vec::new();
    for path in parse_unmerged(&listing) {
        let mut file = ConflictFile {
            base: stage(root, 1, &path),
            left: stage(root, 2, &path),
            right: stage(root, 3, &path),
            path,
            regions: Vec::new(),
            choices: Vec::new(),
            history: Vec::new(),
            resolved: false,
        };
        // A file whose three-way diff cannot be computed is reported as a
        // single whole-file conflict rather than dropped: a conflicted path
        // the resolver silently omits is one the user cannot finish the merge
        // without, and will not know why.
        if hunk::fill(root, &mut file).is_err() {
            file.regions = vec![hunk::Region::Conflict(hunk::Versions {
                base: file.base.clone().unwrap_or_default(),
                left: file.left.clone().unwrap_or_default(),
                right: file.right.clone().unwrap_or_default(),
            })];
            file.choices = vec![Choice::default()];
        }
        files.push(file);
    }

    Ok(ConflictSet { root: root.to_path_buf(), operation, left, right, files })
}

/// One version of a conflicted path, or `None` when that stage is absent.
///
/// Absent is a real state, not a failure: an add/add conflict has no base and
/// a modify/delete has no stage on the side that deleted it.
fn stage(root: &Path, n: u8, path: &str) -> Option<String> {
    let spec = format!(":{n}:{path}");
    git(root, &["cat-file", "blob", &spec]).ok()
}

/// The conflicted paths named by `git ls-files -u -z`.
///
/// The listing repeats a path once per stage it has, so the parse is a
/// de-duplication as much as a split.  NUL-separated because a path may
/// contain anything, including a newline.
pub fn parse_unmerged(listing: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for record in listing.split('\0').filter(|r| !r.is_empty()) {
        // `<mode> <oid> <stage>\t<path>` — the path is everything after the
        // first tab, so a tab inside it survives.
        let Some((_, path)) = record.split_once('\t') else { continue };
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_string());
        }
    }
    paths
}

/// What git is in the middle of.
///
/// Read from the state files rather than inferred, because the *only* thing
/// that distinguishes a merge from a replay is which of these exists — and
/// that is what decides whether stage 2 is your work or somebody else's.
pub fn detect_operation(root: &Path) -> Operation {
    let git_dir = root.join(".git");
    // Rebase first: a rebase in progress also writes CHERRY_PICK_HEAD in some
    // git versions, and the rebase is the more specific answer.
    for (dir, op) in [
        ("rebase-merge", Operation::Rebase),
        ("rebase-apply", Operation::Rebase),
    ] {
        if git_dir.join(dir).is_dir() {
            return op;
        }
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

/// What each side *is*, in words and in commits.
///
/// The whole point of the view, and the one place the ours/theirs inversion is
/// turned into language.  During a replay the wording says outright that the
/// left pane is the branch being landed on and the right is the user's own
/// commit, because "ours" and "theirs" are the two words that cause the
/// confusion this is here to remove.
fn resolve_labels(root: &Path, operation: Operation) -> (SideLabel, SideLabel) {
    let head_branch = git(root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let (left_rev, right_rev, left_role, right_role) = match operation {
        Operation::Merge => (
            "HEAD".to_string(),
            "MERGE_HEAD".to_string(),
            match &head_branch {
                Some(b) => format!("On {b} — your branch"),
                None => "Where you are — your work".to_string(),
            },
            match merged_name(root) {
                Some(name) => format!("Merging in {name}"),
                None => "Coming in".to_string(),
            },
        ),
        Operation::Rebase => (
            rebase_onto(root).unwrap_or_else(|| "HEAD".to_string()),
            "REBASE_HEAD".to_string(),
            match rebase_onto_name(root) {
                Some(name) => format!("On {name} — landing here"),
                None => "The branch you are landing on".to_string(),
            },
            match rebase_branch(root) {
                Some(b) => format!("Your commit from {b}, being replayed"),
                None => "Your commit, being replayed".to_string(),
            },
        ),
        Operation::CherryPick => (
            "HEAD".to_string(),
            "CHERRY_PICK_HEAD".to_string(),
            match &head_branch {
                Some(b) => format!("On {b} — landing here"),
                None => "Where you are — landing here".to_string(),
            },
            "The commit being replayed".to_string(),
        ),
        Operation::Revert => (
            "HEAD".to_string(),
            "REVERT_HEAD".to_string(),
            match &head_branch {
                Some(b) => format!("On {b} — your branch"),
                None => "Where you are".to_string(),
            },
            "The commit being undone".to_string(),
        ),
        Operation::Unknown => (
            "HEAD".to_string(),
            String::new(),
            "Index stage 2".to_string(),
            "Index stage 3".to_string(),
        ),
    };

    (
        describe(root, &left_rev, left_role),
        describe(root, &right_rev, right_role),
    )
}

/// Fill in a side's commit facts, keeping the role wording whatever happens.
///
/// A rev that does not resolve still gets its label: "On main — your branch"
/// with no hash beside it is a usable header, and a pane with no header at all
/// is the state this view exists to replace.
fn describe(root: &Path, rev: &str, role: String) -> SideLabel {
    let mut label = SideLabel { role, ..SideLabel::default() };
    if rev.is_empty() {
        return label;
    }
    let Ok(out) = git(root, &["log", "-1", "--format=%h\x1f%an\x1f%ct\x1f%s", rev]) else {
        return label;
    };
    let mut parts = out.trim_end_matches('\n').split('\x1f');
    label.commit = parts.next().unwrap_or_default().to_string();
    label.author = parts.next().unwrap_or_default().to_string();
    label.when = parts.next().unwrap_or_default().parse().unwrap_or(0);
    label.summary = parts.next().unwrap_or_default().to_string();
    label
}

/// The name of what is being merged in, from `.git/MERGE_MSG`'s first line.
///
/// git writes "Merge branch 'feature'" there, which is the only place the
/// *name* survives — `MERGE_HEAD` is an oid, and a branch tip is not enough to
/// recover it from unambiguously.
fn merged_name(root: &Path) -> Option<String> {
    let msg = std::fs::read_to_string(root.join(".git").join("MERGE_MSG")).ok()?;
    let first = msg.lines().next()?;
    parse_merge_msg(first)
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

fn rebase_state(root: &Path, file: &str) -> Option<String> {
    for dir in ["rebase-merge", "rebase-apply"] {
        let path = root.join(".git").join(dir).join(file);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let value = text.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// The commit the rebase is replaying onto — which is what stage 2 holds.
fn rebase_onto(root: &Path) -> Option<String> {
    rebase_state(root, "onto")
}

/// A name for that commit, if a ref points at it.
fn rebase_onto_name(root: &Path) -> Option<String> {
    let onto = rebase_onto(root)?;
    let named = git(root, &["name-rev", "--name-only", "--refs=refs/heads/*", &onto]).ok()?;
    let named = named.trim();
    (!named.is_empty() && named != "undefined").then(|| named.to_string())
}

/// The branch being rebased — `refs/heads/feature` in the state file.
fn rebase_branch(root: &Path) -> Option<String> {
    let name = rebase_state(root, "head-name")?;
    Some(name.trim_start_matches("refs/heads/").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The listing names a path once per stage it has; the resolver wants each
    /// path once.
    #[test]
    fn a_path_with_three_stages_is_listed_once() {
        let listing = "100644 aaa 1\tf.txt\u{0}100644 bbb 2\tf.txt\u{0}100644 ccc 3\tf.txt\u{0}";
        assert_eq!(parse_unmerged(listing), vec!["f.txt".to_string()]);
    }

    /// An add/add conflict has no stage 1 at all.  Two stages is still one
    /// conflicted file, not a malformed listing.
    #[test]
    fn a_path_with_no_base_stage_is_still_one_file() {
        let listing = "100644 bbb 2\tnew.txt\u{0}100644 ccc 3\tnew.txt\u{0}";
        assert_eq!(parse_unmerged(listing), vec!["new.txt".to_string()]);
    }

    /// NUL-separated precisely so a path containing a newline — or a tab —
    /// survives the parse whole.
    #[test]
    fn a_path_containing_a_newline_or_a_tab_survives() {
        let listing = "100644 aaa 1\tawkward\nname\u{0}100644 bbb 2\ttabbed\tname\u{0}";
        assert_eq!(
            parse_unmerged(listing),
            vec!["awkward\nname".to_string(), "tabbed\tname".to_string()]
        );
    }

    #[test]
    fn an_empty_listing_is_no_conflicts_rather_than_an_error() {
        assert!(parse_unmerged("").is_empty());
    }

    // ---------------------------------------------------------------------
    // Against a real repository
    // ---------------------------------------------------------------------
    //
    // This layer's whole job is reading git's state, and a mock would test the
    // mock.  The cases below — a rebase stopped halfway, an add/add conflict
    // with no base — are also impossible to build as fixtures any other way,
    // which is the reason the parsers above are separated out from the reads.

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
        assert!(set.is_empty());
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
