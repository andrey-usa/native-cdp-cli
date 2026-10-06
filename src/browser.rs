//! Browser sessions on the from-scratch CDP engine (`crate::cdp`).
//!
//! `BrowserSession` is the unit the CLI drives: one browser, a tab list with
//! an active tab, navigation with retry, and the script/text helpers the
//! agent needs. The wire protocol in `browser_tool.rs` is unchanged.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;

use crate::cdp::{self, Browser, LaunchOptions, Page};

/// Tokio runtime for the CDP engine: 2 workers is plenty for a sequential
/// CLI, and keeps RSS/CPU far below a default multi-thread runtime.
fn engine_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("cdp-engine")
        .build()
        .context("build CDP engine runtime")
}

/// Chromium launch flags.
///
/// Beyond the basics, this is the standard automation set that go-rod,
/// chromiumoxide, Puppeteer and Playwright all ship by default: no
/// background networking, component updates, crash reporter, sync, metrics
/// or Translate competing with the page for CPU, and no throttling of
/// background tabs. `--no-startup-window` skips Chrome's initial tab (the
/// session opens its own), saving a renderer process per launch. Site
/// isolation is relaxed as go-rod does, so cross-site iframes share a
/// renderer instead of each spawning one (we already run `--no-sandbox`).
/// Chrome honours only the last `--disable-features` / `--enable-features`
/// flag, so each list is a single flag.
const CHROME_FLAGS: &[&str] = &[
    "--no-sandbox",
    "--disable-dev-shm-usage",
    "--disable-blink-features=AutomationControlled",
    "--no-first-run",
    "--no-default-browser-check",
    "--no-startup-window",
    "--disable-default-apps",
    "--disable-infobars",
    "--window-size=1440,900",
    "--disable-background-networking",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-component-update",
    "--disable-extensions",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-sync",
    "--metrics-recording-only",
    "--password-store=basic",
    "--use-mock-keychain",
    "--force-color-profile=srgb",
    "--disable-site-isolation-trials",
    "--disable-features=Translate,TranslateUI,OptimizationHints,MediaRouter,DialMediaRouteProvider,AutofillServerCommunication,CertificateTransparencyComponentUpdater,InterestFeedContentSuggestions,site-per-process",
    "--enable-features=NetworkService,NetworkServiceInProcess",
];

/// Common Chromium/Chrome/Edge install paths used when no override is given.
fn common_executables() -> Vec<String> {
    let mut paths = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Ok(p) = std::env::var(var) {
            paths.push(format!("{p}\\Microsoft\\Edge\\Application\\msedge.exe"));
            paths.push(format!("{p}\\Google\\Chrome\\Application\\chrome.exe"));
        }
    }
    if let Ok(p) = std::env::var("LOCALAPPDATA") {
        paths.push(format!("{p}\\Google\\Chrome\\Application\\chrome.exe"));
    }
    paths.push("/usr/bin/google-chrome".into());
    paths.push("/usr/bin/google-chrome-stable".into());
    paths.push("/usr/bin/chromium".into());
    paths.push("/usr/bin/chromium-browser".into());
    paths.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
    paths.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
    paths
}

/// Resolve which executable to launch, honoring (in order) an explicit CLI
/// override, `$CDP_CLI_CHROMIUM` / `$RUSTWRIGHT_CHROMIUM` / `$CHROME_BIN`,
/// then the common-path probe.
pub fn resolve_executable(cli_override: Option<&str>) -> Option<String> {
    if let Some(path) = cli_override {
        if !path.trim().is_empty() {
            return Some(path.to_string());
        }
    }
    for var in ["CDP_CLI_CHROMIUM", "RUSTWRIGHT_CHROMIUM", "CHROME_BIN"] {
        if let Ok(path) = std::env::var(var) {
            if !path.trim().is_empty() {
                return Some(path);
            }
        }
    }
    common_executables()
        .into_iter()
        .find(|p| Path::new(p).exists())
}

/// True when `--engine` selects the Lightpanda CDP server instead of Chromium.
pub fn is_lightpanda(engine: &str) -> bool {
    matches!(
        engine.trim().to_ascii_lowercase().as_str(),
        "lightpanda" | "panda"
    )
}

/// Resolve the `lightpanda` binary for direct `lightpanda serve` launch.
///
/// Order: explicit `--chromium <path>` (kept as the generic binary override),
/// `$LIGHTPANDA_BIN`, then `lightpanda` on `PATH`.
fn resolve_lightpanda_bin(cli_override: Option<&str>) -> Result<String> {
    if let Some(path) = cli_override {
        if !path.trim().is_empty() {
            // `--chromium` doubles as the lightpanda override, so a Chromium
            // path that leaked in from $CHROME_BIN-style defaults would be
            // launched as `chrome serve --port …` and then time out waiting
            // for a /json/version that never comes. Fail fast instead.
            let file = Path::new(path)
                .file_name()
                .map(|f| f.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if ["chrome", "chromium", "msedge", "edge", "brave"]
                .iter()
                .any(|b| file.contains(b))
            {
                bail!(
                    "--engine lightpanda was given a Chromium binary ({path}); pass the lightpanda binary via --chromium or $LIGHTPANDA_BIN"
                );
            }
            return Ok(path.to_string());
        }
    }
    if let Ok(path) = std::env::var("LIGHTPANDA_BIN") {
        if !path.trim().is_empty() {
            return Ok(path);
        }
    }
    if let Ok(path) = which_lightpanda() {
        return Ok(path);
    }
    bail!("lightpanda binary not found — install it or set --chromium <path> / $LIGHTPANDA_BIN")
}

/// `lightpanda` on `PATH` (no external `which` dependency).
fn which_lightpanda() -> Result<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(if cfg!(windows) {
            "lightpanda.exe"
        } else {
            "lightpanda"
        });
        if candidate.is_file() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    anyhow::bail!("lightpanda not on PATH")
}

/// One browser + tab list, driven by the from-scratch CDP engine.
pub struct BrowserSession {
    browser: Browser,
    pages: Vec<Page>,
    active: usize,
    /// Executable that was launched (browser binary or `lightpanda`); kept so
    /// [`BrowserSession::close`] can run engine-specific cleanup.
    exe: String,
    /// True when the underlying engine is Lightpanda, which does not emit the
    /// CDP `Page.loadEventFired` event — those sessions navigate with a
    /// `commit` wait instead.
    lightpanda: bool,
    timeout_ms: f64,
    /// Owns the tokio runtime; declared LAST so it drops last, after the
    /// browser/client/pages that use it.
    _runtime: tokio::runtime::Runtime,
}

impl BrowserSession {
    /// Launch a headless CDP browser ready for navigation.
    ///
    /// `engine` is `chrome` (Chromium/Chrome/Edge) or `lightpanda` (a
    /// Lightpanda CDP server, started directly as `lightpanda serve`).
    pub fn launch(
        engine: &str,
        headless: bool,
        executable: Option<&str>,
        nav_timeout_ms: f64,
    ) -> Result<Self> {
        let launch_started = std::time::Instant::now();
        let lightpanda = is_lightpanda(engine);
        let runtime = engine_runtime()?;
        let handle = runtime.handle().clone();

        let (browser, exe) = if lightpanda {
            // Lightpanda is a CDP server, not a Chromium executable: start
            // `lightpanda serve` directly and connect to its CDP endpoint.
            // No Chromium flags, no shim, no X server involved.
            let bin = resolve_lightpanda_bin(executable)?;
            let browser = Browser::launch_lightpanda(&handle, &bin)?;
            eprintln!(
                "[browser] engine=lightpanda launched {} via {bin}",
                browser.ws_url()
            );
            (browser, bin)
        } else {
            let Some(exe) = resolve_executable(executable) else {
                bail!(
                    "no Chromium/Chrome/Edge executable found — install Chrome/Edge or set --chromium <path> or $CDP_CLI_CHROMIUM"
                );
            };
            let browser = Browser::launch(
                &handle,
                &LaunchOptions {
                    exe: exe.clone(),
                    headless,
                    chrome_flags: CHROME_FLAGS.iter().map(|f| f.to_string()).collect(),
                    debugging_port: None,
                },
            )?;
            eprintln!(
                "[browser] engine=chrome launched {} via {exe} (headless={headless})",
                browser.ws_url(),
            );
            (browser, exe)
        };
        crate::timing::record("browser_up", launch_started);
        let page_started = std::time::Instant::now();
        let page = if lightpanda {
            browser.new_page_lightpanda(Some("about:blank"))?
        } else {
            browser.new_page(Some("about:blank"))?
        };
        crate::timing::record("first_page", page_started);
        crate::timing::record("launch_total", launch_started);
        Ok(Self {
            browser,
            pages: vec![page],
            active: 0,
            exe,
            lightpanda,
            timeout_ms: nav_timeout_ms,
            _runtime: runtime,
        })
    }

    fn timeout(&self, override_ms: Option<f64>) -> Duration {
        Duration::from_secs_f64(override_ms.unwrap_or(self.timeout_ms) / 1000.0)
    }

    fn active_tab(&self) -> &Page {
        &self.pages[self.active]
    }

    /// Number of open pages (tabs) in this session.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Index of the active tab.
    pub fn active_page(&self) -> usize {
        self.active
    }

    /// Target ids of all open tabs.
    pub fn page_targets(&self) -> Vec<String> {
        self.pages.iter().map(|p| p.target_id().to_string()).collect()
    }

    /// Open a new tab and make it active; returns its index.
    pub fn new_page(&mut self) -> Result<usize> {
        if self.lightpanda {
            // Lightpanda's CDP is single-target: `Target.createTarget` replies
            // `TargetAlreadyLoaded` as soon as a page exists.
            bail!("the lightpanda engine supports one tab only; use the chrome engine for tabs");
        }
        let page = self.browser.new_page(Some("about:blank"))?;
        self.pages.push(page);
        self.active = self.pages.len() - 1;
        Ok(self.active)
    }

    /// Switch the active tab by zero-based index.
    pub fn select_page(&mut self, index: usize) -> Result<()> {
        if index >= self.pages.len() {
            bail!("no tab {index} (have {})", self.pages.len());
        }
        self.active = index;
        let target_id = self.pages[index].target_id().to_string();
        self.browser.activate_target(&target_id)?;
        Ok(())
    }

    /// Close one tab by index; the last remaining tab cannot be closed.
    pub fn close_page(&mut self, index: usize) -> Result<()> {
        if self.pages.len() <= 1 {
            bail!("refusing to close the session's only tab; use `quit` instead");
        }
        if index >= self.pages.len() {
            bail!("no tab {index} (have {})", self.pages.len());
        }
        let timeout = self.timeout(None);
        self.pages[index].close_target(timeout)?;
        self.pages.remove(index);
        if self.active >= self.pages.len() {
            self.active = self.pages.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        Ok(())
    }

    /// Current page URL.
    pub fn url(&self) -> String {
        self.active_tab().url(self.timeout(None)).unwrap_or_default()
    }

    /// Navigate the active tab and wait for `load`.
    pub fn goto(&self, url: &str) -> Result<()> {
        self.goto_with_retry(url, 3)
    }

    /// Navigate with retry; third-party trackers (doubleclick, pinterest, …)
    /// can abort the main-frame load mid-flight, which is benign.
    ///
    /// Lightpanda sessions use a `commit` wait: Lightpanda emits no
    /// `Page.loadEventFired`, so a full wait would just burn the navigation
    /// timeout. Callers confirm content by polling the DOM.
    pub fn goto_with_retry(&self, url: &str, max_attempts: usize) -> Result<()> {
        let timeout = if self.lightpanda {
            Duration::from_millis(500)
        } else {
            // Honors --timeout-ms (was a hard-coded 40 s per attempt).
            self.timeout(None)
        };
        let mut attempt = 0;
        loop {
            let result = if self.lightpanda {
                // Commit wait: Page.navigate returning is the commit.
                self.active_tab().navigate_commit(url, timeout)
            } else {
                self.active_tab().navigate(url, timeout)
            };
            match result {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let msg = format!("{e:#}");
                    if self.lightpanda && msg.contains("timed out") {
                        return Ok(());
                    }
                    attempt += 1;
                    let transient = msg.contains("ERR_ABORTED")
                        || msg.contains("ERR_CONNECTION_RESET")
                        || msg.contains("net::");
                    if transient && attempt <= max_attempts {
                        eprintln!("[browser] nav hiccup (attempt {attempt}): {msg}");
                        std::thread::sleep(Duration::from_millis(500 * attempt as u64));
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Navigate without waiting for load (commit semantics).
    pub fn goto_commit(&self, url: &str, timeout_ms: Option<f64>) -> Result<()> {
        self.active_tab()
            .navigate_commit(url, self.timeout(timeout_ms))
    }

    /// Run a JS expression in the active tab; result must be JSON.
    pub fn evaluate<T: DeserializeOwned>(&self, expression: &str) -> Result<T> {
        self.evaluate_with_timeout(expression, None)
    }

    /// Run a JS expression with an explicit timeout override.
    pub fn evaluate_with_timeout<T: DeserializeOwned>(
        &self,
        expression: &str,
        timeout_ms: Option<f64>,
    ) -> Result<T> {
        let value = self
            .active_tab()
            .evaluate(expression, self.timeout(timeout_ms))?;
        Ok(serde_json::from_value(value)?)
    }

    /// Click the first element matching `selector`.
    pub fn click(&self, selector: &str, timeout_ms: Option<f64>) -> Result<()> {
        self.active_tab().click(selector, self.timeout(timeout_ms))
    }

    /// Fill the first matching field with `value`.
    pub fn fill(&self, selector: &str, value: &str, timeout_ms: Option<f64>) -> Result<()> {
        self.active_tab()
            .fill(selector, value, self.timeout(timeout_ms))
    }

    /// Click the node with this `backendNodeId` (an `ax` ref).
    pub fn click_ref(&self, backend_node_id: u64, timeout_ms: Option<f64>) -> Result<()> {
        self.active_tab()
            .click_ref(backend_node_id, self.timeout(timeout_ms))
    }

    /// Fill the node with this `backendNodeId` (an `ax` ref) with `value`.
    pub fn fill_ref(&self, backend_node_id: u64, value: &str, timeout_ms: Option<f64>) -> Result<()> {
        self.active_tab()
            .fill_ref(backend_node_id, value, self.timeout(timeout_ms))
    }

    /// First element's text content for `selector`, if present.
    pub fn text(&self, selector: &str) -> Result<Option<String>> {
        self.text_with_timeout(selector, None)
    }

    /// First element's text content with an explicit timeout override.
    pub fn text_with_timeout(
        &self,
        selector: &str,
        timeout_ms: Option<f64>,
    ) -> Result<Option<String>> {
        self.active_tab()
            .text_content(selector, self.timeout(timeout_ms))
    }

    /// Page title of the active tab.
    pub fn title(&self) -> Result<String> {
        self.active_tab().title(self.timeout(None))
    }

    /// Viewport PNG screenshot bytes (`full_page` captures beyond viewport).
    /// Accessibility tree snapshot for the active tab (agent-friendly).
    pub fn ax_tree(
        &self,
        max_depth: Option<u32>,
        all: bool,
        timeout_ms: Option<f64>,
    ) -> Result<serde_json::Value> {
        self.active_tab()
            .ax_tree(self.timeout(timeout_ms), max_depth, all)
    }

    pub fn screenshot_png(&self, full_page: bool) -> Result<Vec<u8>> {
        self.active_tab()
            .screenshot_png(self.timeout(None), full_page)
    }

    /// True when this session drives Lightpanda rather than Chromium.
    pub fn is_lightpanda(&self) -> bool {
        self.lightpanda
    }

    /// The CDP WebSocket endpoint this session is attached to.
    pub fn endpoint(&self) -> String {
        self.browser.ws_url().to_string()
    }

    /// Shut the browser down. (Lightpanda is now launched directly as
    /// `lightpanda serve` and reaped like Chrome; the old shim's `--stop`
    /// cleanup step no longer applies — the real binary has no such flag.)
    pub fn close(&self) {
        // Never panic from close: a panicking shutdown turns a successful
        // session into a failed process exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.browser.close();
        }));
    }

    /// The launched executable (browser binary or `lightpanda`).
    pub fn executable(&self) -> &str {
        &self.exe
    }
}

// Re-exported for the rare caller that still names the engine explicitly.
pub use cdp::{Browser as CdpBrowser, Page as CdpPage};
