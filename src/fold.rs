use std::collections::BTreeSet;

use ropey::Rope;

use crate::highlight::Language;

/// An inclusive (start_line, end_line) foldable range, 0-indexed.
pub type FoldRange = (usize, usize);

// ---------------------------------------------------------------------------
// FoldState
// ---------------------------------------------------------------------------

/// Per-buffer fold state: which ranges exist and which are currently closed.
#[derive(Default)]
pub struct FoldState {
    /// Start lines of currently-folded ranges.
    pub folded: BTreeSet<usize>,
    /// All foldable ranges in the buffer, sorted by start_line.
    /// Recomputed whenever the buffer changes.
    pub ranges: Vec<FoldRange>,
}

impl FoldState {
    /// Find the fold range that starts exactly at `line`.
    pub fn range_starting_at(&self, line: usize) -> Option<FoldRange> {
        self.ranges
            .binary_search_by_key(&line, |&(s, _)| s)
            .ok()
            .map(|i| self.ranges[i])
    }

    /// Find the innermost foldable range that contains `line`.
    pub fn range_containing(&self, line: usize) -> Option<FoldRange> {
        self.ranges
            .iter()
            .filter(|&&(s, e)| s <= line && e >= line)
            .max_by_key(|&&(s, _)| s)
            .copied()
    }

    /// If `line` starts a folded range, return its end line.
    pub fn fold_end_at(&self, line: usize) -> Option<usize> {
        if self.folded.contains(&line) {
            self.range_starting_at(line).map(|(_, e)| e)
        } else {
            None
        }
    }

    /// True if `line` is hidden inside a folded region (not the fold-start line).
    pub fn is_hidden(&self, line: usize) -> bool {
        for &start in &self.folded {
            if let Some((s, e)) = self.range_starting_at(start) {
                if line > s && line <= e {
                    return true;
                }
            }
        }
        false
    }

    /// If `line` is hidden inside a fold, return that fold's start line.
    pub fn fold_start_hiding(&self, line: usize) -> Option<usize> {
        for &start in &self.folded {
            if let Some((s, e)) = self.range_starting_at(start) {
                if line > s && line <= e {
                    return Some(s);
                }
            }
        }
        None
    }

    /// Snap `line` to its fold's start if it is hidden.
    pub fn normalize_line(&self, line: usize) -> usize {
        self.fold_start_hiding(line).unwrap_or(line)
    }

    /// Snap `scroll_row` to its fold's start if it falls inside a hidden region.
    pub fn normalize_scroll_row(&self, scroll_row: usize) -> usize {
        self.normalize_line(scroll_row)
    }

    /// Toggle the innermost fold at/containing `line`.
    pub fn toggle_at_line(&mut self, line: usize) {
        if let Some((start, end)) = self.range_containing(line) {
            if start == end {
                return;
            }
            if self.folded.contains(&start) {
                self.folded.remove(&start);
            } else {
                self.folded.insert(start);
            }
        }
    }

    pub fn close_all(&mut self) {
        for &(start, end) in &self.ranges {
            if start != end {
                self.folded.insert(start);
            }
        }
    }

    pub fn open_all(&mut self) {
        self.folded.clear();
    }

    /// Walk forward from `scroll_row`, yielding up to `count` visible entries.
    /// Each entry is `(buffer_line, fold_end_line_or_none)`.
    /// When `fold_end` is `Some(e)`, the entry is a fold indicator that hides lines
    /// `buffer_line+1 ..= e`.
    pub fn visible_entries(
        &self,
        scroll_row: usize,
        count: usize,
        total_lines: usize,
    ) -> Vec<(usize, Option<usize>)> {
        let mut entries = Vec::with_capacity(count);
        let mut line = scroll_row;
        while entries.len() < count && line < total_lines {
            if let Some(end) = self.fold_end_at(line) {
                entries.push((line, Some(end)));
                line = end + 1;
            } else {
                entries.push((line, None));
                line += 1;
            }
        }
        entries
    }

    /// Count visible rows needed to travel from `from` (inclusive) to `to` (exclusive).
    pub fn visible_row_count(&self, from: usize, to: usize, total_lines: usize) -> usize {
        let mut count = 0;
        let mut line = from;
        while line < to && line < total_lines {
            if let Some(end) = self.fold_end_at(line) {
                count += 1;
                line = end + 1;
            } else {
                count += 1;
                line += 1;
            }
        }
        count
    }

    /// Find the scroll_row such that `cursor_line` appears at visible row `desired_vrow`
    /// within the viewport (0-indexed from the top).
    pub fn scroll_row_for_cursor(&self, cursor_line: usize, desired_vrow: usize) -> usize {
        let mut line = cursor_line;
        let mut remaining = desired_vrow;
        while remaining > 0 {
            if line == 0 {
                break;
            }
            line -= 1;
            // If this line is inside a hidden fold, jump to the fold start.
            if let Some(start) = self.fold_start_hiding(line) {
                line = start;
            }
            remaining -= 1;
        }
        line
    }
}

// ---------------------------------------------------------------------------
// Tree-sitter fold range computation
// ---------------------------------------------------------------------------

/// Compute all foldable ranges in `rope` for the given language.
pub fn compute_fold_ranges(rope: &Rope, language: Language) -> Vec<FoldRange> {
    let text = rope.to_string();
    let ts_lang = language.ts_language();

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(text.as_bytes(), None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut ranges = Vec::new();
    walk_tree(&tree, language, &mut ranges);
    ranges.sort_by_key(|&(s, _)| s);
    ranges.dedup_by_key(|&mut (s, _)| s);
    ranges
}

fn walk_tree(tree: &tree_sitter::Tree, language: Language, ranges: &mut Vec<FoldRange>) {
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let start_row = node.start_position().row;
        let end_row = node.end_position().row;

        if end_row > start_row && is_foldable_node(node.kind(), language) {
            ranges.push((start_row, end_row));
        }

        // DFS: go into children first
        if cursor.goto_first_child() {
            continue;
        }
        // No children: try next sibling, then backtrack
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return; // done
            }
        }
    }
}

fn is_foldable_node(kind: &str, language: Language) -> bool {
    match language {
        Language::Python => matches!(
            kind,
            "function_definition"
                | "class_definition"
                | "for_statement"
                | "while_statement"
                | "if_statement"
                | "with_statement"
                | "try_statement"
                | "decorated_definition"
                | "match_statement"
        ),
        Language::Rust => matches!(
            kind,
            "function_item"
                | "impl_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "mod_item"
                | "match_expression"
                | "closure_expression"
        ),
        Language::JavaScript => matches!(
            kind,
            "function_declaration"
                | "function"
                | "arrow_function"
                | "class_declaration"
                | "class"
                | "method_definition"
                | "if_statement"
                | "for_statement"
                | "while_statement"
                | "switch_statement"
                | "try_statement"
        ),
        Language::Toml => matches!(kind, "table" | "table_array_element" | "array" | "inline_table"),
        Language::Json => matches!(kind, "object" | "array"),
        Language::Yaml => matches!(kind, "block_mapping_pair" | "block_sequence"),
        Language::Bash => matches!(
            kind,
            "function_definition"
                | "if_statement"
                | "for_statement"
                | "while_statement"
                | "case_statement"
                | "subshell"
        ),
        Language::Go => matches!(
            kind,
            "function_declaration"
                | "method_declaration"
                | "type_declaration"
                | "struct_type"
                | "interface_type"
                | "if_statement"
                | "for_statement"
                | "expression_switch_statement"
                | "type_switch_statement"
        ),
        Language::C => matches!(
            kind,
            "function_definition"
                | "struct_specifier"
                | "enum_specifier"
                | "union_specifier"
                | "if_statement"
                | "for_statement"
                | "while_statement"
                | "switch_statement"
        ),
        Language::Html => kind == "element",
        Language::Css => matches!(kind, "rule_set" | "media_statement" | "keyframes_statement"),
    }
}
