//! Human approval for changes that cannot be undone.
//!
//! Before this, "approval" was a token the exec tool handed to the MODEL,
//! which handed it straight back - so no person was ever asked, and the
//! check stopped nothing. A script run with `--output blf2.nam` replaced a
//! 6.8 MB database with a fresh 0.4 MB file without anyone being consulted.
//!
//! Now a change that can destroy data - overwriting or deleting an existing
//! file, a recursive delete, sending data off the machine - is put to the
//! PERSON, with the list of what would change: allow once, allow this kind
//! of change for the rest of the session, or deny. Where nobody can be asked
//! (a scheduled job, a non-interactive run), the answer is no.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

/// What kind of change is being asked about. "Allow for this session" is
/// remembered per kind, so allowing overwrites does not also allow deletes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// An existing file would be replaced.
    Overwrite,
    /// Files or folders would be deleted.
    Delete,
    /// A command matching a destructive pattern (format, recursive delete...).
    Destructive,
    /// Data could leave the machine (upload, mail, file transfer).
    SendData,
    /// Policy says every command needs approval.
    AnyCommand,
}

impl ChangeKind {
    pub fn label(&self) -> &'static str {
        match self {
            ChangeKind::Overwrite => "overwrite existing files",
            ChangeKind::Delete => "delete files",
            ChangeKind::Destructive => "run destructive commands",
            ChangeKind::SendData => "send data off this machine",
            ChangeKind::AnyCommand => "run commands",
        }
    }
}

/// What the person is shown.
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    /// Conversation the request belongs to (session-scoped allows key on it).
    pub session: String,
    pub kind: ChangeKind,
    /// One line: what is about to happen.
    pub title: String,
    /// Every concrete thing that would change, one per line.
    pub changes: Vec<String>,
    /// The exact command or content summary, for the person to inspect.
    pub detail: String,
}

/// The person's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

/// Something that can put a request in front of a person and wait.
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn ask(&self, req: ApprovalRequest) -> ApprovalDecision;
}

/// Outcome of a check, ready to act on.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Allowed,
    /// Not allowed; the text is the tool result to return to the model.
    Refused(String),
}

/// Shared by the tools of one agent.
pub struct ApprovalBroker {
    handler: std::sync::RwLock<Option<Arc<dyn ApprovalHandler>>>,
    /// (session, kind) the person allowed for the rest of the session.
    allowed: Mutex<HashSet<(String, ChangeKind)>>,
    /// Files this agent created in a session; rewriting its own new file is
    /// not destroying anyone's data.
    own_files: Mutex<HashSet<(String, PathBuf)>>,
    /// When set, every decision is made by this broker instead. Team members
    /// forward to the main agent's broker, so the same person is asked and
    /// "allow for this session" covers the whole team.
    forward: std::sync::RwLock<Option<Arc<ApprovalBroker>>>,
}

impl Default for ApprovalBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalBroker {
    /// A broker with nobody to ask: every request is refused.
    pub fn new() -> Self {
        Self {
            handler: std::sync::RwLock::new(None),
            allowed: Mutex::new(HashSet::new()),
            own_files: Mutex::new(HashSet::new()),
            forward: std::sync::RwLock::new(None),
        }
    }

    /// Hand all decisions to another broker.
    pub fn forward_to(&self, other: Arc<ApprovalBroker>) {
        *self.forward.write().unwrap_or_else(|p| p.into_inner()) = Some(other);
    }

    fn forwarded(&self) -> Option<Arc<ApprovalBroker>> {
        self.forward.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Connect a way of asking a person (desktop dialog, terminal prompt,
    /// chat message).
    pub fn set_handler(&self, handler: Arc<dyn ApprovalHandler>) {
        *self.handler.write().unwrap_or_else(|p| p.into_inner()) = Some(handler);
    }

    pub fn has_handler(&self) -> bool {
        if let Some(f) = self.forwarded() {
            return f.has_handler();
        }
        self.handler
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    fn normalise(path: &Path) -> PathBuf {
        let s = path.to_string_lossy().to_lowercase().replace('/', "\\");
        PathBuf::from(s.trim_start_matches("\\\\?\\").to_string())
    }

    /// Record that the agent itself created `path` in this session.
    pub async fn note_own_file(&self, session: &str, path: &Path) {
        if let Some(f) = self.forwarded() {
            return Box::pin(f.note_own_file(session, path)).await;
        }
        self.own_files
            .lock()
            .await
            .insert((session.to_string(), Self::normalise(path)));
    }

    pub async fn is_own_file(&self, session: &str, path: &Path) -> bool {
        if let Some(f) = self.forwarded() {
            return Box::pin(f.is_own_file(session, path)).await;
        }
        self.own_files
            .lock()
            .await
            .contains(&(session.to_string(), Self::normalise(path)))
    }

    /// Ask, honouring earlier "allow for this session" answers.
    pub async fn decide(&self, req: ApprovalRequest) -> Verdict {
        if let Some(f) = self.forwarded() {
            return Box::pin(f.decide(req)).await;
        }
        let key = (req.session.clone(), req.kind);
        if self.allowed.lock().await.contains(&key) {
            return Verdict::Allowed;
        }
        let handler = self
            .handler
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(handler) = handler else {
            return Verdict::Refused(format!(
                "NOT RUN: this needs the user's approval ({}) and nobody is available to approve \
                 it here (a scheduled job or a non-interactive run). Nothing was changed. Do not \
                 try another way to make the same change. Report exactly what you would have \
                 changed:\n{}",
                req.title,
                bullet(&req.changes)
            ));
        };
        let title = req.title.clone();
        let changes = req.changes.clone();
        match handler.ask(req).await {
            ApprovalDecision::AllowOnce => Verdict::Allowed,
            ApprovalDecision::AllowSession => {
                self.allowed.lock().await.insert(key);
                Verdict::Allowed
            }
            ApprovalDecision::Deny => Verdict::Refused(format!(
                "NOT RUN: the user DENIED this ({title}). Nothing was changed. Do not retry it and \
                 do not try another way to make the same change. Tell the user what you were going \
                 to change and ask how they want to proceed. It would have changed:\n{}",
                bullet(&changes)
            )),
        }
    }
}

fn bullet(lines: &[String]) -> String {
    if lines.is_empty() {
        return "  (no details)".to_string();
    }
    lines
        .iter()
        .map(|l| format!("  - {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Describe an existing file for a change list: path, size, last modified.
pub fn describe_file(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(md) if md.is_dir() => format!("{} (folder)", path.display()),
        Ok(md) => {
            let modified = md
                .modified()
                .ok()
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.format("%Y-%m-%d %H:%M").to_string()
                })
                .unwrap_or_default();
            format!(
                "{} ({} bytes, last modified {modified})",
                path.display(),
                group_thousands(md.len())
            )
        }
        Err(_) => path.display().to_string(),
    }
}

fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(ApprovalDecision, std::sync::Mutex<u32>);
    #[async_trait]
    impl ApprovalHandler for Fixed {
        async fn ask(&self, _req: ApprovalRequest) -> ApprovalDecision {
            *self.1.lock().unwrap() += 1;
            self.0
        }
    }

    fn req(session: &str, kind: ChangeKind) -> ApprovalRequest {
        ApprovalRequest {
            session: session.into(),
            kind,
            title: "replace blf2.nam".into(),
            changes: vec!["C:\\x\\blf2.nam (6,813,796 bytes)".into()],
            detail: "python scan.py --output blf2.nam".into(),
        }
    }

    #[tokio::test]
    async fn with_nobody_to_ask_the_answer_is_no() {
        let b = ApprovalBroker::new();
        match b.decide(req("s", ChangeKind::Overwrite)).await {
            Verdict::Refused(msg) => {
                assert!(msg.starts_with("NOT RUN:"), "{msg}");
                assert!(msg.contains("blf2.nam"), "must list what would change: {msg}");
            }
            v => panic!("{v:?}"),
        }
    }

    #[tokio::test]
    async fn a_denial_says_not_to_work_around_it() {
        let b = ApprovalBroker::new();
        b.set_handler(Arc::new(Fixed(ApprovalDecision::Deny, Default::default())));
        match b.decide(req("s", ChangeKind::Overwrite)).await {
            Verdict::Refused(msg) => {
                assert!(msg.contains("DENIED"), "{msg}");
                assert!(msg.contains("do not try another way"), "{msg}");
            }
            v => panic!("{v:?}"),
        }
    }

    #[tokio::test]
    async fn allow_once_asks_again_next_time() {
        let b = ApprovalBroker::new();
        let h = Arc::new(Fixed(ApprovalDecision::AllowOnce, Default::default()));
        b.set_handler(h.clone());
        assert_eq!(b.decide(req("s", ChangeKind::Overwrite)).await, Verdict::Allowed);
        assert_eq!(b.decide(req("s", ChangeKind::Overwrite)).await, Verdict::Allowed);
        assert_eq!(*h.1.lock().unwrap(), 2, "allow-once must ask every time");
    }

    #[tokio::test]
    async fn allow_for_session_is_remembered_per_kind_and_per_session() {
        let b = ApprovalBroker::new();
        let h = Arc::new(Fixed(ApprovalDecision::AllowSession, Default::default()));
        b.set_handler(h.clone());
        assert_eq!(b.decide(req("s1", ChangeKind::Overwrite)).await, Verdict::Allowed);
        assert_eq!(b.decide(req("s1", ChangeKind::Overwrite)).await, Verdict::Allowed);
        assert_eq!(*h.1.lock().unwrap(), 1, "the second overwrite must not ask again");
        // A different kind of change is asked about separately...
        b.decide(req("s1", ChangeKind::Delete)).await;
        assert_eq!(*h.1.lock().unwrap(), 2);
        // ...and so is the same kind in another conversation.
        b.decide(req("s2", ChangeKind::Overwrite)).await;
        assert_eq!(*h.1.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn own_files_are_tracked_per_session_case_insensitively() {
        let b = ApprovalBroker::new();
        b.note_own_file("s", Path::new("C:\\W\\step3.py")).await;
        assert!(b.is_own_file("s", Path::new("c:/w/STEP3.py")).await);
        assert!(!b.is_own_file("other", Path::new("C:\\W\\step3.py")).await);
    }

    #[tokio::test]
    async fn a_team_member_asks_the_same_person_and_shares_session_allows() {
        let main = Arc::new(ApprovalBroker::new());
        let h = Arc::new(Fixed(ApprovalDecision::AllowSession, Default::default()));
        main.set_handler(h.clone());
        let member = ApprovalBroker::new();
        member.forward_to(main.clone());
        assert!(member.has_handler());
        assert_eq!(member.decide(req("s", ChangeKind::Overwrite)).await, Verdict::Allowed);
        // main itself is now covered for the session - one team, one answer.
        assert_eq!(main.decide(req("s", ChangeKind::Overwrite)).await, Verdict::Allowed);
        assert_eq!(*h.1.lock().unwrap(), 1);
    }

    #[test]
    fn sizes_are_grouped_for_reading() {
        assert_eq!(group_thousands(6813796), "6,813,796");
        assert_eq!(group_thousands(999), "999");
    }
}
