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
            "(function_definition       name: (identifier) @name)
             (async_function_definition name: (identifier) @name)
             (class_definition          name: (identifier) @name)",
            &["fn", "async fn", "class"],
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
