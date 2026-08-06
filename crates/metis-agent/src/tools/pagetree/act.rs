//! Act: resolve a ref back to its live DOM node and drive it with real CDP
//! input events.
//!
//! - Click: scrollIntoView → box model center → `Input.dispatchMouseEvent`
//!   (fires the full pointer pipeline some frameworks require); falls back to
//!   `element.click()` when geometry is unavailable.
//! - Fill: native value setter + `input`/`change` events — React/Vue ignore a
//!   plain `.value =`.
//! - Every action pre-checks disabled state and refuses loudly instead of
//!   silently no-opping.

use headless_chrome::protocol::cdp::Input::{
    DispatchMouseEvent, DispatchMouseEventPointer_TypeOption, DispatchMouseEventTypeOption,
    MouseButton,
};
use headless_chrome::protocol::cdp::Runtime::{CallArgument, CallFunctionOn};
use headless_chrome::protocol::cdp::DOM::{
    BackendNodeId, GetBoxModel, ResolveNode, ScrollIntoViewIfNeeded, SetFileInputFiles,
};
use headless_chrome::Tab;
use serde_json::Value as Json;
use std::path::Path;

/// Typed marker for "the backend node behind this ref is gone". The browser
/// tool catches it, re-snapshots to let fingerprint re-binding heal the ref,
/// and retries once before surfacing the error.
#[derive(Debug)]
pub struct StaleRef;

impl std::fmt::Display for StaleRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Ref is stale: that element no longer exists on the page (it re-rendered or navigated). Take a fresh 'snapshot' and act on a current ref."
        )
    }
}

impl std::error::Error for StaleRef {}

/// Resolve a backend node to a JS object id. Fails with `StaleRef` when the
/// node died (page mutated/navigated).
fn resolve_object(tab: &Tab, backend: BackendNodeId) -> anyhow::Result<String> {
    let obj = tab
        .call_method(ResolveNode {
            node_id: None,
            backend_node_id: Some(backend),
            object_group: None,
            execution_context_id: None,
        })
        .map_err(|_| stale_ref_error())?;
    obj.object.object_id.ok_or_else(stale_ref_error)
}

fn stale_ref_error() -> anyhow::Error {
    anyhow::Error::new(StaleRef)
}

/// Call a JS function with `this` bound to the node. Returns the by-value result.
fn call_on(
    tab: &Tab,
    object_id: &str,
    function: &str,
    args: Vec<Json>,
) -> anyhow::Result<Json> {
    let ret = tab
        .call_method(CallFunctionOn {
            function_declaration: function.to_string(),
            object_id: Some(object_id.to_string()),
            arguments: Some(
                args.into_iter()
                    .map(|v| CallArgument {
                        value: Some(v),
                        unserializable_value: None,
                        object_id: None,
                    })
                    .collect(),
            ),
            silent: None,
            return_by_value: Some(true),
            generate_preview: None,
            user_gesture: Some(true),
            await_promise: None,
            execution_context_id: None,
            object_group: None,
            throw_on_side_effect: None,
            unique_context_id: None,
            serialization_options: None,
        })
        .map_err(|e| anyhow::anyhow!("script on element failed: {e}"))?;
    if let Some(ex) = ret.exception_details {
        anyhow::bail!("script on element threw: {}", ex.text);
    }
    Ok(ret.result.value.unwrap_or(Json::Null))
}

const CHECK_DISABLED_JS: &str = r#"function () {
  return this.disabled === true || this.getAttribute('aria-disabled') === 'true';
}"#;

fn ensure_enabled(tab: &Tab, object_id: &str) -> anyhow::Result<()> {
    if call_on(tab, object_id, CHECK_DISABLED_JS, vec![])? == Json::Bool(true) {
        anyhow::bail!("Element is disabled — clicking it would do nothing. Look for what enables it (fill required fields, pick an option) or choose another element.");
    }
    Ok(())
}

fn scroll_into_view(tab: &Tab, backend: BackendNodeId, object_id: &str) -> anyhow::Result<()> {
    let native = tab.call_method(ScrollIntoViewIfNeeded {
        node_id: None,
        backend_node_id: Some(backend),
        object_id: None,
        rect: None,
    });
    if native.is_err() {
        call_on(
            tab,
            object_id,
            "function () { this.scrollIntoView({block: 'center', inline: 'center'}); }",
            vec![],
        )?;
    }
    Ok(())
}

/// Click the node the ref points at.
pub fn click(tab: &Tab, backend: BackendNodeId) -> anyhow::Result<()> {
    let object_id = resolve_object(tab, backend)?;
    ensure_enabled(tab, &object_id)?;
    scroll_into_view(tab, backend, &object_id)?;

    // Prefer a real mouse event at the element's center.
    match tab.call_method(GetBoxModel {
        node_id: None,
        backend_node_id: Some(backend),
        object_id: None,
    }) {
        Ok(ret) => {
            let quad = &ret.model.content;
            // Quad = [x1,y1, x2,y2, x3,y3, x4,y4] clockwise from top-left.
            let cx = (quad[0] + quad[4]) / 2.0;
            let cy = (quad[1] + quad[5]) / 2.0;
            dispatch_click(tab, cx, cy)?;
        }
        Err(_) => {
            // No geometry (hidden overflow, zero-size): synthetic click.
            call_on(tab, &object_id, "function () { this.click(); }", vec![])?;
        }
    }
    Ok(())
}

fn dispatch_click(tab: &Tab, x: f64, y: f64) -> anyhow::Result<()> {
    for (event_type, click_count, buttons) in [
        (DispatchMouseEventTypeOption::MouseMoved, None, None),
        (DispatchMouseEventTypeOption::MousePressed, Some(1), Some(1)),
        (DispatchMouseEventTypeOption::MouseReleased, Some(1), Some(1)),
    ] {
        tab.call_method(DispatchMouseEvent {
            Type: event_type,
            x,
            y,
            modifiers: None,
            timestamp: None,
            button: Some(MouseButton::Left),
            buttons,
            click_count,
            force: None,
            tangential_pressure: None,
            tilt_x: None,
            tilt_y: None,
            twist: None,
            delta_x: None,
            delta_y: None,
            pointer_Type: Some(DispatchMouseEventPointer_TypeOption::Mouse),
        })
        .map_err(|e| anyhow::anyhow!("mouse event failed: {e}"))?;
    }
    Ok(())
}

const FILL_JS: &str = r#"function (text) {
  var el = this;
  var tag = (el.tagName || '').toLowerCase();
  el.focus();
  if (tag === 'input' || tag === 'textarea') {
    var proto = tag === 'input' ? window.HTMLInputElement.prototype : window.HTMLTextAreaElement.prototype;
    var desc = Object.getOwnPropertyDescriptor(proto, 'value');
    if (desc && desc.set) { desc.set.call(el, text); } else { el.value = text; }
  } else if (el.isContentEditable) {
    el.textContent = text;
  } else {
    return 'not-editable';
  }
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return 'ok';
}"#;

/// Replace the field's value (native setter, then input+change events).
pub fn fill(tab: &Tab, backend: BackendNodeId, text: &str) -> anyhow::Result<()> {
    let object_id = resolve_object(tab, backend)?;
    ensure_enabled(tab, &object_id)?;
    scroll_into_view(tab, backend, &object_id)?;
    match call_on(tab, &object_id, FILL_JS, vec![Json::String(text.into())])? {
        Json::String(s) if s == "ok" => Ok(()),
        Json::String(s) if s == "not-editable" => anyhow::bail!(
            "Element is not an editable field (not an input/textarea/contenteditable). Use its ref with 'click', or pick the actual textbox ref."
        ),
        other => anyhow::bail!("unexpected fill result: {other}"),
    }
}

const SELECT_JS: &str = r#"function (name) {
  var el = this;
  if ((el.tagName || '').toLowerCase() !== 'select') { return 'not-select'; }
  var opts = Array.prototype.slice.call(el.options);
  var norm = function (s) { return (s || '').trim(); };
  var idx = opts.findIndex(function (o) { return norm(o.label) === norm(name) || norm(o.textContent) === norm(name); });
  if (idx < 0) {
    return 'not-found:' + opts.map(function (o) { return norm(o.label) || norm(o.textContent); }).join(' | ');
  }
  el.selectedIndex = idx;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return 'ok';
}"#;

/// Select a `<select>` option by its visible label.
pub fn select(tab: &Tab, backend: BackendNodeId, option_name: &str) -> anyhow::Result<()> {
    let object_id = resolve_object(tab, backend)?;
    ensure_enabled(tab, &object_id)?;
    match call_on(tab, &object_id, SELECT_JS, vec![Json::String(option_name.into())])? {
        Json::String(s) if s == "ok" => Ok(()),
        Json::String(s) if s == "not-select" => anyhow::bail!(
            "Element is not a native <select>. For custom dropdowns: 'click' the combobox ref to open it, snapshot, then 'click' the option ref."
        ),
        Json::String(s) if s.starts_with("not-found:") => anyhow::bail!(
            "No option with that label. Available options: {}",
            s.trim_start_matches("not-found:")
        ),
        other => anyhow::bail!("unexpected select result: {other}"),
    }
}

/// Scroll the node into the viewport center.
pub fn scroll_to(tab: &Tab, backend: BackendNodeId) -> anyhow::Result<()> {
    let object_id = resolve_object(tab, backend)?;
    scroll_into_view(tab, backend, &object_id)
}

/// Set the files of an `<input type="file">` element via CDP directly
/// (there is no JS-visible way to fake a FileList from a real path, so this
/// doesn't go through `call_on` like the other actions).
pub fn upload(tab: &Tab, backend: BackendNodeId, files: &[String]) -> anyhow::Result<()> {
    if files.is_empty() {
        anyhow::bail!("'files' must contain at least one path");
    }
    for f in files {
        if !Path::new(f).is_file() {
            anyhow::bail!("File not found: {f}");
        }
    }
    // Resolve first so a dead ref reports the standard StaleRef (and can be
    // healed by the caller) instead of CDP's generic node-not-found.
    let _ = resolve_object(tab, backend)?;
    tab.call_method(SetFileInputFiles {
        files: files.to_vec(),
        node_id: None,
        backend_node_id: Some(backend),
        object_id: None,
    })
    .map_err(|e| {
        anyhow::anyhow!(
            "upload failed ({e}). Is the ref an <input type=\"file\">? For custom upload widgets, 'click' the visible button ref instead — it may open a native file dialog this tool can't drive, or fill a hidden file input you can 'snapshot' to find."
        )
    })?;
    Ok(())
}
