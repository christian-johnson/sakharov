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
