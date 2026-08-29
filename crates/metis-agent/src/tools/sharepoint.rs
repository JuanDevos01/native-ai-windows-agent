//! SharePoint document tools via Microsoft Graph.
//!
//! Scoping is the whole design constraint here. The obvious permission,
//! `Sites.Read.All`, means "read documents and list items in all site
//! collections" — every site in the tenant. `Sites.Selected` instead grants
//! nothing at all until an administrator authorizes a specific site, which is
//! the SharePoint counterpart of the mailbox scoping already applied to
//! email. So these tools are opt-in, take an explicit list of sites from
//! config, and treat a 403 as "this site was never granted" rather than as an
//! opaque failure.
//!
//! Credentials default to the same Azure app used for email, so the client
//! secret is stored once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::base::{require_string, Tool};

/// Everything the SharePoint tools need to talk to Graph.
#[derive(Clone, Debug, Default)]
pub struct SharePointSettings {
    pub enabled: bool,
    pub sites: Vec<String>,
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret: String,
}

impl SharePointSettings {
    /// Usable only when switched on, given at least one site, and holding a
    /// full set of credentials.
    pub fn is_usable(&self) -> bool {
        self.enabled
            && !self.sites.iter().any(|s| s.trim().is_empty())
            && !self.sites.is_empty()
            && !self.tenant_id.trim().is_empty()
            && !self.client_id.trim().is_empty()
            && !self.client_secret.trim().is_empty()
    }
}

/// A token plus the instant it stops being valid.
struct CachedToken {
    value: String,
    expires_at: Instant,
}

/// Shared Graph plumbing: token caching and site-id resolution.
pub struct GraphSites {
    settings: SharePointSettings,
    http: reqwest::Client,
    token: Mutex<Option<CachedToken>>,
    /// `host:/sites/Name` → Graph site id. Site ids never change, so this is
    /// cached for the life of the process.
    site_ids: Mutex<HashMap<String, String>>,
}

impl GraphSites {
    pub fn new(settings: SharePointSettings) -> Self {
        Self {
            settings,
            http: reqwest::Client::new(),
            token: Mutex::new(None),
            site_ids: Mutex::new(HashMap::new()),
        }
    }

    async fn token(&self) -> anyhow::Result<String> {
        {
            let guard = self.token.lock().await;
            if let Some(t) = guard.as_ref() {
                if t.expires_at > Instant::now() {
                    return Ok(t.value.clone());
                }
            }
        }

        let url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.settings.tenant_id.trim()
        );
        let params = [
            ("client_id", self.settings.client_id.trim()),
            ("client_secret", self.settings.client_secret.trim()),
            ("scope", "https://graph.microsoft.com/.default"),
            ("grant_type", "client_credentials"),
        ];
        let resp = self.http.post(&url).form(&params).send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "Could not get a Microsoft token ({status}). The tenant id, client id or client \
                 secret is wrong, or the secret expired. Azure said: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&body)?;
        let access = v
            .get("access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("Microsoft returned no access_token"))?
            .to_string();
        let expires_in = v.get("expires_in").and_then(|e| e.as_u64()).unwrap_or(3600);
        // Renew a minute early so a call cannot start on a token that expires
        // mid-flight.
        let expires_at = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60).max(60));
        *self.token.lock().await = Some(CachedToken {
            value: access.clone(),
            expires_at,
        });
        Ok(access)
    }

    /// Pick the site to act on: the one named, or the first configured.
    fn choose_site(&self, requested: Option<&str>) -> anyhow::Result<String> {
        let Some(want) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
            return self
                .settings
                .sites
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("No SharePoint sites are configured."));
        };
        // Accept either the exact configured path or a bare site name.
        let lower = want.to_lowercase();
        for s in &self.settings.sites {
            let sl = s.to_lowercase();
            if sl == lower || sl.ends_with(&format!("/{lower}")) {
                return Ok(s.clone());
            }
        }
        anyhow::bail!(
            "'{want}' is not one of the SharePoint sites this agent may read. Configured sites: \
             {}. Sites must be added to config and authorized in Azure before they can be used.",
            self.settings.sites.join(", ")
        )
    }

    /// Resolve `host:/sites/Name` to the Graph site id.
    async fn site_id(&self, site: &str) -> anyhow::Result<String> {
        if let Some(id) = self.site_ids.lock().await.get(site) {
            return Ok(id.clone());
        }
        let token = self.token().await?;
        let url = format!("https://graph.microsoft.com/v1.0/sites/{site}");
        let resp = self.http.get(&url).bearer_auth(&token).send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!("{}", access_denied_help(site));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "SharePoint site '{site}' does not exist. Expected the form \
                 host:/sites/Name, for example contoso.sharepoint.com:/sites/Finance."
            );
        }
        if !status.is_success() {
            anyhow::bail!(
                "Graph returned {status} for site '{site}': {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&body)?;
        let id = v
            .get("id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| anyhow::anyhow!("Graph returned no id for site '{site}'"))?
            .to_string();
        self.site_ids
            .lock()
            .await
            .insert(site.to_string(), id.clone());
        Ok(id)
    }
}

/// A 403 under `Sites.Selected` almost always means the site was never
/// granted, which is a one-command fix — so say so instead of surfacing the
/// bare status.
fn access_denied_help(site: &str) -> String {
    format!(
        "Access denied to SharePoint site '{site}'.\n\n\
         Under the Sites.Selected permission an app can read nothing until an administrator \
         authorizes each site individually. That is deliberate: it is what stops this app from \
         reading every site in the tenant.\n\n\
         Fix: an administrator runs the setup script with\n\
         \x20 -GrantSite {site}\n\
         Grants can take a few minutes to take effect."
    )
}

/// Format a Graph driveItem for display.
fn format_item(item: &Value) -> String {
    let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("(unnamed)");
    let url = item.get("webUrl").and_then(|u| u.as_str()).unwrap_or("");
    let id = item.get("id").and_then(|i| i.as_str()).unwrap_or("");
    let modified = item
        .get("lastModifiedDateTime")
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let size = item.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
    let kind = if item.get("folder").is_some() { "folder" } else { "file" };
    let mut line = format!("{name}  [{kind}");
    if size > 0 {
        line.push_str(&format!(", {}", human_size(size)));
    }
    if !modified.is_empty() {
        line.push_str(&format!(", modified {modified}"));
    }
    line.push(']');
    if !id.is_empty() {
        line.push_str(&format!("\n  id: {id}"));
    }
    if !url.is_empty() {
        line.push_str(&format!("\n  {url}"));
    }
    line
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ─────────────────────────────────────────────
// sharepoint_search
// ─────────────────────────────────────────────

/// Searches a SharePoint document library by text.
pub struct SharePointSearchTool {
    graph: Arc<GraphSites>,
}

impl SharePointSearchTool {
    pub fn new(graph: Arc<GraphSites>) -> Self {
        Self { graph }
    }
}

#[async_trait]
impl Tool for SharePointSearchTool {
    fn name(&self) -> &str {
        "sharepoint_search"
    }

    fn description(&self) -> &str {
        "Search documents in SharePoint. Searches filenames and document contents in the site's \
         document library. Returns names, sizes and item ids; use sharepoint_download to fetch a \
         file before reading it."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to look for, e.g. 'invoice 4711' or 'annual report'"
                },
                "site": {
                    "type": "string",
                    "description": "Which configured site to search. Omit to use the default site."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum results (default 25)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let query = require_string(&params, "query")?;
        let query = query.trim();
        if query.is_empty() {
            anyhow::bail!("Give something to search for.");
        }
        let site = self
            .graph
            .choose_site(params.get("site").and_then(|v| v.as_str()))?;
        let top = params
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(25)
            .clamp(1, 100);

        let site_id = self.graph.site_id(&site).await?;
        let token = self.graph.token().await?;
        // Graph wants the search term single-quoted inside the function call,
        // with embedded quotes doubled.
        let escaped = query.replace('\'', "''");
        let url = format!(
            "https://graph.microsoft.com/v1.0/sites/{site_id}/drive/root/search(q='{}')?$top={top}\
             &$select=id,name,webUrl,size,lastModifiedDateTime,folder",
            urlencode(&escaped)
        );
        let resp = self.http_get(&url, &token).await?;
        let (status, body) = resp;

        if status == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!("{}", access_denied_help(&site));
        }
        if !status.is_success() {
            anyhow::bail!(
                "SharePoint search failed ({status}): {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&body)?;
        let items: Vec<&Value> = v
            .get("value")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();

        if items.is_empty() {
            return Ok(format!("No documents matching '{query}' in {site}."));
        }
        let mut out = format!("{} result(s) in {site}:\n\n", items.len());
        for item in items {
            out.push_str(&format_item(item));
            out.push_str("\n\n");
        }
        Ok(out.trim_end().to_string())
    }
}

impl SharePointSearchTool {
    async fn http_get(
        &self,
        url: &str,
        token: &str,
    ) -> anyhow::Result<(reqwest::StatusCode, String)> {
        let resp = self.graph.http.get(url).bearer_auth(token).send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }
}

/// Percent-encode the characters Graph's URL parser cares about. Kept local
/// rather than pulling in a dependency for one call site.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'\'' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ─────────────────────────────────────────────
// sharepoint_download
// ─────────────────────────────────────────────

/// Downloads one SharePoint document into the workspace.
pub struct SharePointDownloadTool {
    graph: Arc<GraphSites>,
    workspace: PathBuf,
}

impl SharePointDownloadTool {
    pub fn new(graph: Arc<GraphSites>, workspace: PathBuf) -> Self {
        Self { graph, workspace }
    }
}

/// Refuse anything big enough to be a problem to hold in memory or to sit in
/// the workspace unnoticed.
const MAX_DOWNLOAD_BYTES: u64 = 100 * 1024 * 1024;

#[async_trait]
impl Tool for SharePointDownloadTool {
    fn name(&self) -> &str {
        "sharepoint_download"
    }

    fn description(&self) -> &str {
        "Download a SharePoint document to the local workspace so it can be read. Takes the item \
         id from sharepoint_search. Returns the local path — then use read_pdf for PDFs, \
         analyze_image for images, or read_file for text."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "item_id": {
                    "type": "string",
                    "description": "The item id reported by sharepoint_search"
                },
                "site": {
                    "type": "string",
                    "description": "Which configured site the item is in. Omit to use the default site."
                },
                "save_as": {
                    "type": "string",
                    "description": "Optional filename to save as, inside the workspace downloads folder"
                }
            },
            "required": ["item_id"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let item_id = require_string(&params, "item_id")?;
        let item_id = item_id.trim();
        if item_id.is_empty() {
            anyhow::bail!("Give the item_id from sharepoint_search.");
        }
        let site = self
            .graph
            .choose_site(params.get("site").and_then(|v| v.as_str()))?;
        let site_id = self.graph.site_id(&site).await?;
        let token = self.graph.token().await?;

        // Metadata first, so size and name are known before downloading.
        let meta_url = format!(
            "https://graph.microsoft.com/v1.0/sites/{site_id}/drive/items/{item_id}\
             ?$select=id,name,size,file"
        );
        let resp = self
            .graph
            .http
            .get(&meta_url)
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!("{}", access_denied_help(&site));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("No document with id '{item_id}' in {site}.");
        }
        if !status.is_success() {
            anyhow::bail!(
                "Could not read document metadata ({status}): {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let meta: Value = serde_json::from_str(&body)?;
        let remote_name = meta
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("download.bin");
        let size = meta.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        if size > MAX_DOWNLOAD_BYTES {
            anyhow::bail!(
                "'{remote_name}' is {} — too large to download automatically (limit {}).",
                human_size(size),
                human_size(MAX_DOWNLOAD_BYTES)
            );
        }

        let requested = params
            .get("save_as")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(remote_name);
        // Take only the final component: a name from a remote system must not
        // be able to steer the write out of the downloads folder.
        let safe_name = sanitize_filename(requested);

        let dir = self.workspace.join("sharepoint");
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(&safe_name);

        let content_url = format!(
            "https://graph.microsoft.com/v1.0/sites/{site_id}/drive/items/{item_id}/content"
        );
        let resp = self
            .graph
            .http
            .get(&content_url)
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!("{}", access_denied_help(&site));
        }
        if !status.is_success() {
            anyhow::bail!("Download failed ({status}) for '{remote_name}'.");
        }
        let bytes = resp.bytes().await?;
        std::fs::write(&dest, &bytes)?;

        Ok(format!(
            "Downloaded '{remote_name}' ({}) to {}\n\nRead it with read_pdf (PDF), analyze_image \
             (image) or read_file (text).",
            human_size(bytes.len() as u64),
            dest.display()
        ))
    }
}

/// Reduce a remote filename to a safe single path component.
fn sanitize_filename(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("download.bin")
        .trim();
    let cleaned: String = base
        .chars()
        .filter(|c| !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'))
        .collect();
    let cleaned = cleaned.trim_matches('.').trim().to_string();
    if cleaned.is_empty() {
        "download.bin".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(sites: &[&str]) -> SharePointSettings {
        SharePointSettings {
            enabled: true,
            sites: sites.iter().map(|s| s.to_string()).collect(),
            tenant_id: "t".into(),
            client_id: "c".into(),
            client_secret: "s".into(),
        }
    }

    #[test]
    fn settings_need_everything_to_be_usable() {
        assert!(settings(&["host:/sites/Finance"]).is_usable());

        let mut off = settings(&["host:/sites/Finance"]);
        off.enabled = false;
        assert!(!off.is_usable(), "disabled must not be usable");

        let mut nosites = settings(&[]);
        nosites.sites.clear();
        assert!(!nosites.is_usable(), "no sites means nothing to read");

        let mut nosecret = settings(&["host:/sites/Finance"]);
        nosecret.client_secret = "  ".into();
        assert!(!nosecret.is_usable(), "blank secret must not be usable");
    }

    #[test]
    fn default_site_is_the_first_configured() {
        let g = GraphSites::new(settings(&["host:/sites/Finance", "host:/sites/HR"]));
        assert_eq!(g.choose_site(None).unwrap(), "host:/sites/Finance");
        assert_eq!(g.choose_site(Some("  ")).unwrap(), "host:/sites/Finance");
    }

    #[test]
    fn a_site_can_be_named_by_its_short_name() {
        let g = GraphSites::new(settings(&["host:/sites/Finance", "host:/sites/HR"]));
        assert_eq!(g.choose_site(Some("HR")).unwrap(), "host:/sites/HR");
        assert_eq!(g.choose_site(Some("hr")).unwrap(), "host:/sites/HR");
        assert_eq!(
            g.choose_site(Some("host:/sites/HR")).unwrap(),
            "host:/sites/HR"
        );
    }

    #[test]
    fn unconfigured_sites_are_refused_locally() {
        // The scoping is enforced in Azure, but refusing here gives a useful
        // message instead of a 403 and avoids a pointless round trip.
        let g = GraphSites::new(settings(&["host:/sites/Finance"]));
        let err = g.choose_site(Some("host:/sites/Secret")).unwrap_err().to_string();
        assert!(err.contains("not one of"), "{err}");
        assert!(err.contains("host:/sites/Finance"), "should list what is allowed: {err}");
    }

    #[test]
    fn access_denied_names_the_repair_command() {
        let msg = access_denied_help("host:/sites/Finance");
        assert!(msg.contains("-GrantSite host:/sites/Finance"), "{msg}");
    }

    #[test]
    fn remote_filenames_cannot_escape_the_download_folder() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(r"..\..\windows\system32\a.dll"), "a.dll");
        assert_eq!(sanitize_filename("C:/tmp/report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("  "), "download.bin");
        assert_eq!(sanitize_filename("..."), "download.bin");
        assert_eq!(sanitize_filename("normal report.xlsx"), "normal report.xlsx");
    }

    #[test]
    fn search_terms_are_escaped_for_the_graph_url() {
        assert_eq!(urlencode("annual report"), "annual%20report");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("plain"), "plain");
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
