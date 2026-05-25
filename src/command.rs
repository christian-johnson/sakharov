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
    /// Enter label-jump mode: overlay labels on visible word starts.
    EnterJumpMode,
    FindCharForward,
    FindCharBackward,
    TillCharForward,
    TillCharBackward,

    // Editing
    DeleteSelection,
    ChangeSelection,
    /// Toggle comment/uncomment on the selected lines.
    CommentRegion,
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

    // Popup / UI
    /// Open the command palette popup.
    OpenCommandPalette,
    /// Open the fuzzy buffer-list picker.
    OpenBufferPicker,
    /// Open the symbol picker (tree-sitter).
    OpenSymbolPicker,
    /// Open the diagnostic picker (LSP diagnostics).
    OpenDiagnosticPicker,
    /// Open a file picker (built-in fuzzy list, or external via config).
    OpenFilePicker,

    // File / application
    Write,
    WriteAs(String),
    Quit,
    ForceQuit,
    WriteQuit,
    /// Close the current buffer; warn if modified.
    BufferClose,
    /// Close the current buffer unconditionally (discard unsaved changes).
    BufferForceClose,
    /// Switch to the next buffer in the open-buffers list.
    BufferNext,
    /// Switch to the previous buffer in the open-buffers list.
    BufferPrev,
    /// Switch to the persistent *scratch* buffer.
    SwitchToScratch,
    /// Switch to the *Messages* buffer showing the message log.
    SwitchToMessages,

    // Scripting / composition
    Shell(String),
    Sequence(Vec<Command>),

    // Notebook navigation
    NotebookNextCell,
    NotebookPrevCell,
    NotebookScrollDown,
    NotebookScrollUp,

    // Notebook editing
    NotebookEnterEdit,
    NotebookExitEdit,
    NotebookExecuteCell,
    NotebookExecuteAndAdvance,
    NotebookNewCellBelow,
    NotebookNewCellAbove,
    NotebookDeleteCell,
    NotebookClearOutputs,

    // Kernel lifecycle
    NotebookRestartKernel,
    NotebookInterruptKernel,
    /// Undo the last structural notebook change (add/delete cell).
    NotebookUndoStructural,
    /// Redo the last undone structural notebook change.
    NotebookRedoStructural,

    // Cell edit overlay
    /// Open focused cell in a full-screen Helix edit overlay.
    NotebookOpenCellEdit,
    /// Save cell content back to notebook and close the overlay.
    NotebookCloseCellEdit,
    /// Abandon edits and close the overlay without writing back.
    NotebookDiscardCellEdit,

    // Notebook
    /// Enter Notebook navigation mode (cell-level j/k/o/e/d bindings).
    EnterNotebook,

    // Search
    /// Enter forward search mode (builds query; Enter jumps to first match).
    SearchForward,
    /// Enter backward search mode.
    SearchBackward,
    /// Jump to the next search match.
    SearchNext,
    /// Jump to the previous search match.
    SearchPrev,
    /// Telescope-style grep buffer popup.
    GrepBuffer,
    /// Telescope-style project-wide grep popup (uses ripgrep/grep).
    GrepProject,

    // Scroll
    /// Scroll half a page up (cursor moves with viewport).
    PageUp,
    /// Scroll half a page down (cursor moves with viewport).
    PageDown,

    // LSP
    /// Show documentation/hover for the symbol under the cursor.
    LspShowDocumentation,
    /// Show code actions available at the cursor/selection.
    LspCodeActions,
    /// Jump to the definition of the symbol under the cursor.
    LspGotoDefinition,
    /// List all references to the symbol under the cursor.
    LspGotoReferences,
    /// Jump to the type definition of the symbol under the cursor.
    LspGotoTypeDefinition,
    /// Jump to the implementation of the symbol under the cursor.
    LspGotoImplementation,
    /// Explicitly request completions at the cursor position.
    LspRequestCompletion,
    /// Toggle visual indicators for the git gutter.
    ToggleGitGutter,
    /// Toggle line number display.
    ToggleLineNumbers,
    /// Toggle relative line numbers.
    ToggleRelativeLineNumbers,

    // Code folding (plain text editor)
    /// Enter fold sub-mode (awaits a second key: a/A).
    EnterFoldMode,
    /// Toggle fold on the innermost range at the cursor line.
    FoldToggle,
    /// Toggle all folds: close all if any are open, otherwise open all.
    FoldToggleAll,

    // Notebook cell folding
    /// Toggle fold on the focused notebook cell (collapse to first line).
    NotebookToggleFoldCell,
    /// Toggle all notebook cells: fold all if any are unfolded, else unfold all.
    NotebookToggleAllFolds,
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
            Command::EnterJumpMode => "enter-jump-mode",
            Command::FindCharForward => "find-char-forward",
            Command::FindCharBackward => "find-char-backward",
            Command::TillCharForward => "till-char-forward",
            Command::TillCharBackward => "till-char-backward",
            Command::DeleteSelection => "delete-selection",
            Command::ChangeSelection => "change-selection",
            Command::CommentRegion => "comment-region",
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
            Command::OpenCommandPalette => "open-command-palette",
            Command::OpenBufferPicker => "open-buffer-picker",
            Command::OpenSymbolPicker => "open-symbol-picker",
            Command::OpenDiagnosticPicker => "open-diagnostic-picker",
            Command::OpenFilePicker => "open-file-picker",
            Command::Write => "write",
            Command::WriteAs(_) => "write-as",
            Command::Quit => "quit",
            Command::ForceQuit => "force-quit",
            Command::WriteQuit => "write-quit",
            Command::BufferClose => "buffer-close",
            Command::BufferForceClose => "buffer-force-close",
            Command::BufferNext => "buffer-next",
            Command::BufferPrev => "buffer-prev",
            Command::SwitchToScratch => "switch-to-scratch",
            Command::SwitchToMessages => "switch-to-messages",
            Command::Shell(_) => "shell",
            Command::Sequence(_) => "sequence",
            Command::NotebookNextCell => "notebook-next-cell",
            Command::NotebookPrevCell => "notebook-prev-cell",
            Command::NotebookScrollDown => "notebook-scroll-down",
            Command::NotebookScrollUp => "notebook-scroll-up",
            Command::NotebookEnterEdit => "notebook-enter-edit",
            Command::NotebookExitEdit => "notebook-exit-edit",
            Command::NotebookExecuteCell => "notebook-execute-cell",
            Command::NotebookExecuteAndAdvance => "notebook-execute-and-advance",
            Command::NotebookNewCellBelow => "notebook-new-cell-below",
            Command::NotebookNewCellAbove => "notebook-new-cell-above",
            Command::NotebookDeleteCell => "notebook-delete-cell",
            Command::NotebookClearOutputs => "notebook-clear-outputs",
            Command::NotebookRestartKernel => "notebook-restart-kernel",
            Command::NotebookInterruptKernel => "notebook-interrupt-kernel",
            Command::NotebookUndoStructural => "notebook-undo-structural",
            Command::NotebookRedoStructural => "notebook-redo-structural",
            Command::NotebookOpenCellEdit => "notebook-open-cell-edit",
            Command::NotebookCloseCellEdit => "notebook-close-cell-edit",
            Command::NotebookDiscardCellEdit => "notebook-discard-cell-edit",
            Command::EnterNotebook => "enter-notebook",
            Command::SearchForward => "search-forward",
            Command::SearchBackward => "search-backward",
            Command::SearchNext => "search-next",
            Command::SearchPrev => "search-prev",
            Command::GrepBuffer => "grep-buffer",
            Command::GrepProject => "grep-project",
            Command::PageUp => "page-up",
            Command::PageDown => "page-down",
            Command::LspShowDocumentation => "lsp-show-documentation",
            Command::LspCodeActions => "lsp-code-actions",
            Command::LspGotoDefinition => "lsp-goto-definition",
            Command::LspGotoReferences => "lsp-goto-references",
            Command::LspGotoTypeDefinition => "lsp-goto-type-definition",
            Command::LspGotoImplementation => "lsp-goto-implementation",
            Command::LspRequestCompletion => "lsp-request-completion",
            Command::ToggleGitGutter => "toggle-git-gutter",
            Command::ToggleLineNumbers => "toggle-line-numbers",
            Command::ToggleRelativeLineNumbers => "toggle-relative-line-numbers",
            Command::EnterFoldMode => "enter-fold-mode",
            Command::FoldToggle => "fold-toggle",
            Command::FoldToggleAll => "fold-toggle-all",
            Command::NotebookToggleFoldCell => "notebook-toggle-fold-cell",
            Command::NotebookToggleAllFolds => "notebook-toggle-all-folds",
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
                        return Some(Command::WriteAs(path.to_string()));
                    }
                }
                Some(Command::Write)
            }
            "q" => Some(Command::Quit),
            "q!" => Some(Command::ForceQuit),
            "wq" | "x" => Some(Command::WriteQuit),
            "u" => Some(Command::Undo),

            // Canonical names with arguments
            "write-as" | "save-as" => {
                let path = arg.unwrap_or("").trim();
                if path.is_empty() {
                    None
                } else {
                    Some(Command::WriteAs(path.to_string()))
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

            // Popup / UI
            "open-command-palette" | "palette" | "commands" => Some(Command::OpenCommandPalette),
            "open-buffer-picker"     | "buffers" => Some(Command::OpenBufferPicker),
            "open-symbol-picker"     | "symbols" => Some(Command::OpenSymbolPicker),
            "open-diagnostic-picker" | "diagnostics" => Some(Command::OpenDiagnosticPicker),
            "open-file-picker" | "open-file" | "e" => Some(Command::OpenFilePicker),

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
            "enter-goto-mode"                     => Some(Command::EnterGotoMode),
            "enter-jump-mode" | "jump-mode" | "jump" => Some(Command::EnterJumpMode),
            "find-char-forward"       => Some(Command::FindCharForward),
            "find-char-backward"      => Some(Command::FindCharBackward),
            "till-char-forward"       => Some(Command::TillCharForward),
            "till-char-backward"      => Some(Command::TillCharBackward),
            "delete-selection" | "delete" => Some(Command::DeleteSelection),
            "change-selection" | "change" => Some(Command::ChangeSelection),
            "comment-region" | "comment" => Some(Command::CommentRegion),
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
            "write" | "save"             => Some(Command::Write),
            "quit"                       => Some(Command::Quit),
            "force-quit"                 => Some(Command::ForceQuit),
            "write-quit"                 => Some(Command::WriteQuit),
            "buffer-close" | "bd"        => Some(Command::BufferClose),
            "buffer-force-close" | "bd!" => Some(Command::BufferForceClose),
            "buffer-next" | "bn"         => Some(Command::BufferNext),
            "buffer-prev" | "bp"         => Some(Command::BufferPrev),
            "switch-to-scratch" | "scratch" => Some(Command::SwitchToScratch),
            "switch-to-messages" | "messages" => Some(Command::SwitchToMessages),

            // Notebook commands
            "notebook-next-cell"             => Some(Command::NotebookNextCell),
            "notebook-prev-cell"             => Some(Command::NotebookPrevCell),
            "notebook-scroll-down"           => Some(Command::NotebookScrollDown),
            "notebook-scroll-up"             => Some(Command::NotebookScrollUp),
            "notebook-enter-edit"            => Some(Command::NotebookEnterEdit),
            "notebook-exit-edit"             => Some(Command::NotebookExitEdit),
            "notebook-execute-cell" | "run"  => Some(Command::NotebookExecuteCell),
            "notebook-execute-and-advance" | "run-next" => Some(Command::NotebookExecuteAndAdvance),
            "notebook-new-cell-below" | "new-cell" => Some(Command::NotebookNewCellBelow),
            "notebook-new-cell-above"        => Some(Command::NotebookNewCellAbove),
            "notebook-delete-cell"           => Some(Command::NotebookDeleteCell),
            "notebook-clear-outputs"         => Some(Command::NotebookClearOutputs),
            "notebook-restart-kernel" | "restart-kernel" | "kernel-restart" => {
                Some(Command::NotebookRestartKernel)
            }
            "notebook-interrupt-kernel" | "interrupt-kernel" | "kernel-interrupt" => {
                Some(Command::NotebookInterruptKernel)
            }
            "notebook-undo-structural" => Some(Command::NotebookUndoStructural),
            "notebook-redo-structural" => Some(Command::NotebookRedoStructural),
            "notebook-open-cell-edit" | "open-cell" | "edit-cell" => {
                Some(Command::NotebookOpenCellEdit)
            }
            "notebook-close-cell-edit" | "close-cell" => {
                Some(Command::NotebookCloseCellEdit)
            }
            "notebook-discard-cell-edit" | "discard-cell" => {
                Some(Command::NotebookDiscardCellEdit)
            }

            // Notebook mode
            "enter-notebook" | "nb" | "notebook" => Some(Command::EnterNotebook),

            // Search
            "search-forward" | "search" | "/" => Some(Command::SearchForward),
            "search-backward" | "?" => Some(Command::SearchBackward),
            "search-next" | "n" => Some(Command::SearchNext),
            "search-prev" | "N" => Some(Command::SearchPrev),
            "grep-buffer" => Some(Command::GrepBuffer),
            "grep-project" | "grep" | "rg" => Some(Command::GrepProject),

            // Scroll
            "page-up" => Some(Command::PageUp),
            "page-down" => Some(Command::PageDown),

            // LSP commands
            "lsp-show-documentation" | "lsp-hover" | "hover" | "doc" => Some(Command::LspShowDocumentation),
            "lsp-code-actions" | "code-actions" | "ga" => Some(Command::LspCodeActions),
            "lsp-goto-definition" | "goto-definition" | "gd" => Some(Command::LspGotoDefinition),
            "lsp-goto-references" | "goto-references" | "gr" => Some(Command::LspGotoReferences),
            "lsp-goto-type-definition" | "goto-type-definition" | "gy" => {
                Some(Command::LspGotoTypeDefinition)
            }
            "lsp-goto-implementation" | "goto-implementation" | "gi" => {
                Some(Command::LspGotoImplementation)
            }
            "lsp-request-completion" | "completion" => Some(Command::LspRequestCompletion),
            "toggle-git-gutter" | "git-gutter" | "gutter" => Some(Command::ToggleGitGutter),
            "toggle-line-numbers" | "line-numbers" => Some(Command::ToggleLineNumbers),
            "toggle-relative-line-numbers" | "relative-line-numbers" => Some(Command::ToggleRelativeLineNumbers),

            // Fold commands
            "enter-fold-mode" | "fold" => Some(Command::EnterFoldMode),
            "fold-toggle" | "za" => Some(Command::FoldToggle),
            "fold-toggle-all" | "zA" => Some(Command::FoldToggleAll),

            // Notebook fold commands
            "notebook-toggle-fold-cell" | "fold-cell" => Some(Command::NotebookToggleFoldCell),
            "notebook-toggle-all-folds" | "fold-all-cells" => Some(Command::NotebookToggleAllFolds),

            _ => None,
        }
    }
}
