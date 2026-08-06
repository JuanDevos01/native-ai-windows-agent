//! Semantic memory store — SQLite-backed long-term memory with hybrid search.
//!
//! Unlike the file-based `MEMORY.md` (which is injected wholesale into every
//! prompt), this store holds an unbounded number of memory entries and
//! retrieves only the most relevant ones per query.
//!
//! Storage: `~/.metis/memory.db` (single SQLite file, bundled driver — no
//! system dependency). Embeddings are stored as little-endian `f32` BLOBs and
//! scored with a brute-force cosine scan in Rust: at personal-assistant scale
//! (thousands of entries) this is well under a millisecond, so no ANN index
//! (and no beta vector-DB dependency) is needed. The store is written behind
//! this module's API so the backend can later be swapped (e.g. for Turso)
//! without touching callers.
//!
//! Search is **hybrid**: keyword overlap always works (even with no embedding
//! model configured); cosine similarity is blended in when embeddings exist.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

/// Minimum combined score for a search hit to be returned.
const MIN_SCORE: f32 = 0.1;

/// Blend weights when both keyword and vector signals are available.
const KEYWORD_WEIGHT: f32 = 0.35;
const VECTOR_WEIGHT: f32 = 0.65;

/// Common words ignored during keyword matching.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "is", "are", "was", "were", "be", "been", "do", "does",
    "did", "what", "who", "how", "when", "where", "why", "which", "you", "your", "my", "me",
    "i", "we", "us", "of", "to", "in", "on", "for", "it", "this", "that", "with", "about",
    "know", "tell", "please", "can", "could", "would", "have", "has",
];

// ─────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────

/// A single stored memory returned from a search.
#[derive(Clone, Debug)]
pub struct MemoryEntry {
    /// Row id.
    pub id: i64,
    /// The memory text itself.
    pub text: String,
    /// Origin: `"manual"` (agent tool), `"compaction"` (session summary), etc.
    pub source: String,
    /// Session key the memory came from (may be empty).
    pub session_key: String,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
    /// Relevance score for the query that produced this entry (0.0 – 1.0).
    pub score: f32,
}

/// SQLite-backed memory store. Thread-safe via an internal `Mutex`.
pub struct MemoryDb {
    conn: Mutex<Connection>,
}

impl MemoryDb {
    /// Open (or create) the memory database at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory database (tests).
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                text        TEXT NOT NULL,
                source      TEXT NOT NULL DEFAULT 'manual',
                session_key TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL,
                embedding   BLOB
            );
            CREATE INDEX IF NOT EXISTS idx_memories_created ON memories(created_at);",
        )?;
        Ok(())
    }

    /// Insert a memory. `embedding` is optional (keyword-only search still works).
    ///
    /// Returns the new row id.
    pub fn insert(
        &self,
        text: &str,
        source: &str,
        session_key: &str,
        embedding: Option<&[f32]>,
    ) -> anyhow::Result<i64> {
        let created_at = chrono::Utc::now().to_rfc3339();
        let blob = embedding.map(embedding_to_blob);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO memories (text, source, session_key, created_at, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![text, source, session_key, created_at, blob],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Delete a memory by id. Returns `true` if a row was removed.
    pub fn delete(&self, id: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Total number of stored memories.
    pub fn count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        Ok(n)
    }

    /// Hybrid search: keyword overlap blended with cosine similarity.
    ///
    /// `query_embedding` is optional — without it (or for rows without an
    /// embedding), scoring falls back to keyword overlap only. Results are
    /// sorted by score descending and truncated to `top_k`; entries below a
    /// minimum relevance threshold are dropped.
    pub fn search(
        &self,
        query: &str,
        query_embedding: Option<&[f32]>,
        top_k: usize,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let query_tokens = tokenize(query);
        let mut hits: Vec<MemoryEntry> = Vec::new();

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, text, source, session_key, created_at, embedding FROM memories",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
            ))
        })?;

        for row in rows {
            let (id, text, source, session_key, created_at, blob) = row?;
            let kw = keyword_score(&query_tokens, &text);
            let score = match (query_embedding, blob.as_deref()) {
                (Some(qe), Some(b)) => {
                    let emb = blob_to_embedding(b);
                    let cos = cosine_similarity(qe, &emb).max(0.0);
                    KEYWORD_WEIGHT * kw + VECTOR_WEIGHT * cos
                }
                _ => kw,
            };
            if score >= MIN_SCORE {
                hits.push(MemoryEntry {
                    id,
                    text,
                    source,
                    session_key,
                    created_at,
                    score,
                });
            }
        }

        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(top_k);
        Ok(hits)
    }
}

// ─────────────────────────────────────────────
// Scoring helpers
// ─────────────────────────────────────────────

/// Lowercase alphanumeric tokens, minus stopwords and single characters.
fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2 && !STOPWORDS.contains(t))
        .map(str::to_string)
        .collect()
}

/// Fraction of query tokens present in the text (0.0 – 1.0).
fn keyword_score(query_tokens: &[String], text: &str) -> f32 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let text_tokens: HashSet<String> = tokenize(text).into_iter().collect();
    let matched = query_tokens
        .iter()
        .filter(|t| text_tokens.contains(*t))
        .count();
    matched as f32 / query_tokens.len() as f32
}

/// Cosine similarity; 0.0 on dimension mismatch or zero vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Encode an embedding as little-endian f32 bytes.
fn embedding_to_blob(e: &[f32]) -> Vec<u8> {
    e.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Decode little-endian f32 bytes back to an embedding.
fn blob_to_embedding(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_count() {
        let db = MemoryDb::open_in_memory().unwrap();
        assert_eq!(db.count().unwrap(), 0);
        db.insert("User prefers dark mode.", "manual", "cli:1", None)
            .unwrap();
        db.insert("Project X runs on port 8080.", "manual", "", None)
            .unwrap();
        assert_eq!(db.count().unwrap(), 2);
    }

    #[test]
    fn test_delete() {
        let db = MemoryDb::open_in_memory().unwrap();
        let id = db.insert("temp fact", "manual", "", None).unwrap();
        assert!(db.delete(id).unwrap());
        assert!(!db.delete(id).unwrap());
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn test_keyword_search() {
        let db = MemoryDb::open_in_memory().unwrap();
        db.insert("User's birthday is March 3rd.", "manual", "", None)
            .unwrap();
        db.insert("The deploy script lives in scripts/deploy.ps1.", "manual", "", None)
            .unwrap();

        let hits = db.search("when is my birthday?", None, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("March 3rd"));
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn test_search_no_match() {
        let db = MemoryDb::open_in_memory().unwrap();
        db.insert("User prefers dark mode.", "manual", "", None)
            .unwrap();
        let hits = db.search("quantum entanglement research", None, 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_vector_search_ranking() {
        let db = MemoryDb::open_in_memory().unwrap();
        // Two orthogonal embeddings; query matches the first.
        db.insert("fact alpha", "manual", "", Some(&[1.0, 0.0, 0.0]))
            .unwrap();
        db.insert("fact beta", "manual", "", Some(&[0.0, 1.0, 0.0]))
            .unwrap();

        let hits = db
            .search("unrelated words entirely", Some(&[0.9, 0.1, 0.0]), 5)
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].text, "fact alpha");
        // beta has near-zero cosine and zero keyword overlap → filtered or ranked last
        if hits.len() > 1 {
            assert!(hits[0].score > hits[1].score);
        }
    }

    #[test]
    fn test_hybrid_beats_keyword_only() {
        let db = MemoryDb::open_in_memory().unwrap();
        db.insert("favorite editor is helix", "manual", "", Some(&[1.0, 0.0]))
            .unwrap();
        db.insert("favorite drink is coffee", "manual", "", Some(&[0.0, 1.0]))
            .unwrap();

        // Both match "favorite" equally on keywords; embedding disambiguates.
        let hits = db.search("favorite", Some(&[0.0, 1.0]), 2).unwrap();
        assert_eq!(hits[0].text, "favorite drink is coffee");
    }

    #[test]
    fn test_top_k_limit() {
        let db = MemoryDb::open_in_memory().unwrap();
        for i in 0..10 {
            db.insert(&format!("rust note number {i}"), "manual", "", None)
                .unwrap();
        }
        let hits = db.search("rust note", None, 3).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn test_embedding_roundtrip() {
        let e = vec![0.25f32, -1.5, 3.75];
        assert_eq!(blob_to_embedding(&embedding_to_blob(&e)), e);
    }

    #[test]
    fn test_cosine_mismatched_dims() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn test_persistence_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        {
            let db = MemoryDb::open(&path).unwrap();
            db.insert("persistent fact about metis", "compaction", "telegram:1", None)
                .unwrap();
        }
        {
            let db = MemoryDb::open(&path).unwrap();
            assert_eq!(db.count().unwrap(), 1);
            let hits = db.search("metis fact", None, 5).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].source, "compaction");
            assert_eq!(hits[0].session_key, "telegram:1");
        }
    }
}
