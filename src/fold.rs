use std::collections::BTreeSet;

use ropey::Rope;

use crate::highlight::Language;

/// An inclusive line range that can be folded, plus what kind of thing it is.
///
/// The `kind`/`depth`/`label` triple is what makes "fold every block like this
/// one" ([`FoldState::close_type`]) possible: a plain `(start, end)` pair says
/// nothing about *what* was folded, so there is no way to find its peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldRange {
    /// First line of the range (the one that stays visible when folded).
    pub start: usize,
    /// Last line of the range, inclusive.
    pub end: usize,
    /// How many foldable ranges enclose this one (0 = outermost).  Assigned by
    /// [`assign_depths`] from containment, so it is consistent across backends.
    pub depth: usize,
    /// Syntactic kind: a tree-sitter node kind (`"object"`, `"function_item"`)
    /// or, for Markdown, `"section"` / `"fence"`.
    pub kind: &'static str,
    /// The key this range is the value of, where the language has such a notion
    /// (JSON pairs, YAML mappings, TOML tables).  `None` for an array element,
    /// a bare block, or a language without keys.
    pub label: Option<String>,
}

impl FoldRange {
    /// A range with no depth (filled in later) and no label.
    pub fn new(start: usize, end: usize, kind: &'static str) -> Self {
        Self { start, end, depth: 0, kind, label: None }
    }

    /// True when `line` falls anywhere in the range, start and end included.
    pub fn contains(&self, line: usize) -> bool {
        self.start <= line && line <= self.end
    }

    /// The `kind`/`depth`/`label` identity used to match sibling blocks.
    pub fn fold_type(&self) -> FoldType {
        FoldType { kind: self.kind, depth: self.depth, label: self.label.clone() }
    }
}

/// The identity of a *class* of fold ranges: everything that is "the same kind
/// of thing at the same nesting level".
///
/// Equality is exact on all three fields, including `label`, which is what
/// makes `zt` on a `"payload": { … }` block fold every other `payload` and
/// leave the `metadata` blocks beside them alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldType {
    pub kind: &'static str,
    pub depth: usize,
    pub label: Option<String>,
}

impl FoldType {
    /// Human-readable name for a message: `"payload"` blocks, or `object`
    /// blocks when the range has no owning key.
    pub fn describe(&self) -> String {
        match &self.label {
            Some(key) => format!("\"{key}\""),
            None => self.kind.to_string(),
        }
    }
}

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
    pub fn range_starting_at(&self, line: usize) -> Option<&FoldRange> {
        self.ranges
            .binary_search_by_key(&line, |r| r.start)
            .ok()
            .map(|i| &self.ranges[i])
    }

    /// Find the innermost foldable range that contains `line`.
    pub fn range_containing(&self, line: usize) -> Option<&FoldRange> {
        self.ranges
            .iter()
            .filter(|r| r.contains(line))
            .max_by_key(|r| r.start)
    }

    /// If `line` starts a folded range, return its end line.
    pub fn fold_end_at(&self, line: usize) -> Option<usize> {
        if self.folded.contains(&line) {
            self.range_starting_at(line).map(|r| r.end)
        } else {
            None
        }
    }

    /// True if `line` is hidden inside a folded region (not the fold-start line).
    pub fn is_hidden(&self, line: usize) -> bool {
        for &start in &self.folded {
            if let Some(r) = self.range_starting_at(start) {
                if line > r.start && line <= r.end {
                    return true;
                }
            }
        }
        false
    }

    /// If `line` is hidden inside a fold, return that fold's start line.
    pub fn fold_start_hiding(&self, line: usize) -> Option<usize> {
        for &start in &self.folded {
            if let Some(r) = self.range_starting_at(start) {
                if line > r.start && line <= r.end {
                    return Some(r.start);
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
        let Some(start) = self
            .range_containing(line)
            .filter(|r| r.end > r.start)
            .map(|r| r.start)
        else {
            return;
        };
        if !self.folded.remove(&start) {
            self.folded.insert(start);
        }
    }

    /// Close the innermost *open* fold containing `line`; returns false when
    /// there is nothing left to close.
    ///
    /// Innermost-open rather than plain innermost so that repeated `zc` walks
    /// outward one level at a time, which is how nested structure (a JSON blob
    /// inside an entry inside the top-level array) gets collapsed by feel.
    pub fn close_at_line(&mut self, line: usize) -> bool {
        let Some(start) = self
            .ranges
            .iter()
            .filter(|r| r.contains(line) && r.end > r.start && !self.folded.contains(&r.start))
            .max_by_key(|r| r.start)
            .map(|r| r.start)
        else {
            return false;
        };
        self.folded.insert(start);
        true
    }

    /// Open the outermost folded range containing `line`; returns false when
    /// nothing there is folded.
    ///
    /// Outermost because an inner fold nested inside a closed one is invisible:
    /// the fold the cursor can actually see is the outermost one, so that is
    /// what `zo` must open, and repeated `zo` then reveals each level in turn.
    pub fn open_at_line(&mut self, line: usize) -> bool {
        let Some(start) = self
            .folded
            .iter()
            .copied()
            .filter(|&s| self.range_starting_at(s).is_some_and(|r| r.contains(line)))
            .min()
        else {
            return false;
        };
        self.folded.remove(&start);
        true
    }

    /// The fold class at `line` — the innermost range containing it.
    pub fn type_at_line(&self, line: usize) -> Option<FoldType> {
        self.range_containing(line)
            .filter(|r| r.end > r.start)
            .map(FoldRange::fold_type)
    }

    /// Start lines of every range belonging to class `ty`.
    pub fn starts_of_type(&self, ty: &FoldType) -> Vec<usize> {
        self.ranges
            .iter()
            .filter(|r| r.end > r.start && r.fold_type() == *ty)
            .map(|r| r.start)
            .collect()
    }

    /// Close every range of class `ty`; returns how many were newly closed.
    pub fn close_type(&mut self, ty: &FoldType) -> usize {
        self.starts_of_type(ty)
            .into_iter()
            .filter(|&s| self.folded.insert(s))
            .count()
    }

    /// Open every range of class `ty`; returns how many were newly opened.
    pub fn open_type(&mut self, ty: &FoldType) -> usize {
        self.starts_of_type(ty)
            .into_iter()
            .filter(|s| self.folded.remove(s))
            .count()
    }

    pub fn close_all(&mut self) {
        let closeable: Vec<usize> = self
            .ranges
            .iter()
            .filter(|r| r.end > r.start)
            .map(|r| r.start)
            .collect();
        self.folded.extend(closeable);
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

/// Assign each range a `depth` from containment.
///
/// Shared by both producers (tree-sitter and Markdown) so that "the same
/// nesting level" means the same thing regardless of which backend found the
/// range.  Expects `ranges` sorted by `start`.
pub fn assign_depths(ranges: &mut [FoldRange]) {
    // Stack of enclosing end lines; anything ending before this range started
    // is a sibling that has closed, not an ancestor.
    let mut enclosing: Vec<usize> = Vec::new();
    for r in ranges.iter_mut() {
        while enclosing.last().is_some_and(|&end| end < r.start) {
            enclosing.pop();
        }
        r.depth = enclosing.len();
        enclosing.push(r.end);
    }
}

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
    walk_tree(&tree, text.as_bytes(), language, &mut ranges);
    ranges.sort_by_key(|r| r.start);
    ranges.dedup_by_key(|r| r.start);
    assign_depths(&mut ranges);
    ranges
}

/// The object key a foldable node is the value of, if the language has keys.
///
/// This is what lets `zt` distinguish `"payload": { … }` from the
/// `"metadata": { … }` sitting next to it at the same depth — the single most
/// useful discrimination when reading a file of repeated records.
fn fold_label(node: tree_sitter::Node, src: &[u8], language: Language) -> Option<String> {
    let key_node = match language {
        // A JSON object/array is the value half of a `pair`; the key is its sibling.
        Language::Json => node
            .parent()
            .filter(|p| p.kind() == "pair")
            .and_then(|p| p.child_by_field_name("key")),
        // A YAML mapping pair *is* the foldable node, so the key is its own field.
        Language::Yaml => node.child_by_field_name("key"),
        _ => None,
    }?;
    let text = key_node.utf8_text(src).ok()?.trim();
    // JSON keys arrive quoted; the quotes are noise in a status message.
    Some(text.trim_matches('"').to_string())
}

fn walk_tree(
    tree: &tree_sitter::Tree,
    src: &[u8],
    language: Language,
    ranges: &mut Vec<FoldRange>,
) {
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let start_row = node.start_position().row;
        let end_row = node.end_position().row;

        if end_row > start_row && is_foldable_node(node.kind(), language) {
            ranges.push(FoldRange {
                label: fold_label(node, src, language),
                ..FoldRange::new(start_row, end_row, node.kind())
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A JSON log: two records with identical shape, each holding a `payload`
    /// and a `metadata` block.  This is the file the whole feature exists for.
    const LOG: &str = r#"{
  "entries": [
    {
      "timestamp": "t1",
      "payload": {
        "a": 1
      },
      "metadata": {
        "b": 2
      }
    },
    {
      "timestamp": "t2",
      "payload": {
        "a": 3
      },
      "metadata": {
        "b": 4
      }
    }
  ]
}
"#;

    fn json_state(src: &str) -> FoldState {
        FoldState {
            ranges: compute_fold_ranges(&Rope::from_str(src), Language::Json),
            folded: BTreeSet::new(),
        }
    }

    fn at(state: &FoldState, line: usize) -> &FoldRange {
        state
            .range_starting_at(line)
            .unwrap_or_else(|| panic!("no range starts at line {line}"))
    }

    #[test]
    fn json_ranges_carry_their_kind_depth_and_owning_key() {
        let s = json_state(LOG);

        assert_eq!(at(&s, 0).kind, "object");
        assert_eq!(at(&s, 0).depth, 0);
        assert_eq!(at(&s, 0).label, None, "the root object has no owning key");

        let entries = at(&s, 1);
        assert_eq!(entries.kind, "array");
        assert_eq!(entries.depth, 1);
        assert_eq!(entries.label.as_deref(), Some("entries"), "quotes stripped");

        // The two records: same kind, same depth, no key (they are array items).
        for start in [2, 11] {
            let r = at(&s, start);
            assert_eq!((r.kind, r.depth, r.label.clone()), ("object", 2, None));
        }
        // Their blocks sit one level deeper and are told apart by their key.
        assert_eq!(at(&s, 4).label.as_deref(), Some("payload"));
        assert_eq!(at(&s, 7).label.as_deref(), Some("metadata"));
        assert_eq!(at(&s, 4).depth, at(&s, 7).depth);
    }

    #[test]
    fn folding_by_type_hits_every_sibling_record() {
        let mut s = json_state(LOG);
        // Cursor anywhere in the first record.
        let ty = s.type_at_line(3).expect("inside a record");
        assert_eq!(s.close_type(&ty), 2, "both records fold");
        assert_eq!(s.folded.iter().copied().collect::<Vec<_>>(), vec![2, 11]);

        assert_eq!(s.open_type(&ty), 2);
        assert!(s.folded.is_empty());
    }

    #[test]
    fn folding_by_type_leaves_differently_keyed_neighbours_alone() {
        let mut s = json_state(LOG);
        // On the `payload` block: its peers are the other `payload`s, not the
        // `metadata` blocks sitting at the same depth beside them.
        let ty = s.type_at_line(4).expect("inside payload");
        assert_eq!(s.close_type(&ty), 2);
        assert_eq!(s.folded.iter().copied().collect::<Vec<_>>(), vec![4, 13]);
        assert!(
            !s.folded.contains(&7) && !s.folded.contains(&16),
            "metadata is the same kind at the same depth but a different key"
        );
    }

    #[test]
    fn type_folding_reports_nothing_to_do_rather_than_miscounting() {
        let mut s = json_state(LOG);
        let ty = s.type_at_line(3).unwrap();
        assert_eq!(s.close_type(&ty), 2);
        assert_eq!(s.close_type(&ty), 0, "already folded, so nothing is new");
        assert_eq!(s.starts_of_type(&ty).len(), 2, "but there are still two");
    }

    #[test]
    fn repeated_close_walks_outward_one_level_at_a_time() {
        let mut s = json_state(LOG);
        // Deep inside the first payload.
        assert!(s.close_at_line(5));
        assert_eq!(s.folded.iter().copied().collect::<Vec<_>>(), vec![4], "payload");
        assert!(s.close_at_line(5));
        assert_eq!(s.folded.iter().copied().collect::<Vec<_>>(), vec![2, 4], "the record");
        assert!(s.close_at_line(5));
        assert!(s.folded.contains(&1), "then the entries array");
    }

    #[test]
    fn open_reveals_the_outermost_fold_first() {
        let mut s = json_state(LOG);
        s.folded.extend([0, 2, 4]);

        // Only the root fold is visible from line 2, so that is what opens.
        assert!(s.open_at_line(2));
        assert!(!s.folded.contains(&0));
        assert!(s.folded.contains(&2), "the nested folds stay closed");

        assert!(s.open_at_line(2));
        assert!(!s.folded.contains(&2));
        assert!(s.folded.contains(&4), "one level per press");
    }

    #[test]
    fn open_and_close_report_when_there_is_nothing_to_do() {
        let mut s = json_state(LOG);
        assert!(!s.open_at_line(3), "nothing is folded yet");
        // Line 5 is `"a": 1` — a one-line pair, so the only foldable things
        // containing it are its ancestors, and once they are all closed there
        // is nothing left to close.
        while s.close_at_line(5) {}
        assert!(!s.close_at_line(5));
    }

    #[test]
    fn a_one_line_object_is_not_foldable() {
        // `{"a": 1}` on a single line: folding it would hide nothing and the
        // fold indicator would replace the very text it stands for.
        let s = json_state("{\"a\": 1}\n");
        assert!(s.ranges.is_empty());
        assert_eq!(s.type_at_line(0), None);
    }
}

