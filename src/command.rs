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
///     // VariantName => "canonical-name" [, aliases: ["a", "b"]] [, palette: "Description"];
///     MoveLeft => "move-left", palette: "Move cursor left";
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

            /// The `:` aliases of the unit command named `name` (empty when
            /// it has none, or is not a unit command).
            pub fn aliases(name: &str) -> &'static [&'static str] {
                match name {
                    $( $uname => &[ $($($ualias),*)? ], )*
                    _ => &[],
                }
            }

            /// `(canonical_name, description)` for every command that opts into the
            /// command palette, in table order. Drives `command_palette_items()`;
            /// the key labels are added there, from the keymap.
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
        Write => "write", aliases: ["save"], palette: "Write file";
        WriteForce => "write-force", aliases: ["w!"], palette: "Write file, overwriting external changes";
        Quit => "quit", aliases: ["q"], palette: "Quit";
        ForceQuit => "force-quit", aliases: ["q!"], palette: "Quit without saving";
        Reload => "reload", aliases: ["e!"], palette: "Reload the file from disk, discarding unsaved changes";
        WriteQuit => "write-quit", aliases: ["wq", "x"], palette: "Write and quit";
        NewFile => "new-file", aliases: ["newfile", "new"], palette: "Create a new file in the current directory (prompts for name)";
        NewNotebook => "new-notebook", aliases: ["newnotebook", "new-nb"], palette: "Create a new notebook in the current directory (prompts for name)";

        // --- Motions ---
        MoveLeft => "move-left", palette: "Move cursor left";
        MoveRight => "move-right", palette: "Move cursor right";
        MoveUp => "move-up", palette: "Move cursor up";
        MoveDown => "move-down", palette: "Move cursor down";
        MoveWordForward => "move-word-forward", palette: "Next word";
        MoveWordBackward => "move-word-backward", palette: "Previous word";
        MoveWordEnd => "move-word-end", palette: "End of word";
        MoveBigWordForward => "move-big-word-forward";
        MoveBigWordBackward => "move-big-word-backward";
        MoveBigWordEnd => "move-big-word-end";
        MoveLineStart => "move-line-start", palette: "Start of line";
        MoveLineFirstNonWs => "move-line-first-non-ws";
        MoveLineEnd => "move-line-end", palette: "End of line";
        GotoFileStart => "goto-file-start", palette: "Go to file start";
        GotoFileEnd => "goto-file-end", palette: "Go to file end";
        SelectLine => "select-line", palette: "Select current line";
        SelectAll => "select-all", palette: "Select entire file";

        // --- Editing ---
        DeleteSelection => "delete-selection", aliases: ["delete"], palette: "Delete selection";
        ChangeSelection => "change-selection", aliases: ["change"], palette: "Delete selection and insert";
        YankSelection => "yank-selection", aliases: ["yank"], palette: "Yank (copy) selection";
        PasteAfter => "paste-after", aliases: ["paste"], palette: "Paste after cursor";
        PasteBefore => "paste-before", palette: "Paste before cursor";
        Undo => "undo", aliases: ["u"], palette: "Undo";
        Redo => "redo", palette: "Redo";
        OpenLineBelow => "open-line-below", palette: "New line below";
        OpenLineAbove => "open-line-above", palette: "New line above";
        CommentRegion => "comment-region", aliases: ["comment"], palette: "Toggle comment/uncomment";
        IndentRegion => "indent-region", aliases: ["indent"];
        DedentRegion => "dedent-region", aliases: ["dedent"];
        KillToEndOfLine => "kill-to-end-of-line", aliases: ["kill-line"], palette: "Kill to end of line";

        // --- Mode transitions ---
        EnterInsert => "enter-insert", palette: "Enter insert mode";
        EnterInsertAfter => "enter-insert-after", palette: "Insert after cursor";
        EnterInsertAtLineStart => "enter-insert-at-line-start", palette: "Insert at line start";
        EnterInsertAtLineEnd => "enter-insert-at-line-end", palette: "Insert at line end";
        EnterSelect => "enter-select", palette: "Enter select mode";
        EnterNormal => "enter-normal", palette: "Return to normal mode";
        EnterCommandMode => "enter-command-mode", palette: "Open command line";

        // --- Sub-mode entries ---
        EnterGotoMode => "enter-goto-mode";
        EnterJumpMode => "enter-jump-mode", aliases: ["jump-mode", "jump"], palette: "Jump to label in view";
        FindCharForward => "find-char-forward";
        FindCharBackward => "find-char-backward";
        TillCharForward => "till-char-forward";
        TillCharBackward => "till-char-backward";
        EnterFoldMode => "enter-fold-mode", aliases: ["fold"];
        EnterMatchMode => "enter-match-mode", aliases: ["match", "match-mode"], palette: "Select a text object — word, pair, function";
        MatchBracket => "match-bracket", palette: "Jump to the matching bracket";

        // --- Pickers / UI popups ---
        OpenCommandPalette => "open-command-palette", aliases: ["palette", "commands"], palette: "Open fuzzy-searchable command palette";
        OpenFilePicker => "open-file-picker", aliases: ["open-file", "e"], palette: "Open file";
        OpenBufferPicker => "open-buffer-picker", aliases: ["buffers"], palette: "Switch buffer";
        OpenSymbolPicker => "open-symbol-picker", aliases: ["symbols"], palette: "Jump to symbol in file";
        OpenDiagnosticPicker => "open-diagnostic-picker", aliases: ["diagnostics"], palette: "Jump to diagnostic";

        // --- Buffers ---
        BufferClose => "buffer-close", aliases: ["bd"], palette: "Close current buffer";
        BufferForceClose => "buffer-force-close", aliases: ["bd!"], palette: "Force-close current buffer (discard changes)";
        BufferNext => "buffer-next", aliases: ["bn"], palette: "Switch to next buffer";
        BufferPrev => "buffer-prev", aliases: ["bp"], palette: "Switch to previous buffer";
        SwitchToScratch => "switch-to-scratch", aliases: ["scratch"], palette: "Switch to *scratch* buffer";
        SwitchToMessages => "switch-to-messages", aliases: ["messages"], palette: "Switch to *Messages* log buffer";

        // --- Search / grep ---
        SearchForward => "search-forward", aliases: ["search", "/"], palette: "Search forward";
        SearchBackward => "search-backward", aliases: ["?"], palette: "Search backward";
        SearchNext => "search-next", aliases: ["n"], palette: "Next match";
        SearchPrev => "search-prev", aliases: ["N"], palette: "Previous match";
        GrepBuffer => "grep-buffer", palette: "Grep current buffer";
        GrepProject => "grep-project", aliases: ["grep", "rg"], palette: "Grep project files";

        // --- Scroll / view ---
        PageDown => "page-down", palette: "Scroll half page down";
        PageUp => "page-up", palette: "Scroll half page up";
        ScrollCursorCenter => "scroll-cursor-center", aliases: ["center", "gz"], palette: "Scroll cursor to centre";

        // --- LSP ---
        LspShowDocumentation => "lsp-show-documentation", aliases: ["lsp-hover", "hover", "doc"], palette: "Show hover documentation";
        LspCodeActions => "lsp-code-actions", aliases: ["code-actions", "ga"], palette: "Show code actions";
        LspGotoDefinition => "lsp-goto-definition", aliases: ["goto-definition", "gd"], palette: "Go to definition";
        LspGotoReferences => "lsp-goto-references", aliases: ["goto-references", "gr"], palette: "Go to references";
        LspGotoTypeDefinition => "lsp-goto-type-definition", aliases: ["goto-type-definition", "gy"], palette: "Go to type definition";
        LspGotoImplementation => "lsp-goto-implementation", aliases: ["goto-implementation", "gi"], palette: "Go to implementation";
        LspRequestCompletion => "lsp-request-completion", aliases: ["completion"], palette: "Request completions";
        LspDoctor => "lsp-doctor", aliases: ["doctor", "lsp-status"], palette: "Diagnose the language server setup for this buffer";
        FormatDocument => "format-document", aliases: ["format", "fmt"], palette: "Format buffer via language server";

        // --- Notebook navigation / editing ---
        NotebookNextCell => "notebook-next-cell", palette: "Next cell";
        NotebookPrevCell => "notebook-prev-cell", palette: "Previous cell";
        NotebookScrollDown => "notebook-scroll-down";
        NotebookScrollUp => "notebook-scroll-up";
        NotebookExecuteCell => "notebook-execute-cell", aliases: ["run"], palette: "Execute cell";
        NotebookExecuteAndAdvance => "notebook-execute-and-advance", aliases: ["run-next"], palette: "Execute cell and advance";
        NotebookExecuteAllCells => "notebook-execute-all-cells", aliases: ["run-all", "execute-all-cells"], palette: "Execute all cells in order";
        NotebookExecuteCellsBelow => "notebook-execute-cells-below", aliases: ["run-all-below", "execute-all-cells-below"], palette: "Execute the focused cell and all below";
        NotebookNewCellBelow => "notebook-new-cell-below", aliases: ["new-cell"], palette: "New cell below";
        NotebookNewCellAbove => "notebook-new-cell-above", palette: "New cell above";
        NotebookDeleteCell => "notebook-delete-cell", palette: "Delete cell";
        NotebookClearOutputs => "notebook-clear-outputs", palette: "Clear cell outputs";
        NotebookCellToMarkdown => "notebook-cell-to-markdown", aliases: ["cell-md", "to-markdown"], palette: "Convert cell to markdown";
        NotebookCellToCode => "notebook-cell-to-code", aliases: ["cell-code", "to-code"], palette: "Convert cell to code";
        NotebookRestartKernel => "notebook-restart-kernel", aliases: ["restart-kernel", "kernel-restart"], palette: "Restart kernel";
        NotebookInterruptKernel => "notebook-interrupt-kernel", aliases: ["interrupt-kernel", "kernel-interrupt"], palette: "Interrupt kernel";
        NotebookUndoStructural => "notebook-undo-structural";
        NotebookRedoStructural => "notebook-redo-structural";
        NotebookOpenCellEdit => "notebook-open-cell-edit", aliases: ["open-cell", "edit-cell"], palette: "Open cell in full-screen editor";
        NotebookCloseCellEdit => "notebook-close-cell-edit", aliases: ["close-cell", "notebook-discard-cell-edit", "discard-cell"], palette: "Save cell and return";
        EnterNotebook => "enter-notebook", aliases: ["nb", "notebook"], palette: "Open the current .ipynb as a notebook";
        NotebookGotoError => "notebook-goto-error", aliases: ["goto-error", "error"], palette: "Jump to the source line of the focused cell's error";
        NotebookFollowError => "notebook-follow-error";

        // --- Code folding ---
        FoldToggle => "fold-toggle", aliases: ["za"], palette: "Toggle fold at cursor";
        FoldToggleAll => "fold-toggle-all", aliases: ["zA"], palette: "Toggle all folds";
        FoldClose => "fold-close", aliases: ["zc"], palette: "Close the fold at the cursor";
        FoldOpen => "fold-open", aliases: ["zo"], palette: "Open the fold at the cursor";
        FoldCloseAll => "fold-close-all", aliases: ["zM"], palette: "Close every fold";
        FoldOpenAll => "fold-open-all", aliases: ["zR"], palette: "Open every fold";
        FoldCloseType => "fold-close-type", aliases: ["zt", "fold-type"], palette: "Fold every block like this one — same kind, depth and key";
        FoldOpenType => "fold-open-type", aliases: ["zT", "unfold-type"], palette: "Unfold every block like this one";
        NotebookToggleFoldCell => "notebook-toggle-fold-cell", aliases: ["fold-cell"], palette: "Toggle cell fold";
        NotebookToggleOutputExpand => "notebook-toggle-output-expand", aliases: ["expand-output", "output-expand"], palette: "Show full cell output (no line cap)";
        NotebookToggleAllFolds => "notebook-toggle-all-folds", aliases: ["fold-all-cells"], palette: "Toggle all cell folds";

        // --- Tabular data view ---
        OpenAsTable => "open-as-table", aliases: ["csv", "table"], palette: "View the current file as a data table";
        TableClose => "table-close", aliases: ["close-table"], palette: "Leave the table view and edit as text";
        TableOpenCell => "table-open-cell", aliases: ["read-cell", "cell-buffer"], palette: "Read the cursor cell's full text in its own buffer";
        TablePeekCell => "table-peek-cell", aliases: ["peek-cell", "peek"], palette: "Peek the cursor cell's full text in a float";
        TableYankCell => "table-yank-cell", aliases: ["yank-cell"], palette: "Copy the cursor cell to the clipboard";
        TableYankRow => "table-yank-row", aliases: ["yank-row"], palette: "Copy the cursor row to the clipboard as TSV";
        TableCloseCell => "table-close-cell", aliases: ["cell-back", "back-to-table"], palette: "Return from a cell buffer to its table";
        TableColumnSummary => "column-summary", aliases: ["summary", "describe"], palette: "Statistics for the cursor's column";
        TableColumnFrequency => "column-frequency", aliases: ["frequency", "value-counts"], palette: "Count the cursor column's values as a new table";
        TableToggleSparkline => "toggle-column-sparkline", aliases: ["sparkline", "column-sparkline"], palette: "Show/hide the distribution row under the column names";
        TableCloseDerived => "close-derived-table", aliases: ["table-back"], palette: "Leave a computed table and go back to the one it came from";
        TableSort => "sort-column", aliases: ["sort"], palette: "Sort by the cursor column — again reverses, again unsorts";
        TableFilter => "filter-column", aliases: ["filter"], palette: "Filter rows on the cursor column";
        TableGroupBy => "group-by-column", aliases: ["group", "groupby"], palette: "Group rows by the cursor column and count them";
        TableUndoTransform => "undo-transform", palette: "Drop the last sort/filter/group";
        TableClearTransforms => "clear-transforms", aliases: ["reset-table"], palette: "Drop every sort/filter/group";
        KernelVariables => "kernel-variables", aliases: ["vars", "variables"], palette: "List the kernel's variables; Enter opens a dataframe as a grid";
        SchemaBrowser => "schema", aliases: ["tables", "schema-browser"], palette: "Browse the tables in every attached database";
        SqlBuffer => "sql", aliases: ["query", "sql-buffer"], palette: "Open the SQL scratch buffer";
        SqlRun => "run-query", aliases: ["sql-run"], palette: "Run the SQL buffer's query and show the result as a grid";

        // --- Version control (see `crate::vcs`) ---
        // Every action here only adds to the plan; `:vc-apply` is the one that writes.
        VcsOpen => "version-control", aliases: ["vc", "git"], palette: "Open the version-control view";
        VcsClose => "version-control-close", aliases: ["vc-close"], palette: "Leave the version-control view";
        VcsRefresh => "version-control-refresh", aliases: ["vc-refresh"], palette: "Re-read the repository, keeping the plan";
        VcsEnter => "version-control-enter", aliases: ["vc-enter"], palette: "Open the directory, or move into the file's diff";
        VcsFocusNext => "version-control-focus-next", aliases: ["vc-focus"], palette: "Cycle the changes tree, the diff and the plan";
        VcsNextFile => "version-control-next-file", aliases: ["vc-next-file"], palette: "Next changed file";
        VcsPrevFile => "version-control-prev-file", aliases: ["vc-prev-file"], palette: "Previous changed file";
        VcsToggleStage => "version-control-toggle-stage", aliases: ["vc-stage"], palette: "Stage or unstage the selected file, directory or change";
        VcsDiscard => "version-control-discard", aliases: ["vc-discard"], palette: "Discard the selected file, directory or change";
        VcsStageAll => "version-control-stage-all", aliases: ["vc-stage-all"], palette: "Stage every change";
        VcsUnstageAll => "version-control-unstage-all", aliases: ["vc-unstage-all"], palette: "Unstage everything";
        VcsBranchMenu => "version-control-branch-keys", aliases: ["vc-branch-keys"], palette: "The branch keys: switch, new, rename, delete, merge, rebase";
        VcsStashMenu => "version-control-stash-keys", aliases: ["vc-stash-keys"], palette: "The stash keys: push, pop, apply, drop";
        VcsLogMenu => "version-control-log-keys", aliases: ["vc-log-keys"], palette: "The history keys: cherry-pick, revert, reset, tag";
        VcsSwitch => "version-control-switch", aliases: ["vc-switch", "switch"], palette: "Plan switching to another branch";
        VcsDeleteBranch => "version-control-delete-branch", aliases: ["vc-delete-branch"], palette: "Plan deleting a branch";
        VcsMerge => "version-control-merge", aliases: ["vc-merge"], palette: "Plan merging a branch into the current one";
        VcsRebase => "version-control-rebase", aliases: ["vc-rebase"], palette: "Plan rebasing the current branch onto another";
        VcsStashPop => "version-control-stash-pop", aliases: ["vc-stash-pop"], palette: "Plan popping a stash";
        VcsStashApply => "version-control-stash-apply", aliases: ["vc-stash-apply"], palette: "Plan applying a stash, keeping it";
        VcsStashDrop => "version-control-stash-drop", aliases: ["vc-stash-drop"], palette: "Plan dropping a stash";
        VcsCherryPick => "version-control-cherry-pick", aliases: ["vc-cherry-pick"], palette: "Plan cherry-picking a commit";
        VcsRevert => "version-control-revert", aliases: ["vc-revert"], palette: "Plan reverting a commit";
        VcsResetSoft => "version-control-reset-soft", aliases: ["vc-reset-soft"], palette: "Plan moving HEAD to a commit, keeping the index and files";
        VcsResetMixed => "version-control-reset-mixed", aliases: ["vc-reset-mixed"], palette: "Plan moving HEAD to a commit and unstaging, keeping files";
        VcsContinue => "version-control-continue", aliases: ["vc-continue"], palette: "Plan continuing the merge / rebase / pick in progress";
        VcsAbort => "version-control-abort", aliases: ["vc-abort"], palette: "Plan aborting the merge / rebase / pick in progress";
        VcsUndoEdit => "version-control-undo-step", aliases: ["vc-undo-step"], palette: "Take back the last planned step";
        VcsRemoveStep => "version-control-remove-step", aliases: ["vc-remove-step"], palette: "Remove the planned step under the cursor";
        VcsReset => "version-control-clear-plan", aliases: ["vc-clear-plan"], palette: "Discard the whole plan";
        VcsApply => "version-control-apply", aliases: ["vc-apply", "apply"], palette: "Apply the plan to the repository";
        VcsUndo => "version-control-undo", aliases: ["vc-undo"], palette: "Put the repository back the way it was before the last apply";
        VcsGitStatus => "version-control-git-status", aliases: ["vc-git-status", "git-status"], palette: "Show `git status` verbatim in a float";
        VcsHelp => "version-control-help", aliases: ["vc-help", "vc-keys"], palette: "Every key the version-control view binds";

        // --- Merge conflicts, in the version-control view's diff panel ---
        ConflictOpen => "conflicts", aliases: ["resolve", "merge-conflicts"], palette: "Resolve the merge conflicts, side by side with labels";
        ConflictTakeSide => "conflict-take-side", aliases: ["resolve-take"], palette: "Take (or drop) the focused side of this conflict";
        ConflictTakeLeft => "conflict-take-left", aliases: ["resolve-left"], palette: "Take only the left side of this conflict";
        ConflictTakeRight => "conflict-take-right", aliases: ["resolve-right"], palette: "Take only the right side of this conflict";
        ConflictTakeLeftAll => "conflict-take-left-all", aliases: ["resolve-left-all"], palette: "Take the left side of every unanswered conflict in this file";
        ConflictTakeRightAll => "conflict-take-right-all", aliases: ["resolve-right-all"], palette: "Take the right side of every unanswered conflict in this file";
        ConflictNextHunk => "conflict-next", aliases: ["resolve-next"], palette: "Next unanswered conflict";
        ConflictPrevHunk => "conflict-prev", aliases: ["resolve-prev"], palette: "Previous unanswered conflict";
        ConflictToggleBase => "conflict-toggle-base", aliases: ["resolve-base"], palette: "Show the common ancestor beside the two versions";
        ConflictEditHunk => "conflict-edit", aliases: ["resolve-edit"], palette: "Edit this conflict's merged text by hand";
        ConflictWriteFile => "conflict-resolve-file", aliases: ["resolve-file"], palette: "Plan this file's resolution";
        ConflictHelp => "conflict-help", aliases: ["resolve-help"], palette: "Every key the conflict resolver binds";

        // --- Toggles / config ---
        ToggleGitGutter => "toggle-git-gutter", aliases: ["git-gutter", "gutter"], palette: "Toggle git gutter indicators";
        ToggleLineNumbers => "toggle-line-numbers", aliases: ["line-numbers"], palette: "Toggle line numbers";
        ToggleRelativeLineNumbers => "toggle-relative-line-numbers", aliases: ["relative-line-numbers"], palette: "Toggle relative line numbers";
        ToggleWordWrap => "toggle-word-wrap", aliases: ["word-wrap", "wrap"], palette: "Toggle soft word-wrap";
        OpenConfig => "open-config", aliases: ["config"], palette: "Open config file in editor";
        ReloadConfig => "reload-config", aliases: ["config-reload"], palette: "Reload config from disk";
        OpenThemePicker => "open-theme-picker", aliases: ["themes"], palette: "Choose color theme";

        // --- Dashboard ---
        ShowDashboard => "show-dashboard", aliases: ["dashboard", "home", "splash"], palette: "Show the welcome / dashboard screen";
    }
    data: {
        // Move the cursor to a 1-based line number (also the numeric `:N` form).
        GotoLine(usize) => "goto-line";
        // Write the buffer to a new path.
        WriteAs(String) => "write-as", palette: "Write to new path";
        // Run a shell command.
        Shell(String) => "shell", palette: "Run a shell command";
        // Render the current notebook / markdown document via Quarto.
        ExportDocument(String) => "export", palette: "Export via Quarto to pdf/html/docx…";
        // Open a kernel dataframe as a grid (`:view df`).
        ViewVariable(String) => "view", palette: "Open a kernel dataframe as a grid";
        // Attach a local database file read-only (`:attach <path> [as <alias>]`).
        Attach(String) => "attach", palette: "Attach a local database file, read-only";
        // Drop one attachment by alias, or all of them when the argument is empty.
        Detach(String) => "detach", palette: "Detach an attached database";
        // Plan a branch at HEAD and switch to it (`:vc-branch <name>`).
        VcsNewBranch(String) => "version-control-branch", palette: "Plan a new branch here, and switch to it";
        // Plan committing what is staged (`:vc-commit <message>`).
        VcsCommit(String) => "version-control-commit", palette: "Plan committing the staged changes";
        // Plan amending the last commit (`:vc-amend <message>`).
        VcsAmend(String) => "version-control-amend", palette: "Plan amending the last commit";
        // Plan renaming the current branch (`:vc-rename <name>`).
        VcsRenameBranch(String) => "version-control-rename-branch", palette: "Plan renaming the current branch";
        // Plan stashing the changes (`:vc-stash [message]`).
        VcsStashPush(String) => "version-control-stash", palette: "Plan stashing the changes";
        // Plan a tag (`:vc-tag <name>` tags HEAD; bare picks a commit).
        VcsTag(String) => "version-control-tag", palette: "Plan tagging a commit";
        // Change any config value for the session (`:set editor.tab_width 2`).
        Set(String) => "set", palette: "Change a setting for this session";
        // Flip an on/off config value for the session (`:toggle word_wrap`).
        Toggle(String) => "toggle", palette: "Turn a setting on or off for this session";
        // Switch to a named color theme (`:theme <name>`; bare `:theme` opens the picker).
        SwitchTheme(String) => "theme";
        // Select the text object at the cursor (`m i w`, `m o f`, ...).
        SelectTextObject(crate::textobject::TextObject, crate::textobject::Scope) => "select-text-object";
        // A list of commands executed in sequence (composition / scripting).
        Sequence(Vec<Command>) => "sequence";
    }
}

/// The palette description of the command named `name`, if it has one.
pub fn describe(name: &str) -> Option<&'static str> {
    Command::palette_entries().into_iter().find(|(n, _)| *n == name).map(|(_, d)| d)
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
            // `:export` defaults to PDF; `:export html` etc. pass the format through.
            "export" | "quarto" => {
                let fmt = arg.unwrap_or("").trim();
                let fmt = if fmt.is_empty() { "pdf" } else { fmt };
                Some(Command::ExportDocument(fmt.to_string()))
            }
            // Bare `:attach` lists what is attached; bare `:detach` drops it all.
            "attach" => Some(Command::Attach(arg.unwrap_or("").trim().to_string())),
            // `:view` with no name lists the namespace instead of erroring.
            "view" => match arg.map(str::trim).filter(|a| !a.is_empty()) {
                Some(name) => Some(Command::ViewVariable(name.to_string())),
                None => Some(Command::KernelVariables),
            },
            "detach" => Some(Command::Detach(arg.unwrap_or("").trim().to_string())),
            // These want an argument but still parse bare: the palette can
            // only invoke a command bare, and `exec::vcs` asks for the missing
            // half (or opens the commit-message buffer) when it is empty.
            "version-control-branch" | "vc-branch" | "branch" => {
                Some(Command::VcsNewBranch(arg.unwrap_or("").trim().to_string()))
            }
            "version-control-commit" | "vc-commit" | "commit" => {
                Some(Command::VcsCommit(arg.unwrap_or("").trim().to_string()))
            }
            "version-control-amend" | "vc-amend" | "amend" => {
                Some(Command::VcsAmend(arg.unwrap_or("").trim().to_string()))
            }
            "version-control-rename-branch" | "vc-rename" => {
                Some(Command::VcsRenameBranch(arg.unwrap_or("").trim().to_string()))
            }
            "version-control-stash" | "vc-stash" | "stash" => {
                Some(Command::VcsStashPush(arg.unwrap_or("").trim().to_string()))
            }
            "version-control-tag" | "vc-tag" | "tag" => {
                Some(Command::VcsTag(arg.unwrap_or("").trim().to_string()))
            }
            "goto-line" => {
                let n = arg.unwrap_or("").trim().parse::<usize>().ok()?;
                Some(Command::GotoLine(n))
            }
            "set" => Some(Command::Set(arg.unwrap_or("").to_string())),
            "toggle" => Some(Command::Toggle(arg.unwrap_or("").to_string())),
            // `:theme <name>` switches directly; bare `:theme` opens the picker.
            "theme" => match arg {
                Some(name) if !name.is_empty() => Some(Command::SwitchTheme(name.to_string())),
                _ => Some(Command::OpenThemePicker),
            },
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
        const ARG_COMMANDS: &[&str] = &[
            "write-as",
            "shell",
            "view",
            "version-control-branch",
            "version-control-commit",
            "version-control-amend",
            "version-control-rename-branch",
            "version-control-stash",
            "version-control-tag",
        ];
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
    fn the_version_control_view_is_reachable_by_every_name_it_advertises() {
        for name in ["version-control", "vc", "git"] {
            assert!(
                matches!(Command::parse(name), Some(Command::VcsOpen)),
                "{name} should open the view"
            );
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
