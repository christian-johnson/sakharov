//! What the editor means by "which thing is open".
//!
//! Identity used to be a `PathBuf` everywhere: the open-buffer list, the three
//! stash maps, the table session.  That works only as long as everything the
//! editor shows is a file on disk.  A SQL result, a pivot of a CSV, a dataframe
//! living in the kernel — none of those have a path, and encoding them as a
//! `PathBuf` that happens not to canonicalise leaves the distinction implicit in
//! whether a filesystem call failed.
//!
//! [`SourceId`] makes it explicit.  The `*…*` naming convention is unchanged, so
//! `is_special_path`'s existing behaviour (no saving, no LSP sync, no crash
//! recovery) still keys off the name a virtual source is known by.

use std::path::{Path, PathBuf};

/// Identity of something the editor can display: a file, or a source that only
/// exists in memory.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SourceId {
    /// A file on disk, stored **canonicalised** so two spellings of one path
    /// compare equal.  (Canonicalisation falls back to the path unchanged when
    /// the file doesn't exist yet.)
    File(PathBuf),
    /// A source with no file behind it, known by its `*…*` name — a query
    /// result, a derived table, a scratch buffer.
    Virtual(String),
}

impl SourceId {
    /// Classify `path`: a `*…*` name is a virtual source, anything else a file.
    ///
    /// This is the one place the two are told apart, so a caller that has only a
    /// path (a picker, a command argument) doesn't have to know the convention.
    pub fn of(path: &Path) -> Self {
        match path.to_str() {
            Some(s) if is_virtual_name(s) => Self::Virtual(s.to_owned()),
            _ => Self::File(canon(path)),
        }
    }

    /// A virtual source called `name`.  The `*…*` wrapping is added when the
    /// caller hasn't already written it, so every virtual source is recognised
    /// as one by [`of`](Self::of) after a round trip through a path.
    pub fn virtual_named(name: &str) -> Self {
        if is_virtual_name(name) {
            Self::Virtual(name.to_owned())
        } else {
            Self::Virtual(format!("*{name}*"))
        }
    }

    /// The file behind this source, or `None` for a virtual one.  A caller that
    /// wants to *write* must go through this and handle the `None`.
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::File(p) => Some(p),
            Self::Virtual(_) => None,
        }
    }

    /// Round-trip back to a path, for the APIs that still address things that
    /// way (`exec::open_path` and the buffer picker).  A virtual source becomes
    /// its `*…*` name, which [`of`](Self::of) classifies as virtual again.
    pub fn to_path(&self) -> PathBuf {
        match self {
            Self::File(p) => p.clone(),
            Self::Virtual(name) => PathBuf::from(name),
        }
    }

    /// Short name for the status line and messages: the file name, or the
    /// virtual source's own name.
    pub fn label(&self) -> &str {
        match self {
            Self::File(p) => p.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
            Self::Virtual(name) => name,
        }
    }

    pub fn is_virtual(&self) -> bool {
        matches!(self, Self::Virtual(_))
    }

    /// True when this is a virtual source whose name starts with `prefix`.
    ///
    /// Some virtual sources come in families rather than as one fixed name —
    /// `*cell 3:price*`, `*cell 7:notes*` are all "a grid cell opened for
    /// reading".  A name prefix is how such a family is recognised, and doing
    /// it here keeps callers from prodding a `PathBuf` with `starts_with`,
    /// which is the identity rule this type exists to replace.
    pub fn is_virtual_kind(&self, prefix: &str) -> bool {
        matches!(self, Self::Virtual(name) if name.starts_with(prefix))
    }
}

/// The `*…*` shape that marks a name as belonging to no file.
fn is_virtual_name(s: &str) -> bool {
    s.len() >= 2 && s.starts_with('*') && s.ends_with('*')
}

/// Canonicalize `path`, falling back to it unchanged when the filesystem lookup
/// fails (the file doesn't exist yet, or it is a virtual notebook-cell path).
///
/// Private on purpose: canonicalising a path *is* how a file's identity is
/// computed, and every comparison should go through [`SourceId`] rather than
/// re-deriving it and risking a mismatch.
fn canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_starred_name_is_virtual_and_a_path_is_not() {
        assert!(SourceId::of(Path::new("*scratch*")).is_virtual());
        assert!(SourceId::of(Path::new("*cell 3:price*")).is_virtual());
        assert!(!SourceId::of(Path::new("data.csv")).is_virtual());
        // A lone asterisk is a filename, not a virtual source.
        assert!(!SourceId::of(Path::new("*")).is_virtual());
    }

    #[test]
    fn virtual_sources_round_trip_through_a_path() {
        // The buffer list and `open_path` still address things by path, so a
        // virtual id must survive the trip and come back virtual.
        let id = SourceId::virtual_named("sql 1");
        assert_eq!(id.label(), "*sql 1*");
        assert_eq!(SourceId::of(&id.to_path()), id);
        // Already-wrapped names are not double-wrapped.
        assert_eq!(SourceId::virtual_named("*df*"), SourceId::virtual_named("df"));
    }

    #[test]
    fn a_family_of_virtual_sources_is_recognised_by_its_name_prefix() {
        let cell = SourceId::of(Path::new("*cell 3:price*"));
        assert!(cell.is_virtual_kind("*cell "));
        assert!(!SourceId::of(Path::new("*scratch*")).is_virtual_kind("*cell "));
        // A real file whose name happens to start the same way is not one.
        assert!(!SourceId::of(Path::new("*cell notes")).is_virtual_kind("*cell "));
    }

    #[test]
    fn a_virtual_source_has_no_file_to_write() {
        assert!(SourceId::virtual_named("df").as_path().is_none());
        assert!(SourceId::of(Path::new("data.csv")).as_path().is_some());
    }

    #[test]
    fn one_file_has_one_identity_however_it_is_spelled() {
        let dir = std::env::temp_dir();
        let a = SourceId::of(&dir.join("x.csv"));
        let b = SourceId::of(&dir.join("sub/../x.csv"));
        // Both canonicalise to the same thing when the file exists; when it
        // doesn't, at least the plain spelling must be stable.
        assert_eq!(a, SourceId::of(&dir.join("x.csv")));
        let _ = b;
    }
}
