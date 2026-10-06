//! Named background sessions: one warm browser behind a Unix socket, driven
//! by one shell command per op.
//!
//! Why this exists: `serve` keeps a browser warm, but only for as long as the
//! caller holds its stdin open. An AI agent driving a shell runs each tool
//! call as a separate command, so it cannot keep that pipe alive between
//! calls — which is exactly why agents end up writing Python/Node wrapper
//! scripts around `serve`. A session server owns the browser instead:
//!
//! ```text
//! browser-tool --session s start              # detached server, returns at once
//! browser-tool --session s goto --url https://example.com
//! browser-tool --session s ax                 # compact a11y view with refs
//! browser-tool --session s click --ref 12
//! browser-tool --session s quit               # browser + server shut down
//! ```
//!
//! The wire format on the socket is the `serve` protocol verbatim (one JSON
//! command per line, one JSON response per line), so any client that can
//! talk to `serve` can talk to a session too.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::protocol::{self, Command, Driver, SessionConfig};

/// Socket path for a session: a value containing `/` is used as-is, a bare
/// name maps to `$TMPDIR/browser-tool-<name>.sock`.
pub fn socket_path(name: &str) -> PathBuf {
    if name.contains('/') {
        PathBuf::from(name)
    } else {
        std::env::temp_dir().join(format!("browser-tool-{name}.sock"))
    }
}

/// `$BROWSER_TOOL_SESSION`, if set: the default session for client calls.
pub fn env_session() -> Option<String> {
    std::env::var("BROWSER_TOOL_SESSION")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn print_json(output: &mut dyn Write, value: &Value, pretty: bool) {
    let text = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .unwrap_or_else(|_| value.to_string());
    let _ = writeln!(output, "{text}");
    let _ = output.flush();
}

/// Session server: launch the browser, then serve the line protocol on the
/// socket, one connection at a time, until `quit` or the idle timeout.
pub fn serve_socket(config: &SessionConfig, name: &str) -> ExitCode {
    let path = socket_path(name);
    if UnixStream::connect(&path).is_ok() {
        eprintln!(
            "browser-tool: a session is already listening on {}",
            path.display()
        );
        return ExitCode::from(1);
    }
    // A socket file nobody answers on is left over from a crashed server.
    let _ = std::fs::remove_file(&path);

    let mut driver = match Driver::launch(config) {
        Ok(driver) => driver,
        Err(e) => {
            eprintln!("browser-tool: {e:#}");
            return ExitCode::from(1);
        }
    };
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("browser-tool: bind {}: {e}", path.display());
            driver.session().close();
            return ExitCode::from(1);
        }
    };
    eprintln!(
        "browser-tool: session listening on {} ({})",
        path.display(),
        driver.session().endpoint()
    );

    let last_activity = Arc::new(AtomicU64::new(now_s()));
    if config.idle_timeout_s > 0 {
        spawn_idle_watchdog(path.clone(), Arc::clone(&last_activity), config.idle_timeout_s);
    }

    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        last_activity.store(now_s(), Ordering::Relaxed);
        if serve_connection(&mut driver, stream, &last_activity) {
            break;
        }
    }
    driver.session().close();
    crate::timing::report();
    let _ = std::fs::remove_file(&path);
    eprintln!("browser-tool: session {} shut down", path.display());
    ExitCode::SUCCESS
}

/// Serve one client connection; returns true once a `quit` was answered.
fn serve_connection(driver: &mut Driver, stream: UnixStream, last: &AtomicU64) -> bool {
    let reader = match stream.try_clone() {
        Ok(read_half) => BufReader::new(read_half),
        Err(_) => return false,
    };
    let mut writer = stream;
    for line in reader.lines() {
        let Ok(raw) = line else { return false };
        last.store(now_s(), Ordering::Relaxed);
        let Some((response, is_quit)) = protocol::handle_line(driver, &raw) else {
            continue;
        };
        let written = protocol::write_response(&mut writer, &response, false);
        last.store(now_s(), Ordering::Relaxed);
        if is_quit {
            return true;
        }
        if written.is_err() {
            return false;
        }
    }
    false
}

/// After `idle_s` without traffic, send ourselves a `quit` over the socket so
/// the browser shuts down through the normal path.
fn spawn_idle_watchdog(path: PathBuf, last: Arc<AtomicU64>, idle_s: u64) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(idle_s.clamp(1, 5)));
        if now_s().saturating_sub(last.load(Ordering::Relaxed)) < idle_s {
            continue;
        }
        eprintln!("browser-tool: idle for {idle_s}s, shutting the session down");
        if let Ok(mut stream) = UnixStream::connect(&path) {
            let _ = writeln!(stream, "{{\"op\":\"quit\"}}");
            let _ = stream.flush();
            let mut reply = String::new();
            let _ = BufReader::new(stream).read_line(&mut reply);
        }
        break;
    });
}

/// Client: send one command to the session server and print its response.
pub fn client(
    config: &SessionConfig,
    name: &str,
    command: &Command,
    output: &mut dyn Write,
) -> ExitCode {
    let path = socket_path(name);
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(e) => {
            let error = format!(
                "no browser-tool session at {} ({e}); start one with `browser-tool --session {name} start`",
                path.display()
            );
            print_json(
                output,
                &json!({ "id": null, "ok": false, "error": error }),
                config.pretty,
            );
            return ExitCode::from(1);
        }
    };
    let mut request = match serde_json::to_value(command) {
        Ok(value) => value,
        Err(e) => {
            print_json(
                output,
                &json!({ "id": null, "ok": false, "error": format!("encode command: {e}") }),
                config.pretty,
            );
            return ExitCode::from(1);
        }
    };
    request["id"] = json!(1);
    let mut writer = &stream;
    if writeln!(writer, "{request}").and_then(|_| writer.flush()).is_err() {
        print_json(
            output,
            &json!({ "id": null, "ok": false, "error": "session closed the connection" }),
            config.pretty,
        );
        return ExitCode::from(1);
    }
    // Done sending: the server sees EOF after answering and takes the next
    // client, so a crashed client can't wedge the session.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut line = String::new();
    let _ = BufReader::new(&stream).read_line(&mut line);
    let response: Value = match serde_json::from_str(line.trim()) {
        Ok(value) => value,
        Err(_) => json!({
            "id": null,
            "ok": false,
            "error": "session ended without a response (see its log next to the socket)",
        }),
    };
    let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(false);
    print_json(output, &response, config.pretty);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn fail(output: &mut dyn Write, pretty: bool, error: String) -> ExitCode {
    print_json(
        output,
        &json!({ "id": null, "ok": false, "error": error }),
        pretty,
    );
    ExitCode::from(1)
}

/// `start`: spawn a detached session server and wait until it accepts.
pub fn start(config: &SessionConfig, name: &str, output: &mut dyn Write) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let path = socket_path(name);
    let log_path = path.with_extension("log");
    let (socket_str, log_str) = (path.display().to_string(), log_path.display().to_string());
    if UnixStream::connect(&path).is_ok() {
        print_json(
            output,
            &json!({ "id": null, "ok": true, "result": {
                "session": name, "socket": socket_str, "log": log_str, "already_running": true,
            }}),
            config.pretty,
        );
        return ExitCode::SUCCESS;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return fail(output, config.pretty, format!("locate browser-tool binary: {e}")),
    };
    let log = match std::fs::File::create(&log_path) {
        Ok(file) => file,
        Err(e) => return fail(output, config.pretty, format!("create {}: {e}", log_path.display())),
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--session")
        .arg(name)
        .arg("--engine")
        .arg(&config.engine)
        .arg("--timeout-ms")
        .arg(config.timeout_ms.to_string())
        .arg("--idle-timeout-s")
        .arg(config.idle_timeout_s.to_string());
    if config.headed {
        cmd.arg("--headed");
    }
    if let Some(chromium) = &config.chromium {
        cmd.arg("--chromium").arg(chromium);
    }
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        // Own process group: the server outlives this command and the
        // agent's shell that ran it.
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return fail(output, config.pretty, format!("spawn session server: {e}")),
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if UnixStream::connect(&path).is_ok() {
            print_json(
                output,
                &json!({ "id": null, "ok": true, "result": {
                    "session": name, "socket": socket_str, "log": log_str, "pid": child.id(),
                }}),
                config.pretty,
            );
            return ExitCode::SUCCESS;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail: String = tail
                .lines()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            return fail(
                output,
                config.pretty,
                format!("session server exited during startup ({status}):\n{tail}"),
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return fail(
                output,
                config.pretty,
                format!("session server did not listen within 60s; see {}", log_path.display()),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
