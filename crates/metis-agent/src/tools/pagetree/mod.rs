//! pagetree — compiles a live page into a compact, annotated, actionable text tree.
//!
//! Phase 1 pipeline:
//!   AX tree (CDP `Accessibility.getFullAXTree`)
//!     → `snapshot`: RawNode tree (ignored nodes spliced out)
//!     → `prune`: InteractionTree (wrapper noise removed, siblings truncated)
//!     → `refs`: interactive nodes get session-stable `[eN]` refs
//!     → `render`: indented one-line-per-node text, global char budget
//!
//! Actions (`act`) resolve refs back to live DOM nodes via backendNodeId and
//! drive them with real CDP input events. `quiesce` provides `wait_settled`,
//! used after navigation and after every action.

pub mod act;
pub mod diff;
pub mod plan;
pub mod prune;
pub mod quiesce;
pub mod refs;
pub mod render;
pub mod snapshot;

use headless_chrome::Tab;

use prune::PageNode;
use refs::RefMap;

/// Default char budget for a rendered snapshot (~1.5k tokens).
pub const DEFAULT_RENDER_BUDGET: usize = 6000;

/// Wait for quiescence, pull the AX tree, prune it (default sibling limits),
/// and reconcile refs. This is the shared front half of snapshot/diff/expand,
/// and also what heals stale refs — `refs::assign_refs` re-binds by
/// fingerprint as a side effect.
pub fn compile(tab: &Tab, refs: &mut RefMap) -> anyhow::Result<Vec<PageNode>> {
    compile_with(tab, refs, Some(prune::SIBLING_LIMIT))
}

/// `compile` with an explicit sibling limit (`None` = no truncation, used by
/// the `expand` virtual action).
pub fn compile_with(
    tab: &Tab,
    refs: &mut RefMap,
    limit: Option<usize>,
) -> anyhow::Result<Vec<PageNode>> {
    quiesce::wait_settled(tab, quiesce::Quiescence::default())?;
    let raw = snapshot::capture(tab)?;
    let mut tree: Vec<PageNode> = prune::prune_with(&raw, limit);
    refs::assign_refs(&mut tree, refs);
    Ok(tree)
}

/// Full snapshot rendered to text.
pub fn snapshot_page(tab: &Tab, refs: &mut RefMap, budget: usize) -> anyhow::Result<String> {
    let tree = compile(tab, refs)?;
    Ok(render::render(&tree, budget))
}
