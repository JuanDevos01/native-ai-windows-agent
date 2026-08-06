//! Diff: after the first snapshot, every action returns what *changed*, not
//! the whole tree — typically 30–80 tokens instead of 1.5k.
//!
//! ```text
//! after click e9:
//!   ~ textbox "Discount code" [e8]: "SAVE10"
//!   + "Code applied — $5 off"
//!   - "Total: $52"
//!   + "Total: $47"
//! ```
//!
//! Keying: interactive nodes are keyed by their ref (stable thanks to
//! fingerprint re-binding), so a value/state change shows as `~`. Unkeyed
//! lines (text, structure) get a multiset diff: net additions `+`, net
//! removals `-`. A full re-snapshot replaces the diff on navigation or when
//! the diff stops being smaller than the tree.

use std::collections::HashMap;

use super::prune::PageNode;
use super::render::line_text;

/// One rendered node line, flattened out of the tree (indentation ignored so
/// wrapper churn can't fake changes).
#[derive(Debug, Clone, PartialEq)]
pub struct FlatLine {
    /// Interactive nodes carry their ref — the diff key.
    pub ref_id: Option<u32>,
    pub text: String,
}

pub fn flatten(tree: &[PageNode]) -> Vec<FlatLine> {
    let mut out = Vec::new();
    walk(tree, &mut out);
    out
}

fn walk(nodes: &[PageNode], out: &mut Vec<FlatLine>) {
    for node in nodes {
        out.push(FlatLine {
            ref_id: node.ref_id.filter(|_| node.kind == super::prune::NodeKind::Interactive),
            text: line_text(node),
        });
        walk(&node.children, out);
    }
}

#[derive(Debug)]
pub struct Diff {
    pub lines: Vec<String>,
    pub changes: usize,
}

impl Diff {
    /// A diff loses its point once it rivals the tree it summarizes.
    pub fn prefer_full(&self, new_len: usize) -> bool {
        self.changes > 60 || self.changes * 2 > new_len.max(8)
    }
}

pub fn diff(old: &[FlatLine], new: &[FlatLine]) -> Diff {
    let old_by_ref: HashMap<u32, &str> = old
        .iter()
        .filter_map(|l| l.ref_id.map(|r| (r, l.text.as_str())))
        .collect();
    let new_by_ref: HashMap<u32, &str> = new
        .iter()
        .filter_map(|l| l.ref_id.map(|r| (r, l.text.as_str())))
        .collect();

    // Multiset counts for unkeyed lines.
    let mut old_counts: HashMap<&str, i64> = HashMap::new();
    for l in old.iter().filter(|l| l.ref_id.is_none()) {
        *old_counts.entry(l.text.as_str()).or_insert(0) += 1;
    }
    let mut new_counts: HashMap<&str, i64> = HashMap::new();
    for l in new.iter().filter(|l| l.ref_id.is_none()) {
        *new_counts.entry(l.text.as_str()).or_insert(0) += 1;
    }

    let mut lines = Vec::new();

    // Changed and added, in new-snapshot order.
    let mut add_budget: HashMap<&str, i64> = HashMap::new();
    for l in new {
        match l.ref_id {
            Some(r) => match old_by_ref.get(&r) {
                Some(&old_text) if old_text != l.text => lines.push(format!("~ {}", l.text)),
                Some(_) => {}
                None => lines.push(format!("+ {}", l.text)),
            },
            None => {
                let old_n = old_counts.get(l.text.as_str()).copied().unwrap_or(0);
                let seen = add_budget.entry(l.text.as_str()).or_insert(0);
                *seen += 1;
                if *seen > old_n {
                    lines.push(format!("+ {}", l.text));
                }
            }
        }
    }

    // Removed, in old-snapshot order.
    let mut removal_budget: HashMap<&str, i64> = HashMap::new();
    for l in old {
        match l.ref_id {
            Some(r) => {
                if !new_by_ref.contains_key(&r) {
                    lines.push(format!("- {}", l.text));
                }
            }
            None => {
                let new_n = new_counts.get(l.text.as_str()).copied().unwrap_or(0);
                let seen = removal_budget.entry(l.text.as_str()).or_insert(0);
                *seen += 1;
                if *seen > new_n {
                    lines.push(format!("- {}", l.text));
                }
            }
        }
    }

    let changes = lines.len();
    Diff { lines, changes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fl(ref_id: Option<u32>, text: &str) -> FlatLine {
        FlatLine {
            ref_id,
            text: text.into(),
        }
    }

    #[test]
    fn no_change_is_empty() {
        let a = vec![fl(Some(1), "button \"Go\" [e1]"), fl(None, "\"hello\"")];
        let d = diff(&a, &a.clone());
        assert_eq!(d.changes, 0);
    }

    #[test]
    fn value_change_is_tilde() {
        let old = vec![fl(Some(8), "textbox \"Code\" [e8] (empty)")];
        let new = vec![fl(Some(8), "textbox \"Code\" [e8]: \"SAVE10\"")];
        let d = diff(&old, &new);
        assert_eq!(d.lines, vec!["~ textbox \"Code\" [e8]: \"SAVE10\""]);
    }

    #[test]
    fn text_change_is_minus_plus() {
        let old = vec![fl(None, "\"Total: $52\"")];
        let new = vec![fl(None, "\"Total: $47\"")];
        let d = diff(&old, &new);
        assert_eq!(d.lines, vec!["+ \"Total: $47\"", "- \"Total: $52\""]);
    }

    #[test]
    fn added_and_removed_refs() {
        let old = vec![fl(Some(1), "button \"Cancel\" [e1]")];
        let new = vec![fl(Some(2), "button \"Confirm\" [e2]")];
        let d = diff(&old, &new);
        assert_eq!(
            d.lines,
            vec!["+ button \"Confirm\" [e2]", "- button \"Cancel\" [e1]"]
        );
    }

    #[test]
    fn duplicate_unkeyed_lines_counted() {
        let old = vec![fl(None, "\":\""), fl(None, "\":\"")];
        let new = vec![fl(None, "\":\"")];
        let d = diff(&old, &new);
        assert_eq!(d.lines, vec!["- \":\""]);
    }

    #[test]
    fn prefer_full_thresholds() {
        let d = Diff { lines: vec![], changes: 61 };
        assert!(d.prefer_full(1000));
        let d = Diff { lines: vec![], changes: 30 };
        assert!(!d.prefer_full(100));
        assert!(d.prefer_full(40));
    }
}
