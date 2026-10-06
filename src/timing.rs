//! Opt-in phase timings: set `BT_TIMINGS=1` and browser-tool prints one
//! `BT_TIMINGS {"phase": ms, ...}` line to stderr when it shuts down.
//!
//! This is how launch and teardown cost is attributed (spawn → DevTools URL,
//! WebSocket connect, first page, close) instead of guessed; the benchmark
//! ladder records these lines for every browser-tool run.

use std::sync::Mutex;
use std::time::Instant;

static PHASES: Mutex<Vec<(&'static str, f64)>> = Mutex::new(Vec::new());

/// True when `BT_TIMINGS` is set.
pub fn enabled() -> bool {
    std::env::var_os("BT_TIMINGS").is_some()
}

/// Record `name` as the time elapsed since `since`.
pub fn record(name: &'static str, since: Instant) {
    if !enabled() {
        return;
    }
    if let Ok(mut phases) = PHASES.lock() {
        phases.push((name, since.elapsed().as_secs_f64() * 1000.0));
    }
}

/// Print the recorded phases (once, at shutdown) when enabled.
pub fn report() {
    if !enabled() {
        return;
    }
    let Ok(phases) = PHASES.lock() else { return };
    let map: serde_json::Map<String, serde_json::Value> = phases
        .iter()
        .map(|(name, ms)| (name.to_string(), serde_json::json!((ms * 1000.0).round() / 1000.0)))
        .collect();
    eprintln!("BT_TIMINGS {}", serde_json::Value::Object(map));
}

/// Diagnostic line on stderr, only with `BT_VERBOSE` set (or `BT_TIMINGS`):
/// agents read stderr too, so routine launch chatter costs them tokens.
pub fn log(line: &str) {
    if std::env::var_os("BT_VERBOSE").is_some() || enabled() {
        eprintln!("{line}");
    }
}
