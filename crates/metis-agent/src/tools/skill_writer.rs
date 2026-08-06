//! `save_skill` — let the agent record a reusable solution as a real skill.
//!
//! Skills already load automatically from `<workspace>/skills/<name>/SKILL.md`
//! (see `skills.rs`), but nothing could *write* one: the agent was asked to do
//! it via a system-prompt instruction and, in practice, never did — weeks
//! passed with zero skills created. A prompt asking a model to remember
//! something is not a mechanism; a tool is.
//!
//! This also guarantees the frontmatter is well formed. Hand-written skill
//! files in the wild were missing it entirely, which makes `parse_frontmatter`
//! return `None` — the skill then has no name or description in the catalogue
//! and is effectively invisible to the model.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

use super::base::{optional_bool, optional_string, require_string, Tool};

/// Writes/updates skills under `<workspace>/skills/<name>/SKILL.md`.
pub struct SaveSkillTool {
    workspace: PathBuf,
}

impl SaveSkillTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    /// Skill names become directory names and are referenced by the loader,
    /// so keep them to a predictable kebab-case slug.
    fn normalize_name(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        let mut prev_dash = false;
        for c in raw.trim().chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() {
                out.push(c);
                prev_dash = false;
            } else if !prev_dash && !out.is_empty() {
                out.push('-');
                prev_dash = true;
            }
        }
        out.trim_matches('-').to_string()
    }

    fn skill_dir(&self, name: &str) -> PathBuf {
        self.workspace.join("skills").join(name)
    }
}

#[async_trait]
impl Tool for SaveSkillTool {
    fn name(&self) -> &str {
        "save_skill"
    }

    fn description(&self) -> &str {
        "Save a reusable solution as a skill so you (and future sessions) can use it again. \
         Call this AFTER you work out something non-obvious and reusable: a working method for a \
         tricky site or API, a multi-step CLI recipe, a fix for a flaky tool, an endpoint that \
         works where an obvious one fails. Record what actually worked — exact commands, URLs, \
         selectors, gotchas — not a narrative of what failed. If a skill with this name exists it \
         is only overwritten when overwrite=true, so check first with read_file if unsure."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Short kebab-case name, e.g. \"kayak-flight-search\""
                },
                "description": {
                    "type": "string",
                    "description": "One line: what this does and when to use it (shown in the skill catalogue)"
                },
                "body": {
                    "type": "string",
                    "description": "The skill content in Markdown: the concrete steps that worked, with exact commands/URLs/selectors"
                },
                "always": {
                    "type": "boolean",
                    "description": "If true the full text is injected into every prompt. Default false — leave false unless it must always apply."
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace an existing skill of the same name (default false)"
                }
            },
            "required": ["name", "description", "body"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let raw_name = require_string(&params, "name")?;
        let description = require_string(&params, "description")?;
        let body = require_string(&params, "body")?;
        let always = optional_bool(&params, "always");
        let overwrite = optional_bool(&params, "overwrite");

        let name = Self::normalize_name(&raw_name);
        if name.is_empty() {
            anyhow::bail!("Invalid skill name '{raw_name}': needs at least one letter or digit.");
        }
        if description.trim().is_empty() {
            anyhow::bail!("`description` cannot be empty — it is what the catalogue shows.");
        }
        if body.trim().is_empty() {
            anyhow::bail!("`body` cannot be empty — record the steps that actually worked.");
        }

        let dir = self.skill_dir(&name);
        let file = dir.join("SKILL.md");
        let existed = file.is_file();
        if existed && !overwrite {
            anyhow::bail!(
                "Skill '{name}' already exists at {}. Read it first, then call save_skill again \
                 with overwrite=true to replace it (merge in what you learned rather than \
                 discarding the existing content).",
                file.display()
            );
        }

        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("Failed to create {}: {e}", dir.display()))?;

        // Description is JSON-escaped so a quote or colon in it cannot break
        // the frontmatter parse and silently strip the skill's identity.
        let escaped = serde_json::to_string(description.trim())
            .unwrap_or_else(|_| format!("\"{}\"", description.trim().replace('"', "'")));
        let content = format!(
            "---\nname: {name}\ndescription: {escaped}\nmetadata: {{\"nanobot\":{{\"always\":{always}}}}}\n---\n\n{}\n",
            body.trim()
        );

        std::fs::write(&file, &content)
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {e}", file.display()))?;

        // Same rule as edit_file: prove it landed instead of assuming.
        let verify = std::fs::read_to_string(&file)
            .map_err(|e| anyhow::anyhow!("Wrote {} but could not read it back: {e}", file.display()))?;
        if verify != content {
            anyhow::bail!("Skill file {} did not save correctly.", file.display());
        }

        info!(skill = %name, updated = existed, "saved skill");
        Ok(format!(
            "{} skill '{name}' at {} (verified). It will be offered in the skill catalogue from the next message; {}",
            if existed { "Updated" } else { "Created" },
            file.display(),
            if always {
                "its full text loads into every prompt."
            } else {
                "read this file with read_file when the situation comes up again."
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(SaveSkillTool::normalize_name("Kayak Flight Search"), "kayak-flight-search");
        assert_eq!(SaveSkillTool::normalize_name("  weird__name!! "), "weird-name");
        assert_eq!(SaveSkillTool::normalize_name("already-kebab"), "already-kebab");
    }

    #[tokio::test]
    async fn writes_parseable_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let tool = SaveSkillTool::new(dir.path().to_path_buf());
        let out = tool
            .execute(params(&[
                ("name", json!("Kayak Flights")),
                ("description", json!("Search flights on kayak: use \"nonstop\" filter")),
                ("body", json!("# Kayak\n\nOpen kayak.com/flights/PEI-BOG/2026-07-24")),
            ]))
            .await
            .unwrap();
        assert!(out.starts_with("Created skill 'kayak-flights'"));

        let content =
            std::fs::read_to_string(dir.path().join("skills/kayak-flights/SKILL.md")).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("name: kayak-flights"));
        // A quote inside the description must not break the frontmatter.
        assert!(content.contains(r#"description: "Search flights on kayak: use \"nonstop\" filter""#));
        assert!(content.contains(r#""always":false"#));
        assert!(content.contains("Open kayak.com"));
    }

    #[tokio::test]
    async fn refuses_silent_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let tool = SaveSkillTool::new(dir.path().to_path_buf());
        let base = [
            ("name", json!("dup")),
            ("description", json!("d")),
            ("body", json!("b")),
        ];
        tool.execute(params(&base)).await.unwrap();

        let err = tool.execute(params(&base)).await.unwrap_err();
        assert!(err.to_string().contains("already exists"));

        let mut with_overwrite = params(&base);
        with_overwrite.insert("overwrite".into(), json!(true));
        let out = tool.execute(with_overwrite).await.unwrap();
        assert!(out.starts_with("Updated skill 'dup'"));
    }

    #[tokio::test]
    async fn rejects_empty_fields() {
        let dir = tempfile::tempdir().unwrap();
        let tool = SaveSkillTool::new(dir.path().to_path_buf());
        let err = tool
            .execute(params(&[
                ("name", json!("x")),
                ("description", json!("   ")),
                ("body", json!("b")),
            ]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("description"));
    }
}
