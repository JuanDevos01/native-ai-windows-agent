//! Refs: session-scoped `[eN]` handles for actionable nodes, stable across
//! page mutation.
//!
//! Three-layer identity (phase 2):
//! 1. **backendNodeId** — fast path while the DOM node stays alive.
//! 2. **Fingerprint** — role + normalized name + anchor path through *kept*
//!    ancestors + ordinal. Fingerprinting the pruned tree (not the DOM) means
//!    wrapper churn — the most common mutation — can't invalidate identity.
//!    When a framework re-render destroys and recreates a node, the ref
//!    silently re-binds to the fingerprint match.
//! 3. **No match** — the ref reports stale, loudly. Never guess: acting on a
//!    wrong-but-plausible node is the worst failure mode in the space.
//!
//! Ref numbers are allocated once and never reused within a session.

use std::collections::{HashMap, HashSet};

use headless_chrome::protocol::cdp::DOM::BackendNodeId;
use serde::{Deserialize, Serialize};

use super::prune::{NodeKind, PageNode};

/// How many nearest kept ancestors participate in the anchor path.
const ANCHOR_LEN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub role: String,
    /// Normalized accessible name (trimmed, lowercased, whitespace collapsed).
    pub name: String,
    /// (role, normalized name) of the nearest kept ancestors, innermost last.
    pub anchor_path: Vec<(String, String)>,
    /// Disambiguates the 3rd "Delete" button: k-th same (role, name) in
    /// document order.
    pub ordinal: usize,
}

#[derive(Debug, Clone)]
struct RefEntry {
    backend_id: BackendNodeId,
    fingerprint: Fingerprint,
    last_seen: u64,
}

#[derive(Debug, Default)]
pub struct RefMap {
    next: u32,
    snap: u64,
    entries: HashMap<u32, RefEntry>,
    by_backend: HashMap<BackendNodeId, u32>,
    /// Counts down from u32::MAX for phantom (not-yet-seen-on-page) entries
    /// imported from a saved plan — real Chrome backendNodeIds are small,
    /// increasing integers, so collision is not a realistic concern.
    phantom_seq: u32,
}

/// One ref's identity, serializable — the unit of a replayable plan. A saved
/// plan's `click(e10)` carries this fingerprint, so re-executing the plan
/// against a freshly loaded page re-binds `e10` to whatever now matches,
/// without ever having seen that page's live backendNodeIds before.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRef {
    #[serde(rename = "ref")]
    pub ref_id: u32,
    pub role: String,
    pub name: String,
    #[serde(rename = "anchorPath")]
    pub anchor_path: Vec<(String, String)>,
    pub ordinal: usize,
}

struct NodeInfo {
    backend: BackendNodeId,
    fp: Fingerprint,
}

fn norm(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Nodes that carry a ref: interactive ones, plus containers whose children
/// were truncated (their ref powers the `expand` action).
fn wants_ref(node: &PageNode) -> bool {
    node.backend_id.is_some() && (node.kind == NodeKind::Interactive || node.expandable)
}

impl RefMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve "e7" (or "7") back to its backend node id.
    pub fn resolve(&self, ref_str: &str) -> anyhow::Result<BackendNodeId> {
        let n = parse_ref(ref_str)?;
        self.entries
            .get(&n)
            .map(|e| e.backend_id)
            .ok_or_else(|| {
                anyhow::anyhow!("Unknown ref e{n}. Take a fresh 'snapshot' and use a ref from it.")
            })
    }

    /// "button \"Save\"" — used in stale-ref error messages.
    pub fn describe(&self, ref_str: &str) -> Option<String> {
        let n = parse_ref(ref_str).ok()?;
        let e = self.entries.get(&n)?;
        Some(format!("{} \"{}\"", e.fingerprint.role, e.fingerprint.name))
    }

    /// Dump every known ref's fingerprint — a replayable plan. Save this
    /// (e.g. as a memory note) and hand it to `import_plan` on a future
    /// session to make the same `eN` numbers reappear on a reload of the
    /// same page shape.
    pub fn export_plan(&self) -> Vec<PlanRef> {
        let mut refs: Vec<PlanRef> = self
            .entries
            .iter()
            .map(|(&ref_id, e)| PlanRef {
                ref_id,
                role: e.fingerprint.role.clone(),
                name: e.fingerprint.name.clone(),
                anchor_path: e.fingerprint.anchor_path.clone(),
                ordinal: e.fingerprint.ordinal,
            })
            .collect();
        refs.sort_by_key(|r| r.ref_id);
        refs
    }

    /// Seed phantom entries from a saved plan. They carry no live backend
    /// node — the very next `process()` (the page's first snapshot) treats
    /// them exactly like dead entries eligible for fingerprint re-binding,
    /// so a matching element on the fresh page reclaims the original ref
    /// number. Entries whose ref number is already known are left alone.
    pub fn import_plan(&mut self, refs: &[PlanRef]) {
        for r in refs {
            if self.entries.contains_key(&r.ref_id) {
                continue;
            }
            self.phantom_seq += 1;
            let phantom_backend = u32::MAX - self.phantom_seq + 1;
            self.entries.insert(
                r.ref_id,
                RefEntry {
                    backend_id: phantom_backend,
                    fingerprint: Fingerprint {
                        role: r.role.clone(),
                        name: r.name.clone(),
                        anchor_path: r.anchor_path.clone(),
                        ordinal: r.ordinal,
                    },
                    last_seen: self.snap,
                },
            );
            self.next = self.next.max(r.ref_id);
        }
    }

    /// Reconcile this snapshot's ref-wanting nodes against known entries.
    /// Returns backend → ref for the current snapshot.
    fn process(&mut self, infos: &[NodeInfo]) -> HashMap<BackendNodeId, u32> {
        self.snap += 1;
        let alive: HashSet<BackendNodeId> = infos.iter().map(|i| i.backend).collect();
        let mut map: HashMap<BackendNodeId, u32> = HashMap::new();
        let mut consumed: HashSet<u32> = HashSet::new();
        let mut pending: Vec<&NodeInfo> = Vec::new();

        // Layer 1: backend id still known → same ref, refreshed fingerprint.
        for info in infos {
            if let Some(&r) = self.by_backend.get(&info.backend) {
                if let Some(e) = self.entries.get_mut(&r) {
                    e.fingerprint = info.fp.clone();
                    e.last_seen = self.snap;
                }
                map.insert(info.backend, r);
                consumed.insert(r);
            } else {
                pending.push(info);
            }
        }

        // Layer 2: fingerprint re-binding for recreated nodes. Candidates are
        // entries whose backend died (not in this snapshot) and that share
        // role + name. Best match: same anchor path, then nearest ordinal;
        // ties break on the lowest ref for determinism.
        for info in pending {
            let mut best: Option<(u32, i32, usize)> = None;
            for (&r, e) in &self.entries {
                if consumed.contains(&r) || alive.contains(&e.backend_id) {
                    continue;
                }
                if e.fingerprint.role != info.fp.role || e.fingerprint.name != info.fp.name {
                    continue;
                }
                let score = i32::from(e.fingerprint.anchor_path == info.fp.anchor_path);
                let dist = e.fingerprint.ordinal.abs_diff(info.fp.ordinal);
                let better = match best {
                    None => true,
                    Some((br, bs, bd)) => {
                        score > bs || (score == bs && (dist < bd || (dist == bd && r < br)))
                    }
                };
                if better {
                    best = Some((r, score, dist));
                }
            }
            let r = match best {
                Some((r, _, _)) => {
                    let old_backend = self.entries[&r].backend_id;
                    self.by_backend.remove(&old_backend);
                    if let Some(e) = self.entries.get_mut(&r) {
                        e.backend_id = info.backend;
                        e.fingerprint = info.fp.clone();
                        e.last_seen = self.snap;
                    }
                    r
                }
                // Layer 3: genuinely new — allocate a never-reused number.
                None => {
                    self.next += 1;
                    self.entries.insert(
                        self.next,
                        RefEntry {
                            backend_id: info.backend,
                            fingerprint: info.fp.clone(),
                            last_seen: self.snap,
                        },
                    );
                    self.next
                }
            };
            self.by_backend.insert(info.backend, r);
            map.insert(info.backend, r);
            consumed.insert(r);
        }
        map
    }
}

/// Parse a model-supplied ref: "e12", "E12", "[e12]", or bare "12".
pub fn parse_ref(s: &str) -> anyhow::Result<u32> {
    let t = s.trim().trim_start_matches('[').trim_end_matches(']');
    let t = t.strip_prefix(['e', 'E']).unwrap_or(t);
    t.parse::<u32>()
        .map_err(|_| anyhow::anyhow!("Invalid ref '{s}': expected the form 'e7'"))
}

/// Walk the pruned tree, fingerprint every ref-wanting node, reconcile with
/// the session RefMap, and write ref numbers back into the tree.
pub fn assign_refs(tree: &mut [PageNode], refs: &mut RefMap) {
    let mut infos = Vec::new();
    let mut ancestors = Vec::new();
    let mut ordinals = HashMap::new();
    collect(tree, &mut ancestors, &mut ordinals, &mut infos);
    let map = refs.process(&infos);
    apply(tree, &map);
}

fn collect(
    nodes: &[PageNode],
    ancestors: &mut Vec<(String, String)>,
    ordinals: &mut HashMap<(String, String), usize>,
    out: &mut Vec<NodeInfo>,
) {
    for node in nodes {
        if wants_ref(node) {
            let key = (node.role.clone(), norm(&node.name));
            let ordinal = {
                let c = ordinals.entry(key.clone()).or_insert(0);
                let v = *c;
                *c += 1;
                v
            };
            let anchor_start = ancestors.len().saturating_sub(ANCHOR_LEN);
            out.push(NodeInfo {
                backend: node.backend_id.expect("wants_ref guarantees backend"),
                fp: Fingerprint {
                    role: key.0,
                    name: key.1,
                    anchor_path: ancestors[anchor_start..].to_vec(),
                    ordinal,
                },
            });
        }
        if !node.children.is_empty() {
            ancestors.push((node.role.clone(), norm(&node.name)));
            collect(&node.children, ancestors, ordinals, out);
            ancestors.pop();
        }
    }
}

fn apply(nodes: &mut [PageNode], map: &HashMap<BackendNodeId, u32>) {
    for node in nodes {
        if wants_ref(node) {
            node.ref_id = node.backend_id.and_then(|b| map.get(&b).copied());
        }
        apply(&mut node.children, map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::pagetree::snapshot::NodeStates;

    fn button(name: &str, backend: BackendNodeId) -> PageNode {
        PageNode {
            kind: NodeKind::Interactive,
            role: "button".into(),
            name: name.into(),
            value: None,
            states: NodeStates::default(),
            backend_id: Some(backend),
            ref_id: None,
            children: Vec::new(),
            truncated_siblings: 0,
            expandable: false,
        }
    }

    fn section(name: &str, backend: BackendNodeId, children: Vec<PageNode>) -> PageNode {
        PageNode {
            kind: NodeKind::Structural,
            role: "region".into(),
            name: name.into(),
            value: None,
            states: NodeStates::default(),
            backend_id: Some(backend),
            ref_id: None,
            children,
            truncated_siblings: 0,
            expandable: false,
        }
    }

    #[test]
    fn stable_refs_and_never_reuse() {
        let mut refs = RefMap::new();
        let mut t1 = vec![button("Save", 100), button("Cancel", 101)];
        assign_refs(&mut t1, &mut refs);
        assert_eq!(t1[0].ref_id, Some(1));
        assert_eq!(t1[1].ref_id, Some(2));

        // Same nodes again: same refs.
        let mut t2 = vec![button("Save", 100), button("Cancel", 101)];
        assign_refs(&mut t2, &mut refs);
        assert_eq!(t2[0].ref_id, Some(1));
        assert_eq!(t2[1].ref_id, Some(2));

        // A brand-new node gets a new number.
        let mut t3 = vec![button("Save", 100), button("Delete", 300)];
        assign_refs(&mut t3, &mut refs);
        assert_eq!(t3[1].ref_id, Some(3));
    }

    #[test]
    fn rebinds_after_react_style_recreation() {
        let mut refs = RefMap::new();
        let mut t1 = vec![button("Save", 100)];
        assign_refs(&mut t1, &mut refs);
        assert_eq!(t1[0].ref_id, Some(1));

        // Re-render destroyed backend 100, recreated the same button as 200.
        let mut t2 = vec![button("Save", 200)];
        assign_refs(&mut t2, &mut refs);
        assert_eq!(t2[0].ref_id, Some(1), "same logical button keeps its ref");
        assert_eq!(refs.resolve("e1").unwrap(), 200);
    }

    #[test]
    fn ordinal_disambiguates_identical_buttons() {
        let mut refs = RefMap::new();
        let mut t1 = vec![button("Delete", 1), button("Delete", 2), button("Delete", 3)];
        assign_refs(&mut t1, &mut refs);
        let before: Vec<_> = t1.iter().map(|n| n.ref_id.unwrap()).collect();

        // All three recreated with new backend ids, same order.
        let mut t2 = vec![button("Delete", 11), button("Delete", 12), button("Delete", 13)];
        assign_refs(&mut t2, &mut refs);
        let after: Vec<_> = t2.iter().map(|n| n.ref_id.unwrap()).collect();
        assert_eq!(before, after, "k-th Delete keeps the k-th ref");
        assert_eq!(refs.resolve(&format!("e{}", after[2])).unwrap(), 13);
    }

    #[test]
    fn anchor_path_beats_ordinal_on_rebind() {
        let mut refs = RefMap::new();
        // Two "Edit" buttons in different sections.
        let mut t1 = vec![
            section("Profile", 50, vec![button("Edit", 1)]),
            section("Billing", 51, vec![button("Edit", 2)]),
        ];
        assign_refs(&mut t1, &mut refs);
        let billing_ref = t1[1].children[0].ref_id.unwrap();

        // Only the Billing button is recreated (Profile section removed).
        let mut t2 = vec![section("Billing", 51, vec![button("Edit", 22)])];
        assign_refs(&mut t2, &mut refs);
        assert_eq!(
            t2[0].children[0].ref_id,
            Some(billing_ref),
            "anchor path binds to the Billing Edit, not the Profile one"
        );
    }

    #[test]
    fn name_normalization() {
        let mut refs = RefMap::new();
        let mut t1 = vec![button("  Save   Changes ", 100)];
        assign_refs(&mut t1, &mut refs);
        let mut t2 = vec![button("save changes", 200)];
        assign_refs(&mut t2, &mut refs);
        assert_eq!(t2[0].ref_id, t1[0].ref_id);
    }

    #[test]
    fn resolves_ref_strings() {
        let mut refs = RefMap::new();
        let mut t = vec![button("Go", 42)];
        assign_refs(&mut t, &mut refs);
        assert_eq!(refs.resolve("e1").unwrap(), 42);
        assert_eq!(refs.resolve("[e1]").unwrap(), 42);
        assert_eq!(refs.resolve("1").unwrap(), 42);
        assert!(refs.resolve("e999").is_err());
        assert!(refs.resolve("banana").is_err());
        assert_eq!(refs.describe("e1").unwrap(), "button \"go\"");
    }

    #[test]
    fn plan_replay_rebinds_on_a_fresh_session() {
        // Session A: browse a form, note its ref numbers, export a plan.
        let mut session_a = RefMap::new();
        let mut form = vec![
            button("Save", 100),
            section("Billing", 101, vec![button("Edit", 102)]),
        ];
        assign_refs(&mut form, &mut session_a);
        let save_ref = form[0].ref_id.unwrap();
        let edit_ref = form[1].children[0].ref_id.unwrap();
        let plan = session_a.export_plan();
        assert_eq!(plan.len(), 2);

        // Session B: brand new RefMap (e.g. a fresh page load), never saw
        // these backend ids before. Import the plan BEFORE the first
        // compile, then compile a page whose nodes happen to match.
        let mut session_b = RefMap::new();
        session_b.import_plan(&plan);
        let mut fresh = vec![
            button("Save", 900), // wildly different backend ids
            section("Billing", 901, vec![button("Edit", 902)]),
        ];
        assign_refs(&mut fresh, &mut session_b);

        assert_eq!(
            fresh[0].ref_id,
            Some(save_ref),
            "Save button reclaims its original ref via fingerprint match"
        );
        assert_eq!(
            fresh[1].children[0].ref_id,
            Some(edit_ref),
            "Edit button reclaims its original ref via fingerprint match"
        );
        assert_eq!(session_b.resolve(&format!("e{save_ref}")).unwrap(), 900);
    }

    #[test]
    fn import_plan_does_not_collide_with_new_allocations() {
        let mut refs = RefMap::new();
        refs.import_plan(&[PlanRef {
            ref_id: 50,
            role: "button".into(),
            name: "ghost".into(),
            anchor_path: vec![],
            ordinal: 0,
        }]);
        // A node that does NOT match the imported fingerprint must get a
        // number that can never collide with e50.
        let mut t = vec![button("Totally Different", 1)];
        assign_refs(&mut t, &mut refs);
        assert!(t[0].ref_id.unwrap() > 50);
    }
}
