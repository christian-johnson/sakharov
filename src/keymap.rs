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

pub struct Keymap {
    normal: HashMap<KeyBinding, Vec<Command>>,
    select: HashMap<KeyBinding, Vec<Command>>,
    notebook_navigate: HashMap<KeyBinding, Vec<Command>>,
    #[allow(dead_code)]
    notebook_edit: HashMap<KeyBinding, Vec<Command>>,
}

impl Keymap {
    /// Build the default key bindings for Normal and Select modes.
    pub fn default_bindings() -> Self {
        let mut normal: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        let mut select: HashMap<KeyBinding, Vec<Command>> = HashMap::new();

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
        normal.insert(KeyBinding::ctrl('s'), vec![Command::Save]);

        // --- Select-mode-only bindings ---

        select.insert(KeyBinding::key(KeyCode::Esc), vec![Command::EnterNormal]);

        // --- Notebook Navigate bindings ---
        let mut notebook_navigate: HashMap<KeyBinding, Vec<Command>> = HashMap::new();

        notebook_navigate.insert(KeyBinding::char('j'), vec![Command::NotebookNextCell]);
        notebook_navigate.insert(
            KeyBinding::key(KeyCode::Down),
            vec![Command::NotebookNextCell],
        );
        notebook_navigate.insert(KeyBinding::char('k'), vec![Command::NotebookPrevCell]);
        notebook_navigate.insert(
            KeyBinding::key(KeyCode::Up),
            vec![Command::NotebookPrevCell],
        );
        notebook_navigate.insert(
            KeyBinding::key(KeyCode::Enter),
            vec![Command::NotebookEnterEdit],
        );
        notebook_navigate.insert(KeyBinding::char('i'), vec![Command::NotebookEnterEdit]);
        notebook_navigate.insert(KeyBinding::char('e'), vec![Command::NotebookExecuteCell]);
        notebook_navigate.insert(KeyBinding::char('E'), vec![Command::NotebookExecuteAndAdvance]);
        notebook_navigate.insert(KeyBinding::char('o'), vec![Command::NotebookNewCellBelow]);
        notebook_navigate.insert(KeyBinding::char('O'), vec![Command::NotebookNewCellAbove]);
        notebook_navigate.insert(KeyBinding::char('d'), vec![Command::NotebookDeleteCell]);
        notebook_navigate.insert(KeyBinding::char('x'), vec![Command::NotebookClearOutputs]);
        notebook_navigate.insert(KeyBinding::char(':'), vec![Command::EnterCommandMode]);
        notebook_navigate.insert(
            KeyBinding::key(KeyCode::Esc),
            vec![Command::EnterNormal],
        );
        notebook_navigate.insert(KeyBinding::ctrl('s'), vec![Command::Save]);
        notebook_navigate.insert(KeyBinding::ctrl('r'), vec![Command::NotebookRestartKernel]);

        // Ctrl+Enter — execute cell
        notebook_navigate.insert(
            KeyBinding {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::CONTROL,
            },
            vec![Command::NotebookExecuteCell],
        );

        // --- Notebook Edit bindings (minimal — most editing is handled directly in input.rs) ---
        let mut notebook_edit: HashMap<KeyBinding, Vec<Command>> = HashMap::new();
        notebook_edit.insert(
            KeyBinding::key(KeyCode::Esc),
            vec![Command::NotebookExitEdit],
        );

        Self {
            normal,
            select,
            notebook_navigate,
            notebook_edit,
        }
    }

    /// Look up a key binding in Normal mode.
    pub fn lookup_normal(&self, kb: &KeyBinding) -> Option<&[Command]> {
        self.normal.get(kb).map(Vec::as_slice)
    }

    /// Look up a key binding in Select mode.
    pub fn lookup_select(&self, kb: &KeyBinding) -> Option<&[Command]> {
        self.select.get(kb).map(Vec::as_slice)
    }

    /// Override or add a Normal-mode binding (for future config support).
    #[allow(dead_code)]
    pub fn set_normal(&mut self, kb: KeyBinding, cmds: Vec<Command>) {
        self.normal.insert(kb, cmds);
    }

    /// Override or add a Select-mode binding (for future config support).
    #[allow(dead_code)]
    pub fn set_select(&mut self, kb: KeyBinding, cmds: Vec<Command>) {
        self.select.insert(kb, cmds);
    }

    /// Look up a key binding in Notebook Navigate mode.
    pub fn lookup_notebook_navigate(&self, kb: &KeyBinding) -> Option<&[Command]> {
        self.notebook_navigate.get(kb).map(Vec::as_slice)
    }

    /// Look up a key binding in Notebook Edit mode.
    #[allow(dead_code)]
    pub fn lookup_notebook_edit(&self, kb: &KeyBinding) -> Option<&[Command]> {
        self.notebook_edit.get(kb).map(Vec::as_slice)
    }
}
