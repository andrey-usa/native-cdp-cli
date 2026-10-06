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
| `bt-brave` (this repo on Brave) | **0.50s** | **0.29 ms** | 0.26s | **<10 ms** | **5 MB** | 369 MB |
| `gorod` (go-rod 0.116.2, Go) | 0.51s | 0.30 ms | **0.21s** | **10 ms** | 13 MB | 378 MB |
| `chromiumoxide` 0.7 (Rust) | 0.57s | 0.50 ms | 0.26s | **10 ms** | 9 MB | 402 MB |
| `bt-serve` (this repo on Chrome) | 0.58s | 0.33 ms | 0.36s | **<10 ms** | **5 MB** | 391 MB |
| `chromedp` 0.19.1 (Go) | 0.73s | 0.47 ms | 0.34s | **20 ms** | 13 MB | 393 MB |
| `puppeteer-core` (Node) | 0.77s | 0.51 ms | 0.45s | 0.24 s | 80 MB | 345 MB |
| `bt-edge` (this repo on Edge) | 0.77s | 0.32 ms | 0.55s | **10 ms** | **5 MB** | 480 MB |
| `playwright-core` (Node) | 0.87s | 0.98 ms | 0.51s | 0.44 s | 149 MB | 380 MB |
| `bt-lightpanda` (this repo on Lightpanda, 1 tab) | 0.65s | 0.09 ms | 0.61s | **<10 ms** | **5 MB** | 34 MB |

Run [37406305289](https://github.com/andrey-usa/native-cdp-cli/actions/runs/37406305289)
(2026-10-06, GitHub-hosted `ubuntu-latest`: AMD EPYC 9V45 96-Core Processor — 4 vCPU · Chrome: Google Chrome 154.0.8037.57). Every driver produced
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

**Reading the table.** Session wall order: bt-brave 0.50s, gorod 0.51s, chromiumoxide 0.57s, bt-serve 0.58s, chromedp 0.73s, puppeteer 0.77s, bt-edge 0.77s, playwright 0.87s. browser-tool's best
(bt-brave) ranks #1 of 8. All native drivers (browser-tool,
go-rod, chromiumoxide, chromedp) spend tens of milliseconds of their own CPU
or less; the Node drivers spend hundreds and carry 80–150 MB of their own RSS.

### Real-world (public internet, best of 3, same run)

example.com goto → title/h1: 8 of 8 pass; gorod 0.31s, chromiumoxide 0.36s, bt-brave 0.41s, chromedp 0.45s, bt-serve 0.48s, puppeteer 0.56s, playwright 0.62s, bt-edge 0.68s.

GitHub browse (awesome-list scroll + trending click-through): 8 of 8 pass; gorod 2.91s, chromiumoxide 3.12s, bt-brave 3.15s, bt-serve 3.20s, chromedp 3.33s, puppeteer 3.35s, playwright 3.53s, bt-edge 3.63s.
Live pages change between runs, so these are informational, not part of the
correctness gate. (The scroll check used to fail at random for every driver:
GitHub sets CSS `scroll-behavior: smooth`, so `scrollY` was read mid-animation;
contenders now scroll with `behavior: 'instant'`.)

**Lightpanda** (experimental) passes the same correctness gate in
0.65s with ~34 MB of browser memory. It has no
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
- `src/bin/browser-tool.rs` — the `browser-tool` binary
- `.claude/skills/browser-tool/SKILL.md` — agent skill (session loop, `ax` refs)
- `bench/ladder/` — the driver comparison ladder (fixtures, contenders, CI); `publish.json` per run
- `docs/browser-tool.md` — op reference
- `.github/workflows/bench.yml` — manual-dispatch benchmark CI
