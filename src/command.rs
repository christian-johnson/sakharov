//! Single source of truth for every editor command.
//!
//! The [`commands!`] macro below generates, from one table:
//!   * the [`Command`] enum,
//!   * [`Command::name`] (variant → canonical string name),
//!   * the unit-command parser used by [`Command::parse`] (canonical name + aliases),
//!   * [`Command::palette_entries`] (the command-palette list with descriptions).
//!
//! To add a command, add one row to the table. Data-carrying variants (those
//! that hold an argument, e.g. `GotoLine(usize)`) live in the `data:` section
//! and get bespoke parsing in [`Command::parse`]; everything else is a `unit:`
//! row and needs no further wiring.

/// Generate the [`Command`] enum plus its `name`, unit-parser, and palette table.
///
/// Row syntax:
/// ```ignore
/// units: {
///     // VariantName => "canonical-name" [, aliases: ["a", "b"]] [, palette: "Description  [key]"];
///     MoveLeft => "move-left", palette: "Move cursor left  [h]";
/// }
/// data: {
///     // VariantName(Type, ...) => "canonical-name" [, palette: "..."];
///     GotoLine(usize) => "goto-line";
/// }
/// ```
macro_rules! commands {
    (
        units: {
            $( $uvar:ident => $uname:literal
                $(, aliases: [ $($ualias:literal),* $(,)? ])?
                $(, palette: $udesc:literal)? ; )*
        }
        data: {
            $( $dvar:ident ( $($dty:ty),* ) => $dname:literal
                $(, palette: $ddesc:literal)? ; )*
        }
    ) => {
        /// Every editor action that can be triggered by a key, the command line, or a script.
        #[derive(Debug, Clone)]
        #[allow(dead_code)]
        pub enum Command {
            $( $uvar, )*
            $( $dvar ( $($dty),* ), )*
        }

        impl Command {
            /// The canonical command name used in docs and the `:` command line.
            #[allow(dead_code)]
            pub fn name(&self) -> &'static str {
                match self {
                    $( Command::$uvar => $uname, )*
                    $( Command::$dvar(..) => $dname, )*
                }
            }

            /// Parse a unit (argument-less) command by canonical name or alias.
            /// Data-carrying commands are handled separately in [`Command::parse`].
            fn parse_unit(cmd: &str) -> Option<Command> {
                match cmd {
                    $( $uname $($( | $ualias )*)? => Some(Command::$uvar), )*
                    _ => None,
                }
            }

            /// `(canonical_name, description)` for every command that opts into the
            /// command palette, in table order. Drives `command_palette_items()`.
            pub fn palette_entries() -> Vec<(&'static str, &'static str)> {
                vec![
                    $( $( ($uname, $udesc), )? )*
                    $( $( ($dname, $ddesc), )? )*
                ]
            }
        }
    };
}

commands! {
    units: {
        // --- File / application ---
        Write => "write", aliases: ["save"], palette: "Write file  [ctrl+s, :w]";
        WriteForce => "write-force", aliases: ["w!"], palette: "Write file, overwriting external changes  [:w!]";
        Quit => "quit", aliases: ["q"], palette: "Quit  [:q]";
        ForceQuit => "force-quit", aliases: ["q!"], palette: "Quit without saving  [:q!]";
        WriteQuit => "write-quit", aliases: ["wq", "x"], palette: "Write and quit  [:wq]";
        NewFile => "new-file", aliases: ["newfile", "new"], palette: "Create a new file in the current directory (prompts for name)  [:new-file]";
        NewNotebook => "new-notebook", aliases: ["newnotebook", "new-nb"], palette: "Create a new notebook in the current directory (prompts for name)  [:new-notebook]";

        // --- Motions ---
        MoveLeft => "move-left", palette: "Move cursor left  [h]";
        MoveRight => "move-right", palette: "Move cursor right  [l]";
        MoveUp => "move-up", palette: "Move cursor up  [k]";
        MoveDown => "move-down", palette: "Move cursor down  [j]";
        MoveWordForward => "move-word-forward", palette: "Next word  [w]";
        MoveWordBackward => "move-word-backward", palette: "Previous word  [b]";
        MoveWordEnd => "move-word-end", palette: "End of word  [e]";
        MoveBigWordForward => "move-big-word-forward";
        MoveBigWordBackward => "move-big-word-backward";
        MoveBigWordEnd => "move-big-word-end";
        MoveLineStart => "move-line-start", palette: "Start of line  [0]";
        MoveLineFirstNonWs => "move-line-first-non-ws";
        MoveLineEnd => "move-line-end", palette: "End of line  [$]";
        GotoFileStart => "goto-file-start", palette: "Go to file start  [gg]";
        GotoFileEnd => "goto-file-end", palette: "Go to file end  [G]";
        SelectLine => "select-line", palette: "Select current line  [x]";
        SelectAll => "select-all", palette: "Select entire file  [%]";

        // --- Editing ---
        DeleteSelection => "delete-selection", aliases: ["delete"], palette: "Delete selection  [d]";
        ChangeSelection => "change-selection", aliases: ["change"], palette: "Delete selection and insert  [c]";
        YankSelection => "yank-selection", aliases: ["yank"], palette: "Yank (copy) selection  [y]";
        PasteAfter => "paste-after", aliases: ["paste"], palette: "Paste after cursor  [p]";
        PasteBefore => "paste-before", palette: "Paste before cursor  [P]";
        Undo => "undo", aliases: ["u"], palette: "Undo  [u]";
        Redo => "redo", palette: "Redo  [U]";
        OpenLineBelow => "open-line-below", palette: "New line below  [o]";
        OpenLineAbove => "open-line-above", palette: "New line above  [O]";
        CommentRegion => "comment-region", aliases: ["comment"], palette: "Toggle comment/uncomment  [gc]";
        IndentRegion => "indent-region", aliases: ["indent"];
        DedentRegion => "dedent-region", aliases: ["dedent"];
        KillToEndOfLine => "kill-to-end-of-line", aliases: ["kill-line"], palette: "Kill to end of line  [ctrl+k]";

        // --- Mode transitions ---
        EnterInsert => "enter-insert", palette: "Enter insert mode  [i]";
        EnterInsertAfter => "enter-insert-after", palette: "Insert after cursor  [a]";
        EnterInsertAtLineStart => "enter-insert-at-line-start", palette: "Insert at line start  [I]";
        EnterInsertAtLineEnd => "enter-insert-at-line-end", palette: "Insert at line end  [A]";
        EnterSelect => "enter-select", palette: "Enter select mode  [v]";
        EnterNormal => "enter-normal", palette: "Return to normal mode  [Esc]";
        EnterCommandMode => "enter-command-mode", palette: "Open command line  [:]";

        // --- Sub-mode entries ---
        EnterGotoMode => "enter-goto-mode";
        EnterJumpMode => "enter-jump-mode", aliases: ["jump-mode", "jump"], palette: "Jump to label in view  [gw]";
        FindCharForward => "find-char-forward";
        FindCharBackward => "find-char-backward";
        TillCharForward => "till-char-forward";
        TillCharBackward => "till-char-backward";
        EnterFoldMode => "enter-fold-mode", aliases: ["fold"];

        // --- Pickers / UI popups ---
        OpenCommandPalette => "open-command-palette", aliases: ["palette", "commands"], palette: "Open fuzzy-searchable command palette  [Space]";
        OpenFilePicker => "open-file-picker", aliases: ["open-file", "e"], palette: "Open file  [ctrl+o, :e]";
        OpenBufferPicker => "open-buffer-picker", aliases: ["buffers"], palette: "Switch buffer  [gb]";
        OpenSymbolPicker => "open-symbol-picker", aliases: ["symbols"], palette: "Jump to symbol in file  [gs]";
        OpenDiagnosticPicker => "open-diagnostic-picker", aliases: ["diagnostics"], palette: "Jump to diagnostic  [gD]";

        // --- Buffers ---
        BufferClose => "buffer-close", aliases: ["bd"], palette: "Close current buffer  [:bd]";
        BufferForceClose => "buffer-force-close", aliases: ["bd!"], palette: "Force-close current buffer (discard changes)  [:bd!]";
        BufferNext => "buffer-next", aliases: ["bn"], palette: "Switch to next buffer  [L, :bn]";
        BufferPrev => "buffer-prev", aliases: ["bp"], palette: "Switch to previous buffer  [H, :bp]";
        SwitchToScratch => "switch-to-scratch", aliases: ["scratch"], palette: "Switch to *scratch* buffer  [:scratch]";
        SwitchToMessages => "switch-to-messages", aliases: ["messages"], palette: "Switch to *Messages* log buffer  [:messages]";

        // --- Search / grep ---
        SearchForward => "search-forward", aliases: ["search", "/"], palette: "Search forward  [/]";
        SearchBackward => "search-backward", aliases: ["?"], palette: "Search backward  [?]";
        SearchNext => "search-next", aliases: ["n"], palette: "Next match  [n]";
        SearchPrev => "search-prev", aliases: ["N"], palette: "Previous match  [N]";
        GrepBuffer => "grep-buffer", palette: "Grep current buffer  [ctrl+f]";
        GrepProject => "grep-project", aliases: ["grep", "rg"], palette: "Grep project files  [ctrl+g]";

        // --- Scroll / view ---
        PageDown => "page-down", palette: "Scroll half page down  [ctrl+d, PgDn]";
        PageUp => "page-up", palette: "Scroll half page up  [ctrl+u, PgUp]";
        ScrollCursorCenter => "scroll-cursor-center", aliases: ["center", "gz"], palette: "Scroll cursor to centre  [gz]";

        // --- LSP ---
        LspShowDocumentation => "lsp-show-documentation", aliases: ["lsp-hover", "hover", "doc"], palette: "Show hover documentation  [gk, K]";
        LspCodeActions => "lsp-code-actions", aliases: ["code-actions", "ga"], palette: "Show code actions  [ga]";
        LspGotoDefinition => "lsp-goto-definition", aliases: ["goto-definition", "gd"], palette: "Go to definition  [gd]";
        LspGotoReferences => "lsp-goto-references", aliases: ["goto-references", "gr"], palette: "Go to references  [gr]";
        LspGotoTypeDefinition => "lsp-goto-type-definition", aliases: ["goto-type-definition", "gy"], palette: "Go to type definition  [gy]";
        LspGotoImplementation => "lsp-goto-implementation", aliases: ["goto-implementation", "gi"], palette: "Go to implementation  [gi]";
        LspRequestCompletion => "lsp-request-completion", aliases: ["completion"], palette: "Request completions  [ctrl+space]";
        FormatDocument => "format-document", aliases: ["format", "fmt"], palette: "Format buffer via language server  [:fmt]";

        // --- Notebook navigation / editing ---
        NotebookNextCell => "notebook-next-cell", palette: "Next cell  [J]";
        NotebookPrevCell => "notebook-prev-cell", palette: "Previous cell  [K]";
        NotebookScrollDown => "notebook-scroll-down";
        NotebookScrollUp => "notebook-scroll-up";
        NotebookExecuteCell => "notebook-execute-cell", aliases: ["run"], palette: "Execute cell  [ctrl+e, shift+enter, :run]";
        NotebookExecuteAndAdvance => "notebook-execute-and-advance", aliases: ["run-next"], palette: "Execute cell and advance  [:run-next]";
        NotebookNewCellBelow => "notebook-new-cell-below", aliases: ["new-cell"], palette: "New cell below  [:new-cell]";
        NotebookNewCellAbove => "notebook-new-cell-above", palette: "New cell above  [:notebook-new-cell-above]";
        NotebookDeleteCell => "notebook-delete-cell", palette: "Delete cell  [:notebook-delete-cell]";
        NotebookClearOutputs => "notebook-clear-outputs", palette: "Clear cell outputs  [:notebook-clear-outputs]";
        NotebookCellToMarkdown => "notebook-cell-to-markdown", aliases: ["cell-md", "to-markdown"], palette: "Convert cell to markdown  [:cell-md]";
        NotebookCellToCode => "notebook-cell-to-code", aliases: ["cell-code", "to-code"], palette: "Convert cell to code  [:cell-code]";
        NotebookRestartKernel => "notebook-restart-kernel", aliases: ["restart-kernel", "kernel-restart"], palette: "Restart kernel  [:restart-kernel]";
        NotebookInterruptKernel => "notebook-interrupt-kernel", aliases: ["interrupt-kernel", "kernel-interrupt"], palette: "Interrupt kernel  [:interrupt-kernel]";
        NotebookUndoStructural => "notebook-undo-structural";
        NotebookRedoStructural => "notebook-redo-structural";
        NotebookOpenCellEdit => "notebook-open-cell-edit", aliases: ["open-cell", "edit-cell"], palette: "Open cell in full-screen editor  [:open-cell]";
        NotebookCloseCellEdit => "notebook-close-cell-edit", aliases: ["close-cell", "notebook-discard-cell-edit", "discard-cell"], palette: "Save cell and return  [ctrl+enter, :close-cell]";
        EnterNotebook => "enter-notebook", aliases: ["nb", "notebook"], palette: "Open the current .ipynb as a notebook  [:nb]";

        // --- Code folding ---
        FoldToggle => "fold-toggle", aliases: ["za"], palette: "Toggle fold at cursor  [za]";
        FoldToggleAll => "fold-toggle-all", aliases: ["zA"], palette: "Toggle all folds  [zA]";
        NotebookToggleFoldCell => "notebook-toggle-fold-cell", aliases: ["fold-cell"], palette: "Toggle cell fold  [:fold-cell]";
        NotebookToggleAllFolds => "notebook-toggle-all-folds", aliases: ["fold-all-cells"], palette: "Toggle all cell folds  [:fold-all-cells]";

        // --- Toggles / config ---
        ToggleGitGutter => "toggle-git-gutter", aliases: ["git-gutter", "gutter"], palette: "Toggle git gutter indicators  [:toggle-git-gutter]";
        ToggleLineNumbers => "toggle-line-numbers", aliases: ["line-numbers"], palette: "Toggle line numbers  [:toggle-line-numbers]";
        ToggleRelativeLineNumbers => "toggle-relative-line-numbers", aliases: ["relative-line-numbers"], palette: "Toggle relative line numbers  [:toggle-relative-line-numbers]";
        ToggleWordWrap => "toggle-word-wrap", aliases: ["word-wrap", "wrap"], palette: "Toggle soft word-wrap  [:wrap]";
        OpenConfig => "open-config", aliases: ["config"], palette: "Open config file in editor  [:config]";
        ReloadConfig => "reload-config", aliases: ["config-reload"], palette: "Reload config from disk  [:config-reload]";

        // --- Dashboard ---
        ShowDashboard => "show-dashboard", aliases: ["dashboard", "home", "splash"], palette: "Show the welcome / dashboard screen  [:dashboard]";
    }
    data: {
        // Move the cursor to a 1-based line number (also the numeric `:N` form).
        GotoLine(usize) => "goto-line";
        // Write the buffer to a new path.
        WriteAs(String) => "write-as", palette: "Write to new path  [:w <path>]";
        // Run a shell command.
        Shell(String) => "shell", palette: "Run a shell command  [:shell <cmd>]";
        // A list of commands executed in sequence (composition / scripting).
        Sequence(Vec<Command>) => "sequence";
    }
}

impl Command {
    /// Parse a command from `:` input. Returns `None` for unknown commands.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }

        // Numeric input → GotoLine.
        if let Ok(n) = input.parse::<usize>() {
            return Some(Command::GotoLine(n));
        }

        // Split into command word and optional argument.
        let (cmd, arg) = match input.find(' ') {
            Some(idx) => (&input[..idx], Some(input[idx + 1..].trim())),
            None => (input, None),
        };

        // Commands that take an argument (and their argument-less fallbacks) are
        // handled here; everything else is a unit command resolved from the table.
        match cmd {
            // `:w` with a path writes-as; bare `:w` writes in place.
            "w" => match arg {
                Some(path) if !path.is_empty() => Some(Command::WriteAs(path.to_string())),
                _ => Some(Command::Write),
            },
            "write-as" | "save-as" => {
                let path = arg.unwrap_or("").trim();
                (!path.is_empty()).then(|| Command::WriteAs(path.to_string()))
            }
            "shell" | "sh" => {
                let shell_cmd = arg.unwrap_or("").trim();
                (!shell_cmd.is_empty()).then(|| Command::Shell(shell_cmd.to_string()))
            }
            "goto-line" => {
                let n = arg.unwrap_or("").trim().parse::<usize>().ok()?;
                Some(Command::GotoLine(n))
            }
            _ => Self::parse_unit(cmd),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command offered in the palette must parse back to a command whose
    /// canonical `name()` matches the palette label — otherwise the palette would
    /// list a command that can't actually be run (the bug the single-source table
    /// was introduced to prevent).
    #[test]
    fn palette_entries_round_trip_through_parse() {
        // These palette entries name argument-taking commands, so the bare name
        // intentionally parses to None (the user supplies the argument on the `:` line).
        const ARG_COMMANDS: &[&str] = &["write-as", "shell"];
        for (name, _desc) in Command::palette_entries() {
            if ARG_COMMANDS.contains(&name) {
                continue;
            }
            let parsed = Command::parse(name)
                .unwrap_or_else(|| panic!("palette entry {name:?} does not parse"));
            assert_eq!(parsed.name(), name, "palette entry {name:?} parsed to a different command");
        }
    }

    #[test]
    fn vim_aliases_and_special_forms_parse() {
        assert!(matches!(Command::parse("42"), Some(Command::GotoLine(42))));
        assert!(matches!(Command::parse("w"), Some(Command::Write)));
        assert!(matches!(Command::parse("w foo.txt"), Some(Command::WriteAs(p)) if p == "foo.txt"));
        assert!(matches!(Command::parse("q!"), Some(Command::ForceQuit)));
        assert!(matches!(Command::parse("bd!"), Some(Command::BufferForceClose)));
        assert!(matches!(Command::parse("sh ls"), Some(Command::Shell(c)) if c == "ls"));
        // Former drift: this alias must now resolve to the real close-cell command.
        assert!(matches!(
            Command::parse("notebook-discard-cell-edit"),
            Some(Command::NotebookCloseCellEdit)
        ));
        assert!(Command::parse("totally-not-a-command").is_none());
    }
}
