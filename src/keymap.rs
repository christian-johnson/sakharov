//! Key bindings: every key, in every view, in one table.
//!
//! A binding maps a key *sequence* (`g d`, `z a`, `ctrl+s`) to commands, in a
//! [`Layer`].  Prefix keys (`g`, `z`) are ordinary bindings whose second key is
//! looked up here too, so the which-key popups, the view key sheets and the
//! palette's key labels are all generated from this table — rebinding a key in
//! `[keys.<layer>]` changes every place it is shown.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::command::Command;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyBinding {
    pub fn key(code: KeyCode) -> Self {
        Self { code, modifiers: KeyModifiers::NONE }
    }

    pub fn char(c: char) -> Self {
        Self::key(KeyCode::Char(c))
    }

    /// Parse one key: a single character (`j`, `-`, `?`), a key name (`Enter`,
    /// `Space`, `PgUp`), optionally after modifiers (`ctrl+d`, `C-d`, `alt+x`).
    pub fn parse(s: &str) -> Option<Self> {
        let mut rest = s.trim();
        let mut modifiers = KeyModifiers::NONE;
        // A lone char is the key itself, even `-` or `+`.
        while rest.chars().count() > 1 {
            let Some((head, tail)) = rest.split_once(['+', '-']) else { break };
            if tail.is_empty() {
                break;
            }
            modifiers |= match head.to_lowercase().as_str() {
                "ctrl" | "control" | "c" => KeyModifiers::CONTROL,
                "alt" | "meta" | "a" | "m" => KeyModifiers::ALT,
                "shift" | "s" => KeyModifiers::SHIFT,
                _ => return None,
            };
            rest = tail;
        }
        let code = match rest.to_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backspace" | "bs" => KeyCode::Backspace,
            "space" | "spc" => KeyCode::Char(' '),
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "insert" => KeyCode::Insert,
            "delete" | "del" => KeyCode::Delete,
            _ => {
                let mut chars = rest.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => KeyCode::Char(c),
                    _ => return None,
                }
            }
        };
        Some(Self { code, modifiers })
    }
}

impl From<KeyEvent> for KeyBinding {
    fn from(ev: KeyEvent) -> Self {
        // Strip SHIFT from char keys (crossterm sometimes sets it for uppercase)
        let modifiers = if matches!(ev.code, KeyCode::Char(_)) {
            ev.modifiers & !KeyModifiers::SHIFT
        } else {
            ev.modifiers
        };
        Self { code: ev.code, modifiers }
    }
}

/// Parse a space-separated key sequence: `"g d"`, `"ctrl+s"`, `"Space"`.
pub fn parse_seq(s: &str) -> Option<Vec<KeyBinding>> {
    let seq: Option<Vec<_>> = s.split_whitespace().map(KeyBinding::parse).collect();
    seq.filter(|s| !s.is_empty())
}

/// Which set of bindings a key is looked up in.
///
/// `Normal` and `Select` are the modes; the rest are **override layers** that
/// shadow `Normal` for as long as something particular is on screen, falling
/// back to it for every key they don't claim (see [`Keymap::lookup_layered`]).
/// A layer is deliberately small — the handful of keys whose meaning genuinely
/// changes — because everything else should keep working as it does elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    Normal,
    Select,
    /// While a notebook is open: `N`/`M` move between cells, `J`/`K` page.
    Notebook,
    /// While the data grid is open: `J` pages, `K` peeks the cell.
    Table,
    /// In a `*cell …*` buffer: `q` returns to the grid it was read out of.
    Cell,
    /// In the `*sql*` buffer: `q` leaves for wherever `:sql` was invoked from.
    Sql,
    /// In a `*commit …*` buffer: `q` returns to the graph it was read from.
    Commit,
    /// While the version-control graph is open: `Space` grabs and drops, and
    /// the everyday git actions sit on single letters.
    Vcs,
    /// The merge-conflict resolver.
    Conflict,
    /// A `*conflict …*` buffer — one section being hand-edited.  A buffer
    /// layer rather than a view one: it really is an ordinary text buffer,
    /// with `q` overridden to take the text back.
    ConflictEdit,
}

impl Layer {
    const ALL: [Layer; 10] = [
        Layer::Normal,
        Layer::Select,
        Layer::Notebook,
        Layer::Table,
        Layer::Cell,
        Layer::Sql,
        Layer::Commit,
        Layer::Vcs,
        Layer::Conflict,
        Layer::ConflictEdit,
    ];

    /// The `[keys.<name>]` table this layer is configured by.
    pub fn name(self) -> &'static str {
        match self {
            Layer::Normal => "normal",
            Layer::Select => "select",
            Layer::Notebook => "notebook",
            Layer::Table => "table",
            Layer::Cell => "cell",
            Layer::Sql => "sql",
            Layer::Commit => "commit",
            Layer::Vcs => "vcs",
            Layer::Conflict => "conflict",
            Layer::ConflictEdit => "conflict-edit",
        }
    }

    fn from_name(name: &str) -> Option<Layer> {
        Layer::ALL.into_iter().find(|l| l.name() == name.replace('_', "-"))
    }

    /// The layer a key falls back to when this one doesn't bind it.
    fn parent(self) -> Option<Layer> {
        match self {
            Layer::Normal | Layer::Select => None,
            _ => Some(Layer::Normal),
        }
    }
}

/// What a key sequence does, and how a which-key popup describes it.
#[derive(Debug, Clone)]
pub struct Binding {
    pub cmds: Vec<Command>,
    /// Popup text; `None` falls back to the command's palette description.
    pub label: Option<String>,
}

impl Binding {
    /// The text a popup shows for this binding.
    pub fn describe(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        self.cmds
            .first()
            .map(|c| crate::command::describe(c.name()).unwrap_or(c.name()).to_string())
            .unwrap_or_default()
    }
}

/// One layer's bindings, in the order they were added (which is the order
/// popups list them in).
#[derive(Default)]
struct LayerMap {
    order: Vec<Vec<KeyBinding>>,
    bindings: HashMap<Vec<KeyBinding>, Binding>,
}

impl LayerMap {
    fn insert(&mut self, seq: Vec<KeyBinding>, binding: Binding) {
        if self.bindings.insert(seq.clone(), binding).is_none() {
            self.order.push(seq);
        }
    }

    fn iter(&self) -> impl Iterator<Item = (&Vec<KeyBinding>, &Binding)> {
        self.order.iter().map(|s| (s, &self.bindings[s]))
    }
}

pub struct Keymap {
    layers: HashMap<Layer, LayerMap>,
}

/// The built-in bindings: `(layers, keys, command, popup label)`.  A label
/// is only given where the palette description doesn't read right in a
/// which-key popup, or where the view gives the command a meaning of its own.
fn defaults() -> Vec<(&'static [Layer], &'static str, Command, Option<&'static str>)> {
    use Command as C;
    use Layer::*;
    const BOTH: &[Layer] = &[Normal, Select];
    const N: &[Layer] = &[Normal];
    const S: &[Layer] = &[Select];
    vec![
        // --- Motions and edits, in Normal and Select ---
        (BOTH, "h", C::MoveLeft, None),
        (BOTH, "Left", C::MoveLeft, None),
        (BOTH, "l", C::MoveRight, None),
        (BOTH, "Right", C::MoveRight, None),
        (BOTH, "j", C::MoveDown, None),
        (BOTH, "Down", C::MoveDown, None),
        (BOTH, "k", C::MoveUp, None),
        (BOTH, "Up", C::MoveUp, None),
        (BOTH, "w", C::MoveWordForward, None),
        (BOTH, "b", C::MoveWordBackward, None),
        (BOTH, "e", C::MoveWordEnd, None),
        (BOTH, "W", C::MoveBigWordForward, None),
        (BOTH, "B", C::MoveBigWordBackward, None),
        (BOTH, "E", C::MoveBigWordEnd, None),
        (BOTH, "0", C::MoveLineStart, None),
        (BOTH, "^", C::MoveLineFirstNonWs, None),
        (BOTH, "$", C::MoveLineEnd, None),
        (BOTH, "G", C::GotoFileEnd, None),
        (BOTH, "PgUp", C::PageUp, None),
        (BOTH, "PgDn", C::PageDown, None),
        (BOTH, "ctrl+u", C::PageUp, None),
        (BOTH, "ctrl+d", C::PageDown, None),
        (BOTH, "g", C::EnterGotoMode, None),
        (BOTH, "f", C::FindCharForward, None),
        (BOTH, "t", C::TillCharForward, None),
        (BOTH, "F", C::FindCharBackward, None),
        (BOTH, "T", C::TillCharBackward, None),
        (BOTH, "x", C::SelectLine, None),
        (BOTH, "%", C::SelectAll, None),
        (BOTH, "d", C::DeleteSelection, None),
        (BOTH, "c", C::ChangeSelection, None),
        (BOTH, "y", C::YankSelection, None),
        (BOTH, "p", C::PasteAfter, None),
        (BOTH, "P", C::PasteBefore, None),
        (BOTH, "u", C::Undo, None),
        (BOTH, "U", C::Redo, None),
        (BOTH, ">", C::IndentRegion, None),
        (BOTH, "<", C::DedentRegion, None),
        (BOTH, "ctrl+>", C::IndentRegion, None),
        (BOTH, "ctrl+<", C::DedentRegion, None),
        (BOTH, "Space", C::OpenCommandPalette, None),
        // `miw` from a selection replaces it with the word.
        (BOTH, "m", C::EnterMatchMode, None),
        (S, "Esc", C::EnterNormal, None),
        // --- Normal only ---
        (N, "/", C::SearchForward, None),
        (N, "?", C::SearchBackward, None),
        (N, "n", C::SearchNext, None),
        (N, "N", C::SearchPrev, None),
        (N, "ctrl+n", C::SearchNext, None),
        (N, "ctrl+p", C::SearchPrev, None),
        (N, "ctrl+f", C::GrepBuffer, None),
        (N, "ctrl+g", C::GrepProject, None),
        (N, "ctrl+o", C::OpenFilePicker, None),
        (N, "z", C::EnterFoldMode, None),
        // Kept for muscle memory; `g k` is the canonical binding.
        (N, "K", C::LspShowDocumentation, None),
        (N, "H", C::BufferPrev, None),
        (N, "L", C::BufferNext, None),
        (N, "i", C::EnterInsert, None),
        (N, "a", C::EnterInsertAfter, None),
        (N, "I", C::EnterInsertAtLineStart, None),
        (N, "A", C::EnterInsertAtLineEnd, None),
        (N, "o", C::OpenLineBelow, None),
        (N, "O", C::OpenLineAbove, None),
        (N, "v", C::EnterSelect, None),
        (N, ":", C::EnterCommandMode, None),
        (N, "Esc", C::EnterNormal, None),
        (N, "ctrl+s", C::Write, None),
        (N, "ctrl+k", C::KillToEndOfLine, None),
        // --- `g`: go somewhere / show me more ---
        (N, "g g", C::GotoFileStart, Some("go to file start")),
        (N, "g e", C::GotoFileEnd, Some("go to file end")),
        (N, "g h", C::MoveLineFirstNonWs, Some("go to line first non-whitespace")),
        (N, "g l", C::MoveLineEnd, Some("go to line end")),
        (N, "g z", C::ScrollCursorCenter, Some("scroll cursor to centre")),
        (N, "g w", C::EnterJumpMode, Some("jump to label in view")),
        (N, "g b", C::OpenBufferPicker, Some("buffer picker")),
        (N, "g s", C::OpenSymbolPicker, Some("symbol picker")),
        (N, "g c", C::CommentRegion, Some("comment/uncomment selection")),
        (N, "g D", C::OpenDiagnosticPicker, Some("diagnostic picker")),
        // The kernel's namespace and the repository are reachable from every
        // view: neither is a property of whichever file happens to be open.
        (N, "g v", C::KernelVariables, Some("kernel variables")),
        (N, "g V", C::VcsOpen, Some("version control")),
        (N, "g a", C::LspCodeActions, Some("code actions  [LSP]")),
        (N, "g k", C::LspShowDocumentation, Some("show documentation  [LSP]")),
        (N, "g d", C::LspGotoDefinition, Some("go to definition  [LSP]")),
        (N, "g r", C::LspGotoReferences, Some("go to references  [LSP]")),
        (N, "g y", C::LspGotoTypeDefinition, Some("go to type definition  [LSP]")),
        (N, "g i", C::LspGotoImplementation, Some("go to implementation  [LSP]")),
        // --- `z`: folds ---
        (N, "z a", C::FoldToggle, Some("toggle fold at cursor")),
        (N, "z c", C::FoldClose, Some("close fold at cursor")),
        (N, "z o", C::FoldOpen, Some("open fold at cursor")),
        (N, "z A", C::FoldToggleAll, Some("toggle all folds")),
        (N, "z M", C::FoldCloseAll, Some("close every fold")),
        (N, "z R", C::FoldOpenAll, Some("open every fold")),
        (N, "z t", C::FoldCloseType, Some("fold every block like this one")),
        (N, "z T", C::FoldOpenType, Some("unfold every block like this one")),
        // Capitalised: `zo` is fold-open in every view.
        (N, "z O", C::NotebookToggleOutputExpand, Some("expand/collapse full cell output")),
        // --- Notebook ---
        // J / K page through the notebook, flowing across cells and outputs;
        // N / M step between cells.  Shift/Ctrl+Enter execute the focused cell
        // (handled in `input::handle_key`, so they also fire from Insert).
        (&[Notebook], "J", C::PageDown, None),
        (&[Notebook], "K", C::PageUp, None),
        (&[Notebook], "N", C::NotebookNextCell, None),
        (&[Notebook], "M", C::NotebookPrevCell, None),
        // Enter on a traceback frame (while browsing output) jumps to its line.
        (&[Notebook], "Enter", C::NotebookFollowError, None),
        // --- Table ---
        // Cell movement reuses the ordinary motions, which `exec::table`
        // reinterprets against the grid; only keys whose meaning differs are
        // here.  `K` keeps its "tell me more" meaning: the cell peek.
        (&[Table], "J", C::PageDown, None),
        (&[Table], "Enter", C::TableOpenCell, None),
        (&[Table], "y", C::TableYankCell, None),
        (&[Table], "x", C::TableYankRow, None),
        (&[Table], "S", C::TableColumnSummary, None),
        (&[Table], "F", C::TableColumnFrequency, None),
        (&[Table], "s", C::TableToggleSparkline, None),
        // Backs out of a *computed* table to the one it came from.
        (&[Table], "q", C::TableCloseDerived, None),
        (&[Table], "g g", C::GotoFileStart, Some("first row")),
        (&[Table], "g e", C::GotoFileEnd, Some("last row")),
        (&[Table], "g h", C::MoveLineFirstNonWs, Some("first column")),
        (&[Table], "g l", C::MoveLineEnd, Some("last column")),
        (&[Table], "g k", C::LspShowDocumentation, Some("peek cell text")),
        (&[Table], "g d", C::TableColumnSummary, Some("describe this column")),
        (&[Table], "g c", C::TableColumnFrequency, Some("count this column's values")),
        (&[Table], "g s", C::TableSort, Some("sort by this column")),
        (&[Table], "g f", C::TableFilter, Some("filter on this column")),
        (&[Table], "g r", C::TableGroupBy, Some("group rows by this column")),
        (&[Table], "g x", C::TableClearTransforms, Some("clear sorts and filters")),
        (&[Table], "g t", C::SchemaBrowser, Some("browse attached tables")),
        (&[Table], "g v", C::KernelVariables, Some("kernel variables")),
        (&[Table], "g V", C::VcsOpen, Some("version control")),
        (&[Table], "g b", C::OpenBufferPicker, Some("buffer picker")),
        // --- Temporary buffers `q` backs out of ---
        (&[Cell], "q", C::TableCloseCell, None),
        (&[Sql], "q", C::BufferClose, None),
        (&[Commit], "q", C::BufferClose, None),
        // Takes the hand-edited section back to the resolver.
        (&[ConflictEdit], "q", C::BufferClose, None),
        // --- Version-control graph ---
        // Space is the grab, the gesture the whole view is built around.
        (&[Vcs], "Space", C::VcsGrab, None),
        // A branch is checked out; anything else shows its commit's diff.
        (&[Vcs], "Enter", C::VcsEnter, None),
        (&[Vcs], "c", C::VcsCheckout, None),
        (&[Vcs], "d", C::VcsDrop, None),
        (&[Vcs], "m", C::VcsMerge, None),
        (&[Vcs], "r", C::VcsRefresh, None),
        // `git status`, verbatim; staging is on `+`/`-`.
        (&[Vcs], "s", C::VcsGitStatus, None),
        (&[Vcs], "+", C::VcsStage, None),
        (&[Vcs], "-", C::VcsUnstage, None),
        (&[Vcs], "w", C::VcsStatus, None),
        (&[Vcs], "?", C::VcsHelp, None),
        (&[Vcs], "b", C::VcsBranches, None),
        (&[Vcs], "x", C::VcsHideBranch, None),
        (&[Vcs], "o", C::VcsFlip, None),
        (&[Vcs], "f", C::VcsFetch, None),
        (&[Vcs], "p", C::VcsPull, None),
        (&[Vcs], "P", C::VcsPush, None),
        // Both prompt for the word they need.
        (&[Vcs], "C", C::VcsCommit(String::new()), None),
        (&[Vcs], "n", C::VcsNewBranch(String::new()), None),
        (&[Vcs], "q", C::VcsClose, None),
        (&[Vcs], "J", C::PageDown, None),
        (&[Vcs], "K", C::PageUp, None),
        (&[Vcs], "g g", C::GotoFileStart, Some("first commit")),
        (&[Vcs], "g e", C::GotoFileEnd, Some("last commit")),
        (&[Vcs], "g c", C::VcsCheckout, Some("check out the selection")),
        (&[Vcs], "g m", C::VcsMerge, Some("plan a merge into the current branch")),
        (&[Vcs], "g d", C::VcsDrop, Some("remove the selected commit")),
        (&[Vcs], "g a", C::VcsApply, Some("apply the planned changes")),
        (&[Vcs], "g x", C::VcsReset, Some("discard the planned changes")),
        // Once-after-something-went-wrong actions, deliberately not bare keys.
        (&[Vcs], "g U", C::VcsUndo, Some("put the branches back after an apply")),
        (&[Vcs], "g A", C::VcsAbort, Some("abort the run a conflict stopped")),
        (&[Vcs], "g C", C::VcsContinue, Some("carry on after resolving a conflict")),
        (&[Vcs], "g u", C::VcsSetUpstream(String::new()), Some("set this branch's upstream")),
        (&[Vcs], "g o", C::VcsOutput, Some("the last command's output")),
        (&[Vcs], "g r", C::VcsRefresh, Some("re-read the repository")),
        (&[Vcs], "g ?", C::VcsGuide, Some("what the gestures mean")),
        (&[Vcs], "g b", C::OpenBufferPicker, Some("buffer picker")),
        // --- Merge-conflict resolver ---
        // Space takes or drops the focused side; `a`/`b` name the sides and
        // their capitals answer every conflict still open.
        (&[Conflict], "Space", C::ConflictTakeSide, None),
        (&[Conflict], "a", C::ConflictTakeLeft, None),
        (&[Conflict], "b", C::ConflictTakeRight, None),
        (&[Conflict], "A", C::ConflictTakeLeftAll, None),
        (&[Conflict], "B", C::ConflictTakeRightAll, None),
        // `n`/`N` tour what is *left*; `j`/`k` (inherited) read the file.
        (&[Conflict], "n", C::ConflictNextHunk, None),
        (&[Conflict], "N", C::ConflictPrevHunk, None),
        (&[Conflict], "]", C::ConflictNextFile, None),
        (&[Conflict], "[", C::ConflictPrevFile, None),
        (&[Conflict], "3", C::ConflictToggleBase, None),
        (&[Conflict], "e", C::ConflictEditHunk, None),
        (&[Conflict], "d", C::ConflictDiff, None),
        (&[Conflict], "r", C::ConflictRefresh, None),
        (&[Conflict], "?", C::ConflictHelp, None),
        // The one key here that touches the disk.
        (&[Conflict], "Enter", C::ConflictWriteFile, None),
        (&[Conflict], "q", C::ConflictClose, None),
        (&[Conflict], "g c", C::VcsContinue, Some("carry on with the operation")),
        (&[Conflict], "g a", C::VcsAbort, Some("abort the whole operation")),
        (&[Conflict], "g w", C::ConflictWriteFile, Some("write this file's resolution")),
        (&[Conflict], "g x", C::ConflictRevertFile, Some("put this file back the way git left it")),
        (&[Conflict], "g s", C::VcsGitStatus, Some("git status, verbatim")),
        (&[Conflict], "g r", C::ConflictRefresh, Some("re-read the conflicted files")),
        (&[Conflict], "g V", C::VcsOpen, Some("the commit graph")),
        (&[Conflict], "g ?", C::ConflictHelp, Some("the keys")),
        (&[Conflict], "g b", C::OpenBufferPicker, Some("buffer picker")),
    ]
}

impl Keymap {
    /// The built-in bindings for every layer.
    pub fn default_bindings() -> Self {
        let mut keymap = Keymap { layers: HashMap::new() };
        for (layers, keys, cmd, label) in defaults() {
            let seq = parse_seq(keys).unwrap_or_else(|| panic!("BUG: bad default key {keys:?}"));
            for &layer in layers {
                keymap.bind(
                    layer,
                    seq.clone(),
                    Binding { cmds: vec![cmd.clone()], label: label.map(str::to_string) },
                );
            }
        }
        keymap
    }

    /// Bind `seq` in `layer`, replacing what it did before.
    pub fn bind(&mut self, layer: Layer, seq: Vec<KeyBinding>, binding: Binding) {
        self.layers.entry(layer).or_default().insert(seq, binding);
    }

    fn get(&self, layer: Layer, seq: &[KeyBinding]) -> Option<&Binding> {
        self.layers.get(&layer)?.bindings.get(seq)
    }

    /// Look `seq` up in `layer`, falling back to `Normal` for override layers.
    ///
    /// The fallback is the whole point of a layer: a view overrides the few
    /// keys whose meaning changes there and inherits the rest, so `:w`, the
    /// palette and buffer switching keep working without every layer having to
    /// restate them.
    pub fn lookup_layered(&self, layer: Layer, seq: &[KeyBinding]) -> Option<&[Command]> {
        self.get(layer, seq)
            .or_else(|| layer.parent().and_then(|p| self.get(p, seq)))
            .map(|b| b.cmds.as_slice())
    }

    /// What can follow `prefix` in `layer`: `(next key, binding)` in table
    /// order.  With `inherit`, keys the layer doesn't bind itself come from
    /// its parent layer too.
    pub fn continuations(
        &self,
        layer: Layer,
        prefix: &[KeyBinding],
        inherit: bool,
    ) -> Vec<(KeyBinding, &Binding)> {
        let mut out: Vec<(KeyBinding, &Binding)> = Vec::new();
        let parent = layer.parent().filter(|_| inherit);
        for l in std::iter::once(layer).chain(parent) {
            let Some(map) = self.layers.get(&l) else { continue };
            for (seq, binding) in map.iter() {
                if seq.len() == prefix.len() + 1 && seq.starts_with(prefix) {
                    let key = seq[prefix.len()].clone();
                    if !out.iter().any(|(k, _)| *k == key) {
                        out.push((key, binding));
                    }
                }
            }
        }
        out
    }

    /// The first key sequence that runs `cmd_name` in `layer` (or, failing
    /// that, its parent), formatted for display — e.g. `"g x"`, `"C-s"`.
    pub fn keys_for(&self, layer: Layer, cmd_name: &str) -> Option<String> {
        std::iter::once(layer).chain(layer.parent()).find_map(|l| {
            let map = self.layers.get(&l)?;
            map.iter()
                .find(|(_, b)| b.cmds.len() == 1 && b.cmds[0].name() == cmd_name)
                .map(|(seq, _)| format_seq(seq))
        })
    }

    /// Every key sequence that runs `cmd_name`, in any layer, deduplicated and
    /// capped at a few — the palette's label.
    pub fn all_keys_for(&self, cmd_name: &str) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        for layer in Layer::ALL {
            let Some(map) = self.layers.get(&layer) else { continue };
            for (seq, binding) in map.iter() {
                if binding.cmds.len() == 1 && binding.cmds[0].name() == cmd_name {
                    let k = format_seq(seq);
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
            }
        }
        keys.truncate(4);
        keys
    }

    /// Apply the user's `[keys.<layer>]` tables.  Returns a warning per entry
    /// whose layer, keys or command could not be understood (it is skipped).
    pub fn apply_custom_bindings(&mut self, keys: &crate::config::KeysConfig) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut tables: Vec<_> = keys.0.iter().collect();
        tables.sort_by_key(|(name, _)| name.as_str());
        for (table, entries) in tables {
            let Some(layer) = Layer::from_name(table) else {
                warnings.push(format!("[keys.{table}] is not a key layer"));
                continue;
            };
            let mut entries: Vec<_> = entries.iter().collect();
            entries.sort();
            for (key_str, cmd_str) in entries {
                match (parse_seq(key_str), Command::parse(cmd_str)) {
                    (Some(seq), Some(cmd)) => {
                        self.bind(layer, seq, Binding { cmds: vec![cmd], label: None });
                    }
                    (None, _) => warnings.push(format!("[keys.{table}] unknown key {key_str:?}")),
                    (_, None) => {
                        warnings.push(format!("[keys.{table}] unknown command {cmd_str:?}"));
                    }
                }
            }
        }
        warnings
    }
}

/// Format a key sequence for display: its keys separated by spaces.
pub fn format_seq(seq: &[KeyBinding]) -> String {
    seq.iter().map(format_key_binding).collect::<Vec<_>>().join(" ")
}

/// Format a key binding as a short human-readable hint, e.g. "C-o", "Space", "Enter".
pub fn format_key_binding(kb: &KeyBinding) -> String {
    let ctrl = kb.modifiers.contains(KeyModifiers::CONTROL);
    let alt  = kb.modifiers.contains(KeyModifiers::ALT);

    let key = match &kb.code {
        KeyCode::Char(' ')  => "Space".to_string(),
        KeyCode::Char(c)    => c.to_string(),
        KeyCode::Enter      => "Enter".to_string(),
        KeyCode::Esc        => "Esc".to_string(),
        KeyCode::Backspace  => "BS".to_string(),
        KeyCode::Tab        => "Tab".to_string(),
        KeyCode::Delete     => "Del".to_string(),
        KeyCode::Up         => "Up".to_string(),
        KeyCode::Down       => "Down".to_string(),
        KeyCode::Left       => "Left".to_string(),
        KeyCode::Right      => "Right".to_string(),
        KeyCode::PageUp     => "PgUp".to_string(),
        KeyCode::PageDown   => "PgDn".to_string(),
        KeyCode::Home       => "Home".to_string(),
        KeyCode::End        => "End".to_string(),
        KeyCode::F(n)       => format!("F{}", n),
        _                   => "?".to_string(),
    };

    match (ctrl, alt) {
        (true,  true)  => format!("C-M-{}", key),
        (true,  false) => format!("C-{}", key),
        (false, true)  => format!("M-{}", key),
        (false, false) => key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(table: &str, entries: &[(&str, &str)]) -> crate::config::KeysConfig {
        let mut cfg = crate::config::KeysConfig::default();
        cfg.0.insert(
            table.into(),
            entries.iter().map(|(k, c)| ((*k).into(), (*c).into())).collect(),
        );
        cfg
    }

    #[test]
    fn bad_custom_bindings_are_reported_not_dropped_silently() {
        let cfg = keys("normal", &[("C-q", "no-such-command"), ("C-w", "write")]);
        let mut unknown_layer = keys("nromal", &[("x", "write")]);
        unknown_layer.0.extend(cfg.0);

        let warnings = Keymap::default_bindings().apply_custom_bindings(&unknown_layer);

        assert_eq!(
            warnings,
            vec![
                r#"[keys.normal] unknown command "no-such-command""#.to_string(),
                "[keys.nromal] is not a key layer".to_string(),
            ]
        );
    }

    /// A prefix binding rebound in the config is what dispatches and what the
    /// popup and palette show.
    #[test]
    fn rebinding_a_prefix_key_updates_dispatch_and_labels() {
        let mut keymap = Keymap::default_bindings();
        keymap.apply_custom_bindings(&keys("normal", &[("g n", "goto-file-end")]));
        let g = KeyBinding::char('g');

        let run = keymap.lookup_layered(Layer::Notebook, &[g.clone(), KeyBinding::char('n')]);
        let hinted = keymap
            .continuations(Layer::Normal, &[g], true)
            .iter()
            .any(|(k, b)| *k == KeyBinding::char('n') && b.cmds[0].name() == "goto-file-end");

        assert_eq!(run.map(|c| c[0].name()), Some("goto-file-end"));
        assert!(hinted);
        assert!(keymap.all_keys_for("goto-file-end").contains(&"g n".to_string()));
    }

    #[test]
    fn a_view_layer_shadows_normal_and_inherits_the_rest() {
        let keymap = Keymap::default_bindings();
        let gd = [KeyBinding::char('g'), KeyBinding::char('d')];
        let gz = [KeyBinding::char('g'), KeyBinding::char('z')];

        assert_eq!(keymap.lookup_layered(Layer::Table, &gd).unwrap()[0].name(), "column-summary");
        assert_eq!(keymap.lookup_layered(Layer::Table, &gz).unwrap()[0].name(), "scroll-cursor-center");
        assert_eq!(keymap.keys_for(Layer::Vcs, "version-control-reset").as_deref(), Some("g x"));
    }

    #[test]
    fn test_key_binding_parse() {
        let ctrl = |c| KeyBinding { code: KeyCode::Char(c), modifiers: KeyModifiers::CONTROL };
        assert_eq!(KeyBinding::parse("j"), Some(KeyBinding::char('j')));
        assert_eq!(KeyBinding::parse("J"), Some(KeyBinding::char('J')));
        assert_eq!(KeyBinding::parse("-"), Some(KeyBinding::char('-')));
        assert_eq!(KeyBinding::parse("ctrl+d"), Some(ctrl('d')));
        assert_eq!(KeyBinding::parse("ctrl-u"), Some(ctrl('u')));
        assert_eq!(KeyBinding::parse("C-o"), Some(ctrl('o')));
        assert_eq!(KeyBinding::parse("ctrl+>"), Some(ctrl('>')));
        assert_eq!(KeyBinding::parse("PgUp"), Some(KeyBinding::key(KeyCode::PageUp)));
        assert_eq!(
            KeyBinding::parse("shift+escape"),
            Some(KeyBinding { code: KeyCode::Esc, modifiers: KeyModifiers::SHIFT })
        );
        assert_eq!(KeyBinding::parse("invalidkeyname"), None);
        assert_eq!(parse_seq("g  d").map(|s| format_seq(&s)).as_deref(), Some("g d"));
    }
}
