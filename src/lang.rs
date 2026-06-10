/// Map a language id to its canonical file extension.
///
/// This is the single source of truth used by the editor, notebook UI, and
/// LSP virtual-path construction.  Add new languages here only.
pub fn lang_to_ext(lang: &str) -> &'static str {
    match lang {
        "python" | "python3" => "py",
        "javascript" | "js" => "js",
        "rust" => "rs",
        _ => "txt",
    }
}

/// Map a file extension to an LSP language id — the inverse of [`lang_to_ext`].
pub fn ext_to_lang(ext: &str) -> Option<&'static str> {
    match ext {
        "py" => Some("python"),
        "rs" => Some("rust"),
        "js" | "ts" | "jsx" | "tsx" => Some("javascript"),
        _ => None,
    }
}
