mod app;
mod buffer;
mod command;
mod config;
mod exec;
mod highlight;
mod input;
mod keymap;
mod kitty;
mod mode;
mod motion;
mod notebook;
mod notebook_state;
mod notebook_ui;
mod selection;
mod theme;
mod ui;

use std::process;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(String::as_str);

    if let Err(e) = app::run(path) {
        eprintln!("ki: {e}");
        process::exit(1);
    }
}
