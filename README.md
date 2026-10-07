# navigera

**`navigera`** (formerly `browser-tool`; repo `native-cdp-cli`) drives
Chrome from the shell, one command per step.
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
# or: cargo install --locked --git https://github.com/andrey-usa/native-cdp-cli navigera
navigera install-skill        # agent guide -> ./.agents/skills (Gemini CLI, Codex, …); --claude -> ./.claude/skills
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/andrey-usa/native-cdp-cli/master/install.ps1 | iex
```

Linux, macOS and Windows are supported. On Windows the session socket is a
loopback TCP port that only accepts clients presenting a random token from
the session file in your temp directory, CDP runs over a DevTools
WebSocket, and Chrome sits in a kill-on-close job object, so it still exits
when navigera dies.

navigera finds Chrome or Chromium on its own: a system install,
`$CHROME_BIN`, or a Playwright/Puppeteer browser cache. For the fastest
cold start, point it at
[chrome-headless-shell](https://developer.chrome.com/blog/chrome-headless-shell):
`npx @puppeteer/browsers install chrome-headless-shell@stable` gives the same
Chrome without its browser UI layer. In the benchmark it cut cold start from
0.32 s to 0.11 s and the scripted session from 0.66 s to 0.32 s (run
[37498679296](https://github.com/andrey-usa/native-cdp-cli/actions/runs/37498679296)).
navigera uses a system Chrome first; with none installed it picks
chrome-headless-shell (or Chromium) from a Playwright or Puppeteer cache.
Set `$CHROME_BIN` to choose explicitly.

## Use (agents and people)

```sh
navigera -s work start                      # warm headless browser behind a local socket
navigera -s work goto example.com
navigera -s work --raw ax                   # page as an indented tree, [ref=N] on actionable nodes
navigera -s work click 12                   # act on a ref
navigera -s work fill 31 "hello" && navigera -s work press Enter
navigera -s work wait --text "Saved"
navigera -s work quit
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
- **Help:** `navigera help <command>`.
- **Agent guide:** the version-matched guide is
  [`.claude/skills/navigera/SKILL.md`](.claude/skills/navigera/SKILL.md),
  also printed by `navigera skill`.
- **Reference:** [docs/navigera.md](docs/navigera.md) has the full
  reference, and [llms.txt](llms.txt) is an index for LLMs.

**Your own browser.** `navigera -s me start --attach` works in the Chrome
you already have open (Chrome 144+, after turning on
`chrome://inspect/#remote-debugging` once; Chrome asks you to Allow the
connection), with your logins, settings and extensions.
`navigera -s me start --profile work` keeps a separate visible browser on
its own persistent profile instead: sign in once and it stays open between
sessions. Either way navigera works in a window of its own, and `quit` only
disconnects. Details: [docs/navigera.md](docs/navigera.md#browsers-and-engines).

Programs can keep one browser on a pipe instead: `navigera serve` reads
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
- the runner's own Chrome, on Linux and on **Windows**.

The suite is weekly as well as on every push, so a new Chrome release is
caught before agents hit it. In its first runs the matrix caught two real
bugs that the runner's Chrome alone missed: Chrome 151 segfaulted with one of
the automation flags (`--disable-features=OptimizationHints`, bisected with
`chrome-bisect.yml`), and on Chrome 153 an Enter-submitted form returned
before its redirect landed. Both are fixed.

Each run also checks **CDP protocol correctness**. navigera records
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
| `bt-shell` (this repo on chrome-headless-shell) | **0.29s** | **0.41 ms** | **0.08s** | **<10 ms** | **5 MB** | 204 MB |
| `bt-brave` (this repo on Brave) | 0.57s | 0.51 ms | 0.27s | **<10 ms** | **5 MB** | 370 MB |
| `bt-serve` (this repo on Chrome) | 0.64s | 0.68 ms | 0.28s | **<10 ms** | 6 MB | 344 MB |
| `bt-edge` (this repo on Edge) | 0.69s | 0.55 ms | 0.31s | **10 ms** | **5 MB** | 467 MB |
| `gorod` (go-rod 0.116.2, Go) | 0.71s | 0.49 ms | 0.29s | **20 ms** | 13 MB | 365 MB |
| `chromiumoxide` 0.7 (Rust) | 0.81s | 0.82 ms | 0.38s | **20 ms** | 9 MB | 473 MB |
| `chromey` 2.x (Rust, maintained chromiumoxide fork) | 0.97s | 0.95 ms | 0.39s | 50 ms | 22 MB | 463 MB |
| `chromedp` 0.19.1 (Go) | 0.99s | 0.58 ms | 0.51s | **30 ms** | 13 MB | 465 MB |
| `puppeteer-core` (Node) | 1.01s | 0.83 ms | 0.62s | 0.37 s | 80 MB | 411 MB |
| `playwright-core` (Node) | 1.24s | 1.32 ms | 0.75s | 0.67 s | 150 MB | 418 MB |
| `bt-lightpanda` (this repo on Lightpanda, 1 tab) | 0.18s | 0.23 ms | 0.11s | **<10 ms** | **5 MB** | 15 MB |

Run [37692160222](https://github.com/andrey-usa/navigera/actions/runs/37692160222)
(2026-10-07, GitHub-hosted `ubuntu-latest`: AMD EPYC 7763 64-Core Processor — 4 vCPU · Chrome: Google Chrome 154.0.8037.97). Every driver produced
byte-identical extracted data and counts (correctness gate); wall is best-of-N.
GitHub runners vary between runs, so compare rows within one run.

**How driver CPU/RSS are measured.** Each driver's *own* `utime+stime`,
read from its zombie's `/proc/<pid>/stat` before reaping (`waitid` with
`WNOWAIT`), and its own VmHWM. The kernel counts CPU in 10 ms ticks, so
values under ~50 ms are a tie. Not `wait4`: its rusage is RUSAGE_BOTH and
adds in every child the driver reaped. navigera, chromedp, puppeteer
and playwright `wait()` on their Chrome, so `wait4` charged them Chrome's
CPU and RSS; go-rod (its leakless helper reaps Chrome) and chromiumoxide
were never charged. Earlier versions of this table reported exactly that
artifact ("0.82s vs 0.03s"). Browser memory is the whole browser process
tree (every renderer/GPU/utility process), sampled as summed PSS every
250 ms (shared pages counted once).

**Reading the table.** Session wall order: bt-shell 0.29s, bt-brave 0.57s, bt-serve 0.64s, bt-edge 0.69s, gorod 0.71s, chromiumoxide 0.81s, chromey 0.97s, chromedp 0.99s, puppeteer 1.01s, playwright 1.24s. navigera's best
(bt-shell) ranks #1 of 10; on regular Chrome, bt-serve (0.64s) is ahead of the fastest other driver, gorod (0.71s). All native drivers (navigera,
go-rod, chromiumoxide, chromedp) spend tens of milliseconds of their own CPU
or less; the Node drivers spend hundreds and carry 80–150 MB of their own RSS.

### Real-world (public internet, best of 3, same run)

example.com goto → title/h1: 10 of 10 pass; bt-shell 0.20s, gorod 0.40s, bt-brave 0.42s, bt-serve 0.43s, chromey 0.46s, bt-edge 0.46s, chromiumoxide 0.47s, chromedp 0.62s, puppeteer 0.74s, playwright 0.84s.

GitHub browse (awesome-list scroll + trending click-through): 10 of 10 pass; bt-shell 3.54s, gorod 3.80s, chromiumoxide 3.99s, chromey 4.04s, bt-serve 4.18s, puppeteer 4.30s, bt-brave 4.47s, playwright 4.49s, chromedp 4.79s, bt-edge 5.01s.
Live pages change between runs, so these are informational, not part of the
correctness gate. (The scroll check used to fail at random for every driver:
GitHub sets CSS `scroll-behavior: smooth`, so `scrollY` was read mid-animation;
contenders now scroll with `behavior: 'instant'`.)

**chrome-headless-shell** (the same Chrome build without its browser UI
layer): cold start 0.28s → 0.08s, session 0.64s → 0.29s, browser memory 344 MB → 204 MB.

**CDP transport A/B** (same build and run): `--remote-debugging-pipe` (default on Linux/macOS)
vs a DevTools WebSocket port: cold start 0.28s vs 0.28s,
session 0.64s vs 0.62s. No speed difference: the WebSocket handshake itself is ~10 ms. The pipe is the default for two other reasons:
it opens no TCP port that another local process could attach to, and Chrome exits when
navigera dies (EOF on its command pipe), so a killed agent leaks no browser
(`tests/edge_cases.rs`: over a WebSocket port the browser outlives its driver).
Windows has no pipe transport here and uses the WebSocket, with a kill-on-close
job object giving the same guarantee (`killed_session_server_takes_its_browser_down`
runs there too).

**This build vs the previous one, same machine and run** (`bt-baseline`,
built from the previous master by the ladder's `baseline_ref` A/B):
session 0.65s → 0.64s (-2%), cold start 0.32s → 0.28s (-12%), browser CPU per session 1.18s → 1.14s. The change: Chrome's throwaway profile moved to tmpfs (`/dev/shm`), where its ~210 `fdatasync` calls per launch cost nothing, and close kills the whole browser process tree and deletes the profile without waiting for the kernel to reap Chrome.

Where navigera's cold start goes (`BT_TIMINGS`, best run): browser up
134 ms, first page 128 ms, close 5 ms.

### Agent tools (scripted, warm session, best of 3)

How an agent's tool calls drive a browser: CLI tools run one process per
step (a shell tool), MCP servers get one `tools/call` per step over a
persistent connection. Same canonical session plus one page snapshot.

| tool | kind | total wall | mean per step | snapshot | snapshot size | gate |
|---|---|---|---|---|---|---|
| `navigera` (this repo) | CLI | 0.81s | 41 ms | 176 ms | 36 KB | ✓ |
| `navigera` on chrome-headless-shell | CLI | 0.50s | 32 ms | 161 ms | 36 KB | ✓ |
| `agent-browser` 0.38 (Vercel Labs, Rust) | CLI | 1.30s | 94 ms | 216 ms | 73 KB | ✓ |
| `playwright-cli` 0.1.22 (Microsoft) | CLI | 10.01s | 798 ms | 440 ms | 50 KB | ✓ |
| Playwright MCP 0.0.83 (Microsoft) | MCP | 5.34s | 404 ms | 72 ms | 50 KB | ✓ |
| Chrome DevTools MCP 1.10.1 (Google) | MCP | 3.35s | 203 ms | 104 ms | 40 KB | ✓ |

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
free-tier API key against the local Acme Supply site, with **navigera**,
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
and site are identical across tools. `os: windows` (or `both`) runs the same
jobs on Windows runners, where the agent's shell is PowerShell.

The three tools run as parallel jobs. Keys live in the `main` environment,
one slot per tool, because Gemini's free quota is per Google Cloud project:
`GEMINI_API_KEY` (navigera), `GEMINI_API_KEY_2` (agent-browser) and
`GEMINI_API_KEY_3` (playwright-cli). An empty slot falls back to
`GEMINI_API_KEY`. Without any key only the scripted baseline runs: it checks
the site, the success checks and the harness with known-good navigera
command plans. Runs that die on a model quota or outage are reported as
**⚠ infra**, not counted as a tool failure.

**0.3.0 vs 0.2.1, same model, launched together** (purchase + account, 2
runs each, all passed): `ax` now waits for data a page is still fetching,
and the skill teaches chaining sure steps in one shell call. Median model
turns 18 → 10, median tokens 288K → 167K; every 0.3.0 run used fewer tokens
than every 0.2.1 run of the same task (runs
[37619227942](https://github.com/andrey-usa/native-cdp-cli/actions/runs/37619227942)
vs [37619231246](https://github.com/andrey-usa/native-cdp-cli/actions/runs/37619231246)).
The scripted session didn't slow down: 0.51 s vs 0.49 s, cold start 0.23 s
both (bench run [37619224427](https://github.com/andrey-usa/native-cdp-cli/actions/runs/37619224427)).

**Natural checks** ([`agent-check.yml`](.github/workflows/agent-check.yml),
every master push, PRs and nightly): Gemini is told in plain words to buy
something with navigera and its skill. On the local shop, once on Linux
and once on Windows through PowerShell (real checks, judged from the
recorded order), and once on Sauce Labs' public demo
shop [saucedemo.com](https://www.saucedemo.com), which exists for automation
practice and takes no real payment (informational, judged from the
confirmation and the order total).

## Working on this repo (humans and agents)

[AGENTS.md](AGENTS.md): how to read CI results through the checks API,
narrow `bench.yml` runs (`only`, `scenarios`), measurement rules, and the
history policy.

## Layout

- `src/protocol.rs`: CLI parsing, the JSON-lines protocol and `Driver` (the `OPS` table is `--help`)
- `src/browser.rs`: `BrowserSession`: browser discovery (incl. chrome-headless-shell), launch flags, tabs
- `src/cdp/`: from-scratch CDP engine: pipe/WebSocket transport, JSON-RPC client (`BT_CDP_TRACE`), page ops, `ax.rs` (snapshot tree), `events.rs` (dialogs, popups, navigation), `procjob.rs` (Windows job object)
- `src/session.rs`: named sessions: socket server (Unix socket; loopback TCP + token on Windows), client, detached `start`
- `src/cdp/attach.rs`: `--attach` (your running browser) and `--profile` (persistent navigera browser)
- `src/timing.rs`: `BT_TIMINGS=1` launch/close phase timings on stderr
- `tests/`: browser e2e: serve protocol, sessions, and the Acme Supply scenario
- `bench/site/server.py`: Acme Supply, a deterministic local shop (SPA, iframes, shadow DOM, dialogs, popups, login, upload)
- `bench/ladder/`: driver ladder and agent-tool benchmark (`publish.json` per run, `gen_readme.py`)
- `bench/agent-eval/`: Gemini CLI agent eval across navigera, playwright-cli and agent-browser
- `tools/`: `cdp_check.py` (protocol correctness), `protocol_dump.sh`, `cft_matrix.py` (Chrome version matrix)
- `.claude/skills/navigera/SKILL.md`: the agent guide (compiled into the binary: `navigera skill`)
- `.github/workflows/`: `ci.yml` (tests, Chrome matrix, protocol check), `bench.yml`, `agent-eval.yml`, `release.yml`, `chrome-bisect.yml`, `vendor.yml`
