//! Regression tests for edge cases an independent review reproduced on the
//! 0.2 branch (`tests/fixtures/edge_site.py` serves one page per case).
//! Each test drives the tool the way an agent does: one CLI process per
//! step against a named session.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn tool_exe() -> String {
    env!("CARGO_BIN_EXE_browser-tool").to_string()
}

/// Same resolver as the tool; `BT_REQUIRE_BROWSER=1` (CI) forbids skipping.
fn chrome_available() -> bool {
    let found = browser_tool::browser::resolve_executable_for(None, true).is_some();
    assert!(
        found || std::env::var_os("BT_REQUIRE_BROWSER").is_none(),
        "BT_REQUIRE_BROWSER is set but no Chrome/Chromium was found"
    );
    found
}

struct Site {
    child: Child,
    base: String,
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_site() -> Site {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/edge_site.py");
    let mut child = Command::new("python3")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("python3 tests/fixtures/edge_site.py");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let base = line
        .trim()
        .strip_prefix("LISTENING ")
        .and_then(|rest| rest.split_whitespace().next())
        .expect("LISTENING line")
        .to_string();
    Site { child, base }
}

struct Session {
    name: String,
}

impl Session {
    fn start(test: &str) -> Session {
        let name = std::env::temp_dir()
            .join(format!("bt-edge-{test}-{}.sock", std::process::id()))
            .display()
            .to_string();
        let s = Session { name };
        s.ok(&["start", "--idle-timeout-s", "120"]);
        s
    }

    /// Run one step from `cwd` (None: the test's own directory).
    fn run_in(&self, cwd: Option<&std::path::Path>, args: &[&str]) -> (Value, Duration) {
        let mut cmd = Command::new(tool_exe());
        cmd.arg("--session").arg(&self.name).args(args).stdin(Stdio::null());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let started = Instant::now();
        let out = cmd.output().expect("run browser-tool");
        let took = started.elapsed();
        let text = String::from_utf8_lossy(&out.stdout);
        let value = serde_json::from_str(text.trim()).unwrap_or_else(|_| {
            panic!("stdout must be one JSON object for {args:?}: {text}\nstderr: {}", String::from_utf8_lossy(&out.stderr))
        });
        (value, took)
    }

    fn run(&self, args: &[&str]) -> (Value, Duration) {
        self.run_in(None, args)
    }

    fn ok(&self, args: &[&str]) -> Value {
        let (r, _) = self.run(args);
        assert_eq!(r["ok"], true, "{args:?} failed: {r}");
        r["result"].clone()
    }

    fn text(&self, js: &str) -> String {
        self.ok(&["eval", js]).as_str().unwrap_or_default().to_string()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = Command::new(tool_exe())
            .args(["--session", &self.name, "quit"])
            .stdin(Stdio::null())
            .output();
    }
}

/// A page that never finishes loading must not wedge the session: the click
/// returns with `loading: true`, and later ops don't wait on that page again.
#[test]
fn never_ending_page_does_not_wedge_the_session() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("hang");
    s.ok(&["goto", &format!("{}/hanglink", site.base)]);
    let (r, took) = s.run(&["click", "--text", "go hang", "--timeout-ms", "3000"]);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["result"]["loading"], true, "{r}");
    assert!(took < Duration::from_secs(8), "click took {took:?}");
    // Committed for a while now: reading the partial page is immediate.
    std::thread::sleep(Duration::from_secs(2));
    let (r, took) = s.run(&["ax"]);
    assert!(took < Duration::from_secs(2), "ax after a hung load took {took:?}: {r}");
    let (r, took) = s.run(&["goto", &format!("{}/landed", site.base), "--timeout-ms", "3000"]);
    assert_eq!(r["ok"], true, "{r}");
    assert!(took < Duration::from_secs(3), "goto away took {took:?}");
}

/// With the default pipe transport, the first command after the browser
/// dies fails at once and says why.
#[cfg(target_os = "linux")]
#[test]
fn browser_death_is_reported_at_once() {
    if !chrome_available() {
        return;
    }
    let s = Session::start("death");
    s.ok(&["goto", "about:blank"]);
    let server_pid = session_server_pid(&s.name).expect("session server pid");
    let chrome = children_of(server_pid)
        .into_iter()
        .find(|pid| comm(*pid).is_some_and(|c| c.contains("chrom") || c.contains("headless")))
        .expect("the session's browser process");
    let _ = Command::new("kill").args(["-9", &chrome.to_string()]).status();
    std::thread::sleep(Duration::from_millis(300));
    let (r, took) = s.run(&["title"]);
    assert_eq!(r["ok"], false, "{r}");
    assert!(took < Duration::from_secs(3), "first op after death took {took:?}");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("browser is gone"), "{err}");
}

#[cfg(target_os = "linux")]
fn session_server_pid(socket: &str) -> Option<u32> {
    // The server is the process whose argv names this session socket.
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let argv = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if argv.contains(socket) && argv.contains(" serve") {
            return Some(pid);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn children_of(parent: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // Field 4 (after the parenthesised comm) is the parent pid.
        let ppid = stat
            .rsplit_once(") ")
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse::<u32>().ok());
        if ppid == Some(parent) {
            out.push(pid);
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()
}

/// A popup still loading is waited for, not read as a blank page.
#[test]
fn slow_popup_is_loaded_before_it_is_read() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("popup");
    s.ok(&["goto", &format!("{}/popup", site.base)]);
    let (r, _) = s.run(&["click", "--text", "open slow"]);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["new_tabs"], serde_json::json!([1]), "{r}");
    let ax = s.ok(&["ax"]);
    assert!(ax.as_str().unwrap_or_default().contains("slow page"), "{ax}");
}

/// `--text` never matches <head> content (the <title>), and visible beats
/// hidden.
#[test]
fn text_target_ignores_the_document_title() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("title");
    s.ok(&["goto", &format!("{}/title", site.base)]);
    let state = s.ok(&["click", "--text", "Products"]);
    assert!(state["url"].as_str().unwrap_or_default().ends_with("/landed"), "{state}");
}

/// A styled checkbox (input covered by its own label's box) gets a trusted
/// click through the label, without the 3 s covered-element wait.
#[test]
fn styled_checkbox_gets_a_trusted_click() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("checkbox");
    s.ok(&["goto", &format!("{}/checkbox", site.base)]);
    let (r, took) = s.run(&["click", "--selector", "#agree"]);
    assert_eq!(r["ok"], true, "{r}");
    assert!(took < Duration::from_secs(2), "click took {took:?}");
    assert_eq!(s.text("document.getElementById('out').textContent"), "trusted=true checked=true");
}

/// Refs inside a cross-origin iframe get trusted clicks (coordinates from
/// CDP, since the frame can't see its own offset).
#[test]
fn cross_origin_iframe_ref_click_is_trusted() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("xframe");
    s.ok(&["goto", &format!("{}/xframe", site.base)]);
    s.ok(&["wait", "--ms", "300"]);
    let ax = s.ok(&["ax"]).as_str().unwrap_or_default().to_string();
    let line = ax.lines().find(|l| l.contains("Pay now")).unwrap_or_else(|| panic!("no Pay now in:\n{ax}"));
    let r = line.split("ref=").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect::<String>();
    s.ok(&["click", &r]);
    let after = s.ok(&["ax"]).as_str().unwrap_or_default().to_string();
    assert!(after.contains("clicked trusted=true"), "{after}");
}

/// A relative upload path means the caller's directory, not the session
/// server's; screenshots report where they were written.
#[test]
fn relative_paths_resolve_in_the_callers_directory() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("paths");
    let dir = std::env::temp_dir().join(format!("bt-edge-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("doc.txt"), "hello").unwrap();
    s.ok(&["goto", &format!("{}/upload", site.base)]);
    let (r, _) = s.run_in(Some(&dir), &["upload", "--selector", "#f", "doc.txt"]);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(s.text("document.getElementById('o').textContent"), "doc.txt:5");
    let (r, _) = s.run_in(Some(&dir), &["screenshot", "shot.png"]);
    assert_eq!(r["ok"], true, "{r}");
    assert!(dir.join("shot.png").exists(), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `wait --selector` / `--gone` look at every match, not just the first.
#[test]
fn wait_considers_every_match() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("twice");
    s.ok(&["goto", &format!("{}/twice", site.base)]);
    let (r, _) = s.run(&["wait", "--selector", ".t", "--timeout-ms", "1000"]);
    assert_eq!(r["ok"], true, "a visible .t exists: {r}");
    let (r, _) = s.run(&["wait", "--gone", ".t", "--timeout-ms", "500"]);
    assert_eq!(r["ok"], false, "a visible .t is still there: {r}");
    // A global --timeout-ms on a client call applies to that op.
    let (r, took) = s.run(&["--timeout-ms", "500", "wait", "--text", "never there"]);
    assert_eq!(r["ok"], false, "{r}");
    assert!(took < Duration::from_secs(3), "global --timeout-ms ignored: {took:?}");
}

/// Shift chords type the shifted character; Enter outside a form doesn't
/// pay the navigation grace.
#[test]
fn keys_shift_and_enter_outside_forms() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("keys");
    s.ok(&["goto", &format!("{}/keys", site.base)]);
    s.ok(&["press", "Shift+a", "--selector", "#i"]);
    s.ok(&["press", "Shift+1", "--selector", "#i"]);
    assert_eq!(s.text("document.getElementById('i').value"), "A!");
    s.ok(&["eval", "document.activeElement.blur()"]);
    let (r, _) = s.run(&["press", "Enter"]);
    assert_eq!(r["ok"], true, "{r}");
    let ms = r["elapsed_ms"].as_f64().unwrap_or(f64::MAX);
    assert!(ms < 140.0, "Enter on <body> waited for a navigation: {ms} ms");
}

/// An accepted prompt() without --prompt-text answers with the page's
/// default value, like pressing OK.
#[test]
fn prompt_is_accepted_with_its_default() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("prompt");
    s.ok(&["goto", &format!("{}/prompt", site.base)]);
    let (r, _) = s.run(&["click", "--text", "ask"]);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["dialogs"][0]["type"], "prompt", "{r}");
    assert_eq!(s.text("document.getElementById('o').textContent"), "got:Ada");
}

/// `ax --format json` keeps the flat list; scoping it is refused clearly.
#[test]
fn ax_json_format_contract() {
    if !chrome_available() {
        return;
    }
    let site = start_site();
    let s = Session::start("axjson");
    s.ok(&["goto", &format!("{}/title", site.base)]);
    let list = s.ok(&["ax", "--format", "json", "--limit", "1"]);
    assert_eq!(list.as_array().map(Vec::len), Some(1), "{list}");
    let (r, _) = s.run(&["ax", "--format", "json", "--selector", "nav"]);
    assert_eq!(r["ok"], false, "{r}");
}

/// With the pipe transport (default), Chrome reads EOF on its command pipe
/// and exits when browser-tool dies, so a killed session server leaks no
/// browser. (Over a WebSocket port the browser would keep running.)
#[cfg(target_os = "linux")]
#[test]
fn killed_session_server_takes_its_browser_down() {
    if !chrome_available() {
        return;
    }
    let s = Session::start("orphan");
    s.ok(&["goto", "about:blank"]);
    let server = session_server_pid(&s.name).expect("session server pid");
    let browser = children_of(server)
        .into_iter()
        .find(|pid| comm(*pid).is_some_and(|c| c.contains("chrom") || c.contains("headless")))
        .expect("the session's browser process");
    let _ = Command::new("kill").args(["-9", &server.to_string()]).status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let state = std::fs::read_to_string(format!("/proc/{browser}/stat")).unwrap_or_default();
        let gone = state.is_empty() || state.rsplit_once(") ").is_some_and(|(_, r)| r.starts_with('Z'));
        if gone {
            let _ = std::fs::remove_file(&s.name);
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = Command::new("kill").args(["-9", &browser.to_string()]).status();
    panic!("browser {browser} outlived its killed session server by 5 s");
}
