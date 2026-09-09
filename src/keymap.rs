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
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    pub fn char(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
        }
    }

    pub fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }

        let parts: Vec<&str> = s.split(['+', '-']).collect();
        let mut modifiers = KeyModifiers::NONE;
        let mut key_part = "";

        for (i, part) in parts.iter().enumerate() {
            let part = part.trim();
            if i == parts.len() - 1 {
                key_part = part;
            } else {
                match part.to_lowercase().as_str() {
                    "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                    "alt" => modifiers |= KeyModifiers::ALT,
                    "shift" => modifiers |= KeyModifiers::SHIFT,
                    _ => {}
                }
            }
        }

        let code = match key_part.to_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "space" => KeyCode::Char(' '),
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
                let chars: Vec<char> = key_part.chars().collect();
                if chars.len() == 1 {
                    KeyCode::Char(chars[0])
                } else {
                    return None;
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
        Self {
            code: ev.code,
            modifiers,
        }
    }
}

/// Which set of bindings a key is looked up in.
///
/// `Normal` and `Select` are the modes; the rest are **override layers** that
/// shadow `Normal` for as long as something particular is on screen, falling
/// back to it for every key they don't claim (see [`Keymap::lookup_layered`]).
/// A layer is deliberately small — the handful of keys whose meaning genuinely
/// changes — because everything else should keep working as it does elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

pub struct Keymap {
    normal: HashMap<KeyBinding, Vec<Command>>,
    select: HashMap<KeyBinding, Vec<Command>>,
    notebook: HashMap<KeyBinding, Vec<Command>>,
    table: HashMap<KeyBinding, Vec<Command>>,
    commit: HashMap<KeyBinding, Vec<Command>>,
    cell: HashMap<KeyBinding, Vec<Command>>,
    sql: HashMap<KeyBinding, Vec<Command>>,
    vcs: HashMap<KeyBinding, Vec<Command>>,
    conflict: HashMap<KeyBinding, Vec<Command>>,
    conflict_edit: HashMap<KeyBinding, Vec<Command>>,
}

impl Keymap {
    /// Build the default key bindings for Normal and Select modes, plus the
    /// notebook override map (bindings that shadow Normal while a notebook is open).
    pub fn default_bindings() -> Self {
        let mut normal: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut select: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut notebook: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut table: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut cell: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut sql: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut commit: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut vcs: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut conflict: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut conflict_edit: HashMap<KeyBinding, Vec<Command>> = HashMap::new();

        // Helper macro to insert into both maps
        macro_rules! both {
            ($key:expr, $cmd:expr) => {
                normal.insert($key.clone(), vec![$cmd.clone()]);
                select.insert($key, vec![$cmd]);
            };
        }

        // --- Motions (both Normal and Select) ---

        // h / Left → MoveLeft
        both!(KeyBinding::char('h'), Command::MoveLeft);
        both!(KeyBinding::key(KeyCode::Left), Command::MoveLeft);

        // l / Right → MoveRight
        both!(KeyBinding::char('l'), Command::MoveRight);
        both!(KeyBinding::key(KeyCode::Right), Command::MoveRight);

        // j / Down → MoveDown
        both!(KeyBinding::char('j'), Command::MoveDown);
        both!(KeyBinding::key(KeyCode::Down), Command::MoveDown);

        // k / Up → MoveUp
        both!(KeyBinding::char('k'), Command::MoveUp);
        both!(KeyBinding::key(KeyCode::Up), Command::MoveUp);

        // w → MoveWordForward
        both!(KeyBinding::char('w'), Command::MoveWordForward);

        // b → MoveWordBackward
        both!(KeyBinding::char('b'), Command::MoveWordBackward);

        // e → MoveWordEnd
        both!(KeyBinding::char('e'), Command::MoveWordEnd);

        // W → MoveBigWordForward
        both!(KeyBinding::char('W'), Command::MoveBigWordForward);

        // B → MoveBigWordBackward
        both!(KeyBinding::char('B'), Command::MoveBigWordBackward);

        // E → MoveBigWordEnd
        both!(KeyBinding::char('E'), Command::MoveBigWordEnd);

        // 0 → MoveLineStart
        both!(KeyBinding::char('0'), Command::MoveLineStart);

        // ^ → MoveLineFirstNonWs
        both!(KeyBinding::char('^'), Command::MoveLineFirstNonWs);

        // $ → MoveLineEnd
        both!(KeyBinding::char('$'), Command::MoveLineEnd);

        // G → GotoFileEnd
        both!(KeyBinding::char('G'), Command::GotoFileEnd);

        // PageUp / PageDown — half-page scroll
        both!(KeyBinding::key(KeyCode::PageUp), Command::PageUp);
        both!(KeyBinding::key(KeyCode::PageDown), Command::PageDown);
        both!(KeyBinding::ctrl('u'), Command::PageUp);
        both!(KeyBinding::ctrl('d'), Command::PageDown);

        // g → EnterGotoMode
        both!(KeyBinding::char('g'), Command::EnterGotoMode);

        // f → FindCharForward
        both!(KeyBinding::char('f'), Command::FindCharForward);

        // t → TillCharForward
        both!(KeyBinding::char('t'), Command::TillCharForward);

        // F → FindCharBackward
        both!(KeyBinding::char('F'), Command::FindCharBackward);

        // T → TillCharBackward
        both!(KeyBinding::char('T'), Command::TillCharBackward);

        // x → SelectLine
        both!(KeyBinding::char('x'), Command::SelectLine);

        // % → SelectAll
        both!(KeyBinding::char('%'), Command::SelectAll);

        // --- Edit operations (both Normal and Select) ---

        // d → DeleteSelection
        both!(KeyBinding::char('d'), Command::DeleteSelection);

        // c → ChangeSelection
        both!(KeyBinding::char('c'), Command::ChangeSelection);

        // y → YankSelection
        both!(KeyBinding::char('y'), Command::YankSelection);

        // p → PasteAfter
        both!(KeyBinding::char('p'), Command::PasteAfter);

        // P → PasteBefore
        both!(KeyBinding::char('P'), Command::PasteBefore);

        // u → Undo
        both!(KeyBinding::char('u'), Command::Undo);

        // U → Redo
        both!(KeyBinding::char('U'), Command::Redo);

        // --- Normal-mode-only bindings ---

        // Search: standard vim n/N bindings for next/prev match.
        normal.insert(KeyBinding::char('/'), vec![Command::SearchForward]);
        normal.insert(KeyBinding::char('?'), vec![Command::SearchBackward]);
        normal.insert(KeyBinding::char('n'), vec![Command::SearchNext]);
        normal.insert(KeyBinding::char('N'), vec![Command::SearchPrev]);
        // Ctrl+N/P also navigate within popups and search matches.
        normal.insert(KeyBinding::ctrl('n'), vec![Command::SearchNext]);
        normal.insert(KeyBinding::ctrl('p'), vec![Command::SearchPrev]);
        // Ctrl+F → grep buffer; Ctrl+G → grep project; Ctrl+O → file picker
        normal.insert(KeyBinding::ctrl('f'), vec![Command::GrepBuffer]);
        normal.insert(KeyBinding::ctrl('g'), vec![Command::GrepProject]);
        normal.insert(KeyBinding::ctrl('o'), vec![Command::OpenFilePicker]);

        // > / < (and Ctrl+> / Ctrl+<) → indent / dedent the selected lines
        // (both Normal and Select)
        both!(KeyBinding::char('>'), Command::IndentRegion);
        both!(KeyBinding::char('<'), Command::DedentRegion);
        both!(KeyBinding::ctrl('>'), Command::IndentRegion);
        both!(KeyBinding::ctrl('<'), Command::DedentRegion);

        // Space opens command palette (both Normal and Select)
        let space = KeyBinding { code: KeyCode::Char(' '), modifiers: KeyModifiers::NONE };
        normal.insert(space.clone(), vec![Command::OpenCommandPalette]);
        select.insert(space, vec![Command::OpenCommandPalette]);

        // z → enter fold sub-mode
        normal.insert(KeyBinding::char('z'), vec![Command::EnterFoldMode]);

        // K → lsp-show-documentation (kept for muscle memory; gk is the canonical binding)
        normal.insert(KeyBinding::char('K'), vec![Command::LspShowDocumentation]);

        // H / L → prev/next buffer (uppercase H and L are unbound motions, repurposed here)
        normal.insert(KeyBinding::char('H'), vec![Command::BufferPrev]);
        normal.insert(KeyBinding::char('L'), vec![Command::BufferNext]);


        normal.insert(KeyBinding::char('i'), vec![Command::EnterInsert]);
        normal.insert(KeyBinding::char('a'), vec![Command::EnterInsertAfter]);
        normal.insert(
            KeyBinding::char('I'),
            vec![Command::EnterInsertAtLineStart],
        );
        normal.insert(KeyBinding::char('A'), vec![Command::EnterInsertAtLineEnd]);
        normal.insert(KeyBinding::char('o'), vec![Command::OpenLineBelow]);
        normal.insert(KeyBinding::char('O'), vec![Command::OpenLineAbove]);
        normal.insert(KeyBinding::char('v'), vec![Command::EnterSelect]);
        normal.insert(KeyBinding::char(':'), vec![Command::EnterCommandMode]);
        normal.insert(KeyBinding::key(KeyCode::Esc), vec![Command::EnterNormal]);
        normal.insert(KeyBinding::ctrl('s'), vec![Command::Write]);
        normal.insert(KeyBinding::ctrl('k'), vec![Command::KillToEndOfLine]);

        // --- Select-mode-only bindings ---

        select.insert(KeyBinding::key(KeyCode::Esc), vec![Command::EnterNormal]);

        // --- Notebook normal-mode overrides ---
        //
        // These shadow the normal-mode bindings *only* while a notebook is open
        // (and not in the full-screen cell overlay). Everything else in a
        // notebook uses the regular normal/select bindings, so editing a cell is
        // exactly like editing a plain buffer. Cell management (new/delete cell,
        // structural undo, clear outputs, cell-type conversion) is available via
        // the command palette and `:` command line.

        // J / K page through the notebook (half a screen at a time, flowing
        // across cells and output blocks) — the motion you reach for most in a
        // long notebook. N / M step to the next / previous cell.
        notebook.insert(KeyBinding::char('J'), vec![Command::PageDown]);
        notebook.insert(KeyBinding::char('K'), vec![Command::PageUp]);
        notebook.insert(KeyBinding::char('N'), vec![Command::NotebookNextCell]);
        notebook.insert(KeyBinding::char('M'), vec![Command::NotebookPrevCell]);
        // Enter on a traceback frame line (while browsing output with j/k) jumps
        // to that source line. A bare Enter is otherwise unbound in a notebook
        // (Shift/Ctrl+Enter execute the cell, handled before dispatch).
        notebook.insert(KeyBinding::key(KeyCode::Enter), vec![Command::NotebookFollowError]);
        // Shift+Enter / Ctrl+Enter execute the focused cell — handled directly in
        // input::handle_key (before mode dispatch) so they fire from Insert too.

        // --- Table (tabular data) overrides ---
        //
        // Cell movement reuses the ordinary motions (h/j/k/l, w/b, 0/$, gg/G),
        // which `exec::table` reinterprets against the grid, so only the keys
        // whose table meaning differs from their text meaning are listed here.
        // J pages half a screen, matching the notebook view.  `K` is *not* its
        // counterpart here: it keeps its editor-wide "tell me more about the
        // thing under the cursor" meaning, which in a grid is the cell peek
        // (`exec::table` routes `LspShowDocumentation` there, so `gk` peeks
        // too).  PageUp is on Ctrl-u / PgUp as everywhere else.
        table.insert(KeyBinding::char('J'), vec![Command::PageDown]);
        // Reading a cell: Enter opens its full text as its own buffer.
        // y / x copy the cell / the row — the grid has no text selection for
        // the usual yank to act on.
        table.insert(KeyBinding::key(KeyCode::Enter), vec![Command::TableOpenCell]);
        table.insert(KeyBinding::char('y'), vec![Command::TableYankCell]);
        table.insert(KeyBinding::char('x'), vec![Command::TableYankRow]);
        // Column intelligence: S describes the cursor's column, F counts its
        // values into a table of their own.  `S` is unbound in Normal mode, and
        // `F` (find-char-backward) is refused in a grid anyway — there are no
        // characters to search along a row of cells.
        table.insert(KeyBinding::char('S'), vec![Command::TableColumnSummary]);
        table.insert(KeyBinding::char('F'), vec![Command::TableColumnFrequency]);
        // Lowercase `s` toggles the distribution row `S` describes in full.
        table.insert(KeyBinding::char('s'), vec![Command::TableToggleSparkline]);
        // `q` backs out of a *computed* table (a frequency table) to the one it
        // came from — the same "back out of the temporary thing" `q` does in a
        // cell buffer.  On a file-backed table there is nowhere to go back to.
        table.insert(KeyBinding::char('q'), vec![Command::TableCloseDerived]);

        // --- `*cell …*` buffer overrides ---
        //
        // A cell buffer is an ordinary text buffer (so search, motions and wrap
        // all work), with one addition: `q` closes it and returns to the grid,
        // the way `q` backs out of a sheet in visidata. `q` is unbound in
        // Normal mode otherwise, so nothing is shadowed.
        cell.insert(KeyBinding::char('q'), vec![Command::TableCloseCell]);

        // --- `*sql*` buffer overrides ---
        //
        // Same bargain as a cell buffer: a temporary thing you back out of with
        // `q`.  Without it the query buffer is a trap — it cannot be closed
        // (`:bd` refuses every `*…*` name) and the only way out is to run
        // something.
        sql.insert(KeyBinding::char('q'), vec![Command::BufferClose]);

        // --- commit-diff buffer override ---
        //
        // `q` backs out of a commit's diff to the graph, the same gesture as
        // `q` in a `*cell …*` buffer or a derived table.
        commit.insert(KeyBinding::char('q'), vec![Command::BufferClose]);

        // --- version-control graph overrides ---
        //
        // Space is the grab: it is the one gesture the whole view is built
        // around, it is unbound in Normal mode (where it opens the palette —
        // which is still reachable here as `:`), and it is the key a user
        // reaches for to mean "pick this up".
        vcs.insert(KeyBinding::char(' '), vec![Command::VcsGrab]);
        // Enter acts on whatever is under the cursor: a branch is checked out,
        // anything else shows its commit's diff.
        vcs.insert(KeyBinding::key(KeyCode::Enter), vec![Command::VcsEnter]);
        // The everyday actions, on the letters they name.  `c`/`d`/`s`/`S`/`m`
        // are all edit commands in Normal mode, which is meaningless here.
        vcs.insert(KeyBinding::char('c'), vec![Command::VcsCheckout]);
        vcs.insert(KeyBinding::char('d'), vec![Command::VcsDrop]);
        vcs.insert(KeyBinding::char('m'), vec![Command::VcsMerge]);
        vcs.insert(KeyBinding::char('r'), vec![Command::VcsRefresh]);
        // `s` is `git status`, verbatim, in a float — the thing every git user
        // types first and the one output this view paraphrases rather than
        // shows.  Staging moves to `+`/`-`, which say what they do to the next
        // commit and are unbound everywhere else.
        vcs.insert(KeyBinding::char('s'), vec![Command::VcsGitStatus]);
        vcs.insert(KeyBinding::char('+'), vec![Command::VcsStage]);
        vcs.insert(KeyBinding::char('-'), vec![Command::VcsUnstage]);
        // `w` for the work tree: the same files as a list you can open one of.
        // It shadows the word motion, which in a graph is only another name
        // for `l`.
        vcs.insert(KeyBinding::char('w'), vec![Command::VcsStatus]);
        // `?` is the whole view explained, since nothing here is a git verb
        // and there is no command line to read the answer off.
        vcs.insert(KeyBinding::char('?'), vec![Command::VcsHelp]);
        // `b` is the branch list — which of them the graph draws — and `x`
        // takes the one under the cursor out of it.  `x` is the gesture you
        // reach for while looking at the branch you are tired of; `b` is the
        // only way back, since what is hidden is not on screen to press a key
        // on.
        vcs.insert(KeyBinding::char('b'), vec![Command::VcsBranches]);
        vcs.insert(KeyBinding::char('x'), vec![Command::VcsHideBranch]);
        // `o` turns the graph a quarter turn — which way history runs is a
        // preference, not a mode, so it is one key rather than a setting to go
        // and find.
        vcs.insert(KeyBinding::char('o'), vec![Command::VcsFlip]);
        // The remote, on the letters that name it.  These are commands the
        // view has had all along and could only be reached by typing their
        // names — which, in a view whose premise is that you never type a git
        // verb, meant they may as well not have been there.  `f`/`p` are word
        // motions in Normal mode, and a graph has no words.
        vcs.insert(KeyBinding::char('f'), vec![Command::VcsFetch]);
        vcs.insert(KeyBinding::char('p'), vec![Command::VcsPull]);
        vcs.insert(KeyBinding::char('P'), vec![Command::VcsPush]);
        // The two that make something new.  Both open a minibuffer prompt for
        // the word they need, so the bare key is the whole gesture.  `C` and
        // `n` rather than `c` and `b`, which are checkout and the branch list
        // — the keys you press far more often.
        vcs.insert(KeyBinding::char('C'), vec![Command::VcsCommit(String::new())]);
        vcs.insert(KeyBinding::char('n'), vec![Command::VcsNewBranch(String::new())]);
        // `q` backs out, as it does from a cell buffer and a derived table.
        vcs.insert(KeyBinding::char('q'), vec![Command::VcsClose]);
        // `J` pages, matching the notebook and the grid.
        vcs.insert(KeyBinding::char('J'), vec![Command::PageDown]);
        vcs.insert(KeyBinding::char('K'), vec![Command::PageUp]);

        // --- merge-conflict resolver overrides ---
        //
        // Space is the switch, as it is in the staging view and the branch
        // picker: each side of a hunk is a thing that is either in the next
        // version of the file or not, and Space is this editor's word for
        // that.  Four resolutions and a reordering out of one key, with no
        // menu to read.
        conflict.insert(KeyBinding::char(' '), vec![Command::ConflictTakeSide]);
        // `a`/`b` name the two sides, and their capitals do the same to every
        // conflict still unanswered — the "the rest are all mine" gesture that
        // is most of the work in a big mechanical conflict.
        conflict.insert(KeyBinding::char('a'), vec![Command::ConflictTakeLeft]);
        conflict.insert(KeyBinding::char('b'), vec![Command::ConflictTakeRight]);
        conflict.insert(KeyBinding::char('A'), vec![Command::ConflictTakeLeftAll]);
        conflict.insert(KeyBinding::char('B'), vec![Command::ConflictTakeRightAll]);
        // `n`/`N` tour what is *left*; `j`/`k` (inherited) read the file.  Two
        // questions, two keys — an `n` that stopped on settled hunks would be
        // the wrong tour in a file with forty conflicts and two outstanding.
        conflict.insert(KeyBinding::char('n'), vec![Command::ConflictNextHunk]);
        conflict.insert(KeyBinding::char('N'), vec![Command::ConflictPrevHunk]);
        conflict.insert(KeyBinding::char(']'), vec![Command::ConflictNextFile]);
        conflict.insert(KeyBinding::char('['), vec![Command::ConflictPrevFile]);
        // `3` for the third pane: the common ancestor, which answers "who
        // changed what" and is off by default because it costs a third of the
        // width on every conflict.
        conflict.insert(KeyBinding::char('3'), vec![Command::ConflictToggleBase]);
        // `e` is the escape hatch for a section that needs merging rather than
        // choosing.
        conflict.insert(KeyBinding::char('e'), vec![Command::ConflictEditHunk]);
        conflict.insert(KeyBinding::char('d'), vec![Command::ConflictDiff]);
        conflict.insert(KeyBinding::char('r'), vec![Command::ConflictRefresh]);
        conflict.insert(KeyBinding::char('?'), vec![Command::ConflictHelp]);
        // Enter writes.  The one key here that touches the disk, and the same
        // "act on what is in front of you" Enter means in the graph.
        conflict.insert(KeyBinding::key(KeyCode::Enter), vec![Command::ConflictWriteFile]);
        conflict.insert(KeyBinding::char('q'), vec![Command::ConflictClose]);

        // --- hand-edited conflict section ---
        //
        // An ordinary text buffer with one key overridden, like a `*cell …*`
        // buffer: `q` takes the edited text back to the resolver as that
        // section's answer.
        conflict_edit.insert(KeyBinding::char('q'), vec![Command::BufferClose]);

        Self { normal, select, notebook, table, cell, sql, commit, vcs, conflict, conflict_edit }
    }

    /// Look `kb` up in exactly one layer.
    pub fn lookup(&self, layer: Layer, kb: &KeyBinding) -> Option<&[Command]> {
        let map = match layer {
            Layer::Normal => &self.normal,
            Layer::Select => &self.select,
            Layer::Notebook => &self.notebook,
            Layer::Table => &self.table,
            Layer::Cell => &self.cell,
            Layer::Sql => &self.sql,
            Layer::Commit => &self.commit,
            Layer::Vcs => &self.vcs,
            Layer::Conflict => &self.conflict,
            Layer::ConflictEdit => &self.conflict_edit,
        };
        map.get(kb).map(Vec::as_slice)
    }

    /// Look `kb` up in an override layer, falling back to `Normal`.
    ///
    /// The fallback is the whole point of a layer: a view overrides the few
    /// keys whose meaning changes there and inherits the rest, so `:w`, the
    /// palette and buffer switching keep working without every layer having to
    /// restate them.
    pub fn lookup_layered(&self, layer: Layer, kb: &KeyBinding) -> Option<&[Command]> {
        match layer {
            Layer::Normal | Layer::Select => self.lookup(layer, kb),
            _ => self.lookup(layer, kb).or_else(|| self.lookup(Layer::Normal, kb)),
        }
    }

    pub fn lookup_normal(&self, kb: &KeyBinding) -> Option<&[Command]> {
        self.lookup(Layer::Normal, kb)
    }

    pub fn lookup_select(&self, kb: &KeyBinding) -> Option<&[Command]> {
        self.lookup(Layer::Select, kb)
    }

    pub fn set_normal(&mut self, kb: KeyBinding, cmds: Vec<Command>) {
        self.normal.insert(kb, cmds);
    }

    pub fn set_select(&mut self, kb: KeyBinding, cmds: Vec<Command>) {
        self.select.insert(kb, cmds);
    }

    /// Reverse-lookup: find the first normal-mode key binding for a command name
    /// and return it formatted as a human-readable hint (e.g. "C-o", "SPC").
    pub fn hint_for_command(&self, cmd_name: &str) -> Option<String> {
        for (kb, cmds) in &self.normal {
            if cmds.iter().any(|c| c.name() == cmd_name) {
                return Some(format_key_binding(kb));
            }
        }
        None
    }

    pub fn apply_custom_bindings(&mut self, keys: &crate::config::KeysConfig) {
        for (key_str, cmd_str) in &keys.normal {
            if let Some(kb) = KeyBinding::parse(key_str) {
                if let Some(cmd) = Command::parse(cmd_str) {
                    self.set_normal(kb, vec![cmd]);
                }
            }
        }
        for (key_str, cmd_str) in &keys.select {
            if let Some(kb) = KeyBinding::parse(key_str) {
                if let Some(cmd) = Command::parse(cmd_str) {
                    self.set_select(kb, vec![cmd]);
                }
            }
        }
    }
}

/// Format a key binding as a short human-readable hint, e.g. "C-o", "SPC", "Enter".
pub fn format_key_binding(kb: &KeyBinding) -> String {
    let ctrl = kb.modifiers.contains(KeyModifiers::CONTROL);
    let alt  = kb.modifiers.contains(KeyModifiers::ALT);

    let key = match &kb.code {
        KeyCode::Char(' ')  => "SPC".to_string(),
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

    #[test]
    fn test_key_binding_parse() {
        assert_eq!(
            KeyBinding::parse("j"),
            Some(KeyBinding {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::NONE
            })
        );
        assert_eq!(
            KeyBinding::parse("J"),
            Some(KeyBinding {
                code: KeyCode::Char('J'),
                modifiers: KeyModifiers::NONE
            })
        );
        assert_eq!(
            KeyBinding::parse("ctrl+d"),
            Some(KeyBinding {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL
            })
        );
        assert_eq!(
            KeyBinding::parse("ctrl-u"),
            Some(KeyBinding {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL
            })
        );
        assert_eq!(
            KeyBinding::parse("PgUp"),
            Some(KeyBinding {
                code: KeyCode::PageUp,
                modifiers: KeyModifiers::NONE
            })
        );
        assert_eq!(
            KeyBinding::parse("shift+escape"),
            Some(KeyBinding {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::SHIFT
            })
        );
        assert_eq!(KeyBinding::parse("invalidkeyname"), None);
    }
}
