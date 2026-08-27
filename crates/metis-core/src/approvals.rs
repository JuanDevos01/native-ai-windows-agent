//! Pending outbound replies awaiting human approval.
//!
//! Mail from a sender that is not on the allow-list is answered by the agent
//! as usual, but the reply is held here instead of being sent. A human then
//! approves or rejects it.
//!
//! The queue is a JSON file rather than in-memory state because the gateway
//! (which owns the mail channel) and the desktop app (where approvals are
//! reviewed) are separate processes. The desktop marks an entry approved; the
//! gateway notices and sends it. That also means a pending reply survives a
//! restart of either side, which matters when the whole point is that nothing
//! is sent without a person seeing it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where an entry is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    /// Waiting for a human.
    Pending,
    /// A human said send it; the gateway has not sent it yet.
    Approved,
    /// A human said no. Kept briefly for visibility, never sent.
    Rejected,
    /// Sent successfully.
    Sent,
    /// Approved but sending failed; `error` says why.
    Failed,
}

/// One held reply.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingReply {
    pub id: String,
    /// Channel the reply would go out on (currently always "email").
    pub channel: String,
    /// Recipient — for email, the sender being replied to.
    pub to: String,
    #[serde(default)]
    pub subject: String,
    /// Enough of the incoming message to judge the reply by.
    #[serde(default)]
    pub incoming_excerpt: String,
    /// The reply the agent produced. Editable before approving.
    pub body: String,
    pub created_at_ms: i64,
    pub status: ApprovalStatus,
    #[serde(default)]
    pub error: Option<String>,
}

impl PendingReply {
    pub fn new(
        channel: impl Into<String>,
        to: impl Into<String>,
        subject: impl Into<String>,
        incoming_excerpt: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            // Short, stable, and easy to type into a chat command.
            id: uuid::Uuid::new_v4().to_string()[..8].to_string(),
            channel: channel.into(),
            to: to.into(),
            subject: subject.into(),
            incoming_excerpt: incoming_excerpt.into(),
            body: body.into(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            status: ApprovalStatus::Pending,
            error: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalStore {
    #[serde(default)]
    pub entries: Vec<PendingReply>,
}

/// Default location: `~/.metis/pending_replies.json`.
pub fn default_path() -> PathBuf {
    crate::utils::get_data_path().join("pending_replies.json")
}

/// Read the queue. A missing file is an empty queue; a corrupt one is
/// reported by the caller rather than silently discarded, since losing a
/// pending reply means losing a message the user never saw.
pub fn load(path: &Path) -> std::io::Result<ApprovalStore> {
    if !path.is_file() {
        return Ok(ApprovalStore::default());
    }
    let text = std::fs::read_to_string(path)?;
    // Same BOM tolerance as the main config: an editor can add one.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// Write the queue atomically — a torn write here would lose replies.
pub fn save(path: &Path, store: &ApprovalStore) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(store)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Queue a reply for approval.
pub fn push(path: &Path, entry: PendingReply) -> std::io::Result<()> {
    let mut store = load(path).unwrap_or_default();
    store.entries.push(entry);
    prune(&mut store);
    save(path, &store)
}

/// Set an entry's status. Returns the updated entry.
pub fn set_status(
    path: &Path,
    id: &str,
    status: ApprovalStatus,
    error: Option<String>,
) -> std::io::Result<Option<PendingReply>> {
    let mut store = load(path).unwrap_or_default();
    let mut updated = None;
    for e in store.entries.iter_mut() {
        if e.id == id {
            e.status = status;
            e.error = error.clone();
            updated = Some(e.clone());
            break;
        }
    }
    if updated.is_some() {
        save(path, &store)?;
    }
    Ok(updated)
}

/// Replace an entry's body (editing a draft before approving).
pub fn set_body(path: &Path, id: &str, body: &str) -> std::io::Result<bool> {
    let mut store = load(path).unwrap_or_default();
    let mut found = false;
    for e in store.entries.iter_mut() {
        if e.id == id {
            e.body = body.to_string();
            found = true;
            break;
        }
    }
    if found {
        save(path, &store)?;
    }
    Ok(found)
}

/// Entries in a given status, oldest first.
pub fn with_status(store: &ApprovalStore, status: ApprovalStatus) -> Vec<&PendingReply> {
    let mut v: Vec<&PendingReply> = store
        .entries
        .iter()
        .filter(|e| e.status == status)
        .collect();
    v.sort_by_key(|e| e.created_at_ms);
    v
}

/// Keep the file from growing without bound: settled entries are history,
/// not state. Pending ones are never dropped — that would silently discard a
/// message a human still has to decide on.
fn prune(store: &mut ApprovalStore) {
    const KEEP_SETTLED: usize = 50;
    let settled: Vec<usize> = store
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !matches!(e.status, ApprovalStatus::Pending | ApprovalStatus::Approved))
        .map(|(i, _)| i)
        .collect();
    if settled.len() <= KEEP_SETTLED {
        return;
    }
    let drop: std::collections::HashSet<usize> =
        settled[..settled.len() - KEEP_SETTLED].iter().copied().collect();
    let mut i = 0;
    store.entries.retain(|_| {
        let keep = !drop.contains(&i);
        i += 1;
        keep
    });
}

#[cfg(test)]
mod tests {

    #[test]
    fn end_to_end_desktop_approves_gateway_sends() {
        // Mirrors the real two-process flow: the email channel queues a
        // held reply, the desktop edits + approves it, and the gateway
        // picks up exactly what the human saw and marks it sent.
        let (_d, p) = tmp();

        // 1. Channel holds a reply to an unlisted sender.
        push(&p, PendingReply::new(
            "email", "stranger@outside.com", "Quote request",
            "Can you send pricing?", "Here is our pricing draft.",
        )).unwrap();

        // 2. Desktop shows exactly the pending ones.
        let store = load(&p).unwrap();
        let waiting = with_status(&store, ApprovalStatus::Pending);
        assert_eq!(waiting.len(), 1);
        let id = waiting[0].id.clone();

        // 3. Human edits the draft, then approves.
        set_body(&p, &id, "Here is our pricing, reviewed by a human.").unwrap();
        set_status(&p, &id, ApprovalStatus::Approved, None).unwrap();

        // 4. Gateway sees it as approved, with the EDITED body.
        let store = load(&p).unwrap();
        let to_send = with_status(&store, ApprovalStatus::Approved);
        assert_eq!(to_send.len(), 1);
        assert_eq!(to_send[0].body, "Here is our pricing, reviewed by a human.");
        assert_eq!(to_send[0].to, "stranger@outside.com");

        // 5. Gateway marks it sent; it must not be picked up again.
        set_status(&p, &id, ApprovalStatus::Sent, None).unwrap();
        let store = load(&p).unwrap();
        assert!(with_status(&store, ApprovalStatus::Approved).is_empty(),
                "a sent reply must never be resent");
        assert!(with_status(&store, ApprovalStatus::Pending).is_empty());
    }

    #[test]
    fn rejected_replies_are_never_sendable() {
        let (_d, p) = tmp();
        push(&p, PendingReply::new("email", "spam@x.com", "", "", "draft")).unwrap();
        let id = load(&p).unwrap().entries[0].id.clone();
        set_status(&p, &id, ApprovalStatus::Rejected, None).unwrap();

        let store = load(&p).unwrap();
        assert!(with_status(&store, ApprovalStatus::Approved).is_empty());
        assert!(with_status(&store, ApprovalStatus::Pending).is_empty());
    }

    use super::*;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("pending.json");
        (d, p)
    }

    #[test]
    fn round_trips_and_survives_missing_file() {
        let (_d, p) = tmp();
        assert!(load(&p).unwrap().entries.is_empty(), "missing file = empty queue");

        push(&p, PendingReply::new("email", "a@x.com", "Subj", "hello?", "hi back")).unwrap();
        let store = load(&p).unwrap();
        assert_eq!(store.entries.len(), 1);
        let e = &store.entries[0];
        assert_eq!(e.to, "a@x.com");
        assert_eq!(e.status, ApprovalStatus::Pending);
        assert_eq!(e.id.len(), 8, "id should be short enough to type");
    }

    #[test]
    fn approve_and_edit_flow() {
        let (_d, p) = tmp();
        push(&p, PendingReply::new("email", "a@x.com", "S", "in", "draft")).unwrap();
        let id = load(&p).unwrap().entries[0].id.clone();

        assert!(set_body(&p, &id, "edited reply").unwrap());
        assert_eq!(load(&p).unwrap().entries[0].body, "edited reply");

        let updated = set_status(&p, &id, ApprovalStatus::Approved, None).unwrap().unwrap();
        assert_eq!(updated.status, ApprovalStatus::Approved);

        let store = load(&p).unwrap();
        assert_eq!(with_status(&store, ApprovalStatus::Approved).len(), 1);
        assert!(with_status(&store, ApprovalStatus::Pending).is_empty());
    }

    #[test]
    fn unknown_id_is_not_an_error() {
        let (_d, p) = tmp();
        assert!(set_status(&p, "nope", ApprovalStatus::Approved, None).unwrap().is_none());
        assert!(!set_body(&p, "nope", "x").unwrap());
    }

    #[test]
    fn prune_keeps_pending_and_trims_settled() {
        let (_d, p) = tmp();
        // Two waiting on a human, plus a long tail of settled history.
        push(&p, PendingReply::new("email", "keep1@x.com", "", "", "b")).unwrap();
        for i in 0..80 {
            let mut e = PendingReply::new("email", format!("old{i}@x.com"), "", "", "b");
            e.status = ApprovalStatus::Sent;
            push(&p, e).unwrap();
        }
        push(&p, PendingReply::new("email", "keep2@x.com", "", "", "b")).unwrap();

        let store = load(&p).unwrap();
        let pending = with_status(&store, ApprovalStatus::Pending);
        assert_eq!(pending.len(), 2, "pending replies must never be pruned");
        assert!(store.entries.len() < 83, "settled history should be trimmed");
    }

    #[test]
    fn tolerates_a_bom() {
        let (_d, p) = tmp();
        let json = r#"{"entries":[]}"#;
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(json.as_bytes());
        std::fs::write(&p, bytes).unwrap();
        assert!(load(&p).is_ok());
    }
}
