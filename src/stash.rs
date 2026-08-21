//! What a view left behind when the user navigated away from it.
//!
//! Buffer switching must not lose work: a notebook keeps its unsaved cells, a
//! file keeps its edits *and* its undo history, a grid keeps its parse and its
//! cursor cell.  So every view hands its state to [`Stashes`] on the way out
//! (`exec::buffers::teardown_current_buffer`) and asks for it back on the way
//! in (`exec::buffers::open_path`).
//!
//! This used to be three parallel `HashMap<SourceId, _>` on `App` —
//! `file_buffers`, `notebook_buffers`, `table_buffers` — which had two
//! problems.  Closing a buffer had to remember to remove the id from all three
//! (`:bd` did; anything new would have had to), and nothing stopped one source
//! being stashed in two of them at once, in which case which one came back
//! depended on the order `open_path` happened to check.
//!
//! One map keyed by [`SourceId`] fixes both: a source has exactly one stash,
//! and its variant says which view it belongs to.  Adding a view means adding
//! a variant, and the compiler then points at every place that has to handle
//! it.

use std::collections::HashMap;

use crate::{
    buffer::Buffer,
    notebook::Notebook,
    notebook_state::NotebookState,
    source::SourceId,
    view::View,
};

/// One view's saved state.
///
/// Boxed where the payload is large: a `Notebook` carries every cell's source
/// and outputs, and the enum is only ever as big as its largest variant.
pub enum Stash {
    /// A plain text buffer, with its unsaved edits and undo history.
    File(Box<Buffer>),
    /// A notebook and its UI state (focused cell, scroll anchor, exec queue).
    Notebook(Box<(Notebook, NotebookState)>),
    /// A tabular data session: the parsed source, transform stack and cursor.
    Table(Box<crate::exec::table::Session>),
}

impl Stash {
    /// Which view this stash belongs to — what `open_path` needs in order to
    /// route a stashed source back to the view it came from.
    pub fn view(&self) -> View {
        match self {
            Stash::File(_) => View::Text,
            Stash::Notebook(_) => View::Notebook,
            Stash::Table(_) => View::Table,
        }
    }

    /// The notebook behind this stash, if it is one.  Mutable because output
    /// from a cell left running streams into the notebook that asked for it,
    /// which may by then have been navigated away from.
    pub fn as_notebook_mut(&mut self) -> Option<&mut (Notebook, NotebookState)> {
        match self {
            Stash::Notebook(nb) => Some(nb),
            _ => None,
        }
    }
}

/// Every view's stashed state, keyed by what it is a stash *of*.
#[derive(Default)]
pub struct Stashes(HashMap<SourceId, Stash>);

impl Stashes {
    /// Stash `state` under `id`, replacing any previous stash for that source.
    pub fn put(&mut self, id: SourceId, state: Stash) {
        self.0.insert(id, state);
    }

    /// Take the stash for `id`, removing it.
    pub fn take(&mut self, id: &SourceId) -> Option<Stash> {
        self.0.remove(id)
    }

    /// Drop the stash for `id` without restoring it — closing a buffer, or a
    /// deliberate exit that should re-read the source next time.
    ///
    /// One call, not one per view: this is what `:bd` used to have to
    /// remember to do to three separate maps.
    pub fn discard(&mut self, id: &SourceId) {
        self.0.remove(id);
    }

    /// Which view `id`'s stash belongs to, if it has one.
    pub fn view_of(&self, id: &SourceId) -> Option<View> {
        self.0.get(id).map(Stash::view)
    }

    #[cfg(test)]
    pub fn contains(&self, id: &SourceId) -> bool {
        self.0.contains_key(id)
    }

    /// Mutable access to a stashed notebook — see [`Stash::as_notebook_mut`].
    pub fn notebook_mut(&mut self, id: &SourceId) -> Option<&mut (Notebook, NotebookState)> {
        self.0.get_mut(id).and_then(Stash::as_notebook_mut)
    }

    /// Every stashed plain-file buffer, with the source it belongs to.
    pub fn files(&self) -> impl Iterator<Item = (&SourceId, &Buffer)> {
        self.0.iter().filter_map(|(id, s)| match s {
            Stash::File(buf) => Some((id, buf.as_ref())),
            _ => None,
        })
    }

    /// Every stashed notebook, with the source it belongs to.
    pub fn notebooks(&self) -> impl Iterator<Item = (&SourceId, &Notebook)> {
        self.0.iter().filter_map(|(id, s)| match s {
            Stash::Notebook(nb) => Some((id, &nb.0)),
            _ => None,
        })
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_stash(name: &str) -> (SourceId, Stash) {
        let id = SourceId::virtual_named(name);
        let source = crate::table::csv::CsvSource::from_reader(
            "a,b\n1,2\n".as_bytes(),
            b',',
            &crate::config::TableConfig::default(),
        )
        .expect("fixture parses");
        let session = crate::exec::table::Session::new(id.clone(), Box::new(source));
        (id, Stash::Table(Box::new(session)))
    }

    /// The reason for one map rather than three: a source has exactly one
    /// stash, so there is never a question of which view it comes back as.
    #[test]
    fn stashing_a_source_twice_keeps_only_the_latest() {
        let mut stashes = Stashes::default();
        let (id, table) = table_stash("one");
        stashes.put(id.clone(), table);
        assert_eq!(stashes.view_of(&id), Some(View::Table));

        stashes.put(id.clone(), Stash::File(Box::new(Buffer::new_empty())));
        assert_eq!(stashes.view_of(&id), Some(View::Text));
        assert_eq!(stashes.len(), 1, "one source, one stash");
    }

    /// `:bd` used to have to remove the id from all three maps by hand.
    #[test]
    fn discarding_removes_a_stash_whatever_view_it_came_from() {
        let mut stashes = Stashes::default();
        let (id, table) = table_stash("two");
        stashes.put(id.clone(), table);
        stashes.discard(&id);
        assert!(!stashes.contains(&id));
        assert_eq!(stashes.view_of(&id), None);
    }

    #[test]
    fn typed_iteration_sees_only_its_own_kind() {
        let mut stashes = Stashes::default();
        let (table_id, table) = table_stash("three");
        stashes.put(table_id, table);
        let file_id = SourceId::of(std::path::Path::new("notes.txt"));
        stashes.put(file_id.clone(), Stash::File(Box::new(Buffer::new_empty())));

        let files: Vec<_> = stashes.files().map(|(id, _)| id.clone()).collect();
        assert_eq!(files, vec![file_id]);
        assert_eq!(stashes.notebooks().count(), 0);
    }
}
