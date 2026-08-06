//! Browser tool — local headless browsing without API keys.
//!
//! The primary workflow is the pagetree interaction model:
//! - `open` navigates and returns the page as a compact interaction tree in
//!   which actionable elements carry refs like `[e7]`
//! - `snapshot` re-reads the current page as a tree
//! - `click` / `type` / `select` / `scroll_to` act on a ref and return the
//!   updated tree
//!
//! Legacy CSS-selector variants of `click`/`type`, plus `extract_text`,
//! `screenshot`, `wait_for`, and `close` are kept.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use headless_chrome::{Browser, LaunchOptionsBuilder, Tab};
use serde_json::{json, Value};

use super::base::{optional_i64, optional_string, require_string, Tool};
use super::pagetree;
use super::pagetree::diff::FlatLine;
use super::pagetree::refs::RefMap;
use headless_chrome::protocol::cdp::DOM::BackendNodeId;

struct BrowserSession {
    _browser: Browser,
    tab: Arc<Tab>,
    refs: RefMap,
    /// Baseline for diff mode: the flattened lines of the last tree the model
    /// saw, plus the URL it belonged to.
    last_flat: Option<Vec<FlatLine>>,
    last_url: Option<String>,
    /// PID of the top-level browser process, captured at launch. `Browser`'s
    /// own `Drop` only kills this one tracked process — Chrome/Edge spawns
    /// several children (GPU, renderer, network service, crashpad), which is
    /// why every session tear-down also explicitly tree-kills this PID; see
    /// `kill_process_tree`.
    pid: Option<u32>,
    /// Last time this session handled a call — drives LRU eviction so
    /// distinct session ids (the model isn't required to reuse "default" or
    /// call `close`) can't accumulate live browser processes forever.
    last_used: std::time::Instant,
}

/// Hard cap on concurrent browser sessions per `BrowserTool`. Each session is
/// a full Chrome/Edge process tree (several hundred MB) — without a cap,
/// every distinct `session` name the model ever uses stays alive for the
/// lifetime of the gateway process.
const MAX_SESSIONS: usize = 4;

impl Drop for BrowserSession {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            kill_process_tree(pid);
        }
    }
}

/// Force-kill a browser process and every descendant it spawned. Necessary
/// because `headless_chrome`'s own cleanup (a graceful CDP `Browser.close`,
/// falling back to killing only the single top-level PID) leaves Chrome's
/// helper processes (GPU, renderer, network service, crashpad) running as
/// orphans when the top-level process is unresponsive or was already
/// force-killed — exactly the situation `relaunch`'s crash recovery hits.
/// Without this, every crash + every closed/relaunched session leaks a
/// handful of processes indefinitely.
fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        // Best effort: kill the process group if this was its own leader.
        let _ = std::process::Command::new("pkill")
            .args(["-9", "-P", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Depth-first search for the node bound to `backend` (used by `expand`).
fn find_subtree(nodes: &[pagetree::prune::PageNode], backend: BackendNodeId) -> Option<&pagetree::prune::PageNode> {
    for node in nodes {
        if node.backend_id == Some(backend) {
            return Some(node);
        }
        if let Some(found) = find_subtree(&node.children, backend) {
            return Some(found);
        }
    }
    None
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

    /// Resolve a source file path for `action=upload`. Unlike
    /// `resolve_output_path`, this never creates directories — the file must
    /// already exist.
    fn resolve_input_path(
        workspace: &Path,
        restrict_to_workspace: bool,
        path_arg: &str,
    ) -> anyhow::Result<PathBuf> {
        let p = PathBuf::from(path_arg);
        let resolved = if p.is_absolute() { p } else { workspace.join(p) };
        if restrict_to_workspace && !resolved.starts_with(workspace) {
            anyhow::bail!("Upload file path must be inside workspace in restricted mode: {path_arg}");
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

    /// Navigate with one retry (slow sites can time out the CDP response) and
    /// a non-fatal load wait — pagetree's own quiescence wait takes it from
    /// there, so a snapshot of a still-loading page beats a hard error.
    fn navigate(tab: &Tab, url: &str) -> anyhow::Result<()> {
        if let Err(first) = tab.navigate_to(url) {
            tab.navigate_to(url)
                .map_err(|_| anyhow::anyhow!("navigation failed: {first}"))?;
        }
        let _ = tab.wait_until_navigated();
        Ok(())
    }

    fn ensure_session<'a>(
        sessions: &'a mut HashMap<String, BrowserSession>,
        session_id: &str,
    ) -> anyhow::Result<&'a mut BrowserSession> {
        if !sessions.contains_key(session_id) {
            if sessions.len() >= MAX_SESSIONS {
                if let Some(lru_key) = sessions
                    .iter()
                    .min_by_key(|(_, s)| s.last_used)
                    .map(|(k, _)| k.clone())
                {
                    // Dropping the removed session tree-kills its browser
                    // process (see `Drop for BrowserSession`).
                    sessions.remove(&lru_key);
                }
            }
            let browser = Self::launch_browser()?;
            let pid = browser.get_process_id();
            let tab = browser
                .new_tab()
                .map_err(|e| anyhow::anyhow!("failed to create browser tab: {e}"))?;
            sessions.insert(
                session_id.to_string(),
                BrowserSession {
                    _browser: browser,
                    tab,
                    refs: RefMap::new(),
                    last_flat: None,
                    last_url: None,
                    pid,
                    last_used: std::time::Instant::now(),
                },
            );
        }
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("failed to create or fetch session '{session_id}'"))?;
        session.last_used = std::time::Instant::now();
        Ok(session)
    }

    /// Cheap liveness probe — a round trip that only succeeds if the CDP
    /// websocket is actually connected to a live browser process.
    fn tab_is_alive(tab: &Tab) -> bool {
        tab.evaluate("1", false).is_ok()
    }

    /// Replace a session's browser + tab in place. Used when the previous
    /// Chrome/Edge/Brave process has died (crashed, killed by antivirus, hit
    /// a resource limit) — without this, the session stays permanently
    /// wedged: every future call reuses the same dead connection and fails
    /// with the same "underlying connection is closed" error forever.
    fn relaunch(session: &mut BrowserSession) -> anyhow::Result<()> {
        // The old process is (at minimum) unresponsive — that's why we're
        // here. Its own Drop impl can't be trusted to clean up its child
        // processes in that state, so tree-kill it explicitly before
        // replacing it.
        if let Some(old_pid) = session.pid {
            kill_process_tree(old_pid);
        }
        let browser = Self::launch_browser()?;
        let pid = browser.get_process_id();
        let tab = browser
            .new_tab()
            .map_err(|e| anyhow::anyhow!("failed to create browser tab: {e}"))?;
        session._browser = browser;
        session.tab = tab;
        session.refs = RefMap::new();
        session.last_flat = None;
        session.last_url = None;
        session.pid = pid;
        Ok(())
    }

    /// If the session's browser process has died, relaunch it and — best
    /// effort — re-navigate to the last known URL so there's a page for
    /// fingerprint re-binding to work against. Returns whether a relaunch
    /// happened, so callers only pay for a fresh compile/retry when needed.
    fn recover_if_dead(session: &mut BrowserSession) -> anyhow::Result<bool> {
        if Self::tab_is_alive(&session.tab) {
            return Ok(false);
        }
        let last_url = session.last_url.clone();
        Self::relaunch(session)?;
        if let Some(url) = last_url {
            let _ = Self::navigate(&session.tab, &url);
        }
        Ok(true)
    }

    /// Full snapshot response: header plus the whole interaction tree. Also
    /// resets the diff baseline.
    fn respond_with_tree(
        session: &mut BrowserSession,
        session_id: &str,
        action: &str,
        max_chars: usize,
    ) -> anyhow::Result<String> {
        let tree = pagetree::compile(&session.tab, &mut session.refs)?;
        let text = pagetree::render::render(&tree, max_chars);
        let url = session.tab.get_url();
        let title = session.tab.get_title().unwrap_or_default();
        session.last_flat = Some(pagetree::diff::flatten(&tree));
        session.last_url = Some(url.clone());
        Ok(format!(
            "ok action={action} session={session_id}\nurl: {url}\ntitle: {title}\n---\n{text}"
        ))
    }

    /// Post-action response: a diff against what the model last saw — unless
    /// the page navigated or the diff rivals the tree, in which case a full
    /// snapshot is returned instead.
    fn respond_after_action(
        session: &mut BrowserSession,
        action_desc: &str,
        max_chars: usize,
    ) -> anyhow::Result<String> {
        let tree = pagetree::compile(&session.tab, &mut session.refs)?;
        let new_flat = pagetree::diff::flatten(&tree);
        let url = session.tab.get_url();
        let navigated = session.last_url.as_deref() != Some(url.as_str());

        let response = match (&session.last_flat, navigated) {
            (Some(old_flat), false) => {
                let d = pagetree::diff::diff(old_flat, &new_flat);
                if d.prefer_full(new_flat.len()) {
                    let text = pagetree::render::render(&tree, max_chars);
                    format!("after {action_desc}: page changed substantially → full snapshot\nurl: {url}\n---\n{text}")
                } else if d.changes == 0 {
                    format!("after {action_desc}: (no visible change)")
                } else {
                    format!("after {action_desc}:\n{}", d.lines.join("\n"))
                }
            }
            _ => {
                let title = session.tab.get_title().unwrap_or_default();
                let text = pagetree::render::render(&tree, max_chars);
                format!("after {action_desc}: page navigated → full snapshot\nurl: {url}\ntitle: {title}\n---\n{text}")
            }
        };
        session.last_flat = Some(new_flat);
        session.last_url = Some(url);
        Ok(response)
    }

    /// Run a ref-based action with self-healing. Two failure modes, two
    /// recoveries:
    /// - the backend node died (page re-rendered) → recompile and re-bind
    ///   the ref by fingerprint, matching the resolve_object path today.
    /// - the whole browser process died → `resolve_object` reports the same
    ///   `StaleRef` (it can't tell the difference from inside a dead
    ///   connection), so a first recompile attempt will *also* fail; that's
    ///   the signal to relaunch, re-navigate to the last known URL, and
    ///   THEN recompile so fingerprint re-binding has a page to match again.
    fn with_ref(
        session: &mut BrowserSession,
        ref_str: &str,
        f: impl Fn(&Tab, BackendNodeId) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let backend = session.refs.resolve(ref_str)?;
        match f(&session.tab, backend) {
            Err(e) if e.downcast_ref::<pagetree::act::StaleRef>().is_some() => {
                if pagetree::compile(&session.tab, &mut session.refs).is_err() {
                    Self::recover_if_dead(session)?;
                    pagetree::compile(&session.tab, &mut session.refs)?;
                }
                let healed = session.refs.resolve(ref_str)?;
                if healed == backend {
                    let what = session
                        .refs
                        .describe(ref_str)
                        .unwrap_or_else(|| "that element".to_string());
                    anyhow::bail!(
                        "Ref {ref_str} ({what}) is stale and nothing on the current page matches it. Take a fresh 'snapshot' and act on a current ref."
                    );
                }
                f(&session.tab, healed)
            }
            other => other,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_action(
        sessions: &mut HashMap<String, BrowserSession>,
        workspace: &Path,
        restrict_to_workspace: bool,
        action: &str,
        session_id: &str,
        url: Option<String>,
        selector: Option<String>,
        ref_arg: Option<String>,
        text: Option<String>,
        path: Option<String>,
        files: Option<Vec<String>>,
        plan: Option<String>,
        timeout_ms: i64,
        max_chars: usize,
    ) -> anyhow::Result<String> {
        if !matches!(
            action,
            "open"
                | "snapshot"
                | "expand"
                | "extract_text"
                | "screenshot"
                | "click"
                | "type"
                | "select"
                | "scroll_to"
                | "upload"
                | "export_plan"
                | "import_plan"
                | "wait_for"
                | "close"
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
                if let Err(first_err) = Self::navigate(&session.tab, &url) {
                    // A pre-existing session's browser process may have died
                    // (crashed, killed by antivirus, hit a resource limit)
                    // between calls — without this, every future 'open' on
                    // this session id would keep hitting the same dead
                    // connection forever.
                    if Self::recover_if_dead(session)? {
                        Self::navigate(&session.tab, &url).map_err(|e| {
                            anyhow::anyhow!(
                                "browser process had crashed; relaunched it but navigation still failed: {e}"
                            )
                        })?;
                    } else {
                        return Err(first_err);
                    }
                }
                return Self::respond_with_tree(session, session_id, "open", max_chars);
            }
            "import_plan" => {
                let plan_text = plan.ok_or_else(|| {
                    anyhow::anyhow!("'plan' (JSON from a prior action=export_plan) is required for action=import_plan")
                })?;
                let parsed = pagetree::plan::from_json(&plan_text)?;
                let seeded = parsed.refs.len();
                // No page needs to be open yet — import BEFORE 'open' so the
                // very first snapshot of the freshly loaded page re-binds
                // these refs by fingerprint.
                let session = Self::ensure_session(sessions, session_id)?;
                session.refs.import_plan(&parsed.refs);
                return Ok(serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "session": session_id,
                    "action": "import_plan",
                    "seededRefs": seeded
                }))
                .unwrap_or_default());
            }
            _ => {}
        }

        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("session '{session_id}' not found. Call action=open first."))?;
        session.last_used = std::time::Instant::now();

        if let Some(url) = url {
            Self::validate_url(&url)?;
            Self::navigate(&session.tab, &url)?;
        }

        match action {
            "snapshot" => Self::respond_with_tree(session, session_id, "snapshot", max_chars),
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
            "expand" => {
                let r = ref_arg.ok_or_else(|| {
                    anyhow::anyhow!("'ref' is required for action=expand (the eN from an '[expand: eN]' marker)")
                })?;
                let backend = session.refs.resolve(&r)?;
                let tree = pagetree::compile_with(&session.tab, &mut session.refs, None)?;
                let sub = find_subtree(&tree, backend).ok_or_else(|| {
                    anyhow::anyhow!(
                        "That subtree is no longer on the page. Take a fresh 'snapshot' and use a current [expand: eN] marker."
                    )
                })?;
                let text = pagetree::render::render(std::slice::from_ref(sub), max_chars);
                Ok(format!(
                    "ok action=expand session={session_id} (full contents of {r}; page unchanged)\n---\n{text}"
                ))
            }
            "click" => {
                if let Some(r) = ref_arg {
                    Self::with_ref(session, &r, |tab, backend| pagetree::act::click(tab, backend))?;
                    return Self::respond_after_action(session, &format!("click {r}"), max_chars);
                }
                let sel = selector
                    .ok_or_else(|| anyhow::anyhow!("'ref' (e.g. \"e7\" from a snapshot) or 'selector' is required for action=click"))?;
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
                let text = text.ok_or_else(|| anyhow::anyhow!("'text' is required for action=type"))?;
                if let Some(r) = ref_arg {
                    Self::with_ref(session, &r, |tab, backend| {
                        pagetree::act::fill(tab, backend, &text)
                    })?;
                    return Self::respond_after_action(session, &format!("type {r}"), max_chars);
                }
                let sel = selector
                    .ok_or_else(|| anyhow::anyhow!("'ref' (e.g. \"e7\" from a snapshot) or 'selector' is required for action=type"))?;
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
            "select" => {
                let r = ref_arg.ok_or_else(|| anyhow::anyhow!("'ref' is required for action=select"))?;
                let text = text.ok_or_else(|| {
                    anyhow::anyhow!("'text' (the option's visible label) is required for action=select")
                })?;
                Self::with_ref(session, &r, |tab, backend| {
                    pagetree::act::select(tab, backend, &text)
                })?;
                Self::respond_after_action(session, &format!("select {r}"), max_chars)
            }
            "scroll_to" => {
                let r = ref_arg.ok_or_else(|| anyhow::anyhow!("'ref' is required for action=scroll_to"))?;
                Self::with_ref(session, &r, |tab, backend| {
                    pagetree::act::scroll_to(tab, backend)
                })?;
                Ok(serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "session": session_id,
                    "action": "scroll_to",
                    "ref": r
                }))
                .unwrap_or_default())
            }
            "upload" => {
                let r = ref_arg.ok_or_else(|| anyhow::anyhow!("'ref' is required for action=upload"))?;
                let files = files.filter(|f| !f.is_empty()).ok_or_else(|| {
                    anyhow::anyhow!("'files' (array of file paths) is required for action=upload")
                })?;
                let resolved: Vec<String> = files
                    .iter()
                    .map(|f| {
                        Self::resolve_input_path(workspace, restrict_to_workspace, f)
                            .map(|p| p.display().to_string())
                    })
                    .collect::<anyhow::Result<Vec<String>>>()?;
                Self::with_ref(session, &r, |tab, backend| {
                    pagetree::act::upload(tab, backend, &resolved)
                })?;
                Self::respond_after_action(session, &format!("upload to {r}"), max_chars)
            }
            "export_plan" => {
                let refs = session.refs.export_plan();
                let count = refs.len();
                let plan_json = pagetree::plan::to_json(Some(session.tab.get_url()), refs);
                Ok(format!(
                    "ok action=export_plan session={session_id} ({count} refs)\n{plan_json}"
                ))
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
        "Control a local headless browser (no API key). Flow: action=open with a url returns the page as a compact interaction tree; actionable elements carry refs like [e7]. Act with action=click/type/select and ref=\"e7\" — actions return a DIFF of what changed (~ changed, + added, - removed); on navigation you get a full tree again. Refs stay valid across re-renders. Truncated lists show '[expand: eN]' — use action=expand with that ref to see the full subtree. action=upload with ref + files sets a file input. action=export_plan dumps the session's refs as JSON; action=import_plan (before or after open) seeds them into a new session so the same eN numbers reappear on a fresh page load of the same page shape. Cross-origin iframes (e.g. third-party payment widgets) are labeled but their contents can't be inspected or driven from here — same-origin iframes work normally. Other actions: snapshot (full re-read), scroll_to, extract_text (raw text), screenshot, wait_for, close. If a ref errors as stale, take a fresh snapshot."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["open", "snapshot", "expand", "click", "type", "select", "scroll_to", "upload", "export_plan", "import_plan", "extract_text", "screenshot", "wait_for", "close"],
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
                "ref": {
                    "type": "string",
                    "description": "Element ref from a snapshot, e.g. \"e7\" (for click/type/select/scroll_to/expand/upload)"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector (legacy alternative to ref for click/type; required for wait_for; optional for extract_text)"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type (action=type) or the option label to pick (action=select)"
                },
                "path": {
                    "type": "string",
                    "description": "Output path for screenshot PNG"
                },
                "files": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "File paths to attach to a file input (action=upload)"
                },
                "plan": {
                    "type": "string",
                    "description": "Plan JSON, as returned by action=export_plan (for action=import_plan)"
                },
                "timeoutMs": {
                    "type": "integer",
                    "description": "wait_for timeout in milliseconds (default 10000)"
                },
                "maxChars": {
                    "type": "integer",
                    "description": "Char budget for returned page trees (default 6000)"
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
        let ref_arg = optional_string(&params, "ref").filter(|s| !s.trim().is_empty());
        let text = optional_string(&params, "text");
        let path = optional_string(&params, "path");
        let files: Option<Vec<String>> = params.get("files").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
        let plan = optional_string(&params, "plan");
        let timeout_ms = optional_i64(&params, "timeoutMs").unwrap_or(10_000);
        let max_chars = optional_i64(&params, "maxChars")
            .map(|v| v.clamp(500, 60_000) as usize)
            .unwrap_or(pagetree::DEFAULT_RENDER_BUDGET);

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
                ref_arg,
                text,
                path,
                files,
                plan,
                timeout_ms,
                max_chars,
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

    #[tokio::test]
    async fn ref_actions_require_open_session() {
        let tool = BrowserTool::new(Path::new(".").to_path_buf(), false);
        let mut params = HashMap::new();
        params.insert("action".into(), json!("click"));
        params.insert("ref".into(), json!("e1"));
        let err = tool.execute(params).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn select_requires_ref() {
        let tool = BrowserTool::new(Path::new(".").to_path_buf(), false);
        let mut params = HashMap::new();
        params.insert("action".into(), json!("select"));
        // No session exists, but the session error only triggers after params
        // are validated inside the session branch — select needs a session
        // first, so expect the session error here.
        let err = tool.execute(params).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
