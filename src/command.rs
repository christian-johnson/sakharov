/// Every editor action that can be triggered by a key, the command line, or a script.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Command {
    // Motions — in Normal mode set a new point selection;
    //           in Select mode extend the existing selection.
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveWordForward,
    MoveWordBackward,
    MoveWordEnd,
    MoveBigWordForward,
    MoveBigWordBackward,
    MoveBigWordEnd,
    MoveLineStart,
    MoveLineFirstNonWs,
    MoveLineEnd,
    GotoFileStart,
    GotoFileEnd,
    GotoLine(usize),
    SelectLine,
    SelectAll,

    // Two-character sequences — enter a pending sub-mode
    EnterGotoMode,
    FindCharForward,
    FindCharBackward,
    TillCharForward,
    TillCharBackward,

    // Editing
    DeleteSelection,
    ChangeSelection,
    YankSelection,
    PasteAfter,
    PasteBefore,
    Undo,
    Redo,
    OpenLineBelow,
    OpenLineAbove,

    // Mode transitions
    EnterInsert,
    EnterInsertAfter,
    EnterInsertAtLineStart,
    EnterInsertAtLineEnd,
    EnterNormal,
    EnterSelect,
    EnterCommandMode,

    // File / application
    Save,
    SaveAs(String),
    Quit,
    ForceQuit,
    WriteQuit,

    // Scripting / composition
    Shell(String),
    Sequence(Vec<Command>),
}

impl Command {
    /// Returns the canonical command name used in docs and `:` command line.
    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        match self {
            Command::MoveLeft => "move-left",
            Command::MoveRight => "move-right",
            Command::MoveUp => "move-up",
            Command::MoveDown => "move-down",
            Command::MoveWordForward => "move-word-forward",
            Command::MoveWordBackward => "move-word-backward",
            Command::MoveWordEnd => "move-word-end",
            Command::MoveBigWordForward => "move-big-word-forward",
            Command::MoveBigWordBackward => "move-big-word-backward",
            Command::MoveBigWordEnd => "move-big-word-end",
            Command::MoveLineStart => "move-line-start",
            Command::MoveLineFirstNonWs => "move-line-first-non-ws",
            Command::MoveLineEnd => "move-line-end",
            Command::GotoFileStart => "goto-file-start",
            Command::GotoFileEnd => "goto-file-end",
            Command::GotoLine(_) => "goto-line",
            Command::SelectLine => "select-line",
            Command::SelectAll => "select-all",
            Command::EnterGotoMode => "enter-goto-mode",
            Command::FindCharForward => "find-char-forward",
            Command::FindCharBackward => "find-char-backward",
            Command::TillCharForward => "till-char-forward",
            Command::TillCharBackward => "till-char-backward",
            Command::DeleteSelection => "delete-selection",
            Command::ChangeSelection => "change-selection",
            Command::YankSelection => "yank-selection",
            Command::PasteAfter => "paste-after",
            Command::PasteBefore => "paste-before",
            Command::Undo => "undo",
            Command::Redo => "redo",
            Command::OpenLineBelow => "open-line-below",
            Command::OpenLineAbove => "open-line-above",
            Command::EnterInsert => "enter-insert",
            Command::EnterInsertAfter => "enter-insert-after",
            Command::EnterInsertAtLineStart => "enter-insert-at-line-start",
            Command::EnterInsertAtLineEnd => "enter-insert-at-line-end",
            Command::EnterNormal => "enter-normal",
            Command::EnterSelect => "enter-select",
            Command::EnterCommandMode => "enter-command-mode",
            Command::Save => "save",
            Command::SaveAs(_) => "save-as",
            Command::Quit => "quit",
            Command::ForceQuit => "force-quit",
            Command::WriteQuit => "write-quit",
            Command::Shell(_) => "shell",
            Command::Sequence(_) => "sequence",
        }
    }

    /// Parse a command from `:` input. Returns `None` for unknown commands.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }

        // Numeric input → GotoLine
        if let Ok(n) = input.parse::<usize>() {
            return Some(Command::GotoLine(n));
        }

        // Split into command and optional argument
        let (cmd, arg) = match input.find(' ') {
            Some(idx) => (&input[..idx], Some(input[idx + 1..].trim())),
            None => (input, None),
        };

        match cmd {
            // Vim aliases
            "w" => {
                if let Some(path) = arg {
                    if !path.is_empty() {
                        return Some(Command::SaveAs(path.to_string()));
                    }
                }
                Some(Command::Save)
            }
            "q" => Some(Command::Quit),
            "q!" => Some(Command::ForceQuit),
            "wq" | "x" => Some(Command::WriteQuit),
            "u" => Some(Command::Undo),

            // Canonical names with arguments
            "save-as" => {
                let path = arg.unwrap_or("").trim();
                if path.is_empty() {
                    None
                } else {
                    Some(Command::SaveAs(path.to_string()))
                }
            }
            "shell" | "sh" => {
                let shell_cmd = arg.unwrap_or("").trim();
                if shell_cmd.is_empty() {
                    None
                } else {
                    Some(Command::Shell(shell_cmd.to_string()))
                }
            }
            "goto-line" => {
                let n = arg.unwrap_or("").trim().parse::<usize>().ok()?;
                Some(Command::GotoLine(n))
            }

            // Canonical no-arg commands
            "move-left"               => Some(Command::MoveLeft),
            "move-right"              => Some(Command::MoveRight),
            "move-up"                 => Some(Command::MoveUp),
            "move-down"               => Some(Command::MoveDown),
            "move-word-forward"       => Some(Command::MoveWordForward),
            "move-word-backward"      => Some(Command::MoveWordBackward),
            "move-word-end"           => Some(Command::MoveWordEnd),
            "move-big-word-forward"   => Some(Command::MoveBigWordForward),
            "move-big-word-backward"  => Some(Command::MoveBigWordBackward),
            "move-big-word-end"       => Some(Command::MoveBigWordEnd),
            "move-line-start"         => Some(Command::MoveLineStart),
            "move-line-first-non-ws"  => Some(Command::MoveLineFirstNonWs),
            "move-line-end"           => Some(Command::MoveLineEnd),
            "goto-file-start"         => Some(Command::GotoFileStart),
            "goto-file-end"           => Some(Command::GotoFileEnd),
            "select-line"             => Some(Command::SelectLine),
            "select-all"              => Some(Command::SelectAll),
            "enter-goto-mode"         => Some(Command::EnterGotoMode),
            "find-char-forward"       => Some(Command::FindCharForward),
            "find-char-backward"      => Some(Command::FindCharBackward),
            "till-char-forward"       => Some(Command::TillCharForward),
            "till-char-backward"      => Some(Command::TillCharBackward),
            "delete-selection" | "delete" => Some(Command::DeleteSelection),
            "change-selection" | "change" => Some(Command::ChangeSelection),
            "yank-selection"   | "yank"   => Some(Command::YankSelection),
            "paste-after"      | "paste"  => Some(Command::PasteAfter),
            "paste-before"               => Some(Command::PasteBefore),
            "undo"                       => Some(Command::Undo),
            "redo"                       => Some(Command::Redo),
            "open-line-below"            => Some(Command::OpenLineBelow),
            "open-line-above"            => Some(Command::OpenLineAbove),
            "enter-insert"               => Some(Command::EnterInsert),
            "enter-insert-after"         => Some(Command::EnterInsertAfter),
            "enter-insert-at-line-start" => Some(Command::EnterInsertAtLineStart),
            "enter-insert-at-line-end"   => Some(Command::EnterInsertAtLineEnd),
            "enter-normal"               => Some(Command::EnterNormal),
            "enter-select"               => Some(Command::EnterSelect),
            "enter-command-mode"         => Some(Command::EnterCommandMode),
            "save"                       => Some(Command::Save),
            "quit"                       => Some(Command::Quit),
            "force-quit"                 => Some(Command::ForceQuit),
            "write-quit"                 => Some(Command::WriteQuit),

            _ => None,
        }
    }
}
