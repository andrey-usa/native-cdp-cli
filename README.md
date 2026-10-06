# native-cdp-cli

**`browser-tool`** drives headless Chrome from the shell, one command per step.
It is a single native binary (Rust, no Node) that speaks the Chrome DevTools
Protocol directly, and it is built for AI agents:

- It reads pages as a compact accessibility tree with `[ref=N]` handles.
- It acts with real mouse and keyboard events.
- A warm browser behind a named session survives between shell calls.
- New tabs, iframes, shadow DOM and `alert`/`confirm` dialogs are handled
  for you.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/andrey-usa/native-cdp-cli/master/install.sh | sh
# or: cargo install --locked --git https://github.com/andrey-usa/native-cdp-cli browser-tool
browser-tool install-skill        # agent guide -> ./.agents/skills (Gemini CLI, Codex, …); --claude -> ./.claude/skills
```

browser-tool finds Chrome or Chromium on its own: a system install,
`$CHROME_BIN`, or a Playwright/Puppeteer browser cache. For the fastest
cold start, point it at
[chrome-headless-shell](https://developer.chrome.com/blog/chrome-headless-shell):
`npx @puppeteer/browsers install chrome-headless-shell@stable` gives the same
Chrome without its browser UI layer. In the benchmark it cut cold start from
0.32 s to 0.11 s and the scripted session from 0.66 s to 0.32 s (run
[37498679296](https://github.com/andrey-usa/native-cdp-cli/actions/runs/37498679296)).
browser-tool uses a system Chrome first; with none installed it picks
chrome-headless-shell (or Chromium) from a Playwright or Puppeteer cache.
Set `$CHROME_BIN` to choose explicitly.

## Use (agents and people)

```sh
browser-tool -s work start                      # warm headless browser behind a Unix socket
browser-tool -s work goto example.com
browser-tool -s work --raw ax                   # page as an indented tree, [ref=N] on actionable nodes
browser-tool -s work click 12                   # act on a ref
browser-tool -s work fill 31 "hello" && browser-tool -s work press Enter
browser-tool -s work wait --text "Saved"
browser-tool -s work quit
```

```text
page "Checkout — Acme" http://shop.test/checkout
- main:
  - heading "Checkout" [level=1]
  - textbox "Full name" [ref=21]
  - combobox "Country" [ref=22] value="Select…" options: "Canada", "Mexico", …
  - radio "Express (1–2 days)" [checked, ref=25]
  - Iframe "Card payment":
    - document "Card details":
      - textbox "Card number" [ref=41]
  - button "Place order" [ref=30]
```

The commands are `goto`, `ax`, `click`, `fill`, `type`, `press`, `select`,
`hover`, `scroll`, `upload`, `wait`, `eval`, `back`/`forward`/`reload`,
`screenshot`, the `tab-*` commands, `dialog` and `quit`.

- **Targets:** elements can be named by ref, `--selector <css>` or
  `--text "<visible text>"`.
- **Output:** every command prints one JSON line, or just the result with
  `--raw`.
- **Help:** `browser-tool help <command>`.
- **Agent guide:** the version-matched guide is
  [`.claude/skills/browser-tool/SKILL.md`](.claude/skills/browser-tool/SKILL.md),
  also printed by `browser-tool skill`.
- **Reference:** [docs/browser-tool.md](docs/browser-tool.md) has the full
  reference, and [llms.txt](llms.txt) is an index for LLMs.

Programs can keep one browser on a pipe instead: `browser-tool serve` reads
one JSON command per stdin line, for example
`{"id":1,"op":"goto","url":"https://example.com"}`. The session socket speaks
the same protocol.

## Supported browsers

CI runs the full end-to-end suite on:

- the current **Chrome Stable** and **the three milestones before it**
  (about four months of releases, covering Extended Stable), all from Chrome
  for Testing;
- **chrome-headless-shell**;
- **Chrome Beta**, as a non-blocking early warning;
- the runner's own Chrome.

The suite is weekly as well as on every push, so a new Chrome release is
caught before agents hit it. In its first runs the matrix caught two real
bugs that the runner's Chrome alone missed: Chrome 151 segfaulted with one of
the automation flags (`--disable-features=OptimizationHints`, bisected with
`chrome-bisect.yml`), and on Chrome 153 an Enter-submitted form returned
before its redirect landed. Both are fixed.

Each run also checks **CDP protocol correctness**. browser-tool records
every command it sends (`BT_CDP_TRACE`), and
[`tools/cdp_check.py`](tools/cdp_check.py) validates every method, parameter
and enum value against the `/json/protocol` that *that* browser serves.
Deprecated or unknown usage fails the build. Edge and Brave are covered by
the benchmark. Lightpanda is experimental (single tab, no rendering).

## Benchmarks

Same scripted session (launch → goto listing → title/count/extract evals →
fill+click filter → visible-count eval → screenshot → new tab → goto detail →
title/row-count evals → close) driven against the same headless browser by
each driver. Fixtures are deterministic and local (500-card listing,
1000-row detail). Run in CI via `.github/workflows/bench.yml`.

| contender | session wall (best of 3) | warm eval mean (200×) | cold start (best of 5) | driver CPU (own) | driver peak RSS (own) | browser memory (PSS) |
|---|---|---|---|---|---|---|
| `bt-shell` (this repo on chrome-headless-shell) | **0.32s** | **0.43 ms** | **0.11s** | **10 ms** | 6 MB | 228 MB |
| `bt-brave` (this repo on Brave) | 0.60s | 0.52 ms | 0.31s | **10 ms** | **5 MB** | 341 MB |
| `bt-serve` (this repo on Chrome) | 0.66s | 0.60 ms | 0.32s | **10 ms** | **5 MB** | 348 MB |
| `bt-edge` (this repo on Edge) | 0.70s | 0.51 ms | 0.34s | **10 ms** | **5 MB** | 454 MB |
| `gorod` (go-rod 0.116.2, Go) | 0.71s | 0.54 ms | 0.31s | **20 ms** | 15 MB | 380 MB |
| `chromiumoxide` 0.7 (Rust) | 0.81s | 1.02 ms | 0.38s | **20 ms** | 9 MB | 467 MB |
| `chromey` 2.x (Rust, maintained chromiumoxide fork) | 0.98s | 0.97 ms | 0.36s | 50 ms | 21 MB | 468 MB |
| `chromedp` 0.19.1 (Go) | 1.01s | 0.59 ms | 0.49s | **40 ms** | 13 MB | 469 MB |
| `puppeteer-core` (Node) | 1.02s | 0.83 ms | 0.66s | 0.37 s | 80 MB | 415 MB |
| `playwright-core` (Node) | 1.26s | 1.34 ms | 0.77s | 0.68 s | 149 MB | 416 MB |
| `bt-lightpanda` (this repo on Lightpanda, 1 tab) | 0.18s | 0.22 ms | 0.11s | **<10 ms** | 6 MB | 15 MB |

Run [37498679296](https://github.com/andrey-usa/native-cdp-cli/actions/runs/37498679296)
(2026-10-06, GitHub-hosted `ubuntu-latest`: AMD EPYC 7763 64-Core Processor — 4 vCPU · Chrome: Google Chrome 154.0.8037.97). Every driver produced
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

**Reading the table.** Session wall order: bt-shell 0.32s, bt-brave 0.60s, bt-serve 0.66s, bt-edge 0.70s, gorod 0.71s, chromiumoxide 0.81s, chromey 0.98s, chromedp 1.01s, puppeteer 1.02s, playwright 1.26s. browser-tool's best
(bt-shell) ranks #1 of 10; on regular Chrome, bt-serve (0.66s) is ahead of the fastest other driver, gorod (0.71s). All native drivers (browser-tool,
go-rod, chromiumoxide, chromedp) spend tens of milliseconds of their own CPU
or less; the Node drivers spend hundreds and carry 80–150 MB of their own RSS.

### Real-world (public internet, best of 3, same run)

example.com goto → title/h1: 10 of 10 pass; bt-shell 0.22s, gorod 0.41s, chromey 0.45s, chromiumoxide 0.45s, bt-serve 0.46s, bt-brave 0.47s, bt-edge 0.51s, chromedp 0.60s, puppeteer 0.74s, playwright 0.88s.

GitHub browse (awesome-list scroll + trending click-through): 10 of 10 pass; bt-shell 3.44s, gorod 3.65s, bt-serve 3.75s, chromey 3.81s, chromiumoxide 3.95s, puppeteer 4.11s, bt-brave 4.16s, bt-edge 4.20s, chromedp 4.34s, playwright 4.37s.
Live pages change between runs, so these are informational, not part of the
correctness gate. (The scroll check used to fail at random for every driver:
GitHub sets CSS `scroll-behavior: smooth`, so `scrollY` was read mid-animation;
contenders now scroll with `behavior: 'instant'`.)

**chrome-headless-shell** (the same Chrome build without its browser UI
layer): cold start 0.32s → 0.11s, session 0.66s → 0.32s, browser memory 348 MB → 228 MB.

**CDP transport A/B** (same build and run): `--remote-debugging-pipe` (default)
vs a DevTools WebSocket port: cold start 0.32s vs 0.32s,
session 0.66s vs 0.65s. No speed difference: the WebSocket handshake itself is ~10 ms. The pipe is the default for two other reasons:
it opens no TCP port that another local process could attach to, and Chrome exits when
browser-tool dies (EOF on its command pipe), so a killed agent leaks no browser
(`tests/edge_cases.rs`: over a WebSocket port the browser outlives its driver).

**This build vs the previous one, same machine and run** (`bt-baseline`,
built from the previous master by the ladder's `baseline_ref` A/B):
session 0.65s → 0.66s (+1%), cold start 0.31s → 0.32s (+1%), browser CPU per session 1.19s → 1.09s. No measurable change: the new commands, dialog/popup/navigation tracking and the text snapshot cost nothing on the canonical session.

Where browser-tool's cold start goes (`BT_TIMINGS`, best run): browser up
154 ms, first page 113 ms, close 20 ms.

### Agent tools (scripted, warm session, best of 3)

How an agent's tool calls drive a browser: CLI tools run one process per
step (a shell tool), MCP servers get one `tools/call` per step over a
persistent connection. Same canonical session plus one page snapshot.

| tool | kind | total wall | mean per step | snapshot | snapshot size | gate |
|---|---|---|---|---|---|---|
| `browser-tool` (this repo) | CLI | 0.83s | 42 ms | 171 ms | 36 KB | ✓ |
| `browser-tool` on chrome-headless-shell | CLI | 0.49s | 31 ms | 146 ms | 36 KB | ✓ |
| `agent-browser` 0.38 (Vercel Labs, Rust) | CLI | 1.25s | 94 ms | 185 ms | 73 KB | ✓ |
| `playwright-cli` 0.1.22 (Microsoft) | CLI | 9.88s | 788 ms | 420 ms | 50 KB | ✓ |
| Playwright MCP 0.0.83 (Microsoft) | MCP | 5.29s | 402 ms | 65 ms | 50 KB | ✓ |
| Chrome DevTools MCP 1.10.1 (Google) | MCP | 3.35s | 201 ms | 100 ms | 40 KB | ✓ |

Per-step time includes process start (CLI) or JSON-RPC (MCP), the hop to
the daemon, and the CDP work. playwright-cli and Playwright MCP share Playwright's tool
backend, which waits a fixed 500 ms after every action (`timeouts.settle`); playwright-cli
also starts a Node process per step.

**Lightpanda** (experimental) passes the same correctness gate in
0.18s with ~15 MB of browser memory. It has no
rendering engine and one tab (the session opens the second page in the
same tab), so it is listed apart from the Chrome-family ranking.

## Agent eval: how a generic agent copes with each tool

Benchmarks time scripted steps. The agent eval asks the question that
matters for agents: can a generic coding agent, given only the tool and its
own documentation, finish real tasks? [`agent-eval.yml`](.github/workflows/agent-eval.yml)
runs [Gemini CLI](https://github.com/google-gemini/gemini-cli) headless on a
free-tier API key against the local Acme Supply site, with **browser-tool**,
**playwright-cli** and **agent-browser** in turn:

- **skilled:** the tool is installed and its vendor's own Agent Skill is in
  the workspace (`.agents/skills/`);
- **onboard:** the agent gets only the tool's name and repository URL and
  must install it from its docs, then do a short task.

The five tasks are a price lookup, a purchase with a coupon and an iframe
payment form, a login plus a paginated order table, docs in a new tab with a
collapsed FAQ, and a support form inside an iframe. Success is judged from
the site's recorded state and the final answer, never from the agent's
claim. Every run also records model requests, tokens, shell commands and
off-tool workarounds (curl, ad-hoc scripts). Model, prompt template, Chrome
and site are identical across tools.

It needs a `GEMINI_API_KEY` repository secret (free key:
[aistudio.google.com/apikey](https://aistudio.google.com/apikey)). Without
it, only the scripted baseline runs, which checks the site, the success
checks and the harness with known-good browser-tool command plans.

## Working on this repo (humans and agents)

[AGENTS.md](AGENTS.md): how to read CI results through the checks API,
narrow `bench.yml` runs (`only`, `scenarios`), measurement rules, and the
history policy.

## Layout

- `src/protocol.rs`: CLI parsing, the JSON-lines protocol and `Driver` (the `OPS` table is `--help`)
- `src/browser.rs`: `BrowserSession`: browser discovery (incl. chrome-headless-shell), launch flags, tabs
- `src/cdp/`: from-scratch CDP engine: pipe/WebSocket transport, JSON-RPC client (`BT_CDP_TRACE`), page ops, `ax.rs` (snapshot tree), `events.rs` (dialogs, popups, navigation)
- `src/session.rs`: named sessions: Unix-socket server, client, detached `start`
- `src/timing.rs`: `BT_TIMINGS=1` launch/close phase timings on stderr
- `tests/`: browser e2e: serve protocol, sessions, and the Acme Supply scenario
- `bench/site/server.py`: Acme Supply, a deterministic local shop (SPA, iframes, shadow DOM, dialogs, popups, login, upload)
- `bench/ladder/`: driver ladder and agent-tool benchmark (`publish.json` per run, `gen_readme.py`)
- `bench/agent-eval/`: Gemini CLI agent eval across browser-tool, playwright-cli and agent-browser
- `tools/`: `cdp_check.py` (protocol correctness), `protocol_dump.sh`, `cft_matrix.py` (Chrome version matrix)
- `.claude/skills/browser-tool/SKILL.md`: the agent guide (compiled into the binary: `browser-tool skill`)
- `.github/workflows/`: `ci.yml` (tests, Chrome matrix, protocol check), `bench.yml`, `agent-eval.yml`, `release.yml`, `chrome-bisect.yml`, `vendor.yml`
