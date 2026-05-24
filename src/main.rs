mod app;
mod buffer;
mod command;
mod config;
mod exec;
mod fold;
mod git;
mod highlight;
mod indent;
mod input;
mod jump;
mod keymap;
mod kitty;
mod lang;
mod lsp;
mod lsp_manager;
mod mode;
mod motion;
mod notebook;
mod notebook_state;
mod notebook_ui;
mod popup;
mod popup_input;
mod popup_ui;
mod selection;
mod symbols;
mod theme;
mod ui;

use std::process;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(String::as_str);

    if let Err(e) = app::run(path) {
        eprintln!("mj: {e}");
        process::exit(1);
    }
}
