//! Browser tool — local headless browsing without API keys.
//!
//! Phase 2 actions:
//! - `open`: open/navigate a stateful session tab
//! - `extract_text`: read rendered text (optionally from selector)
//! - `screenshot`: capture PNG screenshot
//! - `click`: click a CSS selector
//! - `type`: type text into a CSS selector
//! - `wait_for`: wait until selector appears
//! - `close`: close a browser session

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use headless_chrome::{Browser, LaunchOptionsBuilder, Tab};
use serde_json::{json, Value};

use super::base::{optional_i64, optional_string, require_string, Tool};

struct BrowserSession {
    _browser: Browser,
    tab: Arc<Tab>,
}

/// Browser automation tool backed by a local Chromium/Brave process.
pub struct BrowserTool {
    workspace: PathBuf,
    restrict_to_workspace: bool,
    sessions: Arc<Mutex<HashMap<String, BrowserSession>>>,
}

impl BrowserTool {
    pub fn new(workspace: PathBuf, restrict_to_workspace: bool) -> Self {
        Self {
            workspace,
            restrict_to_workspace,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn launch_browser() -> anyhow::Result<Browser> {
        let mut builder = LaunchOptionsBuilder::default();
        builder
            .headless(true)
            .sandbox(false)
            .window_size(Some((1366, 900)));

        // Optional override for local environments (e.g. Brave on Windows).
        if let Ok(path) = std::env::var("METIS_BROWSER_EXECUTABLE") {
            if !path.trim().is_empty() {
                builder.path(Some(PathBuf::from(path.trim())));
            }
        }

        let options = builder
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build browser launch options: {e}"))?;
        Browser::new(options).map_err(|e| anyhow::anyhow!("failed to launch local browser: {e}"))
    }

    fn validate_url(url: &str) -> anyhow::Result<()> {
        if url.starts_with("http://") || url.starts_with("https://") {
            Ok(())
        } else {
            anyhow::bail!("Invalid URL: must start with http:// or https://")
        }
    }

    fn resolve_output_path(
        workspace: &Path,
        restrict_to_workspace: bool,
        path_arg: Option<String>,
    ) -> anyhow::Result<PathBuf> {
        let p = match path_arg {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => workspace.join("browser_screenshot.png"),
        };
        let resolved = if p.is_absolute() { p } else { workspace.join(p) };

        if restrict_to_workspace && !resolved.starts_with(workspace) {
            anyhow::bail!("Screenshot path must be inside workspace in restricted mode");
        }
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(resolved)
    }

    fn extract_with_selector(tab: &Tab, selector: &str) -> anyhow::Result<String> {
        let sel_json = serde_json::to_string(selector)
            .map_err(|e| anyhow::anyhow!("failed to encode selector: {e}"))?;
        let js = format!(
            "(function() {{ const el = document.querySelector({sel}); return el ? (el.innerText || el.textContent || '') : ''; }})()",
            sel = sel_json
        );
        let value = tab
            .evaluate(&js, false)
            .map_err(|e| anyhow::anyhow!("selector evaluation failed: {e}"))?
            .value
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        Ok(value)
    }

    fn extract_page_text(tab: &Tab) -> anyhow::Result<String> {
        let value = tab
            .evaluate("document.body ? (document.body.innerText || '') : ''", false)
            .map_err(|e| anyhow::anyhow!("text extraction failed: {e}"))?
            .value
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        Ok(value)
    }

    fn ensure_session<'a>(
        sessions: &'a mut HashMap<String, BrowserSession>,
        session_id: &str,
    ) -> anyhow::Result<&'a mut BrowserSession> {
        if !sessions.contains_key(session_id) {
            let browser = Self::launch_browser()?;
            let tab = browser
                .new_tab()
                .map_err(|e| anyhow::anyhow!("failed to create browser tab: {e}"))?;
            sessions.insert(
                session_id.to_string(),
                BrowserSession {
                    _browser: browser,
                    tab,
                },
            );
        }
        sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("failed to create or fetch session '{session_id}'"))
    }

    fn run_action(
        sessions: &mut HashMap<String, BrowserSession>,
        workspace: &Path,
        restrict_to_workspace: bool,
        action: &str,
        session_id: &str,
        url: Option<String>,
        selector: Option<String>,
        text: Option<String>,
        path: Option<String>,
        timeout_ms: i64,
    ) -> anyhow::Result<String> {
        if !matches!(
            action,
            "open" | "extract_text" | "screenshot" | "click" | "type" | "wait_for" | "close"
        ) {
            anyhow::bail!("Unknown action: {action}");
        }

        match action {
            "close" => {
                let removed = sessions.remove(session_id).is_some();
                return Ok(
                    serde_json::to_string_pretty(&json!({ "ok": true, "session": session_id, "closed": removed }))
                        .unwrap_or_default(),
                );
            }
            "open" => {
                let url = url.ok_or_else(|| anyhow::anyhow!("'url' is required for action=open"))?;
                Self::validate_url(&url)?;
                let session = Self::ensure_session(sessions, session_id)?;
                session
                    .tab
                    .navigate_to(&url)
                    .map_err(|e| anyhow::anyhow!("navigation failed: {e}"))?;
                session
                    .tab
                    .wait_until_navigated()
                    .map_err(|e| anyhow::anyhow!("navigation wait failed: {e}"))?;
                let title = session.tab.get_title().unwrap_or_default();
                return Ok(
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "session": session_id,
                        "url": url,
                        "title": title
                    }))
                    .unwrap_or_default(),
                );
            }
            _ => {}
        }

        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("session '{session_id}' not found. Call action=open first."))?;

        if let Some(url) = url {
            Self::validate_url(&url)?;
            session
                .tab
                .navigate_to(&url)
                .map_err(|e| anyhow::anyhow!("navigation failed: {e}"))?;
            session
                .tab
                .wait_until_navigated()
                .map_err(|e| anyhow::anyhow!("navigation wait failed: {e}"))?;
        }

        match action {
            "extract_text" => {
                let text = if let Some(sel) = selector {
                    if sel.trim().is_empty() {
                        Self::extract_page_text(&session.tab)?
                    } else {
                        Self::extract_with_selector(&session.tab, sel.trim())?
                    }
                } else {
                    Self::extract_page_text(&session.tab)?
                };
                Ok(serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "session": session_id,
                    "length": text.len(),
                    "text": text
                }))
                .unwrap_or_default())
            }
            "screenshot" => {
                let out = Self::resolve_output_path(workspace, restrict_to_workspace, path)?;
                let png = session
                    .tab
                    .capture_screenshot(
                        headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption::Png,
                        None,
                        None,
                        true,
                    )
                    .map_err(|e| anyhow::anyhow!("failed to capture screenshot: {e}"))?;
                std::fs::write(&out, png)
                    .map_err(|e| anyhow::anyhow!("failed to write screenshot: {e}"))?;
                Ok(serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "session": session_id,
                    "path": out.display().to_string()
                }))
                .unwrap_or_default())
            }
            "click" => {
                let sel = selector.ok_or_else(|| anyhow::anyhow!("'selector' is required for action=click"))?;
                let elem = session
                    .tab
                    .wait_for_element(&sel)
                    .map_err(|e| anyhow::anyhow!("selector not found: {e}"))?;
                elem.click()
                    .map_err(|e| anyhow::anyhow!("click failed: {e}"))?;
                Ok(serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "session": session_id,
                    "action": "click",
                    "selector": sel
                }))
                .unwrap_or_default())
            }
            "type" => {
                let sel = selector.ok_or_else(|| anyhow::anyhow!("'selector' is required for action=type"))?;
                let text = text.ok_or_else(|| anyhow::anyhow!("'text' is required for action=type"))?;
                let elem = session
                    .tab
                    .wait_for_element(&sel)
                    .map_err(|e| anyhow::anyhow!("selector not found: {e}"))?;
                elem.click()
                    .map_err(|e| anyhow::anyhow!("click before type failed: {e}"))?;
                elem.type_into(&text)
                    .map_err(|e| anyhow::anyhow!("type failed: {e}"))?;
                Ok(serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "session": session_id,
                    "action": "type",
                    "selector": sel,
                    "typedChars": text.len()
                }))
                .unwrap_or_default())
            }
            "wait_for" => {
                let sel = selector.ok_or_else(|| anyhow::anyhow!("'selector' is required for action=wait_for"))?;
                let timeout_ms = timeout_ms.clamp(100, 120_000) as u64;
                session
                    .tab
                    .wait_for_element_with_custom_timeout(&sel, Duration::from_millis(timeout_ms))
                    .map_err(|e| anyhow::anyhow!("wait_for failed: {e}"))?;
                Ok(serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "session": session_id,
                    "action": "wait_for",
                    "selector": sel,
                    "timeoutMs": timeout_ms
                }))
                .unwrap_or_default())
            }
            _ => anyhow::bail!("Unknown action: {action}"),
        }
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Control a local headless browser (no API key). Actions: open, extract_text, screenshot, click, type, wait_for, close."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["open", "extract_text", "screenshot", "click", "type", "wait_for", "close"],
                    "description": "Browser action to run"
                },
                "session": {
                    "type": "string",
                    "description": "Session id for stateful browsing (default: default)"
                },
                "url": {
                    "type": "string",
                    "description": "HTTP/HTTPS URL (required for open; optional for others)"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector (required for click/type/wait_for; optional for extract_text)"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type (required for type)"
                },
                "path": {
                    "type": "string",
                    "description": "Output path for screenshot PNG"
                },
                "timeoutMs": {
                    "type": "integer",
                    "description": "wait_for timeout in milliseconds (default 10000)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let action = require_string(&params, "action")?;
        let session_id = optional_string(&params, "session")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "default".to_string());
        let url = optional_string(&params, "url");
        let selector = optional_string(&params, "selector");
        let text = optional_string(&params, "text");
        let path = optional_string(&params, "path");
        let timeout_ms = optional_i64(&params, "timeoutMs").unwrap_or(10_000);

        let sessions = self.sessions.clone();
        let workspace = self.workspace.clone();
        let restrict = self.restrict_to_workspace;

        tokio::task::spawn_blocking(move || {
            let mut guard = sessions
                .lock()
                .map_err(|_| anyhow::anyhow!("browser session lock poisoned"))?;
            Self::run_action(
                &mut guard,
                &workspace,
                restrict,
                &action,
                &session_id,
                url,
                selector,
                text,
                path,
                timeout_ms,
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("browser worker failed: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_name() {
        let tool = BrowserTool::new(Path::new(".").to_path_buf(), false);
        assert_eq!(tool.to_definition().function.name, "browser");
    }

    #[tokio::test]
    async fn rejects_unknown_action() {
        let tool = BrowserTool::new(Path::new(".").to_path_buf(), false);
        let mut params = HashMap::new();
        params.insert("action".into(), json!("nope"));
        let err = tool.execute(params).await.unwrap_err();
        assert!(err.to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn open_requires_url() {
        let tool = BrowserTool::new(Path::new(".").to_path_buf(), false);
        let mut params = HashMap::new();
        params.insert("action".into(), json!("open"));
        let err = tool.execute(params).await.unwrap_err();
        assert!(err.to_string().contains("'url' is required"));
    }

    #[tokio::test]
    async fn rejects_invalid_url() {
        let tool = BrowserTool::new(Path::new(".").to_path_buf(), false);
        let mut params = HashMap::new();
        params.insert("action".into(), json!("open"));
        params.insert("url".into(), json!("example.com"));
        let err = tool.execute(params).await.unwrap_err();
        assert!(err.to_string().contains("Invalid URL"));
    }
}
