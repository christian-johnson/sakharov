use ropey::Rope;

pub struct Symbol {
    pub name: String,
    pub kind: &'static str,
    /// 0-indexed line in the source.
    pub line: usize,
    /// 0-indexed column (byte column from tree-sitter).
    pub col: usize,
}

/// Extract top-level symbols from `rope` using tree-sitter.
/// Returns symbols sorted by line number.  Returns an empty vec for
/// unsupported languages or when tree-sitter fails to parse.
pub fn extract_symbols(rope: &Rope, language: &str) -> Vec<Symbol> {
    let source = rope.to_string();
    match language {
        "python" | "python3" => run(
            &source,
            tree_sitter_python::language(),
            // Each pattern corresponds to one entry in `kinds` by index.
            "(function_definition name: (identifier) @name)
             (class_definition    name: (identifier) @name)",
            &["fn", "class"],
        ),
        "rust" => run(
            &source,
            tree_sitter_rust::language(),
            "(function_item name: (identifier)      @name)
             (struct_item   name: (type_identifier) @name)
             (enum_item     name: (type_identifier) @name)
             (trait_item    name: (type_identifier) @name)
             (const_item    name: (identifier)      @name)
             (impl_item     type: (type_identifier) @name)",
            &["fn", "struct", "enum", "trait", "const", "impl"],
        ),
        "javascript" | "js" => run(
            &source,
            tree_sitter_javascript::language(),
            "(function_declaration name: (identifier)         @name)
             (class_declaration    name: (identifier)         @name)
             (method_definition    name: (property_identifier) @name)",
            &["fn", "class", "method"],
        ),
        _ => vec![],
    }
}

fn run(
    source: &str,
    language: tree_sitter::Language,
    query_src: &str,
    kinds: &[&'static str],
) -> Vec<Symbol> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(source.as_bytes(), None) else {
        return vec![];
    };
    let Ok(query) = tree_sitter::Query::new(&language, query_src) else {
        return vec![];
    };
    let Some(name_idx) = query.capture_index_for_name("name") else {
        return vec![];
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut symbols: Vec<Symbol> = Vec::new();

    for m in cursor.matches(&query, tree.root_node(), source.as_bytes()) {
        let kind = kinds.get(m.pattern_index).copied().unwrap_or("symbol");
        // Each pattern has exactly one @name capture.
        if let Some(cap) = m.captures.iter().find(|c| c.index == name_idx) {
            let name = cap
                .node
                .utf8_text(source.as_bytes())
                .unwrap_or("?")
                .to_owned();
            symbols.push(Symbol {
                name,
                kind,
                line: cap.node.start_position().row,
                col: cap.node.start_position().column,
            });
        }
    }

    symbols.sort_by_key(|s| s.line);
    symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_python_symbols() {
        let code = "def foo():\n    pass\n\nasync def bar():\n    pass\n\nclass Bar:\n    def baz():\n        pass";
        let rope = ropey::Rope::from_str(code);
        let syms = extract_symbols(&rope, "python");
        assert_eq!(syms.len(), 4);
        assert_eq!(syms[0].name, "foo");
        assert_eq!(syms[0].kind, "fn");
        assert_eq!(syms[1].name, "bar");
        assert_eq!(syms[1].kind, "fn");
        assert_eq!(syms[2].name, "Bar");
        assert_eq!(syms[2].kind, "class");
        assert_eq!(syms[3].name, "baz");
        assert_eq!(syms[3].kind, "fn");
    }

    #[test]
    fn test_rust_symbols() {
        let code = "fn foo() {} \nstruct Bar; \nimpl Bar {}";
        let rope = ropey::Rope::from_str(code);
        let syms = extract_symbols(&rope, "rust");
        assert_eq!(syms.len(), 3);
        assert_eq!(syms[0].name, "foo");
        assert_eq!(syms[0].kind, "fn");
        assert_eq!(syms[1].name, "Bar");
        assert_eq!(syms[1].kind, "struct");
        assert_eq!(syms[2].name, "Bar");
        assert_eq!(syms[2].kind, "impl");
    }

    #[test]
    fn test_javascript_symbols() {
        let code = "function foo() {} \nclass Bar { baz() {} }";
        let rope = ropey::Rope::from_str(code);
        let syms = extract_symbols(&rope, "javascript");
        assert_eq!(syms.len(), 3);
        assert_eq!(syms[0].name, "foo");
        assert_eq!(syms[0].kind, "fn");
        assert_eq!(syms[1].name, "Bar");
        assert_eq!(syms[1].kind, "class");
        assert_eq!(syms[2].name, "baz");
        assert_eq!(syms[2].kind, "method");
    }
}
