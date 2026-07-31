/// Map a language id to its canonical file extension.
///
/// This is the single source of truth used by the editor, notebook UI, and
/// LSP virtual-path construction.  Add new languages here only.
pub fn lang_to_ext(lang: &str) -> &'static str {
    match lang {
        "python" | "python3" => "py",
        "javascript" | "js" => "js",
        "rust" => "rs",
        "markdown" => "md",
        "toml" => "toml",
        "json" => "json",
        "yaml" => "yaml",
        "bash" | "sh" | "shell" => "sh",
        "go" => "go",
        "c" => "c",
        "html" => "html",
        "css" => "css",
        _ => "txt",
    }
}

/// Map a file extension to an LSP language id — the inverse of [`lang_to_ext`].
pub fn ext_to_lang(ext: &str) -> Option<&'static str> {
    match ext {
        "py" => Some("python"),
        "rs" => Some("rust"),
        "js" | "ts" | "jsx" | "tsx" => Some("javascript"),
        "md" | "markdown" | "qmd" => Some("markdown"),
        "toml" => Some("toml"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "sh" | "bash" | "zsh" => Some("bash"),
        "go" => Some("go"),
        "c" | "h" => Some("c"),
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        _ => None,
    }
}

/// Shell rc/profile filenames that carry no extension at all (`.zshrc`,
/// `.bashrc`, ...). `Path::extension()` returns `None` for a name that starts
/// with `.` and has no other `.` in it, so these dotfiles need to be matched
/// on their full filename rather than an extension.
pub const SHELL_DOTFILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".bash_aliases",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".profile",
];

/// Map a bare filename (no extension) to an LSP language id, for files like
/// [`SHELL_DOTFILES`] that `ext_to_lang` can never see.
pub fn filename_to_lang(filename: &str) -> Option<&'static str> {
    SHELL_DOTFILES.contains(&filename).then_some("bash")
}
