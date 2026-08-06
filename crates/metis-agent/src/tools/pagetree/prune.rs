//! Prune: RawNode tree → InteractionTree. The DOM is ~95% wrapper noise;
//! this pass keeps interactive nodes (they get refs), structural landmarks
//! (orientation), and budgeted text runs — and splices everything else.

use headless_chrome::protocol::cdp::DOM::BackendNodeId;

use super::snapshot::{NodeStates, RawNode};

/// Per-text-run truncation limit.
const TEXT_LIMIT: usize = 120;
/// Repeated siblings beyond this are truncated with "…and N more".
pub const SIBLING_LIMIT: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// Actionable: gets a ref.
    Interactive,
    /// Orientation only: headings, landmarks, tables, alerts. No ref.
    Structural,
    /// Text run.
    Content,
}

#[derive(Debug, Clone)]
pub struct PageNode {
    pub kind: NodeKind,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub states: NodeStates,
    pub backend_id: Option<BackendNodeId>,
    /// Assigned by `refs::assign_refs` for Interactive nodes.
    pub ref_id: Option<u32>,
    pub children: Vec<PageNode>,
    /// How many same-role siblings were cut after this node's parent hit
    /// the sibling limit (recorded on the last kept sibling).
    pub truncated_siblings: usize,
    /// True when this node's child list was truncated — it then carries a ref
    /// so the `expand` virtual action can re-render the subtree in full.
    pub expandable: bool,
}

fn is_interactive(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "textfield"
            | "textboxwithsuggestions"
            | "searchbox"
            | "checkbox"
            | "radio"
            | "combobox"
            | "listbox"
            | "option"
            | "menulistoption"
            | "popupbutton"
            | "slider"
            | "switch"
            | "tab"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "spinbutton"
            | "togglebutton"
            | "disclosuretriangle"
            | "treeitem"
            | "datetime"
            | "date"
            | "time"
            | "colorwell"
    )
}

fn is_structural(role: &str) -> bool {
    matches!(
        role,
        "heading"
            | "navigation"
            | "main"
            | "banner"
            | "contentinfo"
            | "complementary"
            | "form"
            | "search"
            | "region"
            | "dialog"
            | "alertdialog"
            | "alert"
            | "status"
            | "log"
            | "table"
            | "grid"
            | "row"
            | "cell"
            | "gridcell"
            | "columnheader"
            | "rowheader"
            | "list"
            | "listitem"
            | "descriptionlist"
            | "tablist"
            | "menu"
            | "menubar"
            | "toolbar"
            | "tree"
            | "figure"
            | "article"
            | "progressbar"
            | "meter"
            | "tabpanel"
            | "rowgroup"
    )
}

fn is_content(role: &str) -> bool {
    matches!(role, "statictext" | "text" | "paragraph" | "listmarker" | "code")
}

/// Roles whose subtree is never useful.
fn is_dropped(role: &str) -> bool {
    matches!(role, "scrollbar" | "linebreak" | "inlinetextbox" | "presentation" | "none")
}

fn is_iframe_host(role: &str) -> bool {
    matches!(role, "iframe" | "iframepresentational")
}

/// Entry point: prune the raw root's children (the RootWebArea wrapper itself
/// is not rendered; its name — the page title — is emitted by the renderer).
pub fn prune(root: &RawNode) -> Vec<PageNode> {
    prune_with(root, Some(SIBLING_LIMIT))
}

/// Prune with an explicit sibling limit; `None` disables sibling truncation
/// (used by the `expand` virtual action).
pub fn prune_with(root: &RawNode, limit: Option<usize>) -> Vec<PageNode> {
    let mut out = Vec::new();
    for child in &root.children {
        prune_node(child, &mut out, limit);
    }
    truncate_repeats(&mut out, limit);
    out
}

/// Convert one raw node, appending 0..n pruned nodes onto `out` (splicing
/// produces the >1 case).
fn prune_node(raw: &RawNode, out: &mut Vec<PageNode>, limit: Option<usize>) {
    let role = raw.role.as_str();

    if is_dropped(role) {
        return;
    }

    if is_interactive(role) {
        let mut children = Vec::new();
        // Keep children only where they carry real structure (e.g. options
        // inside a listbox/combobox); plain text children just repeat the
        // accessible name.
        for c in &raw.children {
            if !is_content(c.role.as_str()) {
                prune_node(c, &mut children, limit);
            }
        }
        // Text that survived via spliced wrappers just repeats the accessible
        // name or the current value — an interactive node's line already
        // carries both.
        children.retain(|c| c.kind != NodeKind::Content);
        truncate_repeats(&mut children, limit);
        let expandable = children.iter().any(|c| c.truncated_siblings > 0);
        out.push(PageNode {
            kind: NodeKind::Interactive,
            role: normalize_role(role),
            name: clip(&raw.name, TEXT_LIMIT),
            value: raw.value.clone(),
            states: raw.states.clone(),
            backend_id: raw.backend_id,
            ref_id: None,
            children,
            truncated_siblings: 0,
            expandable,
        });
        return;
    }

    if is_structural(role) {
        let mut children = Vec::new();
        for c in &raw.children {
            prune_node(c, &mut children, limit);
        }
        truncate_repeats(&mut children, limit);
        let expandable = children.iter().any(|c| c.truncated_siblings > 0);

        // Headings carry their text in `name` — drop the duplicate text child.
        if role == "heading" {
            children.retain(|c| !(c.kind == NodeKind::Content && c.name == clip(&raw.name, TEXT_LIMIT)));
        }

        let named = !raw.name.trim().is_empty();
        // Alerts/status are load-bearing even when empty of children (they can
        // appear then fill); other structural nodes must earn their line.
        let always_keep = matches!(role, "alert" | "alertdialog" | "status" | "dialog" | "heading");
        if !always_keep && !named && children.len() <= 1 && !expandable {
            // Unnamed wrapper with nothing to organize: splice.
            out.append(&mut children);
            return;
        }
        if !always_keep && children.is_empty() && !named {
            return;
        }
        out.push(PageNode {
            kind: NodeKind::Structural,
            role: normalize_role(role),
            name: clip(&raw.name, TEXT_LIMIT),
            value: raw.value.clone(),
            states: raw.states.clone(),
            backend_id: raw.backend_id,
            ref_id: None,
            children,
            truncated_siblings: 0,
            expandable,
        });
        return;
    }

    if is_content(role) {
        let text = raw.name.trim();
        if text.is_empty() {
            // e.g. paragraph wrappers: descend.
            for c in &raw.children {
                prune_node(c, out, limit);
            }
            return;
        }
        out.push(PageNode {
            kind: NodeKind::Content,
            role: "text".into(),
            name: clip(text, TEXT_LIMIT),
            value: None,
            states: NodeStates::default(),
            backend_id: raw.backend_id,
            ref_id: None,
            children: Vec::new(),
            truncated_siblings: 0,
            expandable: false,
        });
        return;
    }

    // Images: keep only when labeled (a labeled image is information).
    if role == "image" || role == "img" || role == "svgroot" {
        let text = raw.name.trim();
        if !text.is_empty() {
            out.push(PageNode {
                kind: NodeKind::Content,
                role: "image".into(),
                name: clip(text, TEXT_LIMIT),
                value: None,
                states: NodeStates::default(),
                backend_id: raw.backend_id,
                ref_id: None,
                children: Vec::new(),
                truncated_siblings: 0,
                expandable: false,
            });
        }
        return; // never descend into SVG/image internals
    }

    // Iframe host element. Same-origin frames arrive with their content
    // already pierced into the AX tree (Chrome does this transparently) —
    // splice those children up like any other wrapper. A frame Chrome could
    // NOT pierce (cross-origin, out-of-process) shows up with no children at
    // all; say so explicitly instead of silently vanishing, since an agent
    // that doesn't know a frame exists there can't explain why "the payment
    // form" isn't on the page.
    if is_iframe_host(role) {
        let mut children = Vec::new();
        for c in &raw.children {
            prune_node(c, &mut children, limit);
        }
        if children.is_empty() {
            let label = if raw.name.trim().is_empty() {
                "embedded frame".to_string()
            } else {
                clip(&raw.name, TEXT_LIMIT)
            };
            out.push(PageNode {
                kind: NodeKind::Structural,
                role: "iframe".into(),
                name: label,
                value: None,
                states: NodeStates::default(),
                backend_id: raw.backend_id,
                ref_id: None,
                children: vec![PageNode {
                    kind: NodeKind::Content,
                    role: "text".into(),
                    name: "(cross-origin — content not inspectable from here)".into(),
                    value: None,
                    states: NodeStates::default(),
                    backend_id: None,
                    ref_id: None,
                    children: Vec::new(),
                    truncated_siblings: 0,
                    expandable: false,
                }],
                truncated_siblings: 0,
                expandable: false,
            });
        } else {
            out.append(&mut children);
        }
        return;
    }

    // Everything else (generic, genericcontainer, rootwebarea of nested
    // frames, sections, …): wrapper — splice children up.
    for c in &raw.children {
        prune_node(c, out, limit);
    }
}

/// Map Chrome's internal role spellings onto the taxonomy the model sees.
fn normalize_role(role: &str) -> String {
    match role {
        "textfield" | "textboxwithsuggestions" => "textbox".into(),
        "menulistoption" => "option".into(),
        "popupbutton" => "combobox".into(),
        "togglebutton" => "button".into(),
        "descriptionlist" => "list".into(),
        "gridcell" => "cell".into(),
        other => other.into(),
    }
}

/// Truncate long runs of same-role siblings (lists, tables, feeds) to the
/// first `limit`, recording the cut count on the last kept node.
fn truncate_repeats(nodes: &mut Vec<PageNode>, limit: Option<usize>) {
    let Some(sibling_limit) = limit else {
        return; // expand mode: no truncation
    };
    if nodes.len() <= sibling_limit {
        return;
    }
    let mut result: Vec<PageNode> = Vec::with_capacity(nodes.len());
    let mut run_role = String::new();
    let mut run_len = 0usize;
    let mut cut = 0usize;
    for node in nodes.drain(..) {
        if node.role == run_role && node.kind != NodeKind::Interactive {
            run_len += 1;
        } else {
            if cut > 0 {
                if let Some(last) = result.last_mut() {
                    last.truncated_siblings = cut;
                }
            }
            run_role = node.role.clone();
            run_len = 1;
            cut = 0;
        }
        if run_len > sibling_limit {
            cut += 1;
        } else {
            result.push(node);
        }
    }
    if cut > 0 {
        if let Some(last) = result.last_mut() {
            last.truncated_siblings = cut;
        }
    }
    *nodes = result;
}

fn clip(s: &str, limit: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let clipped: String = s.chars().take(limit).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::pagetree::snapshot::build_tree;
    use crate::tools::pagetree::snapshot::tests::ax;
    use serde_json::json;

    fn raw(role: &str, name: &str, children: Vec<RawNode>) -> RawNode {
        raw_id(role, name, 1, children)
    }

    fn raw_id(role: &str, name: &str, backend: u32, children: Vec<RawNode>) -> RawNode {
        RawNode {
            backend_id: Some(backend),
            role: role.into(),
            name: name.into(),
            value: None,
            states: NodeStates::default(),
            children,
        }
    }

    #[test]
    fn splices_generic_wrappers() {
        let root = raw(
            "rootwebarea",
            "T",
            vec![raw(
                "genericcontainer",
                "",
                vec![raw("genericcontainer", "", vec![raw("button", "Go", vec![])])],
            )],
        );
        let tree = prune(&root);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].role, "button");
        assert_eq!(tree[0].kind, NodeKind::Interactive);
    }

    #[test]
    fn keeps_heading_without_duplicate_text_child(){
        let root = raw(
            "rootwebarea",
            "T",
            vec![raw("heading", "Welcome", vec![raw("statictext", "Welcome", vec![])])],
        );
        let tree = prune(&root);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].role, "heading");
        assert!(tree[0].children.is_empty());
    }

    #[test]
    fn truncates_repeated_siblings() {
        let items: Vec<RawNode> = (0..40).map(|i| raw("listitem", &format!("Item {i}"), vec![])).collect();
        let root = raw("rootwebarea", "T", vec![raw("list", "", items)]);
        let tree = prune(&root);
        assert_eq!(tree.len(), 1);
        let list = &tree[0];
        assert_eq!(list.children.len(), SIBLING_LIMIT);
        assert_eq!(list.children.last().unwrap().truncated_siblings, 40 - SIBLING_LIMIT);
        assert!(list.expandable, "truncated container is expandable");
    }

    #[test]
    fn unlimited_mode_keeps_everything() {
        let items: Vec<RawNode> = (0..40).map(|i| raw_id("listitem", &format!("Item {i}"), 100 + i, vec![])).collect();
        let root = raw("rootwebarea", "T", vec![raw("list", "", items)]);
        let tree = prune_with(&root, None);
        assert_eq!(tree[0].children.len(), 40);
        assert!(!tree[0].expandable);
    }

    #[test]
    fn drops_unlabeled_images_keeps_labeled() {
        let root = raw(
            "rootwebarea",
            "T",
            vec![raw("image", "", vec![]), raw("image", "Company logo", vec![])],
        );
        let tree = prune(&root);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "Company logo");
    }

    #[test]
    fn cross_origin_iframe_labeled_not_dropped() {
        let root = raw(
            "rootwebarea",
            "T",
            vec![raw("iframe", "Payment form", vec![])],
        );
        let tree = prune(&root);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].role, "iframe");
        assert_eq!(tree[0].name, "Payment form");
        assert_eq!(tree[0].children.len(), 1);
        assert!(tree[0].children[0].name.contains("cross-origin"));
    }

    #[test]
    fn same_origin_iframe_content_spliced_transparently() {
        let root = raw(
            "rootwebarea",
            "T",
            vec![raw("iframe", "Widget", vec![raw("button", "Click me", vec![])])],
        );
        let tree = prune(&root);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].role, "button", "pierced content splices up, no iframe wrapper line");
    }

    #[test]
    fn end_to_end_from_ax_json() {
        let nodes = vec![
            ax(json!({
                "nodeId": "1", "ignored": false,
                "role": {"type": "role", "value": "RootWebArea"},
                "name": {"type": "computedString", "value": "Shop"},
                "childIds": ["2", "3", "4"], "backendDOMNodeId": 10
            })),
            ax(json!({
                "nodeId": "2", "ignored": false,
                "role": {"type": "role", "value": "textbox"},
                "name": {"type": "computedString", "value": "Discount code"},
                "backendDOMNodeId": 11
            })),
            ax(json!({
                "nodeId": "3", "ignored": false,
                "role": {"type": "role", "value": "button"},
                "name": {"type": "computedString", "value": "Apply"},
                "backendDOMNodeId": 12
            })),
            ax(json!({
                "nodeId": "4", "ignored": false,
                "role": {"type": "role", "value": "StaticText"},
                "name": {"type": "computedString", "value": "Total: $52"},
                "backendDOMNodeId": 13
            })),
        ];
        let tree = prune(&build_tree(&nodes).unwrap());
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].role, "textbox");
        assert_eq!(tree[1].role, "button");
        assert_eq!(tree[2].role, "text");
        assert_eq!(tree[2].name, "Total: $52");
    }
}
