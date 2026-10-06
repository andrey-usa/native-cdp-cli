//! Browser-level CDP: launch, tab targets, shutdown.

use std::process::Child;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::client::CdpClient;
use super::page::Page;
use super::transport::{self, LaunchedChrome};

const CMD_TIMEOUT: Duration = Duration::from_secs(30);

pub struct LaunchOptions {
    pub exe: String,
    pub headless: bool,
    /// Extra Chromium flags (skipped for non-Chromium engines).
    pub chrome_flags: Vec<String>,
    /// Pre-picked remote debugging port (used by engines whose launcher
    /// needs to know it, e.g. the Lightpanda shim).
    pub debugging_port: Option<u16>,
}

/// A launched browser: one CDP client on the browser target, plus the child
/// process handle so `close` can reap it without orphans.
pub struct Browser {
    handle: tokio::runtime::Handle,
    client: CdpClient,
    child: Mutex<Option<Child>>,
    /// Kept alive for the session; the temp profile is deleted on drop.
    /// `None` for engines that need no profile (Lightpanda).
    _profile_dir: Option<tempfile::TempDir>,
    ws_url: String,
}

impl Browser {
    /// Launch `lightpanda serve` directly (no Chromium flags, no shim) and
    /// connect the browser-level CDP session.
    ///
    /// Lightpanda is a CDP server, not a Chromium executable: it must be
    /// started as `lightpanda serve --host … --port …`, and the browser
    /// WebSocket URL is discovered via its `/json/version` endpoint.
    pub fn launch_lightpanda(handle: &tokio::runtime::Handle, bin: &str) -> Result<Self> {
        let port = free_port().context("pick a free port for lightpanda serve")?;
        // The lightpanda nightly currently requires an X server even for
        // `serve` (upstream bug — it is supposed to be headless). Wrap with
        // `xvfb-run` when there is no display and xvfb is available.
        let (program, args) = if std::env::var_os("DISPLAY").is_none() && xvfb_available() {
            eprintln!("[browser] no $DISPLAY — launching lightpanda serve under xvfb-run");
            (
                "xvfb-run".to_string(),
                vec![
                    "-a".to_string(),
                    bin.to_string(),
                    "serve".to_string(),
                    "--host".to_string(),
                    "127.0.0.1".to_string(),
                    "--port".to_string(),
                    port.to_string(),
                ],
            )
        } else {
            (
                bin.to_string(),
                vec![
                    "serve".to_string(),
                    "--host".to_string(),
                    "127.0.0.1".to_string(),
                    "--port".to_string(),
                    port.to_string(),
                ],
            )
        };
        // Write child output to a temp file for diagnostics (not piped to avoid
        // the pipe-buffer deadlock). The file is read only on poll failure.
        let log_path = std::env::temp_dir().join(format!("lightpanda-serve-{port}.log"));
        let log_file = std::fs::File::create(&log_path).ok();
        let log_file_err = log_file.as_ref().and_then(|f| f.try_clone().ok());
        let mut child = std::process::Command::new(&program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(log_file.map(std::process::Stdio::from).unwrap_or(std::process::Stdio::null()))
            .stderr(log_file_err.map(std::process::Stdio::from).unwrap_or(std::process::Stdio::null()))
            .spawn()
            .with_context(|| format!("spawn `{program} {}`", args.join(" ")))?;
        let ws_url = match transport::poll_ws_url_standalone(port, Duration::from_secs(60)) {
            Ok(url) => url,
            Err(e) => {
                let log_tail = std::fs::read_to_string(&log_path)
                    .map(|s| {
                        let lines: Vec<&str> = s.lines().collect();
                        let start = lines.len().saturating_sub(30);
                        lines[start..].join("\n")
                    })
                    .unwrap_or_default();
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "`{program} {}` did not come up on 127.0.0.1:{port}: {e:#}\nlog tail ({log_path:?}):\n{log_tail}",
                    args.join(" ")
                );
            }
        };
        eprintln!("[browser] lightpanda serve on 127.0.0.1:{port} -> {ws_url}");
        eprintln!("[browser] connecting CDP websocket...");
        let client = handle
            .block_on(CdpClient::connect(&ws_url))
            .context("CDP connect to lightpanda")?;
        eprintln!("[browser] CDP websocket connected");
        Ok(Self {
            handle: handle.clone(),
            client,
            child: Mutex::new(Some(child)),
            _profile_dir: None,
            ws_url,
        })
    }

    /// Launch the engine and connect the browser-level CDP session.
    pub fn launch(handle: &tokio::runtime::Handle, opts: &LaunchOptions) -> Result<Self> {
        let LaunchedChrome {
            child,
            profile_dir,
            ws_url,
            ..
        } = transport::launch_chrome(&opts.exe, opts.headless, &opts.chrome_flags, opts.debugging_port)
            .context("launch chrome")?;
        let client = handle
            .block_on(CdpClient::connect(&ws_url))
            .context("CDP connect")?;
        Ok(Self {
            handle: handle.clone(),
            client,
            child: Mutex::new(Some(child)),
            _profile_dir: Some(profile_dir),
            ws_url,
        })
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.handle.block_on(fut)
    }

    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    /// Open a new tab target, attach a session, enable Page/Runtime domains.
    /// Create a page on Lightpanda: it requires an explicit browser context
    /// per connection (one context + one page per process).
    pub fn new_page_lightpanda(&self, url: Option<&str>) -> Result<Page> {
        self.block_on(async {
            eprintln!("[browser] sending Target.createBrowserContext...");
            let ctx = self
                .client
                .send(
                    "Target.createBrowserContext",
                    json!({}),
                    None,
                    CMD_TIMEOUT,
                )
                .await
                .context("Target.createBrowserContext")?;
            eprintln!("[browser] got browser context");
            let browser_context_id = ctx
                .get("browserContextId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("createBrowserContext: no browserContextId"))?
                .to_string();
            let target = self
                .client
                .send(
                    "Target.createTarget",
                    json!({
                        "url": url.unwrap_or("about:blank"),
                        "browserContextId": browser_context_id,
                    }),
                    None,
                    CMD_TIMEOUT,
                )
                .await
                .context("Target.createTarget")?;
            let target_id = target
                .get("targetId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("createTarget: no targetId"))?
                .to_string();
            let session = self
                .client
                .send(
                    "Target.attachToTarget",
                    json!({ "targetId": target_id, "flatten": true }),
                    None,
                    CMD_TIMEOUT,
                )
                .await
                .context("Target.attachToTarget")?;
            let session_id = session
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("attachToTarget: no sessionId"))?
                .to_string();
            let page = Page::new(self.handle.clone(), self.client.clone(), session_id, target_id);
            page.enable().await?;
            Ok(page)
        })
    }

    pub fn new_page(&self, url: Option<&str>) -> Result<Page> {
        self.block_on(async {
            let target = self
                .client
                .send(
                    "Target.createTarget",
                    json!({ "url": url.unwrap_or("about:blank") }),
                    None,
                    CMD_TIMEOUT,
                )
                .await
                .context("Target.createTarget")?;
            let target_id = target
                .get("targetId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("createTarget: no targetId"))?
                .to_string();
            let session = self
                .client
                .send(
                    "Target.attachToTarget",
                    json!({ "targetId": target_id, "flatten": true }),
                    None,
                    CMD_TIMEOUT,
                )
                .await
                .context("Target.attachToTarget")?;
            let session_id = session
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("attachToTarget: no sessionId"))?
                .to_string();
            let page = Page::new(self.handle.clone(), self.client.clone(), session_id, target_id);
            page.enable().await?;
            Ok(page)
        })
    }

    /// List open tab targets: (target_id, url).
    pub fn tab_targets(&self) -> Result<Vec<(String, String)>> {
        self.block_on(async {
            let targets = self
                .client
                .send("Target.getTargets", json!({}), None, CMD_TIMEOUT)
                .await?;
            let mut out = Vec::new();
            if let Some(list) = targets.get("targetInfos").and_then(Value::as_array) {
                for t in list {
                    if t.get("type").and_then(Value::as_str) != Some("page") {
                        continue;
                    }
                    let id = t.get("targetId").and_then(Value::as_str).unwrap_or("");
                    let url = t.get("url").and_then(Value::as_str).unwrap_or("");
                    out.push((id.to_string(), url.to_string()));
                }
            }
            Ok(out)
        })
    }

    pub fn activate_target(&self, target_id: &str) -> Result<()> {
        self.block_on(async {
            self.client
                .send(
                    "Target.activateTarget",
                    json!({ "targetId": target_id }),
                    None,
                    CMD_TIMEOUT,
                )
                .await?;
            Ok(())
        })
    }

    pub fn close_target(&self, target_id: &str) -> Result<()> {
        self.block_on(async {
            self.client
                .send(
                    "Target.closeTarget",
                    json!({ "targetId": target_id }),
                    None,
                    CMD_TIMEOUT,
                )
                .await?;
            Ok(())
        })
    }

    /// Shut the browser down gracefully (like chromiumoxide): send the
    /// `Browser.close` CDP command, then reap the child. Falls back to
    /// killing the child if the graceful close fails or times out.
    pub fn close(&self) {
        // Graceful: ask Chrome to shut itself down via CDP.
        let _ = self.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.client.send(
                    "Browser.close",
                    serde_json::json!({}),
                    None,
                    std::time::Duration::from_secs(5),
                ),
            )
            .await
        });
        // Reap the child; kill if it's still alive after the graceful close.
        if let Ok(mut child) = self.child.lock() {
            if let Some(mut child) = child.take() {
                // Poll for exit instead of sleeping unconditionally: Chrome
                // usually exits within ~50ms of Browser.close; the old fixed
                // 500ms sleep added half a second to every cold start.
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        _ => {
                            if std::time::Instant::now() >= deadline {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                }
                match child.try_wait() {
                    Ok(Some(_)) => {
                        let _ = child.wait();
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            }
        }
    }
}

/// True when `xvfb-run` is on PATH.
fn xvfb_available() -> bool {
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|d| {
                d.join(if cfg!(windows) { "xvfb-run.exe" } else { "xvfb-run" })
                    .is_file()
            })
        })
        .unwrap_or(false)
}

/// Pick a free localhost TCP port by binding to port 0 and releasing it.
fn free_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").context("bind 127.0.0.1:0 for a free port")?;
    Ok(listener.local_addr()?.port())
}
