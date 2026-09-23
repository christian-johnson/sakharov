//! `:set` and `:toggle` — change any config value for the rest of the session.
//!
//! The live config is turned into a TOML tree, the one value is replaced and
//! the tree is parsed back, so a setting is type-checked exactly like the
//! config file and an unknown name is refused rather than ignored.  Every
//! config change — these commands, the dedicated toggles, `:config-reload` —
//! lands through [`replace_config`].

use crate::app::App;
use crate::config::Config;

/// `:set <setting> <value>`.
///
/// `setting` is a dotted path (`editor.tab_width`) or a name unique across
/// sections (`tab_width`).  `value` is TOML; a bare word is taken as a string,
/// so `:set theme.name nord` needs no quotes.
pub fn set(app: &mut App, arg: &str) {
    let Some((key, raw)) = arg.trim().split_once(char::is_whitespace) else {
        app.messages.show("Usage: :set <setting> <value>");
        return;
    };
    let value = parse_value(raw.trim());
    let result = update(app, key, |_| Ok(value));
    report(app, result);
}

/// `:toggle <setting>` — flip an on/off setting.
pub fn toggle(app: &mut App, key: &str) {
    let result = update(app, key, |old| match old {
        toml::Value::Boolean(b) => Ok(toml::Value::Boolean(!b)),
        _ => Err("is not an on/off setting".to_string()),
    });
    report(app, result);
}

/// Install `config` and bring everything derived from it up to date.
/// Returns warnings about values that could not be applied.
pub fn replace_config(app: &mut App, config: Config) -> Vec<String> {
    let old = std::mem::replace(&mut app.config, config);
    let warnings = app.apply_config();

    if app.config.editor.git_gutter != old.editor.git_gutter {
        if app.config.editor.git_gutter {
            super::refresh_git(app);
        } else {
            app.git_diff.clear();
        }
    }
    // Wrapped text has no horizontal scroll.
    if app.config.editor.word_wrap {
        app.scroll_col = 0;
    }
    if let Some(state) = app.vcs.as_mut() {
        if state.options.orientation != app.config.vcs.orientation {
            state.flip();
        }
    }
    if let Some(state) = app.conflict.as_mut() {
        state.show_base = app.config.conflict.show_base;
    }
    warnings
}

fn report(app: &mut App, result: Result<(String, toml::Value), String>) {
    let msg = match result {
        Ok((path, toml::Value::Boolean(on))) => format!("{path}: {}", if on { "on" } else { "off" }),
        Ok((path, value)) => format!("{path} = {value}"),
        Err(e) => e,
    };
    app.messages.show(msg);
}

/// Replace the setting `key` names with `f(current value)`.  Returns the
/// setting's full path and new value.
fn update(
    app: &mut App,
    key: &str,
    f: impl FnOnce(&toml::Value) -> Result<toml::Value, String>,
) -> Result<(String, toml::Value), String> {
    let mut tree =
        toml::Value::try_from(&app.config).map_err(|e| format!("Cannot read settings: {e}"))?;
    let path = resolve(&tree, key).ok_or_else(|| format!("Unknown setting: {key}"))?;
    let slot = path
        .split('.')
        .try_fold(&mut tree, |node, part| node.get_mut(part))
        .expect("resolve returned an existing path");
    let value = f(slot).map_err(|e| format!("{path} {e}"))?;
    *slot = value.clone();
    let config: Config = tree.try_into().map_err(|e| format!("{path}: {e}"))?;
    for warning in replace_config(app, config) {
        app.messages.show(warning);
    }
    Ok((path, value))
}

/// The full dotted path of the setting `key` names, if exactly one does.
/// Dashes and underscores are interchangeable (`word-wrap`).
fn resolve(tree: &toml::Value, key: &str) -> Option<String> {
    let key = key.trim().replace('-', "_");
    if key.contains('.') {
        let found = key.split('.').try_fold(tree, |node, part| node.get(part))?;
        return (!found.is_table()).then_some(key);
    }
    let mut matches = Vec::new();
    find_leaves(tree, "", &key, &mut matches);
    (matches.len() == 1).then(|| matches.remove(0))
}

fn find_leaves(node: &toml::Value, prefix: &str, name: &str, out: &mut Vec<String>) {
    let Some(table) = node.as_table() else { return };
    for (k, v) in table {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        if v.is_table() {
            find_leaves(v, &path, name, out);
        } else if k == name {
            out.push(path);
        }
    }
}

/// Parse a `:set` value as TOML, falling back to a plain string.
fn parse_value(raw: &str) -> toml::Value {
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| toml::Value::String(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(None, Config::load()).unwrap()
    }

    #[test]
    fn set_changes_a_value_by_short_or_dotted_name() {
        let mut app = app();

        set(&mut app, "tab_width 2");
        set(&mut app, "table.null_display NULL");

        assert_eq!(app.config.editor.tab_width, 2);
        assert_eq!(app.config.table.null_display, "NULL");
    }

    #[test]
    fn set_refuses_unknown_names_and_wrong_types() {
        let mut app = app();
        let before = app.config.editor.tab_width;

        set(&mut app, "tab_wdth 2");
        assert_eq!(app.messages.current(), Some("Unknown setting: tab_wdth"));
        set(&mut app, "tab_width lots");

        assert_eq!(app.config.editor.tab_width, before);
        assert!(app.messages.current().unwrap().starts_with("editor.tab_width"));
    }

    #[test]
    fn toggle_flips_booleans_only() {
        let mut app = app();
        let wrap = app.config.editor.word_wrap;

        toggle(&mut app, "word-wrap");
        toggle(&mut app, "tab_width");

        assert_eq!(app.config.editor.word_wrap, !wrap);
        assert_eq!(app.messages.current(), Some("editor.tab_width is not an on/off setting"));
    }
}
