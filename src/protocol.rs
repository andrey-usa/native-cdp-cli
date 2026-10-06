//! `browser_tool` — the Node-free realtime browser CLI library.
//!
//! This is the library behind the `browser-tool` binary: it replaces the
//! Playwright CLI (and its Node.js driver process) with a Chrome-first,
//! line-oriented JSON protocol on top of the same [`BrowserSession`] the
//! production agent uses. The binary is a thin wrapper — all parsing,
//! protocol and session logic lives here so it is reusable and testable.
//!
//! ```text
//! # One-shot: one command, one response, browser closes on exit.
//! browser-tool eval --expression "() => document.title" --pretty
//! browser-tool goto --url <url>
//!
//! # Persistent session: one warm Chrome, many commands, no relaunch cost.
//! browser-tool serve
//! {"id":1,"op":"goto","url":"https://example.com"}
//! {"id":2,"op":"eval","expression":"() => document.title","timeout_ms":25000}
//! {"id":3,"op":"quit"}
//! ```
//!
//! Model contract: open a tab with `tab-new`, navigate with `goto`, read with
//! `eval`/`text`/`title`/`url`, act with `click`/`fill`, manage with
//! `tab-list`/`tab-select`/`tab-close`, finish with `quit`. Every tab-affecting
//! result includes `{tab, tabs, url}` so the model always knows which tab it
//! is driving.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::browser::BrowserSession;

/// Default action timeout when a command carries none of its own.
pub const DEFAULT_TIMEOUT_MS: f64 = 35_000.0;

/// One command from the model, either a CLI subcommand or a serve-mode line.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "op")]
pub enum Command {
    /// Open the browser (serve mode only; one-shots launch implicitly).
    #[serde(rename = "open")]
    Open {
        #[serde(default)]
        url: Option<String>,
    },
    /// Navigate the live page.
    #[serde(rename = "goto")]
    Goto {
        url: String,
        #[serde(default)]
        wait: Option<String>,
    },
    /// Click the first element matching the CSS selector, or the node with
    /// this `ref` (a `backendNodeId` from `ax`).
    #[serde(rename = "click")]
    Click {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        node_ref: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Fill the first element matching the CSS selector, or the `ref` node.
    #[serde(rename = "fill")]
    Fill {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        node_ref: Option<u64>,
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// First element's textContent for the selector.
    #[serde(rename = "text")]
    Text {
        selector: String,
        #[serde(default)]
        timeout_ms: Option<f64>,
    },
    /// Evaluate a JS expression; result must be JSON-serializable.
    #[serde(rename = "eval")]
    Eval {
        expression: String,
        #[serde(default)]
        timeout_ms: Option<f64>,
    },
    /// Page title.
    #[serde(rename = "title")]
    Title {},
    /// Accessibility tree snapshot (compact, agent-friendly).
    /// One `Accessibility.getFullAXTree` round trip; returns a flat array of
    /// `{ref, role, name, value?}`: interactive nodes plus named content,
    /// with wrapper/duplicate text nodes dropped. `ref` is the node's
    /// `backendNodeId` and can be passed to `click`/`fill`. `all: true`
    /// returns every non-ignored node in the raw `{id, role, name,
    /// backendNodeId}` shape.
    #[serde(rename = "ax")]
    Ax {
        #[serde(default)]
        max_depth: Option<u32>,
        #[serde(default)]
        all: Option<bool>,
        #[serde(default)]
        timeout_ms: Option<f64>,
    },
    /// Current page URL and tab state.
    #[serde(rename = "url")]
    Url {},
    /// PNG screenshot to disk; result is the written path + byte length.
    #[serde(rename = "screenshot")]
    Screenshot {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        full_page: Option<bool>,
    },
    /// List open tabs: index, target id, and whether it is the active tab.
    #[serde(rename = "tab-list")]
    TabList {},
    /// Open a fresh tab (optionally at a URL) and make it active.
    #[serde(rename = "tab-new")]
    TabNew {
        #[serde(default)]
        url: Option<String>,
    },
    /// Switch the active tab by zero-based index.
    #[serde(rename = "tab-select")]
    TabSelect { index: usize },
    /// Close one tab by index (defaults to the active tab).
    #[serde(rename = "tab-close")]
    TabClose {
        #[serde(default)]
        index: Option<usize>,
    },
    /// Alias of `tab-close`.
    #[serde(rename = "close-page")]
    ClosePage {
        #[serde(default)]
        index: Option<usize>,
    },
    /// Close the session and exit (serve mode).
    #[serde(rename = "quit")]
    Quit {},
}

/// JSON response for one command.
#[derive(Debug, Serialize, PartialEq)]
pub struct Response {
    pub id: Value,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

/// Launch/session settings shared by one-shot and serve mode.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    /// Browser engine (`chrome` by default — the validated production path).
    pub engine: String,
    pub headed: bool,
    pub chromium: Option<String>,
    pub timeout_ms: f64,
    pub pretty: bool,
    /// Named background session (`--session <name|socket path>`): `serve`
    /// listens on it, `start` spawns that server detached, and every other
    /// command becomes a client call against the warm browser behind it.
    pub session: Option<String>,
    /// A session server with no traffic for this long shuts its browser down
    /// (0 = never). Keeps forgotten agent sessions from leaking Chrome.
    pub idle_timeout_s: u64,
}

/// Default idle shutdown for session servers.
pub const DEFAULT_IDLE_TIMEOUT_S: u64 = 1800;

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            engine: "chrome".to_string(),
            headed: false,
            chromium: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            pretty: false,
            session: None,
            idle_timeout_s: DEFAULT_IDLE_TIMEOUT_S,
        }
    }
}

/// Result of [`parse_args`]: launch config plus an optional one-shot command.
#[derive(Debug, PartialEq)]
pub struct ParsedArgs {
    pub config: SessionConfig,
    /// `None` selects serve mode (stdin/stdout protocol, or the socket when
    /// `config.session` is set).
    pub command: Option<Command>,
    /// `start`: spawn a detached session server for `config.session`.
    pub start: bool,
}

/// Help text for `--help` / argument errors.
pub fn usage(program: &str) -> String {
    format!(
        "Node-free realtime browser driver (RustWright + Chromium, Chrome by default).\n\
         \n\
         USAGE:\n  \
         {program} [GLOBAL] <open|goto|click|fill|text|eval|title|ax|url|tab-list|tab-new|tab-select|tab-close|screenshot> [ARGS]\n  \
         {program} [GLOBAL] serve\n  \
         {program} --session <name> <start|OP ...|quit>\n\
         \n\
         GLOBAL:\n  \
         --engine <chrome|lightpanda>   (default: chrome)\n  \
         --headed                       visible window (default: headless)\n  \
         --chromium <path>              browser executable override\n  \
         --timeout-ms <ms>              default action timeout (default: {DEFAULT_TIMEOUT_MS})\n  \
         --pretty                       pretty-print one-shot JSON responses\n  \
         --session <name|path>          named warm session over a Unix socket (or $BROWSER_TOOL_SESSION)\n  \
         --idle-timeout-s <s>           session server shuts down after this idle time (default: {DEFAULT_IDLE_TIMEOUT_S}, 0 = never)\n\
         \n\
         ONE-SHOT OPS (browser launches, runs one command, closes):\n  \
         goto --url <url> [--wait <load|domcontentloaded|commit>]\n  \
         click (--selector <css> | --ref <n>) [--timeout-ms <ms>]\n  \
         fill (--selector <css> | --ref <n>) --value <text> [--timeout-ms <ms>]\n  \
         text --selector <css> [--timeout-ms <ms>]\n  \
         eval --expression <js> [--timeout-ms <ms>]\n  \
         title | url | ax [--all] [--max-depth <n>]\n  \
         tab-list | tab-new [--url <url>] | tab-select --index <n>\n  \
         tab-close [--index <n>] | close-page [--index <n>]\n  \
         screenshot [--path <file>] [--full-page]\n\
         \n\
         SERVE MODE (one warm browser, JSON lines on stdin/stdout):\n  \
         {program} serve [--headed] [--engine <e>] [--chromium <p>] [--timeout-ms <ms>]\n  \
         {{\"id\":1,\"op\":\"goto\",\"url\":\"https://example.com\"}}\n  \
         {{\"id\":2,\"op\":\"eval\",\"expression\":\"() => document.title\"}}\n  \
         {{\"id\":3,\"op\":\"quit\"}}\n\
         \n\
         SESSIONS (one warm browser, one shell command per op — for agents):\n  \
         {program} --session s start          spawn a detached session server\n  \
         {program} --session s goto --url <url>\n  \
         {program} --session s ax             compact a11y snapshot; refs feed click/fill --ref\n  \
         {program} --session s quit           shut the browser down"
    )
}

fn next_value(args: &mut std::collections::VecDeque<String>, flag: &str) -> Result<String, String> {
    args.pop_front()
        .ok_or_else(|| format!("missing value after {flag}"))
}

/// Why [`parse_args`] failed: `Help` is a deliberate `--help` (exit 0),
/// `Invalid` is bad usage (exit 2).
#[derive(Debug, PartialEq)]
pub enum ArgsError {
    /// Usage text, requested explicitly (`--help`) or because no args given.
    Help(String),
    /// Invalid invocation; message may embed the usage text.
    Invalid(String),
}

fn parse_f64(raw: &str, flag: &str) -> Result<f64, String> {
    raw.parse::<f64>()
        .map_err(|_| format!("{flag} must be a number, got {raw:?}"))
}

fn parse_index(raw: &str) -> Result<usize, String> {
    raw.parse::<usize>()
        .map_err(|_| format!("--index must be a number, got {raw:?}"))
}

/// Scan one `--flag value` out of the remaining argv (errors on duplicates).
fn extract_value(
    args: &mut std::collections::VecDeque<String>,
    flag: &str,
) -> Result<Option<String>, String> {
    let mut value = None;
    let mut rest = std::collections::VecDeque::new();
    while let Some(arg) = args.pop_front() {
        if arg == flag {
            if value.is_some() {
                return Err(format!("duplicate {flag}"));
            }
            value = Some(next_value(args, flag)?);
        } else {
            rest.push_back(arg);
        }
    }
    *args = rest;
    Ok(value)
}

/// Scan one boolean `--flag` out of the remaining argv.
fn extract_present(args: &mut std::collections::VecDeque<String>, flag: &str) -> bool {
    let mut found = false;
    let mut rest = std::collections::VecDeque::new();
    while let Some(arg) = args.pop_front() {
        if arg == flag {
            found = true;
        } else {
            rest.push_back(arg);
        }
    }
    *args = rest;
    found
}

/// Global flags may appear before *or* after the command (`eval … --pretty`).
/// Consume any stragglers before leftovers are rejected.
fn consume_trailing_globals(
    args: &mut std::collections::VecDeque<String>,
    config: &mut SessionConfig,
) -> Result<(), String> {
    if let Some(raw) = extract_value(args, "--engine")? {
        config.engine = raw;
    }
    if extract_present(args, "--headed") {
        config.headed = true;
    }
    if let Some(path) = extract_value(args, "--chromium")? {
        config.chromium = Some(path);
    }
    if let Some(raw) = extract_value(args, "--timeout-ms")? {
        config.timeout_ms = parse_f64(&raw, "--timeout-ms")?;
    }
    if extract_present(args, "--pretty") {
        config.pretty = true;
    }
    if let Some(name) = extract_value(args, "--session")? {
        config.session = Some(name);
    }
    if let Some(raw) = extract_value(args, "--idle-timeout-s")? {
        config.idle_timeout_s = raw
            .parse::<u64>()
            .map_err(|_| format!("--idle-timeout-s must be whole seconds, got {raw:?}"))?;
    }
    Ok(())
}

/// Exactly one of `--selector <css>` / `--ref <backendNodeId>`.
fn target_flags(
    args: &mut std::collections::VecDeque<String>,
    op: &str,
) -> Result<(Option<String>, Option<u64>), ArgsError> {
    let selector = str_flag(args, "--selector")?;
    let node_ref = match str_flag(args, "--ref")? {
        Some(raw) => Some(raw.parse::<u64>().map_err(|_| {
            ArgsError::Invalid(format!("--ref must be a node ref number from `ax`, got {raw:?}"))
        })?),
        None => None,
    };
    if selector.is_some() == node_ref.is_some() {
        return Err(ArgsError::Invalid(format!(
            "{op} needs exactly one of --selector <css> or --ref <n>"
        )));
    }
    Ok((selector, node_ref))
}

/// `--flag <value>` for string arguments (absent → `None`).
fn str_flag(
    args: &mut std::collections::VecDeque<String>,
    flag: &str,
) -> Result<Option<String>, ArgsError> {
    extract_value(args, flag).map_err(ArgsError::Invalid)
}

/// `--timeout-ms <ms>` for action commands (absent → `None`).
fn numeric_flag(
    args: &mut std::collections::VecDeque<String>,
    flag: &str,
) -> Result<Option<f64>, ArgsError> {
    match extract_value(args, flag).map_err(ArgsError::Invalid)? {
        Some(raw) => Ok(Some(parse_f64(&raw, flag).map_err(ArgsError::Invalid)?)),
        None => Ok(None),
    }
}

/// Parse a full argv (`argv[0]` = program name) into a session config and,
/// for one-shot invocation, the single command to run.
///
/// `Err(ArgsError::Help)` means usage was requested (`--help`, `help`, or no
/// arguments at all); every other failure is `ArgsError::Invalid`.
pub fn parse_args<I: IntoIterator<Item = String>>(argv: I) -> Result<ParsedArgs, ArgsError> {
    let mut argv = argv.into_iter();
    let program = argv.next().unwrap_or_else(|| "browser-tool".to_string());
    let mut args: std::collections::VecDeque<String> = argv.collect();
    let mut config = SessionConfig::default();

    while args.front().is_some_and(|a| a.starts_with("--")) {
        let flag = args.pop_front().expect("front checked");
        match flag.as_str() {
            "--engine" => {
                config.engine = next_value(&mut args, "--engine").map_err(ArgsError::Invalid)?
            }
            "--headed" => config.headed = true,
            "--chromium" => {
                config.chromium =
                    Some(next_value(&mut args, "--chromium").map_err(ArgsError::Invalid)?)
            }
            "--timeout-ms" => {
                let raw = next_value(&mut args, "--timeout-ms").map_err(ArgsError::Invalid)?;
                config.timeout_ms = parse_f64(&raw, "--timeout-ms").map_err(ArgsError::Invalid)?;
            }
            "--pretty" => config.pretty = true,
            "--session" => {
                config.session =
                    Some(next_value(&mut args, "--session").map_err(ArgsError::Invalid)?)
            }
            "--idle-timeout-s" => {
                let raw =
                    next_value(&mut args, "--idle-timeout-s").map_err(ArgsError::Invalid)?;
                config.idle_timeout_s = raw.parse::<u64>().map_err(|_| {
                    ArgsError::Invalid(format!(
                        "--idle-timeout-s must be whole seconds, got {raw:?}"
                    ))
                })?;
            }
            "-h" | "--help" => return Err(ArgsError::Help(usage(&program))),
            other => {
                return Err(ArgsError::Invalid(format!(
                    "unknown flag {other:?}\n\n{}",
                    usage(&program)
                )))
            }
        }
    }

    let Some(op) = args.pop_front() else {
        return Err(ArgsError::Help(usage(&program)));
    };
    if op == "-h" || op == "--help" || op == "help" {
        return Err(ArgsError::Help(usage(&program)));
    }
    if op == "serve" {
        consume_trailing_globals(&mut args, &mut config).map_err(ArgsError::Invalid)?;
        if let Some(extra) = args.front() {
            return Err(ArgsError::Invalid(format!(
                "unexpected argument {extra:?} after serve"
            )));
        }
        return Ok(ParsedArgs {
            config,
            command: None,
            start: false,
        });
    }
    if op == "start" {
        consume_trailing_globals(&mut args, &mut config).map_err(ArgsError::Invalid)?;
        if let Some(extra) = args.front() {
            return Err(ArgsError::Invalid(format!(
                "unexpected argument {extra:?} after start"
            )));
        }
        return Ok(ParsedArgs {
            config,
            command: None,
            start: true,
        });
    }

    let command = match op.as_str() {
        "open" => Command::Open {
            url: str_flag(&mut args, "--url")?,
        },
        "goto" => Command::Goto {
            url: str_flag(&mut args, "--url")?
                .ok_or_else(|| ArgsError::Invalid("goto needs --url <url>".into()))?,
            wait: str_flag(&mut args, "--wait")?,
        },
        "click" => {
            let (selector, node_ref) = target_flags(&mut args, "click")?;
            Command::Click {
                selector,
                node_ref,
                timeout_ms: numeric_flag(&mut args, "--timeout-ms")?,
            }
        }
        "fill" => {
            let (selector, node_ref) = target_flags(&mut args, "fill")?;
            Command::Fill {
                selector,
                node_ref,
                value: str_flag(&mut args, "--value")?
                    .ok_or_else(|| ArgsError::Invalid("fill needs --value <text>".into()))?,
                timeout_ms: numeric_flag(&mut args, "--timeout-ms")?,
            }
        }
        "text" => Command::Text {
            selector: str_flag(&mut args, "--selector")?
                .ok_or_else(|| ArgsError::Invalid("text needs --selector <css>".into()))?,
            timeout_ms: numeric_flag(&mut args, "--timeout-ms")?,
        },
        "eval" => Command::Eval {
            expression: str_flag(&mut args, "--expression")?
                .ok_or_else(|| ArgsError::Invalid("eval needs --expression <js>".into()))?,
            timeout_ms: numeric_flag(&mut args, "--timeout-ms")?,
        },
        "title" => Command::Title {},
        "ax" => Command::Ax {
            max_depth: numeric_flag(&mut args, "--max-depth")?.map(|v| v as u32),
            all: extract_present(&mut args, "--all").then_some(true),
            timeout_ms: numeric_flag(&mut args, "--timeout-ms")?,
        },
        "url" => Command::Url {},
        // Mostly for sessions (`--session s quit` stops the server); as a
        // one-shot it just launches and closes.
        "quit" => Command::Quit {},
        "tab-list" => Command::TabList {},
        "tab-new" => Command::TabNew {
            url: str_flag(&mut args, "--url")?,
        },
        "tab-select" => Command::TabSelect {
            index: match str_flag(&mut args, "--index")? {
                Some(raw) => parse_index(&raw).map_err(ArgsError::Invalid)?,
                None => return Err(ArgsError::Invalid("tab-select needs --index <n>".into())),
            },
        },
        "tab-close" => Command::TabClose {
            index: str_flag(&mut args, "--index")?.map(|raw| parse_index(&raw)).transpose()
                .map_err(ArgsError::Invalid)?,
        },
        "close-page" => Command::ClosePage {
            index: str_flag(&mut args, "--index")?.map(|raw| parse_index(&raw)).transpose()
                .map_err(ArgsError::Invalid)?,
        },
        "screenshot" => Command::Screenshot {
            path: str_flag(&mut args, "--path")?,
            full_page: if extract_present(&mut args, "--full-page") {
                Some(true)
            } else {
                None
            },
        },
        unknown => {
            return Err(ArgsError::Invalid(format!(
                "unknown command {unknown:?}\n\n{}",
                usage(&program)
            )))
        }
    };
    if extract_present(&mut args, "--expression") {
        return Err(ArgsError::Invalid(
            "only `eval` takes --expression; see usage".into(),
        ));
    }
    consume_trailing_globals(&mut args, &mut config).map_err(ArgsError::Invalid)?;
    if let Some(extra) = args.front() {
        return Err(ArgsError::Invalid(format!(
            "unexpected argument {extra:?} for `{op}`"
        )));
    }
    Ok(ParsedArgs {
        config,
        command: Some(command),
        start: false,
    })
}

/// A live browser plus the policy defaults the model gets for free.
///
/// Construct once per session; every [`Command`] runs against the same warm
/// Chrome (no relaunch between ops).
pub struct Driver {
    session: BrowserSession,
}

impl Driver {
    /// Launch the engine described by `config` (Chrome by default).
    pub fn launch(config: &SessionConfig) -> anyhow::Result<Self> {
        let session = BrowserSession::launch(
            &config.engine,
            !config.headed,
            config.chromium.as_deref(),
            config.timeout_ms,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "browser launch failed (engine={}, headed={}): {e:#}",
                config.engine,
                config.headed
            )
        })?;
        Ok(Self {
            session,
        })
    }

    /// The active page (tab) this driver's ops target.
    pub fn session(&self) -> &BrowserSession {
        &self.session
    }

    /// Active tab state the model needs after every op: index, tab count,
    /// and current URL.
    fn state_json(&self) -> Value {
        json!({
            "tab": self.session.active_page(),
            "tabs": self.session.page_count(),
            "url": self.session.url(),
        })
    }

    /// Tab inventory for `tab-list` / `tab-new` results.
    fn tabs_json(&self) -> Value {
        let active = self.session.active_page();
        let tabs: Vec<Value> = self
            .session
            .page_targets()
            .into_iter()
            .enumerate()
            .map(|(index, target)| {
                json!({ "index": index, "target": target, "active": index == active })
            })
            .collect();
        json!({ "tabs": tabs, "active": active, "url": self.session.url() })
    }

    /// Run one op; returns the JSON `result` value.
    pub fn run(&mut self, command: &Command) -> anyhow::Result<Value> {
        match command {
            Command::Open { url } => {
                if let Some(url) = url {
                    self.goto(url, None)?;
                }
                Ok(self.state_json())
            }
            Command::Goto { url, wait } => {
                self.goto(url, wait.as_deref())?;
                Ok(self.state_json())
            }
            Command::Click {
                selector,
                node_ref,
                timeout_ms,
            } => {
                let label = target_label(selector.as_deref(), *node_ref);
                match (selector.as_deref(), *node_ref) {
                    (Some(sel), _) => self.session.click(sel, *timeout_ms),
                    (None, Some(node)) => self.session.click_ref(node, *timeout_ms),
                    (None, None) => Err(anyhow::anyhow!("needs `selector` or `ref`")),
                }
                .map_err(|e| anyhow::anyhow!("click {label}: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Fill {
                selector,
                node_ref,
                value,
                timeout_ms,
            } => {
                let label = target_label(selector.as_deref(), *node_ref);
                match (selector.as_deref(), *node_ref) {
                    (Some(sel), _) => self.session.fill(sel, value, *timeout_ms),
                    (None, Some(node)) => self.session.fill_ref(node, value, *timeout_ms),
                    (None, None) => Err(anyhow::anyhow!("needs `selector` or `ref`")),
                }
                .map_err(|e| anyhow::anyhow!("fill {label}: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Text {
                selector,
                timeout_ms,
            } => {
                let text = self
                    .session
                    .text_with_timeout(selector, *timeout_ms)
                    .map_err(|e| anyhow::anyhow!("text {selector:?}: {e:#}"))?;
                Ok(json!(text))
            }
            Command::Eval {
                expression,
                timeout_ms,
            } => {
                let value: serde_json::Value = self
                    .session
                    .evaluate_with_timeout(expression, *timeout_ms)
                    .map_err(|e| {
                        anyhow::anyhow!("eval failed: {e:#}\nexpression: {expression}")
                    })?;
                Ok(value)
            }
            Command::Title {} => {
                let title = self
                    .session
                    .title()
                    .map_err(|e| anyhow::anyhow!("title: {e:#}"))?;
                Ok(json!(title))
            }
            Command::Ax {
                max_depth,
                all,
                timeout_ms,
            } => {
                let tree = self
                    .session
                    .ax_tree(*max_depth, all.unwrap_or(false), *timeout_ms)
                    .map_err(|e| anyhow::anyhow!("ax: {e:#}"))?;
                Ok(tree)
            }
            Command::Url {} => Ok(self.state_json()),
            Command::TabList {} => Ok(self.tabs_json()),
            Command::TabNew { url } => {
                self.session
                    .new_page()
                    .map_err(|e| anyhow::anyhow!("tab-new: {e:#}"))?;
                if let Some(url) = url {
                    self.goto(url, None)?;
                }
                Ok(self.tabs_json())
            }
            Command::TabSelect { index } => {
                self.session
                    .select_page(*index)
                    .map_err(|e| anyhow::anyhow!("tab-select {index}: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::TabClose { index } | Command::ClosePage { index } => {
                let index = index.unwrap_or_else(|| self.session.active_page());
                self.session
                    .close_page(index)
                    .map_err(|e| anyhow::anyhow!("tab-close {index}: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Screenshot { path, full_page } => {
                let path = path.clone().unwrap_or_else(|| {
                    format!(
                        "shot-{}.png",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    )
                });
                let bytes = self
                    .session
                    .screenshot_png(full_page.unwrap_or(false))
                    .map_err(|e| anyhow::anyhow!("screenshot: {e:#}"))?;
                std::fs::write(&path, &bytes)
                    .map_err(|e| anyhow::anyhow!("screenshot: writing {path}: {e:#}"))?;
                Ok(json!({ "path": path, "bytes": bytes.len() }))
            }
            Command::Quit {} => Ok(json!({ "bye": true })),
        }
    }

    /// Navigate the active page, applying the engine-aware policy.
    fn goto(&self, url: &str, wait: Option<&str>) -> anyhow::Result<()> {
        // Default path already applies the engine-aware policy (Lightpanda:
        // short `commit` wait + DOM confirmation).
        if wait.is_none() {
            return self
                .session
                .goto(url)
                .map_err(|e| anyhow::anyhow!("goto {url}: {e:#}"));
        }
        let wait = wait.unwrap_or("load");
        if self.session.is_lightpanda() || wait == "commit" {
            return self
                .session
                .goto_commit(url, None)
                .map_err(|e| anyhow::anyhow!("goto {url} (wait=commit): {e:#}"));
        }
        // wait == "load" (or anything else): full load wait via goto().
        self.session
            .goto(url)
            .map_err(|e| anyhow::anyhow!("goto {url} (wait={wait}): {e:#}"))
    }
}

/// Human-readable click/fill target for error messages.
fn target_label(selector: Option<&str>, node_ref: Option<u64>) -> String {
    match (selector, node_ref) {
        (Some(sel), _) => format!("{sel:?}"),
        (None, Some(node)) => format!("ref {node}"),
        (None, None) => "(no target)".to_string(),
    }
}

/// Parse and run one protocol line against the live driver.
///
/// Returns `None` for blank lines, else the response plus whether the line
/// was `quit` (the caller owns shutdown). Shared by stdin `serve` and the
/// socket session server so both speak exactly the same protocol.
pub(crate) fn handle_line(driver: &mut Driver, raw: &str) -> Option<(Response, bool)> {
    if raw.trim().is_empty() {
        return None;
    }
    let (id, command) = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(mut map)) => {
            let id = map.remove("id").unwrap_or(Value::Null);
            match serde_json::from_value::<Command>(Value::Object(map)) {
                Ok(command) => (id, command),
                Err(e) => {
                    return Some((
                        err_response(id, format!("bad command: {e}"), Instant::now()),
                        false,
                    ))
                }
            }
        }
        _ => {
            return Some((
                err_response(
                    Value::Null,
                    "each line must be a JSON object".into(),
                    Instant::now(),
                ),
                false,
            ))
        }
    };
    let started = Instant::now();
    let is_quit = matches!(command, Command::Quit {});
    let response = match driver.run(&command) {
        Ok(result) => ok_response(id, result, started),
        Err(e) => err_response(id, format!("{e:#}"), started),
    };
    Some((response, is_quit))
}

pub(crate) fn write_response(
    output: &mut dyn Write,
    response: &Response,
    pretty: bool,
) -> io::Result<()> {
    let line = if pretty {
        serde_json::to_string_pretty(response)
    } else {
        serde_json::to_string(response)
    }
    .expect("response serializes");
    writeln!(output, "{line}")?;
    output.flush()
}

fn ok_response(id: Value, result: Value, started: Instant) -> Response {
    Response {
        id,
        ok: true,
        result: Some(result),
        error: None,
        elapsed_ms: started.elapsed().as_millis(),
    }
}

fn err_response(id: Value, error: String, started: Instant) -> Response {
    Response {
        id,
        ok: false,
        result: None,
        error: Some(error),
        elapsed_ms: started.elapsed().as_millis(),
    }
}

/// Serve mode: launch one warm browser, then read JSON commands from `input`
/// and write one JSON response per line to `output`.
///
/// Returns the process exit code (`0` after `quit` or EOF, `1` on launch /
/// IO failure). Logs go to stderr so stdout stays protocol-clean.
/// Peak RSS of this process in KB (VmHWM from /proc/self/status), if readable.
/// Used for the BT_RSS_REPORT diagnostic.
fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

pub fn serve(
    config: &SessionConfig,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> ExitCode {
    let mut driver = match Driver::launch(config) {
        Ok(driver) => driver,
        Err(e) => {
            eprintln!("browser-tool: {e:#}");
            return ExitCode::from(1);
        }
    };
    eprintln!(
        "browser-tool: live session on {} (engine={}); send JSON lines, {{\"op\":\"quit\"}} to exit",
        driver.session().endpoint(),
        config.engine,
    );

    for raw in input.lines() {
        let raw = match raw {
            Ok(line) => line,
            Err(e) => {
                eprintln!("browser-tool: stdin read failed: {e}");
                return ExitCode::from(1);
            }
        };
        let Some((response, is_quit)) = handle_line(&mut driver, &raw) else {
            continue;
        };
        // Serve mode is a line protocol: one response == one line, always.
        // (`--pretty` would split a response across lines and desync any
        // line-reading client; it only applies to one-shot output.)
        if let Err(e) = write_response(output, &response, false) {
            eprintln!("[browser-tool] failed to write response: {e:#}");
            return ExitCode::from(1);
        }
        if is_quit {
            driver.session().close();
            if std::env::var("BT_RSS_REPORT").is_ok() {
                eprintln!("[browser-tool] peak RSS at quit: {:?} KB", peak_rss_kb());
            }
            return ExitCode::SUCCESS;
        }
    }
    // EOF: shut the session down cleanly.
    driver.session().close();
    eprintln!("browser-tool: stdin closed, session shut down");
    if std::env::var("BT_RSS_REPORT").is_ok() {
        eprintln!("[browser-tool] peak RSS at EOF-shutdown: {:?} KB", peak_rss_kb());
    }
    ExitCode::SUCCESS
}

/// One-shot mode: launch the browser, run one command, close, write response.
pub fn oneshot(
    config: &SessionConfig,
    command: &Command,
    output: &mut dyn Write,
) -> ExitCode {
    let started = Instant::now();
    let mut driver = match Driver::launch(config) {
        Ok(driver) => driver,
        Err(e) => {
            eprintln!("browser-tool: {e:#}");
            return ExitCode::from(1);
        }
    };
    let response = match driver.run(command) {
        Ok(result) => ok_response(Value::Null, result, started),
        Err(e) => err_response(Value::Null, format!("{e:#}"), started),
    };
    driver.session().close();
    let ok = response.ok;
    if write_response(output, &response, config.pretty).is_err() {
        return ExitCode::from(1);
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_one_shot_eval_with_trailing_global_flag() {
        let parsed = parse_args(argv(&[
            "browser-tool",
            "eval",
            "--expression",
            "() => 40 + 2",
            "--pretty",
        ]))
        .expect("valid invocation");
        assert!(parsed.config.pretty, "globals may trail the command");
        assert_eq!(parsed.config.engine, "chrome");
        assert_eq!(
            parsed.command,
            Some(Command::Eval {
                expression: "() => 40 + 2".into(),
                timeout_ms: None,
            })
        );
    }

    #[test]
    fn parses_globals_before_command() {
        let parsed = parse_args(argv(&[
            "browser-tool",
            "--headed",
            "--engine",
            "chrome",
            "--chromium",
            "C:/chrome.exe",
            "goto",
            "--url",
            "https://example.com",
            "--wait",
            "domcontentloaded",
        ]))
        .expect("valid invocation");
        assert!(parsed.config.headed);
        assert_eq!(parsed.config.engine, "chrome");
        assert_eq!(parsed.config.chromium.as_deref(), Some("C:/chrome.exe"));
        assert_eq!(
            parsed.command,
            Some(Command::Goto {
                url: "https://example.com".into(),
                wait: Some("domcontentloaded".into()),
            })
        );
    }

    #[test]
    fn serve_mode_has_no_command() {
        let parsed = parse_args(argv(&["browser-tool", "serve", "--timeout-ms", "5000"]))
            .expect("valid invocation");
        assert!(parsed.command.is_none());
        assert_eq!(parsed.config.timeout_ms, 5000.0);
    }

    #[test]
    fn help_and_invalid_invocations_are_distinguished() {
        let help = parse_args(argv(&["browser-tool", "--help"])).expect_err("help");
        assert!(
            matches!(help, ArgsError::Help(_)),
            "explicit help is Help, not Invalid: {help:?}"
        );

        let no_args = parse_args(argv(&["browser-tool"])).expect_err("no args");
        assert!(matches!(no_args, ArgsError::Help(_)));

        let unknown = parse_args(argv(&["browser-tool", "frobnicate"])).expect_err("unknown");
        match unknown {
            ArgsError::Invalid(message) => {
                assert!(message.contains("unknown command"), "{message}");
                assert!(message.contains("USAGE:"), "errors embed usage");
            }
            other => panic!("unknown command must be Invalid: {other:?}"),
        }

        let missing = parse_args(argv(&["browser-tool", "goto"])).expect_err("missing url");
        assert!(matches!(missing, ArgsError::Invalid(_)));
    }

    #[test]
    fn tab_commands_parse_indexes() {
        let parsed = parse_args(argv(&["browser-tool", "tab-select", "--index", "2"]))
            .expect("valid invocation");
        assert_eq!(
            parsed.command,
            Some(Command::TabSelect { index: 2 })
        );

        let parsed =
            parse_args(argv(&["browser-tool", "tab-close"])).expect("valid invocation");
        assert_eq!(parsed.command, Some(Command::TabClose { index: None }));

        assert!(parse_args(argv(&["browser-tool", "tab-select"])).is_err());
        assert!(parse_args(argv(&["browser-tool", "tab-select", "--index", "x"])).is_err());
    }

    #[test]
    fn command_deserializes_from_protocol_json() {
        let command: Command = serde_json::from_str(
            r##"{"op":"fill","selector":"#kw","value":"600-10070","timeout_ms":10000}"##,
        )
        .expect("valid command");
        assert_eq!(
            command,
            Command::Fill {
                selector: Some("#kw".into()),
                node_ref: None,
                value: "600-10070".into(),
                timeout_ms: Some(10000.0),
            }
        );
    }

    #[test]
    fn click_and_fill_take_a_selector_or_an_ax_ref() {
        let parsed = parse_args(argv(&["browser-tool", "click", "--ref", "42"]))
            .expect("valid invocation");
        assert_eq!(
            parsed.command,
            Some(Command::Click {
                selector: None,
                node_ref: Some(42),
                timeout_ms: None,
            })
        );
        let both = parse_args(argv(&[
            "browser-tool", "click", "--ref", "1", "--selector", "a",
        ]));
        assert!(both.is_err(), "selector and ref are mutually exclusive");
        assert!(parse_args(argv(&["browser-tool", "fill", "--value", "x"])).is_err());

        let command: Command =
            serde_json::from_str(r#"{"op":"fill","ref":7,"value":"hi"}"#).expect("ref json");
        assert_eq!(
            command,
            Command::Fill {
                selector: None,
                node_ref: Some(7),
                value: "hi".into(),
                timeout_ms: None,
            }
        );
    }

    #[test]
    fn commands_round_trip_through_json_for_session_clients() {
        for command in [
            Command::Goto {
                url: "https://example.com".into(),
                wait: None,
            },
            Command::Click {
                selector: None,
                node_ref: Some(9),
                timeout_ms: Some(500.0),
            },
            Command::Ax {
                max_depth: None,
                all: Some(true),
                timeout_ms: None,
            },
            Command::Quit {},
        ] {
            let wire = serde_json::to_string(&command).expect("serializes");
            let back: Command = serde_json::from_str(&wire).expect("deserializes");
            assert_eq!(back, command, "{wire}");
        }
        let wire = serde_json::to_string(&Command::Click {
            selector: None,
            node_ref: Some(9),
            timeout_ms: None,
        })
        .unwrap();
        assert_eq!(wire, r#"{"op":"click","ref":9}"#);
    }

    #[test]
    fn session_flags_and_start() {
        let parsed = parse_args(argv(&[
            "browser-tool", "--session", "work", "--idle-timeout-s", "60", "start",
        ]))
        .expect("valid invocation");
        assert!(parsed.start);
        assert!(parsed.command.is_none());
        assert_eq!(parsed.config.session.as_deref(), Some("work"));
        assert_eq!(parsed.config.idle_timeout_s, 60);

        let parsed = parse_args(argv(&["browser-tool", "title", "--session", "work"]))
            .expect("trailing --session");
        assert_eq!(parsed.config.session.as_deref(), Some("work"));
        assert!(!parsed.start);
    }

    #[test]
    fn response_serializes_protocol_shape() {
        let ok = ok_response(
            serde_json::json!(7),
            serde_json::json!({"url": "https://example.com/"}),
            Instant::now(),
        );
        let line = serde_json::to_string(&ok).expect("serializes");
        assert!(line.contains("\"id\":7"));
        assert!(line.contains("\"ok\":true"));
        assert!(!line.contains("error"), "successful responses omit `error`");

        let err = err_response(Value::Null, "boom".into(), Instant::now());
        let line = serde_json::to_string(&err).expect("serializes");
        assert!(line.contains("\"ok\":false") && line.contains("\"error\":\"boom\""));
    }
}

