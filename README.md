# native-cdp-cli

**`browser-tool`** — a fast, Node-free CLI for the Chrome DevTools Protocol.
One Rust binary drives headless Chromium: persistent `serve` mode (JSON lines
on stdin/stdout, one warm browser) plus one-shot commands. No Node, no
`node_modules`, no per-command browser relaunch.

```sh
cargo build --release --bin browser-tool

# one warm browser, realtime commands
./target/release/browser-tool serve
{"id":1,"op":"goto","url":"https://example.com"}
{"id":2,"op":"eval","expression":"() => document.title"}
{"id":3,"op":"quit"}

# or one-shot
./target/release/browser-tool eval --expression "() => document.title" --url https://example.com
```

See [docs/browser-tool.md](docs/browser-tool.md) for the full op reference.

### Agent use: named sessions

`serve` stays warm only while its caller holds stdin open — an AI agent's
shell runs every tool call as a separate process, so it can't. A named
session puts the warm browser behind a Unix socket instead, one command per
step:

```sh
browser-tool --session work start                 # detached server, returns at once
browser-tool --session work goto --url https://example.com
browser-tool --session work ax                    # compact a11y view: [{ref, role, name}]
browser-tool --session work click --ref 12        # act on a ref from `ax`
browser-tool --session work quit
```

The socket speaks the `serve` protocol verbatim. An idle session shuts down
after 30 min (`--idle-timeout-s`). `click`/`fill` wait for their selector
(up to `--timeout-ms`), and clicks are trusted CDP mouse events at the
element's centre. The agent skill lives in
[`.claude/skills/browser-tool/SKILL.md`](.claude/skills/browser-tool/SKILL.md)
(picked up automatically in this repo; copy the folder to
`~/.claude/skills/` to use it anywhere).

## Benchmarks

Same scripted session (launch → goto listing → title/count/extract evals →
fill+click filter → visible-count eval → screenshot → new tab → goto detail →
title/row-count evals → close) driven against the same headless browser by
each driver. Fixtures are deterministic and local (500-card listing,
1000-row detail). Run in CI via `.github/workflows/bench.yml`.

| contender | session wall (best of 3) | warm eval mean (200×) | cold start (best of 5) | driver CPU (own) | driver peak RSS (own) | browser memory (PSS) |
|---|---|---|---|---|---|---|
| `bt-brave` (this repo on Brave) | **0.61s** | 0.59 ms | 0.34s | **10 ms** | **5 MB** | 387 MB |
| `bt-serve` (this repo on Chrome) | 0.65s | 0.68 ms | 0.31s | **10 ms** | **5 MB** | 307 MB |
| `bt-edge` (this repo on Edge) | 0.69s | 0.61 ms | 0.35s | **10 ms** | **5 MB** | 472 MB |
| `gorod` (go-rod 0.116.2, Go) | 0.71s | **0.56 ms** | **0.31s** | **20 ms** | 13 MB | 325 MB |
| `chromiumoxide` 0.7 (Rust) | 0.83s | 0.95 ms | 0.39s | **20 ms** | 9 MB | 394 MB |
| `chromedp` 0.19.1 (Go) | 0.99s | 0.65 ms | 0.49s | **30 ms** | 11 MB | 466 MB |
| `chromey` 2.x (Rust, maintained chromiumoxide fork) | 1.01s | 0.94 ms | 0.37s | 50 ms | 16 MB | 438 MB |
| `puppeteer-core` (Node) | 1.05s | 0.85 ms | 0.63s | 0.40 s | 82 MB | 371 MB |
| `playwright-core` (Node) | 1.27s | 1.36 ms | 0.77s | 0.69 s | 151 MB | 386 MB |
| `bt-lightpanda` (this repo on Lightpanda, 1 tab) | 0.18s | 0.24 ms | 0.11s | **<10 ms** | **5 MB** | n/a |

Run [37412581539](https://github.com/andrey-usa/native-cdp-cli/actions/runs/37412581539)
(2026-10-06, GitHub-hosted `ubuntu-latest`: AMD EPYC 7763 64-Core Processor — 4 vCPU · Chrome: Google Chrome 154.0.8037.57). Every driver produced
byte-identical extracted data and counts (correctness gate); wall is best-of-N.
GitHub runners vary between runs, so compare rows within one run.

**How driver CPU/RSS are measured.** Each driver's *own* `utime+stime`,
read from its zombie's `/proc/<pid>/stat` before reaping (`waitid` with
`WNOWAIT`), and its own VmHWM. The kernel counts CPU in 10 ms ticks, so
values under ~50 ms are a tie. Not `wait4`: its rusage is RUSAGE_BOTH and
adds in every child the driver reaped. browser-tool, chromedp, puppeteer
and playwright `wait()` on their Chrome, so `wait4` charged them Chrome's
CPU and RSS; go-rod (its leakless helper reaps Chrome) and chromiumoxide
were never charged. Earlier versions of this table reported exactly that
artifact ("0.82s vs 0.03s"). Browser memory is the whole browser process
tree (every renderer/GPU/utility process), sampled as summed PSS every
250 ms (shared pages counted once).

**Reading the table.** Session wall order: bt-brave 0.61s, bt-serve 0.65s, bt-edge 0.69s, gorod 0.71s, chromiumoxide 0.83s, chromedp 0.99s, chromey 1.01s, puppeteer 1.05s, playwright 1.27s. browser-tool's best
(bt-brave) ranks #1 of 9. All native drivers (browser-tool,
go-rod, chromiumoxide, chromedp) spend tens of milliseconds of their own CPU
or less; the Node drivers spend hundreds and carry 80–150 MB of their own RSS.

### Real-world (public internet, best of 3, same run)

example.com goto → title/h1: 9 of 9 pass; gorod 0.43s, bt-serve 0.47s, chromey 0.48s, chromiumoxide 0.48s, bt-brave 0.52s, bt-edge 0.53s, chromedp 0.64s, puppeteer 0.76s, playwright 0.84s.

GitHub browse (awesome-list scroll + trending click-through): 9 of 9 pass; gorod 3.64s, bt-serve 3.68s, chromey 3.90s, bt-edge 3.96s, chromiumoxide 3.99s, bt-brave 4.09s, puppeteer 4.20s, chromedp 4.31s, playwright 4.35s.
Live pages change between runs, so these are informational, not part of the
correctness gate. (The scroll check used to fail at random for every driver:
GitHub sets CSS `scroll-behavior: smooth`, so `scrollY` was read mid-animation;
contenders now scroll with `behavior: 'instant'`.)

**This build vs the previous one, same machine and run** (`bt-baseline`,
built from the previous master by the ladder's `baseline_ref` A/B):
session 0.84s → 0.65s (-22%), cold start 0.46s → 0.31s (-33%), browser CPU per session 1.65s → 1.15s. The gain is launch and teardown:
the standard automation flags go-rod and chromiumoxide ship, no unused
startup tab, and killing the throwaway-profile browser instead of waiting
for Chrome's graceful shutdown.

Where browser-tool's cold start goes (`BT_TIMINGS`, best run): Chrome to
DevTools URL 122 ms, WebSocket connect 11 ms, first page 133 ms, close 19 ms.

### Agent-style CLI (one process per step, warm background session, best of 3)

How an AI agent's shell tool drives a browser: every step is its own
command. Same canonical session plus one accessibility snapshot.

| tool | total wall | mean per step | first command (incl. browser launch) | snapshot | snapshot output | gate |
|---|---|---|---|---|---|---|
| `browser-tool --session` | 0.80s | 41.1 ms | 387 ms | 187 ms | 51 KB | ✓ |
| `agent-browser` 0.38 (Vercel Labs, native Rust daemon) | 1.30s | 94.8 ms | 669 ms | 200 ms | 73 KB | ✓ |

Per-step time includes process start, socket hop and the CDP work; the
snapshot of the 500-card page is dominated by Chrome's
`Accessibility.getFullAXTree`, which both tools use.

**Lightpanda** (experimental) passes the same correctness gate in
0.18s. It has no
rendering engine and one tab (the session opens the second page in the
same tab), so it is listed apart from the Chrome-family ranking.

## Working on this repo (humans and agents)

[AGENTS.md](AGENTS.md): how to read CI results through the checks API,
narrow `bench.yml` runs (`only`, `scenarios`), measurement rules, and the
history policy.

## Layout

- `src/protocol.rs` — the `browser-tool` CLI protocol (serve + one-shot commands)
- `src/browser.rs` — `BrowserSession`: engine launch, tab management
- `src/cdp/` — from-scratch CDP engine (transport, JSON-RPC client, browser, page)
- `src/session.rs` — named sessions: Unix-socket server, client, detached `start`
- `src/timing.rs` — `BT_TIMINGS=1`: launch/close phase timings on stderr
- `src/bin/browser-tool.rs` — the `browser-tool` binary
- `.claude/skills/browser-tool/SKILL.md` — agent skill (session loop, `ax` refs)
- `bench/ladder/` — the driver comparison ladder (fixtures, contenders, CI); `publish.json` per run
- `docs/browser-tool.md` — op reference
- `.github/workflows/bench.yml` — manual-dispatch benchmark CI
