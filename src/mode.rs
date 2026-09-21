/// Direction for find-char motions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindDir {
    Forward,
    Backward,
}

/// What a minibuffer text `Prompt` is collecting a filename for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// Create a new plain file with the entered name.
    NewFile,
    /// Create a new `.ipynb` notebook with the entered name.
    NewNotebook,
    /// Filter the table's cursor column (`> 100`, `= oslo`, `~ osl`, `null`).
    TableFilter,
    /// Group the table by its cursor column, with optional aggregates.
    TableGroupBy,
    /// Path (and optional alias) of a local database file to attach read-only.
    Attach,
    /// Message for the commit the version-control view is about to make.
    VcsCommit,
    /// Name for a new branch at the selected commit.
    VcsBranch,
    /// `remote/branch` the current branch should track.
    VcsUpstream,
}

/// Editor mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Default mode: motions move a point selection.
    Normal,
    /// Text insertion mode.
    Insert,
    /// Visual selection: motions extend selection head, anchor stays fixed.
    Select,
    /// Bottom command line, ':' prefix.
    Command,
    /// Waiting for second key after 'g'.
    /// `extend` is true when entered from Select mode — motions extend the selection.
    Goto { extend: bool },
    /// Waiting for target char after f/t/F/T.
    FindChar { dir: FindDir, till: bool },
    /// Buffer search — typing builds the query; Enter confirms, Esc cancels.
    Search { forward: bool },
    /// Label-jump mode — visible word starts are labelled; type label to jump.
    /// `extend` is true when entered from Select mode — the jump extends the selection.
    Jump { extend: bool },
    /// Waiting for second key after 'z' (fold operations).
    Fold,
    /// Waiting for the rest of a `m` text-object gesture.  `scope` is `None`
    /// until `i` or `o` says whether the delimiters count; `from_select`
    /// remembers that cancelling should leave the selection alone.
    Match { scope: Option<crate::textobject::Scope>, from_select: bool },
    /// Minibuffer text prompt — typing builds a filename; Enter confirms, Esc cancels.
    Prompt { kind: PromptKind },
}

impl Mode {
    /// Short label shown in the status bar.
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Insert => "INS",
            Mode::Select => "SEL",
            Mode::Command => "CMD",
            Mode::Goto { .. } => "GTO",
            Mode::FindChar { .. } => "FND",
            Mode::Search { .. } => "SRC",
            Mode::Jump { .. } => "JMP",
            Mode::Fold => "FLD",
            Mode::Match { .. } => "MCH",
            Mode::Prompt { .. } => "CMD",
        }
    }
}
