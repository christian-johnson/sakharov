mod app;
mod buffer;
mod clipboard;
mod command;
mod compute;
mod config;
mod conflict;
mod conflict_ui;
mod exec;
mod fold;
mod git;
mod git_highlight;
mod highlight;
mod history;
mod indent;
mod input;
mod jump;
mod keymap;
mod kitty;
mod lang;
mod lsp;
mod lsp_manager;
mod markdown;
mod mode;
mod motion;
mod notebook;
mod notebook_state;
mod notebook_ui;
mod popup;
mod popup_input;
mod popup_ui;
mod recovery;
mod render_util;
mod selection;
mod source;
mod spinner;
mod splash;
mod stash;
mod sql_highlight;
mod statusline;
mod symbols;
mod table;
mod table_ui;
mod textobject;
mod theme;
mod ui;
mod vcs;
mod vcs_ui;
mod view;

use std::process;

fn main() {
    let (path, line) = parse_args(std::env::args().skip(1));

    if let Err(e) = app::run(path.as_deref(), line) {
        eprintln!("sv: {e}");
        process::exit(1);
    }
}

/// Split the command line into the file to open and a vim-style `+N` start line.
///
/// Only `+` followed by digits is a line number; anything else (e.g. a file
/// literally named `+notes`) is taken as the path. Later arguments win.
///
/// Returns `(path, line)`, where `line` is 1-based.
fn parse_args(args: impl IntoIterator<Item = String>) -> (Option<String>, Option<usize>) {
    let mut path = None;
    let mut line = None;
    for arg in args {
        match arg.strip_prefix('+').map(str::parse::<usize>) {
            Some(Ok(n)) => line = Some(n),
            _ => path = Some(arg),
        }
    }
    (path, line)
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    fn parse(args: &[&str]) -> (Option<String>, Option<usize>) {
        parse_args(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn plus_n_sets_the_start_line_on_either_side_of_the_path() {
        assert_eq!(parse(&["+42", "a.rs"]), (Some("a.rs".into()), Some(42)));
        assert_eq!(parse(&["a.rs", "+7"]), (Some("a.rs".into()), Some(7)));
        assert_eq!(parse(&["a.rs"]), (Some("a.rs".into()), None));
        assert_eq!(parse(&[]), (None, None));
    }

    #[test]
    fn a_plus_without_digits_is_a_path() {
        assert_eq!(parse(&["+notes"]), (Some("+notes".into()), None));
    }
}
