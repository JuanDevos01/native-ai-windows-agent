//! Quiescence: "loaded" means the DOM stopped mutating, not that onload fired.
//! Snapshot too early and you compile a skeleton screen.
//!
//! Strategy (phase 1): inject a MutationObserver stamping the last-mutation
//! time on `window`, then poll until `document.readyState` is complete AND the
//! DOM has been silent for `dom_stable`, racing a hard `max_wait` cap — after
//! which we snapshot whatever exists.

use std::time::{Duration, Instant};

use headless_chrome::Tab;

#[derive(Debug, Clone, Copy)]
pub struct Quiescence {
    /// Required DOM silence.
    pub dom_stable: Duration,
    /// Hard cap on total waiting.
    pub max_wait: Duration,
    /// Poll interval.
    pub poll: Duration,
}

impl Default for Quiescence {
    fn default() -> Self {
        Self {
            dom_stable: Duration::from_millis(300),
            max_wait: Duration::from_secs(8),
            poll: Duration::from_millis(100),
        }
    }
}

const OBSERVER_JS: &str = r#"(function () {
  if (!window.__metis_quiesce) {
    window.__metis_quiesce = { last: Date.now() };
    try {
      new MutationObserver(function () { window.__metis_quiesce.last = Date.now(); })
        .observe(document.documentElement || document, {
          subtree: true, childList: true, attributes: true, characterData: true
        });
    } catch (e) {}
  }
  return JSON.stringify({
    ready: document.readyState,
    quietMs: Date.now() - window.__metis_quiesce.last
  });
})()"#;

/// Block until the page settles (or `max_wait` elapses). Never errors on
/// timeout — a slow page still gets snapshotted.
pub fn wait_settled(tab: &Tab, q: Quiescence) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        let status = poll_status(tab);
        match status {
            Ok((ready, quiet_ms)) => {
                let quiet_needed = q.dom_stable.as_millis() as i64;
                if ready == "complete" && quiet_ms >= quiet_needed {
                    return Ok(());
                }
                // Late in the window, accept "interactive" + silence: some
                // pages hold readyState hostage on a slow third-party asset.
                if start.elapsed() > q.max_wait / 2 && ready != "loading" && quiet_ms >= quiet_needed {
                    return Ok(());
                }
            }
            Err(_) => {
                // Evaluate can fail mid-navigation (context destroyed) — keep
                // polling; the next context will answer.
            }
        }
        if start.elapsed() >= q.max_wait {
            return Ok(());
        }
        std::thread::sleep(q.poll);
    }
}

fn poll_status(tab: &Tab) -> anyhow::Result<(String, i64)> {
    let value = tab
        .evaluate(OBSERVER_JS, false)
        .map_err(|e| anyhow::anyhow!("quiescence probe failed: {e}"))?
        .value
        .and_then(|v| v.as_str().map(str::to_string))
        .ok_or_else(|| anyhow::anyhow!("quiescence probe returned no value"))?;
    let parsed: serde_json::Value = serde_json::from_str(&value)?;
    let ready = parsed
        .get("ready")
        .and_then(|v| v.as_str())
        .unwrap_or("loading")
        .to_string();
    let quiet_ms = parsed.get("quietMs").and_then(|v| v.as_i64()).unwrap_or(0);
    Ok((ready, quiet_ms))
}
