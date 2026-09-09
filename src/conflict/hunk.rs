//! The three-way diff: turning three versions of a file into regions.
//!
//! The regions come from `git merge-file -p --diff3` run over the three index
//! stages written to temporaries — **not** from the conflict markers git left
//! in the working file.  That is the module's one load-bearing decision, and
//! it buys two things:
//!
//! - The marker style is **ours**.  `merge.conflictStyle` is a user setting,
//!   and `zdiff3` (git 2.35+) or a custom `conflict-marker-size` would change
//!   the shape of what is parsed.  Our invocation, our labels, our markers.
//! - The **base** is always present.  Without it the view cannot say who
//!   changed what, which is a third of the reason it exists.
//!
//! Parsing our own controlled output is a different proposition from parsing
//! whatever was left in the working tree, but it is still parsing, so the
//! labels are chosen to be impossible: [`MARK_LEFT`] and friends embed a
//! sentinel no source file contains, and a line is only a marker if it matches
//! one exactly.

use std::path::Path;

use super::{ConflictFile, Choice};

/// One stretch of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    /// Text both sides agree on.  Includes the parts git merged successfully
    /// on its own, which is most of a conflicted file.
    Agreed(String),
    /// Text they do not.
    Conflict(Versions),
}

impl Region {
    pub fn is_conflict(&self) -> bool {
        matches!(self, Region::Conflict(_))
    }
}

/// The three versions of one conflicted stretch.
///
/// Each keeps its trailing newline, so a region is a substring of the file and
/// the fold in [`ConflictFile::merged`] is a concatenation rather than a
/// re-join that has to guess where the newlines went.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Versions {
    pub base: String,
    pub left: String,
    pub right: String,
}

impl Versions {
    /// One side's text, split into display lines (no trailing empty line).
    pub fn lines(text: &str) -> Vec<&str> {
        if text.is_empty() {
            return Vec::new();
        }
        text.strip_suffix('\n').unwrap_or(text).split('\n').collect()
    }

    /// The tallest of the three, which is how many rows a region needs.
    pub fn height(&self, with_base: bool) -> usize {
        let mut h = Self::lines(&self.left).len().max(Self::lines(&self.right).len());
        if with_base {
            h = h.max(Self::lines(&self.base).len());
        }
        h.max(1)
    }
}

// Our markers.
//
// Two things make a line of the user's own text impossible to mistake for one
// of these.  The **length**: `--marker-size=32` is four and a half times git's
// default, so a run of that many `=` in a real file is not a thing that
// happens.  And the **labels**: git writes `<marker> <label>`, so the opening,
// ancestor and closing markers each carry a sentinel word as well.
//
// The separator is the one that cannot: git emits it bare, with no label, so
// its length is all it has.  That is why the length matters at all, and why it
// is only ever matched *inside* a conflict, where the surrounding markers have
// already established that this is our own output being read back.
const MARKER_SIZE: &str = "32";
const LABEL_LEFT: &str = "SAKHAROV-LEFT";
const LABEL_BASE: &str = "SAKHAROV-BASE";
const LABEL_RIGHT: &str = "SAKHAROV-RIGHT";

const MARK_LEFT: &str = "<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<< SAKHAROV-LEFT";
const MARK_BASE: &str = "|||||||||||||||||||||||||||||||| SAKHAROV-BASE";
const MARK_SPLIT: &str = "================================";
const MARK_RIGHT: &str = ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> SAKHAROV-RIGHT";

/// Split `merged` — the output of a `--diff3` merge with our labels — into
/// regions.
///
/// A pure function of the text, so the interesting shapes (a conflict at the
/// very start, two in a row, an empty side) are testable without a repository.
/// Anything that is not one of our four exact markers is content, including a
/// line of `=======` in the user's own file.
pub fn parse_diff3(merged: &str) -> Vec<Region> {
    let mut regions = Vec::new();
    let mut agreed = String::new();
    let mut versions = Versions::default();
    // Which of the three buffers the lines are currently going into.
    let mut part: Option<u8> = None;

    for line in merged.split_inclusive('\n') {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        match bare {
            MARK_LEFT => {
                if !agreed.is_empty() {
                    regions.push(Region::Agreed(std::mem::take(&mut agreed)));
                }
                versions = Versions::default();
                part = Some(0);
            }
            MARK_BASE if part.is_some() => part = Some(1),
            MARK_SPLIT if part.is_some() => part = Some(2),
            MARK_RIGHT if part.is_some() => {
                regions.push(Region::Conflict(std::mem::take(&mut versions)));
                part = None;
            }
            _ => match part {
                Some(0) => versions.left.push_str(line),
                Some(1) => versions.base.push_str(line),
                Some(2) => versions.right.push_str(line),
                _ => agreed.push_str(line),
            },
        }
    }
    // An unterminated conflict means git wrote something we do not understand.
    // Keeping the text as agreed content is the safe reading: it is visible,
    // and nothing is silently dropped from the file.
    if part.is_some() {
        agreed.push_str(&versions.left);
        agreed.push_str(&versions.base);
        agreed.push_str(&versions.right);
    }
    if !agreed.is_empty() {
        regions.push(Region::Agreed(agreed));
    }
    regions
}

/// Run the three-way merge and build the file's regions.
///
/// `git merge-file` exits 1 when there are conflicts and >1 on a real error,
/// so a non-zero status is not by itself a failure — the *output* is what is
/// wanted either way.
pub fn regions_for(
    root: &Path,
    file: &ConflictFile,
) -> Result<Vec<Region>, String> {
    // A side missing its stage is a modify/delete: the deleted side is an
    // empty file, which makes the whole thing one conflict between the
    // surviving text and nothing.  That is exactly the right shape — "keep it"
    // and "let the deletion stand" are the two switches.
    let dir = tempdir(root)?;
    let write = |name: &str, text: &Option<String>| -> Result<std::path::PathBuf, String> {
        let path = dir.join(name);
        std::fs::write(&path, text.as_deref().unwrap_or(""))
            .map_err(|e| format!("cannot stage the conflict for reading: {e}"))?;
        Ok(path)
    };
    let left = write("left", &file.left)?;
    let base = write("base", &file.base)?;
    let right = write("right", &file.right)?;

    let out = std::process::Command::new("git")
        .arg("merge-file")
        .arg("-p")
        .arg("--diff3")
        .arg(format!("--marker-size={MARKER_SIZE}"))
        .args(["-L", LABEL_LEFT])
        .args(["-L", LABEL_BASE])
        .args(["-L", LABEL_RIGHT])
        .arg(&left)
        .arg(&base)
        .arg(&right)
        .output();
    let _ = std::fs::remove_dir_all(&dir);

    let out = out.map_err(|e| format!("cannot run git merge-file: {e}"))?;
    // Exit codes above 1 are real errors; 1 just means "there were conflicts",
    // which is the only reason we are here.
    if !out.status.success() && out.status.code().unwrap_or(2) > 1 {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(parse_diff3(&String::from_utf8_lossy(&out.stdout)))
}

/// A private scratch directory under the repository's own `.git`.
///
/// Inside `.git` rather than the system temp dir so the blobs never land
/// somewhere world-readable, and so a crash leaves them where the repository
/// they came from is.
fn tempdir(root: &Path) -> Result<std::path::PathBuf, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = root.join(".git").join("sakharov-merge").join(stamp.to_string());
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot make a scratch directory: {e}"))?;
    Ok(dir)
}

/// Give `file` its regions and a fresh, undecided choice per conflict.
pub fn fill(root: &Path, file: &mut ConflictFile) -> Result<(), String> {
    file.regions = regions_for(root, file)?;
    let conflicts = file.regions.iter().filter(|r| r.is_conflict()).count();
    file.choices = vec![Choice::default(); conflicts];
    file.history.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff3(body: &str) -> String {
        body.replace("<<<", MARK_LEFT)
            .replace("|||", MARK_BASE)
            .replace("===", MARK_SPLIT)
            .replace(">>>", MARK_RIGHT)
    }

    #[test]
    fn a_conflict_between_agreed_text_comes_out_as_three_regions() {
        let regions = parse_diff3(&diff3(
            "a\n<<<\ntimeout = 45\n|||\ntimeout = 30\n===\ntimeout = 60\n>>>\nz\n",
        ));
        assert_eq!(
            regions,
            vec![
                Region::Agreed("a\n".into()),
                Region::Conflict(Versions {
                    left: "timeout = 45\n".into(),
                    base: "timeout = 30\n".into(),
                    right: "timeout = 60\n".into(),
                }),
                Region::Agreed("z\n".into()),
            ]
        );
    }

    /// Every region keeps its newlines, so the fold that writes the file is a
    /// concatenation rather than a re-join that has to guess where they went.
    #[test]
    fn concatenating_every_region_reproduces_one_side_exactly() {
        let regions = parse_diff3(&diff3("a\n<<<\nL\n|||\nB\n===\nR\n>>>\nz\n"));
        let left: String = regions
            .iter()
            .map(|r| match r {
                Region::Agreed(t) => t.clone(),
                Region::Conflict(v) => v.left.clone(),
            })
            .collect();
        assert_eq!(left, "a\nL\nz\n");
    }

    /// A conflict at the very first line has no agreed region before it, and
    /// one at the very last has none after.
    #[test]
    fn a_conflict_at_either_end_produces_no_empty_agreed_region() {
        let regions = parse_diff3(&diff3("<<<\nL\n|||\nB\n===\nR\n>>>\n"));
        assert_eq!(regions.len(), 1);
        assert!(regions[0].is_conflict());
    }

    /// Two conflicts in a row, with nothing between them.
    #[test]
    fn back_to_back_conflicts_stay_two_regions() {
        let regions = parse_diff3(&diff3(
            "<<<\nL1\n|||\nB1\n===\nR1\n>>>\n<<<\nL2\n|||\nB2\n===\nR2\n>>>\n",
        ));
        assert_eq!(regions.iter().filter(|r| r.is_conflict()).count(), 2);
        assert!(!regions.iter().any(|r| matches!(r, Region::Agreed(t) if t.is_empty())));
    }

    /// A side with no lines at all — one branch deleted what the other kept.
    /// The empty side is a real answer ("let the deletion stand"), not a
    /// malformed region.
    #[test]
    fn a_side_that_deleted_everything_is_an_empty_version_not_a_missing_one() {
        let regions = parse_diff3(&diff3("<<<\nkept\n|||\nkept\n===\n>>>\n"));
        let Region::Conflict(ref v) = regions[0] else { panic!("a conflict") };
        assert_eq!(v.left, "kept\n");
        assert_eq!(v.right, "");
        assert_eq!(v.height(false), 1, "an empty side still needs a row to say so");
    }

    /// The user's own file may contain git's ordinary markers — a file that
    /// records a past conflict, a document *about* merge conflicts.  Only an
    /// exact match on our own long, labelled markers counts, which is why they
    /// are long and labelled at all.
    #[test]
    fn text_that_looks_like_a_marker_is_content() {
        let text = "=======\n<<<<<<< HEAD\nnot ours\n>>>>>>> other\n";
        assert_eq!(parse_diff3(text), vec![Region::Agreed(text.into())]);
    }

    /// The markers really are the length the merge is asked for.  If the two
    /// ever drift, every conflict parses as one giant agreed region and the
    /// view silently shows a file with nothing to resolve.
    #[test]
    fn the_marker_constants_match_the_size_the_merge_is_run_with() {
        let size: usize = MARKER_SIZE.parse().expect("a number");
        for (mark, ch) in [
            (MARK_LEFT, '<'),
            (MARK_BASE, '|'),
            (MARK_SPLIT, '='),
            (MARK_RIGHT, '>'),
        ] {
            let run = mark.chars().take_while(|c| *c == ch).count();
            assert_eq!(run, size, "{mark:?} is not {size} {ch}s");
        }
        assert_eq!(MARK_SPLIT.len(), size, "the separator carries no label");
    }

    /// Output we do not understand must not silently lose text.  Keeping it as
    /// visible content is the safe reading.
    #[test]
    fn an_unterminated_conflict_keeps_its_text_rather_than_dropping_it() {
        let regions = parse_diff3(&diff3("a\n<<<\nL\n|||\nB\n===\nR\n"));
        let all: String = regions
            .iter()
            .map(|r| match r {
                Region::Agreed(t) => t.clone(),
                Region::Conflict(v) => format!("{}{}{}", v.left, v.base, v.right),
            })
            .collect();
        for fragment in ["a\n", "L\n", "B\n", "R\n"] {
            assert!(all.contains(fragment), "{fragment:?} was dropped");
        }
    }
}
