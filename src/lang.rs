use std::path::Path;

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
        "sql" => "sql",
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
        "sql" => Some("sql"),
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

/// The lowercased extension of `path`.
pub fn extension(path: &Path) -> Option<String> {
    path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase)
}

/// The language id of `path`: by extension (any case), falling back to a
/// filename match for extensionless shell dotfiles like `.zshrc`.  The one
/// place a file's language is decided — highlighting, folding and the LSP
/// all start here.
pub fn lang_for_path(path: &Path) -> Option<&'static str> {
    if let Some(lang) = extension(path).as_deref().and_then(ext_to_lang) {
        return Some(lang);
    }
    let name = path.file_name()?.to_str()?;
    SHELL_DOTFILES.contains(&name).then_some("bash")
}

/// True for a Jupyter notebook (`.ipynb`).
pub fn is_notebook(path: &Path) -> bool {
    extension(path).as_deref() == Some("ipynb")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every consumer sees the same answer whatever the extension's case.
    #[test]
    fn detection_ignores_extension_case() {
        assert_eq!(lang_for_path(Path::new("README.MD")), Some("markdown"));
        assert_eq!(lang_for_path(Path::new("main.RS")), Some("rust"));
        assert_eq!(lang_for_path(Path::new(".zshrc")), Some("bash"));
        assert_eq!(lang_for_path(Path::new("data.bin")), None);
        assert!(is_notebook(Path::new("Analysis.IPYNB")));
    }
}
