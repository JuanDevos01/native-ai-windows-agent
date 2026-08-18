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

    /// Fetch unread messages from the given folder (default Inbox).
    pub async fn fetch_unread(&self, folder: &str, limit: u32) -> anyhow::Result<Vec<GraphMessage>> {
        let token = self.token().await?;
        let folder = if folder.trim().is_empty() { "Inbox" } else { folder.trim() };
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
