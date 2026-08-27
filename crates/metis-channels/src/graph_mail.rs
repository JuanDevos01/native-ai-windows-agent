//! Microsoft Graph mail backend for the email channel.
//!
//! Office 365 cannot be reached over IMAP any more: Microsoft disabled Basic
//! authentication for IMAP/POP/SMTP in Exchange Online on 2022-10-01, so the
//! IMAP backend gets `NO AUTHENTICATE failed` no matter how correct the
//! credentials are (app passwords are Basic auth too, so they fail the same
//! way). Graph is the supported path: OAuth2 for auth, REST for everything
//! else — no IMAP/SMTP protocol handling at all.
//!
//! Uses the client-credentials (app-only) flow, which suits a headless
//! assistant: no interactive sign-in and no refresh-token storage. Requires
//! an Azure AD app with the **application** permission `Mail.ReadWrite`
//! (and `Mail.Send` to reply), granted admin consent.

use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::debug;

/// A message fetched from Graph, reduced to what the channel needs.
#[derive(Debug, Clone)]
pub struct GraphMessage {
    pub id: String,
    pub subject: String,
    pub from: String,
    pub body: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: i64,
}

#[derive(Deserialize)]
struct MessageList {
    #[serde(default)]
    value: Vec<RawMessage>,
}

#[derive(Deserialize)]
struct RawMessage {
    id: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    from: Option<Recipient>,
    #[serde(default)]
    body: Option<ItemBody>,
    #[serde(rename = "bodyPreview", default)]
    body_preview: String,
}

#[derive(Deserialize)]
struct Recipient {
    #[serde(rename = "emailAddress")]
    email_address: Option<EmailAddress>,
}

#[derive(Deserialize)]
struct EmailAddress {
    #[serde(default)]
    address: String,
}

#[derive(Deserialize)]
struct ItemBody {
    #[serde(default)]
    content: String,
    #[serde(rename = "contentType", default)]
    content_type: String,
}

/// Minimal Graph mail client with a cached bearer token.
pub struct GraphMailClient {
    tenant_id: String,
    client_id: String,
    client_secret: String,
    /// Mailbox to operate on (UPN or object id).
    user_id: String,
    http: reqwest::Client,
    /// Cached token and the unix time it stops being usable.
    token: Arc<Mutex<Option<(String, i64)>>>,
}

impl GraphMailClient {
    pub fn new(
        tenant_id: String,
        client_id: String,
        client_secret: String,
        user_id: String,
    ) -> Self {
        Self {
            tenant_id,
            client_id,
            client_secret,
            user_id,
            http: reqwest::Client::new(),
            token: Arc::new(Mutex::new(None)),
        }
    }

    /// True when every field needed for the app-only flow is present.
    pub fn is_configured(&self) -> bool {
        !self.tenant_id.trim().is_empty()
            && !self.client_id.trim().is_empty()
            && !self.client_secret.trim().is_empty()
            && !self.user_id.trim().is_empty()
    }

    /// Fetch (or reuse) an app-only access token.
    async fn token(&self) -> anyhow::Result<String> {
        let now = chrono::Utc::now().timestamp();
        {
            let cached = self.token.lock().await;
            if let Some((tok, expires_at)) = cached.as_ref() {
                if *expires_at > now {
                    return Ok(tok.clone());
                }
            }
        }

        let url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        );
        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("scope", "https://graph.microsoft.com/.default"),
            ("grant_type", "client_credentials"),
        ];
        let resp = self
            .http
            .post(&url)
            .form(&params)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Graph token request failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // Azure's error body names the actual problem (wrong secret,
            // missing admin consent, wrong tenant) — surface it verbatim
            // rather than a generic "auth failed".
            anyhow::bail!(
                "Graph token request rejected ({status}): {}",
                text.chars().take(400).collect::<String>()
            );
        }
        let parsed: TokenResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Graph token response was not JSON: {e}"))?;

        // Refresh a minute early so a token can't expire mid-request.
        let expires_at = now + parsed.expires_in.max(60) - 60;
        *self.token.lock().await = Some((parsed.access_token.clone(), expires_at));
        debug!(expires_in = parsed.expires_in, "acquired Graph token");
        Ok(parsed.access_token)
    }

    fn base(&self) -> String {
        format!("https://graph.microsoft.com/v1.0/users/{}", self.user_id)
    }

    /// Map an IMAP-style folder name onto Graph's well-known name.
    ///
    /// The config field is shared with the IMAP backend, whose default is
    /// "INBOX"; Graph expects "inbox". Anything unrecognised is passed
    /// through so custom folders still work.
    fn graph_folder(folder: &str) -> String {
        let f = folder.trim();
        if f.is_empty() {
            return "inbox".to_string();
        }
        match f.to_ascii_lowercase().as_str() {
            "inbox" => "inbox".to_string(),
            "sent" | "sent items" | "sentitems" => "sentitems".to_string(),
            "drafts" => "drafts".to_string(),
            "archive" => "archive".to_string(),
            "junk" | "junk email" | "spam" => "junkemail".to_string(),
            "deleted" | "deleted items" | "trash" => "deleteditems".to_string(),
            _ => f.to_string(),
        }
    }

    /// Fetch unread messages from the given folder (default Inbox).
    pub async fn fetch_unread(&self, folder: &str, limit: u32) -> anyhow::Result<Vec<GraphMessage>> {
        let token = self.token().await?;
        let folder = Self::graph_folder(folder);
        let url = format!(
            "{}/mailFolders/{}/messages?$filter=isRead%20eq%20false&$top={}&$select=id,subject,from,body,bodyPreview&$orderby=receivedDateTime%20asc",
            self.base(),
            folder,
            limit.clamp(1, 50)
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Graph fetch failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "Graph message fetch rejected ({status}): {}",
                text.chars().take(400).collect::<String>()
            );
        }
        let list: MessageList = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Graph message list was not JSON: {e}"))?;

        Ok(list
            .value
            .into_iter()
            .map(|m| {
                let from = m
                    .from
                    .and_then(|r| r.email_address)
                    .map(|e| e.address)
                    .unwrap_or_default();
                // Prefer the full body; fall back to the preview. HTML is
                // converted by the caller, which already has that helper.
                let body = match m.body {
                    Some(b) if !b.content.trim().is_empty() => {
                        if b.content_type.eq_ignore_ascii_case("html") {
                            format!("<html>{}", b.content)
                        } else {
                            b.content
                        }
                    }
                    _ => m.body_preview,
                };
                GraphMessage {
                    id: m.id,
                    subject: m.subject,
                    from,
                    body,
                }
            })
            .collect())
    }

    /// Mark a message read so it is not polled again.
    pub async fn mark_read(&self, message_id: &str) -> anyhow::Result<()> {
        let token = self.token().await?;
        let url = format!("{}/messages/{}", self.base(), message_id);
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({ "isRead": true }))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Graph mark-read failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Graph mark-read rejected ({status}): {}",
                text.chars().take(300).collect::<String>()
            );
        }
        Ok(())
    }

    /// Reply to a message, keeping it on the original thread.
    pub async fn reply(&self, message_id: &str, body: &str) -> anyhow::Result<()> {
        let token = self.token().await?;
        let url = format!("{}/messages/{}/reply", self.base(), message_id);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({ "comment": body }))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Graph reply failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Graph reply rejected ({status}): {}",
                text.chars().take(300).collect::<String>()
            );
        }
        Ok(())
    }

    /// Send a new message to `to`.
    pub async fn send_mail(&self, to: &str, subject: &str, body: &str) -> anyhow::Result<()> {
        let token = self.token().await?;
        let url = format!("{}/sendMail", self.base());
        let payload = serde_json::json!({
            "message": {
                "subject": subject,
                "body": { "contentType": "Text", "content": body },
                "toRecipients": [{ "emailAddress": { "address": to } }]
            },
            "saveToSentItems": true
        });
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Graph sendMail failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Graph sendMail rejected ({status}): {}",
                text.chars().take(300).collect::<String>()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_configured_requires_all_fields() {
        let full = GraphMailClient::new("t".into(), "c".into(), "s".into(), "u@x.com".into());
        assert!(full.is_configured());

        let missing = GraphMailClient::new("t".into(), "c".into(), String::new(), "u@x.com".into());
        assert!(!missing.is_configured());

        let blank = GraphMailClient::new("  ".into(), "c".into(), "s".into(), "u@x.com".into());
        assert!(!blank.is_configured());
    }

    #[test]
    fn base_url_targets_the_configured_mailbox() {
        let c = GraphMailClient::new("t".into(), "c".into(), "s".into(), "info@contoso.com".into());
        assert_eq!(
            c.base(),
            "https://graph.microsoft.com/v1.0/users/info@contoso.com"
        );
    }
}

// ─────────────────────────────────────────────
// Diagnostics
// ─────────────────────────────────────────────

/// Decode the payload of a JWT without verifying it.
///
/// Only used to read the `roles` claim of our own freshly-issued token, to
/// tell "the tenant never granted the permission" apart from "the permission
/// is granted but this mailbox is out of scope". Both surface as a bare 403.
fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    // JWTs use base64url without padding.
    let mut b64: String = payload.replace('-', "+").replace('_', "/");
    while b64.len() % 4 != 0 {
        b64.push('=');
    }
    let bytes = base64_decode(&b64)?;
    serde_json::from_slice(&bytes).ok()
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        if chunk.len() < 4 {
            return None;
        }
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        let mut n = 0u32;
        for &c in chunk {
            let v = if c == b'=' { 0 } else { val(c)? };
            n = (n << 6) | v;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

impl GraphMailClient {
    /// Work out *why* the mailbox is unreachable, rather than reporting the
    /// first error verbatim.
    ///
    /// A Graph app-only setup fails at one of four distinct points, and all
    /// but the first look identical from the outside (`403
    /// ErrorAccessDenied`): bad credentials, permission never consented,
    /// mailbox does not exist, or the app is scoped away from this mailbox
    /// by an Application Access Policy / RBAC assignment. Each needs a
    /// different fix, so each is reported separately.
    pub async fn diagnose(&self, folder: &str) -> Result<String, String> {
        if !self.is_configured() {
            return Err("Graph is selected but tenant id / client id / secret / mailbox are not all filled in.".into());
        }

        // 1. Credentials.
        let token = self.token().await.map_err(|e| {
            format!(
                "Could not get a token — the tenant id, client id or client secret is wrong, \
                 or the secret has expired.\n\nAzure said: {e}"
            )
        })?;
        let mut report = String::from("✓ Token acquired (tenant id, client id and secret are valid).\n");

        // 2. Permissions actually granted to the app.
        let roles: Vec<String> = decode_jwt_payload(&token)
            .and_then(|v| v.get("roles").cloned())
            .and_then(|r| serde_json::from_value::<Vec<String>>(r).ok())
            .unwrap_or_default();
        if roles.is_empty() {
            return Err(format!(
                "{report}\n✗ The token carries NO application permissions.\n\n\
                 The permissions were requested but never granted admin consent, so Graph will \
                 refuse every call with 403 ErrorAccessDenied.\n\n\
                 Fix: an administrator must grant consent — Azure portal → App registrations → \
                 your app → API permissions → \"Grant admin consent\", or open:\n\
                 https://login.microsoftonline.com/{}/adminconsent?client_id={}",
                self.tenant_id, self.client_id
            ));
        }
        report.push_str(&format!("✓ Permissions granted: {}\n", roles.join(", ")));
        if !roles.iter().any(|r| r.starts_with("Mail.")) {
            report.push_str("  ! No Mail.* permission in the token — reading mail will fail.\n");
        }

        // 3. Does the mailbox exist / is it visible to this app?
        let url = format!("{}?$select=id,userPrincipalName,mail", self.base());
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| format!("{report}\n✗ Network error contacting Graph: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(format!(
                "{report}\n✗ Mailbox '{}' was not found in this tenant.\n\n\
                 Check the address is exactly right, and that it is a real mailbox rather than \
                 an alias or a distribution list.",
                self.user_id
            ));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(format!(
                "{report}\n✗ 403 ErrorAccessDenied reading mailbox '{}'.\n\n\
                 The permission IS granted, so this is mailbox scoping: the app is restricted to \
                 a set of mailboxes that does not include this one. That restriction is what the \
                 setup script creates to stop the app reading the whole tenant.\n\n\
                 Check which mailboxes it may access:\n\
                 \x20 Get-ManagementRoleAssignment -App {} | Format-List Name,Role,CustomResourceScope\n\
                 \x20 Test-ApplicationAccessPolicy -Identity {} -AppId {}\n\n\
                 Scoping changes can take up to ~30 minutes to take effect, so if you only just \
                 ran the setup, wait and try again.",
                self.user_id, self.client_id, self.user_id, self.client_id
            ));
        }
        if !status.is_success() {
            return Err(format!(
                "{report}\n✗ Graph returned {status} for the mailbox: {}",
                body.chars().take(300).collect::<String>()
            ));
        }
        report.push_str(&format!("✓ Mailbox '{}' is visible to the app.\n", self.user_id));

        // 4. The actual thing the channel does every poll.
        let folder = Self::graph_folder(folder);
        match self.fetch_unread(&folder, 1).await {
            Ok(msgs) => {
                report.push_str(&format!(
                    "✓ Folder '{folder}' readable — {} unread message(s) waiting.\n\nEverything works.",
                    msgs.len()
                ));
                Ok(report)
            }
            Err(e) => Err(format!(
                "{report}\n✗ Could not read folder '{folder}': {e}\n\n\
                 If the mailbox itself was visible above, check the folder name."
            )),
        }
    }
}

#[cfg(test)]
mod diag_tests {
    use super::*;

    #[test]
    fn imap_folder_names_map_to_graph_well_known_names() {
        // The config field is shared with IMAP, whose default is "INBOX".
        assert_eq!(GraphMailClient::graph_folder("INBOX"), "inbox");
        assert_eq!(GraphMailClient::graph_folder("Inbox"), "inbox");
        assert_eq!(GraphMailClient::graph_folder(""), "inbox");
        assert_eq!(GraphMailClient::graph_folder("Sent Items"), "sentitems");
        assert_eq!(GraphMailClient::graph_folder("Junk"), "junkemail");
        // Custom folders pass through untouched.
        assert_eq!(GraphMailClient::graph_folder("Invoices"), "Invoices");
    }

    #[test]
    fn base64_decode_matches_known_vectors() {
        assert_eq!(base64_decode("SGVsbG8=").unwrap(), b"Hello");
        assert_eq!(base64_decode("SGk=").unwrap(), b"Hi");
        assert_eq!(base64_decode("QUJD").unwrap(), b"ABC");
    }

    #[test]
    fn reads_roles_from_a_jwt_payload() {
        // header.payload.signature — only the payload is looked at, and the
        // signature is deliberately not verified (this is our own token).
        let payload = r#"{"aud":"https://graph.microsoft.com","roles":["Mail.ReadWrite","Mail.Send"]}"#;
        let b64 = {
            const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let data = payload.as_bytes();
            let mut out = String::new();
            for chunk in data.chunks(3) {
                let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                out.push(T[(n >> 18) as usize & 63] as char);
                out.push(T[(n >> 12) as usize & 63] as char);
                out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
                out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
            }
            out
        };
        let token = format!("header.{b64}.signature");
        let claims = decode_jwt_payload(&token).expect("payload should decode");
        let roles: Vec<String> =
            serde_json::from_value(claims.get("roles").unwrap().clone()).unwrap();
        assert_eq!(roles, vec!["Mail.ReadWrite", "Mail.Send"]);
    }

    #[test]
    fn garbage_token_does_not_panic() {
        assert!(decode_jwt_payload("not-a-jwt").is_none());
        assert!(decode_jwt_payload("a.!!!.c").is_none());
    }
}
