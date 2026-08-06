//! Semantic memory index — glues the SQLite memory store to an embedding
//! provider.
//!
//! The store itself (`metis_core::memory_db::MemoryDb`) is provider-agnostic;
//! this wrapper adds best-effort embedding on save/search. Embedding failures
//! never block a save or a search — they just degrade to keyword-only
//! matching, so memory keeps working with no embedding model configured.

use std::sync::Arc;

use metis_core::memory_db::{MemoryDb, MemoryEntry};
use metis_providers::traits::LlmProvider;
use tracing::{debug, warn};

/// Memory store + optional embedder.
pub struct MemoryIndex {
    db: MemoryDb,
    /// Provider + model used for embeddings, if configured.
    embedder: Option<(Arc<dyn LlmProvider>, String)>,
}

impl MemoryIndex {
    /// Create an index over `db`. `embed_provider`/`embed_model` are optional;
    /// without them, search is keyword-only.
    pub fn new(
        db: MemoryDb,
        embed_provider: Option<Arc<dyn LlmProvider>>,
        embed_model: impl Into<String>,
    ) -> Self {
        let model = embed_model.into();
        let embedder = match embed_provider {
            Some(p) if !model.trim().is_empty() => Some((p, model)),
            _ => None,
        };
        Self { db, embedder }
    }

    /// Whether vector search is configured.
    pub fn has_embedder(&self) -> bool {
        self.embedder.is_some()
    }

    /// Embed a single text, best-effort. `None` on failure or no embedder.
    async fn embed_one(&self, text: &str) -> Option<Vec<f32>> {
        let (provider, model) = self.embedder.as_ref()?;
        match provider.embeddings(&[text.to_string()], model).await {
            Ok(mut vecs) if !vecs.is_empty() => Some(vecs.remove(0)),
            Ok(_) => None,
            Err(e) => {
                warn!(error = %e, "embedding failed; storing/searching without vector");
                None
            }
        }
    }

    /// Save a memory (embedding it if possible). Returns the new row id.
    pub async fn save(
        &self,
        text: &str,
        source: &str,
        session_key: &str,
    ) -> anyhow::Result<i64> {
        let embedding = self.embed_one(text).await;
        let id = self
            .db
            .insert(text, source, session_key, embedding.as_deref())?;
        debug!(id, source, embedded = embedding.is_some(), "memory saved");
        Ok(id)
    }

    /// Search memories relevant to `query`. Errors are logged and produce an
    /// empty result — recall must never break message processing.
    pub async fn search(&self, query: &str, top_k: usize) -> Vec<MemoryEntry> {
        let query_embedding = self.embed_one(query).await;
        match self.db.search(query, query_embedding.as_deref(), top_k) {
            Ok(hits) => hits,
            Err(e) => {
                warn!(error = %e, "memory search failed");
                Vec::new()
            }
        }
    }

    /// Number of stored memories (0 on error).
    pub fn count(&self) -> i64 {
        self.db.count().unwrap_or(0)
    }

    /// Render search hits as a context block for the system prompt.
    /// `None` when there are no hits.
    pub fn format_recall_block(hits: &[MemoryEntry]) -> Option<String> {
        Self::format_recall_block_capped(hits, RECALL_SNIPPET_MAX_CHARS)
    }

    /// Like [`Self::format_recall_block`] but with a caller-chosen snippet cap
    /// (direct chat on CPU-bound local models uses a much smaller one — every
    /// injected token there costs tens of milliseconds of prompt evaluation).
    pub fn format_recall_block_capped(hits: &[MemoryEntry], max_chars: usize) -> Option<String> {
        if hits.is_empty() {
            return None;
        }
        let mut out = String::from(
            "# Recalled Memories\n\nLong-term memories retrieved for the current message \
             (relevance-ranked; may be incomplete — use memory_search for more):\n",
        );
        for h in hits {
            let date = h.created_at.get(..10).unwrap_or(&h.created_at);
            out.push_str(&format!("- [{date}] {}\n", recall_snippet(&h.text, max_chars)));
        }
        Some(out)
    }
}

/// Max characters of one memory in an automatic recall block. Injection
/// happens on every message and (in direct chat mode) is persisted with it,
/// so an uncapped memory — e.g. a long compaction summary — would bloat every
/// turn's prompt. The `memory_search` tool returns full texts on demand.
pub const RECALL_SNIPPET_MAX_CHARS: usize = 600;

/// A memory's text exactly as it appears in a recall block (truncated form).
/// Also used to detect memories already present in the session window — the
/// caller must pass the same cap the block was rendered with.
pub fn recall_snippet(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyword_only_index() -> MemoryIndex {
        MemoryIndex::new(MemoryDb::open_in_memory().unwrap(), None, "")
    }

    #[tokio::test]
    async fn test_save_and_search_without_embedder() {
        let index = keyword_only_index();
        index
            .save("User's dog is named Bruno.", "manual", "cli:1")
            .await
            .unwrap();
        assert!(!index.has_embedder());
        assert_eq!(index.count(), 1);

        let hits = index.search("dog named bruno", 5).await;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("Bruno"));
    }

    #[tokio::test]
    async fn test_search_empty_db() {
        let index = keyword_only_index();
        assert!(index.search("anything", 5).await.is_empty());
    }

    #[test]
    fn test_format_recall_block() {
        assert!(MemoryIndex::format_recall_block(&[]).is_none());
        let hits = vec![MemoryEntry {
            id: 1,
            text: "User prefers Rust.".into(),
            source: "manual".into(),
            session_key: String::new(),
            created_at: "2026-07-06T10:00:00Z".into(),
            score: 0.9,
        }];
        let block = MemoryIndex::format_recall_block(&hits).unwrap();
        assert!(block.contains("# Recalled Memories"));
        assert!(block.contains("[2026-07-06] User prefers Rust."));
    }
}
