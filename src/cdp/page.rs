//! Tab-level operations, implemented directly on CDP.
//!
//! The key performance decision: `Runtime.evaluate` with
//! `returnByValue: true` returns object results in ONE round trip — no
//! evaluate + callFunctionOn + releaseObject dance. Expressions are wrapped
//! so bare arrows (`() => …`), IIFEs, and plain expressions all evaluate to
//! their value.

use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use serde_json::{Value, json};

use super::client::CdpClient;

const CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// Click preparation, run in the page: scroll the element into view and, if
/// it is visible and actually receives a pointer event at its centre,
/// return that point for a *trusted* CDP mouse click (`isTrusted: true`,
/// default actions, focus, user activation — what real sites expect).
/// Otherwise (no layout, zero size, covered by another element, or
/// `force`), fall back to DOM-dispatched mouse events and report it.
const CLICK_PREP: &str = r#"function (el, force) {
    el.scrollIntoView({ block: 'center', inline: 'center' });
    const r = el.getBoundingClientRect();
    const x = r.left + r.width / 2, y = r.top + r.height / 2;
    const hit = (!force && r.width > 0 && r.height > 0) ? document.elementFromPoint(x, y) : null;
    if (hit && (hit === el || el.contains(hit))) return { x, y };
    for (const type of ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click']) {
        el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, view: window }));
    }
    return { synthetic: true };
}"#;

/// Fill `el` with `value`: native value setter (so React & co. see it) plus
/// `input`/`change` events.
const FILL_EL: &str = r#"function (el, value) {
    el.focus();
    const proto = (el instanceof HTMLTextAreaElement) ? HTMLTextAreaElement.prototype
        : (el instanceof HTMLInputElement) ? HTMLInputElement.prototype : null;
    const desc = proto ? Object.getOwnPropertyDescriptor(proto, 'value') : null;
    if (desc && desc.set) { desc.set.call(el, value); } else { el.value = value; }
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
}"#;

/// Auto-wait: resolve the first match for `sel`, polling every 50 ms for up
/// to `ms` (SPAs render after load; an agent shouldn't have to poll).
const WAIT_FOR: &str = r#"async function (sel, ms, what) {
    const end = Date.now() + ms;
    for (;;) {
        const el = document.querySelector(sel);
        if (el) return el;
        if (Date.now() >= end) throw new Error(what + ': no element matches selector within ' + ms + ' ms');
        await new Promise((r) => setTimeout(r, 50));
    }
}"#;

/// JS-side wait budget: a bit under the CDP command timeout, so the page's
/// own "not found" error wins the race against the transport timeout.
fn wait_budget_ms(timeout: Duration) -> u128 {
    timeout.as_millis().saturating_sub(250)
}

/// AX roles an agent can act on; always kept by the compact `ax` view.
const AX_INTERACTIVE: &[&str] = &[
    "button", "link", "textbox", "searchbox", "checkbox", "radio", "combobox",
    "listbox", "option", "menuitem", "menuitemcheckbox", "menuitemradio", "tab",
    "switch", "slider", "spinbutton", "treeitem",
];

/// AX roles that carry no information of their own in the compact view.
const AX_STRUCTURAL: &[&str] = &["generic", "none", "presentation", "InlineTextBox", "LineBreak"];

/// `node[key].value` as a string ("" when absent).
fn ax_str<'a>(node: &'a Value, key: &str) -> &'a str {
    node.get(key)
        .and_then(|v| v.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

#[derive(Clone)]
pub struct Page {
    handle: tokio::runtime::Handle,
    client: CdpClient,
    session_id: String,
    target_id: String,
}

impl Page {
    pub fn new(
        handle: tokio::runtime::Handle,
        client: CdpClient,
        session_id: String,
        target_id: String,
    ) -> Self {
        Self {
            handle,
            client,
            session_id,
            target_id,
        }
    }

    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.handle.block_on(fut)
    }

    pub async fn enable(&self) -> Result<()> {
        self.client
            .send("Page.enable", json!({}), Some(&self.session_id), CMD_TIMEOUT)
            .await?;
        self.client
            .send(
                "Runtime.enable",
                json!({}),
                Some(&self.session_id),
                CMD_TIMEOUT,
            )
            .await?;
        Ok(())
    }

    fn send(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.block_on(self.client.send(
            method,
            params,
            Some(&self.session_id),
            timeout,
        ))
    }

    /// Evaluate a JS expression; the result comes back by value in one round
    /// trip. Bare arrow functions are invoked; IIFEs and plain expressions
    /// evaluate to their value.
    pub fn evaluate(&self, expression: &str, timeout: Duration) -> Result<Value> {
        self.block_on(async {
            // Wrap once: if the expression evaluates to a function, call it.
            // Handles `() => …`, `(() => …)()`, and plain `document.title`.
            let wrapped = format!(
                "(() => {{ const __r = ({}); return (typeof __r === 'function') ? __r() : __r; }})()",
                expression
            );
            let res = self
                .client
                .send(
                    "Runtime.evaluate",
                    json!({
                        "expression": wrapped,
                        "returnByValue": true,
                        "awaitPromise": true,
                    }),
                    Some(&self.session_id),
                    timeout,
                )
                .await
                .with_context(|| format!("evaluate: {expression}"))?;
            if let Some(details) = res.get("exceptionDetails") {
                anyhow::bail!("evaluate threw: {details}");
            }
            // returnByValue => result.result.value holds the JSON value.
            Ok(res
                .get("result")
                .and_then(|r| r.get("value"))
                .cloned()
                .unwrap_or(Value::Null))
        })
    }

    /// Navigate the active frame and wait for its `Page.loadEventFired`.
    ///
    /// Subscribes to CDP events *before* navigating so a fast local page that
    /// loads before the response returns isn't missed, then waits for the
    /// matching frame's load event (or `errorText` from the navigation).
    pub fn navigate(&self, url: &str, timeout: Duration) -> Result<()> {
        self.block_on(async {
            let mut events = self.client.subscribe();
            let res = self
                .client
                .send(
                    "Page.navigate",
                    json!({ "url": url }),
                    Some(&self.session_id),
                    timeout,
                )
                .await
                .with_context(|| format!("navigate to {url}"))?;
            if let Some(error) = res.get("errorText").and_then(Value::as_str) {
                anyhow::bail!("navigate to {url}: {error}");
            }
            let frame_id = res
                .get("frameId")
                .and_then(Value::as_str)
                .map(str::to_string);
            // Wait for this frame's load event (or any load event if Chrome didn't
            // return a frameId for this navigation).
            tokio::time::timeout(timeout, async {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            if event.get("method").and_then(Value::as_str)
                                != Some("Page.loadEventFired")
                            {
                                continue;
                            }
                            let got = event
                                .get("params")
                                .and_then(|p| p.get("frameId"))
                                .and_then(Value::as_str);
                            match (&frame_id, got) {
                                (Some(want), Some(got)) if want != got => continue,
                                _ => return Ok::<(), anyhow::Error>(()),
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(e) => anyhow::bail!("CDP event stream ended: {e}"),
                    }
                }
            })
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for load of {url}"))??;
            Ok(())
        })
    }

    /// Navigate and return as soon as the navigation commits (no load wait).
    pub fn navigate_commit(&self, url: &str, timeout: Duration) -> Result<()> {
        self.block_on(async {
            let res = self
                .client
                .send(
                    "Page.navigate",
                    json!({ "url": url }),
                    Some(&self.session_id),
                    timeout,
                )
                .await
                .with_context(|| format!("navigate to {url}"))?;
            if let Some(error) = res.get("errorText").and_then(Value::as_str) {
                anyhow::bail!("navigate to {url}: {error}");
            }
            Ok(())
        })
    }

    pub fn title(&self, timeout: Duration) -> Result<String> {
        let value = self.evaluate("document.title", timeout)?;
        Ok(value.as_str().unwrap_or_default().to_string())
    }

    pub fn url(&self, timeout: Duration) -> Result<String> {
        let value = self.evaluate("location.href", timeout)?;
        Ok(value.as_str().unwrap_or_default().to_string())
    }

    /// Click the first element matching `selector`: a trusted CDP mouse
    /// click at its centre, or DOM-dispatched events when it has no hittable
    /// box (see [`CLICK_PREP`]).
    pub fn click(&self, selector: &str, timeout: Duration) -> Result<()> {
        let sel = serde_json::to_string(selector)?;
        let wait_ms = wait_budget_ms(timeout);
        let prep = |force: bool| {
            format!(
                "(async () => {{ const el = await ({WAIT_FOR})({sel}, {wait_ms}, 'click'); \
                 return ({CLICK_PREP})(el, {force}); }})()"
            )
        };
        let target = self.evaluate(&prep(false), timeout)?;
        if self.trusted_click(&target, timeout).is_err() {
            // Engine without the Input domain (e.g. Lightpanda): DOM events.
            self.evaluate(&prep(true), timeout)?;
        }
        Ok(())
    }

    /// Dispatch a real mouse click at `target.{x,y}` (no-op when the page
    /// already fell back to DOM events).
    fn trusted_click(&self, target: &Value, timeout: Duration) -> Result<()> {
        let (Some(x), Some(y)) = (
            target.get("x").and_then(Value::as_f64),
            target.get("y").and_then(Value::as_f64),
        ) else {
            return Ok(());
        };
        for (kind, button, buttons) in [
            ("mouseMoved", "none", 0),
            ("mousePressed", "left", 1),
            ("mouseReleased", "left", 0),
        ] {
            self.send(
                "Input.dispatchMouseEvent",
                json!({
                    "type": kind, "x": x, "y": y,
                    "button": button, "buttons": buttons, "clickCount": 1,
                }),
                timeout,
            )?;
        }
        Ok(())
    }

    /// Fill the first matching input/textarea: native setter + input/change.
    pub fn fill(&self, selector: &str, value: &str, timeout: Duration) -> Result<()> {
        let sel = serde_json::to_string(selector)?;
        let val = serde_json::to_string(value)?;
        let wait_ms = wait_budget_ms(timeout);
        let expr = format!(
            "(async () => {{ const el = await ({WAIT_FOR})({sel}, {wait_ms}, 'fill'); \
             return ({FILL_EL})(el, {val}); }})()"
        );
        self.evaluate(&expr, timeout)?;
        Ok(())
    }

    /// Resolve an `ax` ref (`backendNodeId`) and call `function` on it.
    fn call_on_ref(
        &self,
        backend_node_id: u64,
        function: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.block_on(async {
            let resolved = self
                .client
                .send(
                    "DOM.resolveNode",
                    json!({ "backendNodeId": backend_node_id, "objectGroup": "bt-ref" }),
                    Some(&self.session_id),
                    timeout,
                )
                .await
                .with_context(|| {
                    format!("resolve ref {backend_node_id} (stale after navigation? take a fresh `ax`)")
                })?;
            let object_id = resolved
                .get("object")
                .and_then(|o| o.get("objectId"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("ref {backend_node_id} has no JS object"))?
                .to_string();
            let res = self
                .client
                .send(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": function,
                        "arguments": arguments,
                        "returnByValue": true,
                        "awaitPromise": true,
                    }),
                    Some(&self.session_id),
                    timeout,
                )
                .await;
            // Free the resolved handle whatever happened.
            let _ = self
                .client
                .send(
                    "Runtime.releaseObjectGroup",
                    json!({ "objectGroup": "bt-ref" }),
                    Some(&self.session_id),
                    timeout,
                )
                .await;
            let res = res.with_context(|| format!("call on ref {backend_node_id}"))?;
            if let Some(details) = res.get("exceptionDetails") {
                anyhow::bail!("ref {backend_node_id} threw: {details}");
            }
            Ok(res
                .get("result")
                .and_then(|r| r.get("value"))
                .cloned()
                .unwrap_or(Value::Null))
        })
    }

    /// Click the node behind an `ax` ref (trusted when hittable, like
    /// [`Page::click`]).
    pub fn click_ref(&self, backend_node_id: u64, timeout: Duration) -> Result<()> {
        // AX refs often point at a text node: climb to its element first.
        let prep = |force: bool| {
            format!(
                "function () {{ const el = this.nodeType === 1 ? this : this.parentElement; \
                 if (!el) throw new Error('click: ref is not inside an element'); \
                 return ({CLICK_PREP})(el, {force}); }}"
            )
        };
        let target = self.call_on_ref(backend_node_id, &prep(false), json!([]), timeout)?;
        if self.trusted_click(&target, timeout).is_err() {
            self.call_on_ref(backend_node_id, &prep(true), json!([]), timeout)?;
        }
        Ok(())
    }

    /// Fill the field behind an `ax` ref.
    pub fn fill_ref(&self, backend_node_id: u64, value: &str, timeout: Duration) -> Result<()> {
        let function = format!(
            "function (value) {{ const el = this.nodeType === 1 ? this : this.parentElement; \
             if (!el) throw new Error('fill: ref is not inside an element'); \
             return ({FILL_EL})(el, value); }}"
        );
        self.call_on_ref(backend_node_id, &function, json!([{ "value": value }]), timeout)?;
        Ok(())
    }

    /// First matching element's textContent, if present.
    pub fn text_content(&self, selector: &str, timeout: Duration) -> Result<Option<String>> {
        let sel = serde_json::to_string(selector)?;
        let value = self.evaluate(
            &format!("(document.querySelector({sel}) || {{}}).textContent ?? null"),
            timeout,
        )?;
        Ok(value.as_str().map(str::to_string))
    }

    /// PNG screenshot bytes; `full_page` captures beyond the viewport.
    pub fn screenshot_png(&self, timeout: Duration, full_page: bool) -> Result<Vec<u8>> {
        let mut params = json!({ "format": "png" });
        if full_page {
            params["captureBeyondViewport"] = Value::Bool(true);
        }
        let res = self.send("Page.captureScreenshot", params, timeout)?;
        let data = res
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("captureScreenshot: no data"))?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("screenshot base64 decode")
    }

    /// Accessibility tree snapshot for agents, in ONE `getFullAXTree` call.
    ///
    /// Compact view (default): `[{ref, role, name, value?}]` holding every
    /// interactive node plus named content, minus structural wrappers
    /// (generic/none/InlineTextBox) and StaticText that merely repeats its
    /// parent's name (a link's own label, a heading's text). `ref` is the
    /// node's `backendNodeId`; pass it to `click`/`fill` as `ref`.
    /// `all = true` returns every non-ignored node as `{id, role, name,
    /// backendNodeId}` (the raw shape, for debugging).
    pub fn ax_tree(&self, timeout: Duration, max_depth: Option<u32>, all: bool) -> Result<Value> {
        let mut params = json!({});
        if let Some(d) = max_depth {
            params["depth"] = json!(d);
        }
        let res = self.send("Accessibility.getFullAXTree", params, timeout)?;
        let nodes = res
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let live = nodes
            .iter()
            .filter(|n| !n.get("ignored").and_then(Value::as_bool).unwrap_or(false));
        if all {
            return Ok(Value::Array(
                live.map(|n| {
                    json!({
                        "id": n.get("nodeId").and_then(Value::as_str).unwrap_or(""),
                        "role": ax_str(n, "role"),
                        "name": ax_str(n, "name"),
                        "backendNodeId": n.get("backendDOMNodeId").and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect(),
            ));
        }
        let names: std::collections::HashMap<&str, &str> = nodes
            .iter()
            .filter_map(|n| Some((n.get("nodeId")?.as_str()?, ax_str(n, "name"))))
            .collect();
        let compact: Vec<Value> = live
            .filter_map(|n| {
                let role = ax_str(n, "role");
                let name = ax_str(n, "name");
                let node_ref = n.get("backendDOMNodeId").and_then(Value::as_u64)?;
                let interactive = AX_INTERACTIVE.contains(&role);
                if !interactive {
                    if name.is_empty() || AX_STRUCTURAL.contains(&role) {
                        return None;
                    }
                    let parent_name = n
                        .get("parentId")
                        .and_then(Value::as_str)
                        .and_then(|p| names.get(p).copied())
                        .unwrap_or("");
                    if role == "StaticText" && name == parent_name {
                        return None;
                    }
                }
                let mut out = json!({ "ref": node_ref, "role": role, "name": name });
                let value = ax_str(n, "value");
                if !value.is_empty() {
                    out["value"] = json!(value);
                }
                Some(out)
            })
            .collect();
        Ok(Value::Array(compact))
    }

    /// Close this tab's target.
    pub fn close_target(&self, timeout: Duration) -> Result<()> {
        self.block_on(async {
            self.client
                .send(
                    "Target.closeTarget",
                    json!({ "targetId": self.target_id }),
                    None,
                    timeout,
                )
                .await?;
            Ok(())
        })
    }
}
