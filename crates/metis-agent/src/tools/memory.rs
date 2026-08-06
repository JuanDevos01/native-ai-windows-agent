//! Memory tools — let the agent actively save and recall long-term memories.
//!
//! Backed by the SQLite semantic memory store (`MemoryIndex`). These are the
//! active counterpart to the passive per-message recall the agent loop injects
//! into context.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::base::{optional_i64, require_string, Tool};
use crate::memory_index::MemoryIndex;

/// Default number of results for `memory_search`.
const DEFAULT_SEARCH_LIMIT: usize = 5;
const MAX_SEARCH_LIMIT: usize = 20;

// ─────────────────────────────────────────────
// memory_save
// ─────────────────────────────────────────────

/// Save an important fact to long-term memory.
pub struct MemorySaveTool {
    index: Arc<MemoryIndex>,
}

impl MemorySaveTool {
    pub fn new(index: Arc<MemoryIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl Tool for MemorySaveTool {
    fn name(&self) -> &str {
        "memory_save"
    }

    fn description(&self) -> &str {
        "Save an important fact, user preference, or decision to long-term memory so it can be \
         recalled in future conversations (semantic search). Use for durable information, not \
         transient conversation details."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The fact to remember, written as a standalone sentence with full context (e.g. \"User's birthday is March 3rd\", not \"it's March 3rd\")."
                }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let text = require_string(&params, "text")?;
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!("Cannot save an empty memory");
        }
        let id = self.index.save(text, "manual", "").await?;
        Ok(format!("Memory saved (id {id})."))
    }
}

// ─────────────────────────────────────────────
// memory_search
// ─────────────────────────────────────────────

/// Search long-term memory.
pub struct MemorySearchTool {
    index: Arc<MemoryIndex>,
}

impl MemorySearchTool {
    pub fn new(index: Arc<MemoryIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search long-term memory for facts, preferences, decisions, and summaries of past \
         conversations. Use when the user refers to something from the past that is not in the \
         current conversation."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to look for (natural language)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default 5, max 20)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let query = require_string(&params, "query")?;
        let limit = optional_i64(&params, "limit")
            .map(|n| (n.max(1) as usize).min(MAX_SEARCH_LIMIT))
            .unwrap_or(DEFAULT_SEARCH_LIMIT);

        let hits = self.index.search(&query, limit).await;
        if hits.is_empty() {
            return Ok(format!(
                "No memories found for '{query}' ({} memories stored in total).",
                self.index.count()
            ));
        }

        let mut out = format!("Found {} memor{}:\n", hits.len(), if hits.len() == 1 { "y" } else { "ies" });
        for h in &hits {
            let date = h.created_at.get(..10).unwrap_or(&h.created_at);
            out.push_str(&format!(
                "- [{date}] ({}, score {:.2}) {}\n",
                h.source, h.score, h.text
            ));
        }
        Ok(out)
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use metis_core::memory_db::MemoryDb;

    fn make_index() -> Arc<MemoryIndex> {
        Arc::new(MemoryIndex::new(
            MemoryDb::open_in_memory().unwrap(),
            None,
            "",
        ))
    }

    #[tokio::test]
    async fn test_save_then_search() {
        let index = make_index();
        let save = MemorySaveTool::new(index.clone());
        let search = MemorySearchTool::new(index);

        let mut params = HashMap::new();
        params.insert("text".into(), json!("User's favorite pizza is margherita."));
        let result = save.execute(params).await.unwrap();
        assert!(result.contains("Memory saved"));

        let mut params = HashMap::new();
        params.insert("query".into(), json!("favorite pizza"));
        let result = search.execute(params).await.unwrap();
        assert!(result.contains("margherita"));
    }

    #[tokio::test]
    async fn test_save_empty_rejected() {
        let save = MemorySaveTool::new(make_index());
        let mut params = HashMap::new();
        params.insert("text".into(), json!("   "));
        assert!(save.execute(params).await.is_err());
    }

    #[tokio::test]
    async fn test_search_no_results() {
        let search = MemorySearchTool::new(make_index());
        let mut params = HashMap::new();
        params.insert("query".into(), json!("nonexistent topic"));
        let result = search.execute(params).await.unwrap();
        assert!(result.contains("No memories found"));
    }

    #[test]
    fn test_definitions() {
        let index = make_index();
        let save = MemorySaveTool::new(index.clone());
        let search = MemorySearchTool::new(index);
        assert_eq!(save.to_definition().function.name, "memory_save");
        assert_eq!(search.to_definition().function.name, "memory_search");
    }
}
