/// Direction for find-char motions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindDir {
    Forward,
    Backward,
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
    Goto,
    /// Waiting for target char after f/t/F/T.
    FindChar { dir: FindDir, till: bool },
    /// Buffer search — typing builds the query; Enter confirms, Esc cancels.
    Search { forward: bool },
    /// Notebook cell-navigation mode — j/k move between cells, o/e/d etc.
    Notebook,
    /// Label-jump mode — visible word starts are labelled; type label to jump.
    Jump,
}

impl Mode {
    /// Short label shown in the status bar.
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Insert => "INS",
            Mode::Select => "SEL",
            Mode::Command => "CMD",
            Mode::Goto => "GTO",
            Mode::FindChar { .. } => "FND",
            Mode::Search { .. } => "SRC",
            Mode::Notebook => "NB ",
            Mode::Jump => "JMP",
        }
    }
}
