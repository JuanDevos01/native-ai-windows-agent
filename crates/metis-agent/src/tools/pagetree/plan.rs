//! Plan: the JSON envelope for a replayable set of refs.
//!
//! `export_plan` on one session, `import_plan` on another (or the same
//! session after a reload) — fingerprints do the rest. This is what makes a
//! saved automation ("fill this form", "click through this checkout")
//! reusable across page loads instead of a one-shot script tied to whatever
//! backendNodeIds happened to exist when it was recorded.

use serde::{Deserialize, Serialize};

use super::refs::PlanRef;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    /// Informational only — not used for matching. Refs bind by fingerprint,
    /// so a plan recorded on one URL can still partially apply to another
    /// page that happens to share element shapes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub refs: Vec<PlanRef>,
}

pub fn to_json(url: Option<String>, refs: Vec<PlanRef>) -> String {
    let plan = Plan { url, refs };
    serde_json::to_string_pretty(&plan).unwrap_or_else(|_| "{\"refs\":[]}".to_string())
}

pub fn from_json(text: &str) -> anyhow::Result<Plan> {
    serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("Invalid plan JSON: {e}. Expected the output of action=export_plan."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let refs = vec![PlanRef {
            ref_id: 3,
            role: "button".into(),
            name: "submit".into(),
            anchor_path: vec![("form".into(), String::new())],
            ordinal: 0,
        }];
        let json = to_json(Some("https://example.com".into()), refs.clone());
        let plan = from_json(&json).unwrap();
        assert_eq!(plan.url.as_deref(), Some("https://example.com"));
        assert_eq!(plan.refs.len(), 1);
        assert_eq!(plan.refs[0].ref_id, 3);
    }

    #[test]
    fn rejects_garbage() {
        assert!(from_json("not json").is_err());
        assert!(from_json("{}").is_err(), "refs is required");
    }
}
