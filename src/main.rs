mod app;
mod buffer;
mod clipboard;
mod command;
mod compute;
mod config;
mod exec;
mod fold;
mod git;
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
mod sql_highlight;
mod statusline;
mod symbols;
mod table;
mod table_ui;
mod theme;
mod ui;
mod view;

use std::process;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(String::as_str);

    if let Err(e) = app::run(path) {
        eprintln!("sv: {e}");
        process::exit(1);
    }
}
