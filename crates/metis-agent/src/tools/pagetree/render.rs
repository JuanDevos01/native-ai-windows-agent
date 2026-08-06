//! Render: InteractionTree → indented text, one line per node, states inline,
//! under a global char budget.
//!
//! ```text
//! heading "Your basket (3 items)" (h2)
//! textbox "Discount code" [e8] (empty)
//! button "Apply" [e9]
//! button "Checkout" [e10] (disabled)
//! ```

use super::prune::{NodeKind, PageNode};

pub fn render(tree: &[PageNode], budget: usize) -> String {
    let mut out = String::new();
    let mut over_budget = false;
    let mut skipped = 0usize;
    render_nodes(tree, 0, None, budget, &mut out, &mut over_budget, &mut skipped);
    if over_budget {
        out.push_str(&format!(
            "… output truncated ({skipped} more nodes). Interact with what is visible or navigate closer to what you need.\n"
        ));
    }
    if out.is_empty() {
        out.push_str("(page has no visible content)\n");
    }
    out
}

fn render_nodes(
    nodes: &[PageNode],
    depth: usize,
    parent_ref: Option<u32>,
    budget: usize,
    out: &mut String,
    over_budget: &mut bool,
    skipped: &mut usize,
) {
    for node in nodes {
        if *over_budget {
            *skipped += 1 + count(&node.children);
            continue;
        }
        let line = format!("{}{}\n", "  ".repeat(depth), line_text(node));
        if out.len() + line.len() > budget {
            *over_budget = true;
            *skipped += 1 + count(&node.children);
            continue;
        }
        out.push_str(&line);
        render_nodes(
            &node.children,
            depth + 1,
            node.ref_id,
            budget,
            out,
            over_budget,
            skipped,
        );
        if node.truncated_siblings > 0 && !*over_budget {
            let expand = match parent_ref {
                Some(r) => format!(" [expand: e{r}]"),
                None => String::new(),
            };
            let note = format!(
                "{}…and {} more {}{}\n",
                "  ".repeat(depth),
                node.truncated_siblings,
                plural_role(&node.role),
                expand
            );
            if out.len() + note.len() <= budget {
                out.push_str(&note);
            }
        }
    }
}

fn count(nodes: &[PageNode]) -> usize {
    nodes.iter().map(|n| 1 + count(&n.children)).sum()
}

/// One node's line, without indentation or newline — also the unit of
/// comparison for diff mode, so it must be deterministic for a given node.
pub fn line_text(node: &PageNode) -> String {
    let mut line = String::new();
    match node.kind {
        NodeKind::Content => {
            if node.role == "image" {
                line.push_str(&format!("image \"{}\"", node.name));
            } else {
                line.push_str(&format!("\"{}\"", node.name));
            }
        }
        _ => {
            line.push_str(&node.role);
            if !node.name.is_empty() {
                line.push_str(&format!(" \"{}\"", node.name));
            }
            // Structural nodes may hold a ref for `expand`, but only
            // interactive lines advertise theirs — the expand ref appears in
            // the truncation marker instead.
            if node.kind == NodeKind::Interactive {
                if let Some(r) = node.ref_id {
                    line.push_str(&format!(" [e{r}]"));
                }
            }
            if let Some(v) = &node.value {
                line.push_str(&format!(": \"{v}\""));
            } else if node.kind == NodeKind::Interactive && is_texty(&node.role) {
                line.push_str(" (empty)");
            }
            let states = state_annotations(node);
            if !states.is_empty() {
                line.push_str(&format!(" ({})", states.join(", ")));
            }
        }
    }
    line
}

fn is_texty(role: &str) -> bool {
    matches!(role, "textbox" | "searchbox" | "combobox" | "spinbutton")
}

fn state_annotations(node: &PageNode) -> Vec<String> {
    let st = &node.states;
    let mut a = Vec::new();
    if st.disabled {
        a.push("disabled".into());
    }
    if st.readonly {
        a.push("readonly".into());
    }
    if st.required {
        a.push("required".into());
    }
    if st.focused {
        a.push("focused".into());
    }
    if st.multiline {
        a.push("multiline".into());
    }
    match st.checked.as_deref() {
        Some("true") => a.push("checked".into()),
        Some("mixed") => a.push("mixed".into()),
        Some("false") if matches!(node.role.as_str(), "checkbox" | "radio" | "switch") => {
            a.push("unchecked".into())
        }
        _ => {}
    }
    match st.pressed.as_deref() {
        Some("true") => a.push("pressed".into()),
        _ => {}
    }
    match st.expanded {
        Some(true) => a.push("expanded".into()),
        Some(false) => a.push("collapsed".into()),
        None => {}
    }
    if st.selected == Some(true) {
        a.push("selected".into());
    }
    if node.role == "heading" {
        if let Some(l) = st.level {
            a.push(format!("h{l}"));
        }
    }
    if st.modal {
        a.push("modal".into());
    }
    a
}

fn plural_role(role: &str) -> String {
    match role {
        "listitem" => "items".into(),
        "row" => "rows".into(),
        "option" => "options".into(),
        "cell" => "cells".into(),
        other => format!("{other} nodes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::pagetree::snapshot::NodeStates;

    fn node(kind: NodeKind, role: &str, name: &str, ref_id: Option<u32>) -> PageNode {
        PageNode {
            kind,
            role: role.into(),
            name: name.into(),
            value: None,
            states: NodeStates::default(),
            backend_id: None,
            ref_id,
            children: Vec::new(),
            truncated_siblings: 0,
            expandable: false,
        }
    }

    #[test]
    fn renders_expected_lines() {
        let mut btn = node(NodeKind::Interactive, "button", "Checkout", Some(10));
        btn.states.disabled = true;
        let mut tb = node(NodeKind::Interactive, "textbox", "Discount code", Some(8));
        tb.value = Some("SAVE10".into());
        let mut heading = node(NodeKind::Structural, "heading", "Your basket", None);
        heading.states.level = Some(2);
        let text = node(NodeKind::Content, "text", "Total: $52", None);

        let out = render(&[heading, tb, btn, text], 6000);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "heading \"Your basket\" (h2)");
        assert_eq!(lines[1], "textbox \"Discount code\" [e8]: \"SAVE10\"");
        assert_eq!(lines[2], "button \"Checkout\" [e10] (disabled)");
        assert_eq!(lines[3], "\"Total: $52\"");
    }

    #[test]
    fn empty_textbox_marked() {
        let tb = node(NodeKind::Interactive, "textbox", "Email", Some(1));
        let out = render(&[tb], 6000);
        assert_eq!(out.trim(), "textbox \"Email\" [e1] (empty)");
    }

    #[test]
    fn respects_budget() {
        let nodes: Vec<PageNode> = (0..500)
            .map(|i| node(NodeKind::Content, "text", &format!("line number {i}"), None))
            .collect();
        let out = render(&nodes, 500);
        assert!(out.len() < 700); // budget + truncation notice
        assert!(out.contains("output truncated"));
    }

    #[test]
    fn truncated_siblings_note() {
        let mut item = node(NodeKind::Structural, "listitem", "Item", None);
        item.truncated_siblings = 47;
        let out = render(&[item], 6000);
        assert!(out.contains("…and 47 more items"));
    }

    #[test]
    fn expand_marker_uses_parent_ref() {
        let mut item = node(NodeKind::Structural, "row", "Row 15", None);
        item.truncated_siblings = 47;
        let mut table = node(NodeKind::Structural, "table", "Orders", None);
        table.ref_id = Some(12);
        table.expandable = true;
        table.children.push(item);
        let out = render(&[table], 6000);
        assert!(out.contains("…and 47 more rows [expand: e12]"), "got: {out}");
        // The structural table line itself must not advertise the ref.
        assert!(out.lines().next().unwrap() == "table \"Orders\"");
    }
}
