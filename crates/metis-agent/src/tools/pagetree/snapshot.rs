//! Snapshot: pull the full accessibility tree over CDP and rebuild it as a
//! `RawNode` tree. The AX tree is the primary source — it pierces shadow DOM
//! and same-process iframes, and it encodes meaning (an ARIA `role=button`
//! div arrives as a button).
//!
//! We deliberately do NOT use the crate's generated `Accessibility` return
//! types: real browsers ship CDP additions faster than the crate's vendored
//! protocol JSON (e.g. Brave emits the AX property `uninteresting`, unknown
//! to headless_chrome 1.0.21's enum, which makes strict deserialization
//! reject the entire tree). Instead we define our own lenient mirror types —
//! unknown properties and value shapes are ignored, never fatal.

use std::collections::HashMap;

use headless_chrome::protocol::cdp::types::Method;
use headless_chrome::protocol::cdp::Accessibility;
use headless_chrome::protocol::cdp::DOM::BackendNodeId;
use headless_chrome::Tab;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

// ─────────────────────────────────────────────
// Lenient CDP call
// ─────────────────────────────────────────────

#[derive(Serialize, Debug)]
struct GetFullAxTreeLenient {}

impl Method for GetFullAxTreeLenient {
    const NAME: &'static str = "Accessibility.getFullAXTree";
    type ReturnObject = AxTreeLenient;
}

#[derive(Deserialize, Debug)]
pub struct AxTreeLenient {
    #[serde(default)]
    pub nodes: Vec<AxNode>,
}

/// Lenient mirror of CDP's AXNode: every field optional, unknown fields and
/// enum values ignored.
#[derive(Deserialize, Debug, Default, Clone)]
pub struct AxNode {
    #[serde(rename = "nodeId", default)]
    pub node_id: String,
    #[serde(default)]
    pub ignored: bool,
    #[serde(default)]
    pub role: Option<AxValue>,
    #[serde(default)]
    pub name: Option<AxValue>,
    #[serde(default)]
    pub value: Option<AxValue>,
    #[serde(default)]
    pub properties: Option<Vec<AxProperty>>,
    #[serde(rename = "childIds", default)]
    pub child_ids: Option<Vec<String>>,
    #[serde(rename = "backendDOMNodeId", default)]
    pub backend_dom_node_id: Option<BackendNodeId>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AxProperty {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: Option<AxValue>,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct AxValue {
    #[serde(default)]
    pub value: Option<Json>,
}

// ─────────────────────────────────────────────
// Raw tree
// ─────────────────────────────────────────────

/// States that matter for rendering and acting. Extracted from AX properties.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeStates {
    pub disabled: bool,
    pub focused: bool,
    pub editable: bool,
    pub readonly: bool,
    pub required: bool,
    pub multiline: bool,
    pub modal: bool,
    /// "true" | "false" | "mixed" for checkboxes/radios/switches.
    pub checked: Option<String>,
    /// "true" | "false" | "mixed" for toggle buttons.
    pub pressed: Option<String>,
    pub expanded: Option<bool>,
    pub selected: Option<bool>,
    /// Heading level (1..6).
    pub level: Option<i64>,
    /// Link target, when Chrome exposes the `url` property.
    pub url: Option<String>,
}

/// One node of the merged raw tree, pre-pruning.
#[derive(Debug, Clone)]
pub struct RawNode {
    pub backend_id: Option<BackendNodeId>,
    /// Normalized (lowercased) AX role, e.g. "button", "statictext".
    pub role: String,
    /// Accessible name (label).
    pub name: String,
    /// Current value (inputs, comboboxes).
    pub value: Option<String>,
    pub states: NodeStates,
    pub children: Vec<RawNode>,
}

/// Pull the full AX tree for the tab's main frame and rebuild it.
pub fn capture(tab: &Tab) -> anyhow::Result<RawNode> {
    tab.call_method(Accessibility::Enable(None))
        .map_err(|e| anyhow::anyhow!("Accessibility.enable failed: {e}"))?;
    let result = tab
        .call_method(GetFullAxTreeLenient {})
        .map_err(|e| anyhow::anyhow!("Accessibility.getFullAXTree failed: {e}"))?;
    build_tree(&result.nodes)
}

/// Rebuild the flat `nodes` list (id → childIds links) into a `RawNode` tree.
/// `ignored` AX nodes are spliced out (their children move up).
pub fn build_tree(nodes: &[AxNode]) -> anyhow::Result<RawNode> {
    if nodes.is_empty() {
        anyhow::bail!("empty accessibility tree");
    }
    let by_id: HashMap<&str, &AxNode> = nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

    // Root: a node nobody references as a child (usually the first entry).
    let referenced: std::collections::HashSet<&str> = nodes
        .iter()
        .filter_map(|n| n.child_ids.as_ref())
        .flatten()
        .map(|s| s.as_str())
        .collect();
    let root = nodes
        .iter()
        .find(|n| !referenced.contains(n.node_id.as_str()))
        .unwrap_or(&nodes[0]);

    let mut children = Vec::new();
    collect(root, &by_id, &mut children, 0);
    // The RootWebArea itself carries the document name (page title).
    Ok(RawNode {
        backend_id: root.backend_dom_node_id,
        role: ax_role(root),
        name: ax_str(&root.name),
        value: opt_ax_str(&root.value),
        states: ax_states(root),
        children,
    })
}

const MAX_DEPTH: usize = 96;

/// Append the converted subtree of `node`'s children onto `out`, splicing
/// ignored nodes.
fn collect(node: &AxNode, by_id: &HashMap<&str, &AxNode>, out: &mut Vec<RawNode>, depth: usize) {
    if depth > MAX_DEPTH {
        return; // cyclic or pathological tree — stop descending
    }
    let Some(child_ids) = node.child_ids.as_ref() else {
        return;
    };
    for id in child_ids {
        let Some(child) = by_id.get(id.as_str()) else {
            continue;
        };
        if child.ignored {
            // Splice: ignored wrapper's children rise to this level.
            collect(child, by_id, out, depth + 1);
            continue;
        }
        let mut grandchildren = Vec::new();
        collect(child, by_id, &mut grandchildren, depth + 1);
        out.push(RawNode {
            backend_id: child.backend_dom_node_id,
            role: ax_role(child),
            name: ax_str(&child.name),
            value: opt_ax_str(&child.value),
            states: ax_states(child),
            children: grandchildren,
        });
    }
}

fn ax_role(node: &AxNode) -> String {
    node.role
        .as_ref()
        .and_then(|v| v.value.as_ref())
        .and_then(json_to_string)
        .unwrap_or_default()
        .to_lowercase()
}

fn ax_str(v: &Option<AxValue>) -> String {
    v.as_ref()
        .and_then(|v| v.value.as_ref())
        .and_then(json_to_string)
        .unwrap_or_default()
}

fn opt_ax_str(v: &Option<AxValue>) -> Option<String> {
    let s = ax_str(v);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn json_to_string(v: &Json) -> Option<String> {
    match v {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn ax_states(node: &AxNode) -> NodeStates {
    let mut st = NodeStates::default();
    let Some(props) = node.properties.as_ref() else {
        return st;
    };
    for prop in props {
        let val = prop.value.as_ref().and_then(|v| v.value.as_ref());
        let as_bool = || val.and_then(|v| v.as_bool()).unwrap_or(false);
        let as_str = || val.and_then(json_to_string);
        match prop.name.as_str() {
            "disabled" => st.disabled = as_bool(),
            "focused" => st.focused = as_bool(),
            "editable" => st.editable = val.is_some(),
            "readonly" => st.readonly = as_bool(),
            "required" => st.required = as_bool(),
            "multiline" => st.multiline = as_bool(),
            "modal" => st.modal = as_bool(),
            "checked" => st.checked = as_str(),
            "pressed" => st.pressed = as_str(),
            "expanded" => st.expanded = val.and_then(|v| v.as_bool()),
            "selected" => st.selected = val.and_then(|v| v.as_bool()),
            "level" => st.level = val.and_then(|v| v.as_i64()),
            "url" => st.url = as_str(),
            _ => {} // unknown/newer properties: ignore, never fail
        }
    }
    st
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// Build a lenient AxNode from JSON for tests (shared with prune tests).
    pub(crate) fn ax(v: Json) -> AxNode {
        serde_json::from_value(v).expect("valid AxNode json")
    }

    pub(crate) fn simple_tree() -> Vec<AxNode> {
        vec![
            ax(json!({
                "nodeId": "1", "ignored": false,
                "role": {"type": "role", "value": "RootWebArea"},
                "name": {"type": "computedString", "value": "Test Page"},
                "childIds": ["2", "3"], "backendDOMNodeId": 100
            })),
            ax(json!({
                "nodeId": "2", "ignored": true,
                "role": {"type": "role", "value": "genericContainer"},
                "childIds": ["4"], "backendDOMNodeId": 101
            })),
            ax(json!({
                "nodeId": "3", "ignored": false,
                "role": {"type": "role", "value": "button"},
                "name": {"type": "computedString", "value": "Submit"},
                "properties": [
                    {"name": "disabled", "value": {"type": "boolean", "value": true}},
                    {"name": "uninteresting", "value": {"type": "boolean", "value": true}}
                ],
                "backendDOMNodeId": 102
            })),
            ax(json!({
                "nodeId": "4", "ignored": false,
                "role": {"type": "role", "value": "heading"},
                "name": {"type": "computedString", "value": "Welcome"},
                "properties": [{"name": "level", "value": {"type": "integer", "value": 2}}],
                "backendDOMNodeId": 103
            })),
        ]
    }

    #[test]
    fn builds_tree_and_splices_ignored() {
        let root = build_tree(&simple_tree()).unwrap();
        assert_eq!(root.role, "rootwebarea");
        assert_eq!(root.name, "Test Page");
        // Ignored container spliced: heading and button are direct children.
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].role, "heading");
        assert_eq!(root.children[0].states.level, Some(2));
        assert_eq!(root.children[1].role, "button");
        assert!(root.children[1].states.disabled);
        assert_eq!(root.children[1].backend_id, Some(102));
    }

    #[test]
    fn unknown_properties_and_fields_are_ignored() {
        // Mimics newer-browser CDP output: unknown property names, unknown
        // top-level fields, exotic value shapes.
        let node = ax(json!({
            "nodeId": "9", "ignored": false,
            "role": {"type": "internalRole", "value": "button"},
            "someFutureField": {"a": 1},
            "properties": [
                {"name": "brandNewThing", "value": {"type": "x", "value": {"nested": true}}},
                {"name": "disabled", "value": {"type": "boolean", "value": true}}
            ]
        }));
        let root = build_tree(&[ax(json!({
            "nodeId": "r", "ignored": false,
            "role": {"type": "role", "value": "RootWebArea"},
            "childIds": ["9"]
        })), node]).unwrap();
        assert_eq!(root.children.len(), 1);
        assert!(root.children[0].states.disabled);
    }

    #[test]
    fn empty_tree_errors() {
        assert!(build_tree(&[]).is_err());
    }
}
