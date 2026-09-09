//! The only module here that writes.
//!
//! Everything above this is a pure function of a snapshot; this is where a
//! resolution reaches the working tree.  Two rules, both of them the same
//! rules `vcs::apply` follows:
//!
//! - **Nothing partial.**  A file is written only once every conflicted region
//!   in it has an answer, so there is no state where half a resolution is on
//!   disk and the markers for the other half are gone.
//! - **The write is atomic**, via [`crate::buffer::atomic_write`] — the same
//!   temp-file-and-rename the editor's own saves use.  A crash mid-write must
//!   not leave a truncated file where a conflicted one was, because the
//!   conflicted one is not recoverable from anything on disk.
//!
//! Staging is `git add`, deliberately: it is what tells git the path is
//! resolved, and running the user's own git is the standing rule for anything
//! that touches the repository.

use std::path::Path;

use super::ConflictFile;
use crate::vcs::load::git;

/// Write `file`'s merged text and stage it.
///
/// Refuses an undecided file rather than writing what it has: a file written
/// with an unanswered region silently *deletes* that region (the fold
/// contributes nothing for it), and the markers that would have shown the
/// mistake are exactly what was just removed.
pub fn resolve(root: &Path, file: &ConflictFile) -> Result<(), String> {
    if !file.is_decided() {
        let n = file.undecided_count();
        return Err(format!(
            "{n} conflict{} in {} still unanswered",
            if n == 1 { "" } else { "s" },
            file.path
        ));
    }
    let path = root.join(&file.path);
    let merged = file.merged();

    // An empty result means both sides were switched off for every region and
    // there was nothing else in the file: the answer is that the file should
    // not exist.  Writing an empty file and staging it would record something
    // the user did not ask for.
    if merged.is_empty() && file.regions.iter().all(|r| r.is_conflict()) {
        std::fs::remove_file(&path)
            .map_err(|e| format!("cannot remove {}: {e}", file.path))?;
        git(root, &["rm", "--cached", "--force", "--", &file.path])?;
        return Ok(());
    }

    crate::buffer::atomic_write(&path, &merged)
        .map_err(|e| format!("cannot write {}: {e}", file.path))?;
    git(root, &["add", "--", &file.path])?;
    Ok(())
}

/// Put a file back the way git left it: markers and all, unstaged.
///
/// The way out of a resolution that turned out to be wrong, and the reason
/// there is no confirmation on `Enter`: anything written here is one keypress
/// from being undone, because git still holds all three stages until the
/// operation finishes.
pub fn revert(root: &Path, path: &str) -> Result<(), String> {
    git(root, &["checkout", "--merge", "--", path]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::super::{hunk::{Region, Versions}, Choice, ConflictFile};
    use super::*;

    fn file(decided: bool) -> ConflictFile {
        ConflictFile {
            path: "f.txt".into(),
            base: None,
            left: None,
            right: None,
            regions: vec![
                Region::Agreed("a\n".into()),
                Region::Conflict(Versions {
                    base: "b\n".into(),
                    left: "l\n".into(),
                    right: "r\n".into(),
                }),
            ],
            choices: vec![if decided {
                Choice::only(super::super::Side::Left)
            } else {
                Choice::default()
            }],
            history: Vec::new(),
            resolved: false,
        }
    }

    use super::super::testrepo::diverged;
    use super::super::{load, Side};

    /// The whole chain against real git: read a conflicted merge, answer the
    /// region, write it, and check that git now considers the path resolved.
    /// This layer's job *is* driving git, so a mock would test the mock.
    #[test]
    fn resolving_a_file_writes_the_chosen_text_and_git_calls_it_resolved() {
        let Some(repo) = diverged() else { return };
        let _ = git(&repo.root, &["merge", "feature"]);

        let set = load::read(&repo.root).expect("the read");
        let mut file = set.files[0].clone();
        file.choose(0, Choice::only(Side::Right));

        resolve(&repo.root, &file).expect("the write");

        let on_disk = std::fs::read_to_string(repo.root.join("f.txt")).unwrap();
        assert_eq!(on_disk, "a\ntimeout = 60\nz\n", "the chosen side, and no markers");

        // git's own verdict: the path has no unmerged stages left.
        let unmerged = git(&repo.root, &["ls-files", "-u", "--", "f.txt"]).unwrap();
        assert!(unmerged.trim().is_empty(), "git still calls it conflicted: {unmerged}");
        // …and it is staged, which is what `git add` is for here.
        let staged = git(&repo.root, &["diff", "--cached", "--name-only"]).unwrap();
        assert!(staged.contains("f.txt"), "the resolution was not staged");
    }

    /// Keeping both sides writes both, in file order — the answer for two
    /// imports or two list entries, and the one a "take ours / take theirs"
    /// menu cannot express.
    #[test]
    fn keeping_both_sides_writes_both_in_file_order() {
        let Some(repo) = diverged() else { return };
        let _ = git(&repo.root, &["merge", "feature"]);

        let set = load::read(&repo.root).expect("the read");
        let mut file = set.files[0].clone();
        file.choose(0, Choice::Sides { left: true, right: true });
        resolve(&repo.root, &file).expect("the write");

        assert_eq!(
            std::fs::read_to_string(repo.root.join("f.txt")).unwrap(),
            "a\ntimeout = 45\ntimeout = 60\nz\n"
        );
    }

    /// A resolution that turned out to be wrong is one command from being put
    /// back — which is why `Enter` needs no confirmation.  git still holds all
    /// three stages until the operation finishes.
    #[test]
    fn a_written_resolution_can_be_put_back_the_way_git_left_it() {
        let Some(repo) = diverged() else { return };
        let _ = git(&repo.root, &["merge", "feature"]);

        let set = load::read(&repo.root).expect("the read");
        let mut file = set.files[0].clone();
        file.choose(0, Choice::only(Side::Left));
        resolve(&repo.root, &file).expect("the write");
        assert!(!std::fs::read_to_string(repo.root.join("f.txt")).unwrap().contains("<<<"));

        revert(&repo.root, "f.txt").expect("the revert");
        let back = std::fs::read_to_string(repo.root.join("f.txt")).unwrap();
        assert!(back.contains("<<<<<<<"), "the conflict did not come back: {back}");
        let unmerged = git(&repo.root, &["ls-files", "-u", "--", "f.txt"]).unwrap();
        assert!(!unmerged.trim().is_empty(), "the stages did not come back");
    }

    /// The one thing this module must never do: write a file with an
    /// unanswered region in it.  The fold contributes nothing for that region,
    /// so the write would silently delete it — and take the markers that would
    /// have shown the mistake with it.
    #[test]
    fn an_undecided_file_is_refused_rather_than_written() {
        let dir = std::env::temp_dir().join("sakharov-conflict-write-test");
        let _ = std::fs::create_dir_all(&dir);
        let err = resolve(&dir, &file(false)).expect_err("it has to refuse");
        assert!(err.contains("unanswered"), "{err}");
        assert!(!dir.join("f.txt").exists(), "nothing may be written");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
