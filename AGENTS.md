# Working on native-cdp-cli — guide for AI agents

Read this before your first change. It exists because a full day of agent
work here went into symptoms (CPU "optimizations" for a measurement bug, 15+
CI runs debugging a launch that was starting the wrong binary) while master
CI sat red. The rules below are what would have saved that day.

## 1. Start every session with state, not code

```sh
gh run list -R andrey-usa/native-cdp-cli -w ci -L 3   # is master green?
gh run list -R andrey-usa/native-cdp-cli -w bench -L 3
```

If master CI is red, fixing it is task #1 — before any feature, benchmark or
README number.

## 2. Read CI results through the checks API

Agent sandboxes usually can't download job logs or artifacts (blob storage
is blocked). Annotations come back through the plain API, so every workflow
here publishes what you need as annotations:

```sh
R=andrey-usa/native-cdp-cli
JOB=$(gh api repos/$R/actions/runs/<run-id>/jobs -q '.jobs[0].id')
gh api repos/$R/actions/jobs/$JOB -q '.steps[] | "\(.name) = \(.conclusion)"'
gh api repos/$R/check-runs/$JOB/annotations -q '.[] | "[\(.title)] \(.message)"'
```

| annotation title | from | contents |
|---|---|---|
| `cargo test failure` | ci.yml | full output of failing tests |
| `cargo test failure (windows)` / `cargo test summary (windows)` | ci.yml `windows` job | failing output / which test binaries and e2e tests ran on Windows |
| `install.sh failure` / `install.ps1 failure` | ci.yml `install` jobs | last lines of the installer run |
| compiler / clippy file:line | ci.yml (problem matcher) | the error itself |
| `ladder table N/M` | bench.yml | the markdown results table |
| `ladder json N/M` | bench.yml | `publish.json` (chart-ready results; join chunks in order) |
| `ladder log tail N/M` | bench.yml (on failure) | last 30 KB of ladder.log |
| `cdp check …` | ci.yml (every browser in the matrix) | CDP methods/params/enums the tests sent vs that Chrome's `/json/protocol` |
| `cargo test failure (<browser>)` | ci.yml compat matrix | failing test output on that Chrome build |
| `agent eval scripted/onboard/skilled N/M` | agent-eval.yml | pass table per tool and task, failure reasons |
| `agent eval json <mode> N/M` | agent-eval.yml | every run: answer, checks, requests, tokens, shell commands |
| `gemini smoke failure (<tool>, <key slot>)` | agent-eval.yml | why the model call failed (quota, model id, key) |
| `agent eval key` | agent-eval.yml (each tool job) | which key slot that tool's job used |
| `agent check <task> (<os>)` / `agent check json <task> (<os>)` | agent-check.yml | natural purchase checks (local shop on Linux and Windows, live demo shop) |
| `agent run log (<…>) (exit N)` | agent-check.yml, agent-eval.yml | last 6 KB of the harness output when an agent run step fails (a crash before any table is written) |

If something you need isn't there, add an annotation for it (see
`bench/ladder/annotate.py`) — don't push a debug commit to find out.

## 3. Fast loop

1. **Local first.** `cargo test` runs the browser e2e tests against a local
   Chrome. navigera finds `$CHROME_BIN`, a system Chrome, or the
   Playwright/Puppeteer caches (`/opt/pw-browsers`, `~/.cache/ms-playwright`).
   The tests cover the serve protocol, sessions, and the full Acme Supply
   scenario in `tests/site_scenarios.rs` (about 10 s).
   `cargo clippy --all-targets -- -D warnings` is the lint gate.
   **No crates.io in your sandbox?** CI publishes the vendored dependencies to
   a git ref, so you can still build offline (git to GitHub usually works):
   ```sh
   git fetch origin refs/cache/vendor:refs/cache/vendor
   mkdir -p ../bt-vendor && git archive refs/cache/vendor | tar -x -C ../bt-vendor
   mkdir -p ~/.cargo && printf '[source.crates-io]\nreplace-with = "v"\n[source.v]\ndirectory = "%s"\n' "$(cd ../bt-vendor && pwd)/vendor" >> ~/.cargo/config.toml
   cargo test --offline
   ```
   The ref is refreshed by `vendor.yml` whenever `Cargo.lock` changes.
   Manual poking: run `python3 bench/site/server.py --port 8765`, then drive
   it with `navigera -s dev …`.
2. **No local browser/registry?** Push a *branch* and dispatch narrow runs:
   ```sh
   gh workflow run ci.yml    --ref my-branch                      # ~3 min
   gh workflow run bench.yml --ref my-branch -f reps=1 \
       -f only=bt-serve,gorod -f scenarios=                        # ~3 min, session gate only
   ```
   `only` takes any contender names (`bt-serve bt-shell bt-edge bt-brave
   bt-lightpanda playwright puppeteer chromiumoxide chromey chromedp gorod`);
   `scenarios` is any of `eval,cold,realworld,browse,agent` (empty = the
   session gate only; `agent` = one step per CLI process / MCP call:
   navigera, agent-browser, playwright-cli, Playwright MCP, Chrome
   DevTools MCP).
3. **Judge a perf change with an A/B in ONE run.** GitHub runners differ
   between runs by more than most changes are worth, so never compare
   numbers across runs. `baseline_ref` builds a second navigera from any
   ref and runs it as `bt-baseline` beside your build on the same machine:
   ```sh
   gh workflow run bench.yml --ref my-branch -f reps=3 \
       -f only=bt-serve,bt-baseline,gorod -f scenarios=eval,cold \
       -f baseline_ref=master        # branch, tag or full 40-char SHA
   ```
   Every navigera run also reports where its launch time went
   (`BT_TIMINGS`: devtools_url, ws_connect, first_page, close) in the table.
4. **Windows** has no local loop here (no Windows target in the sandbox):
   `ci.yml`'s `windows` job builds, lints and runs every test on the
   runner's Chrome in ~2 min; compile errors come back as problem-matcher
   annotations. Windows differs in three places only: CDP over WebSocket
   (no pipe), sessions over loopback TCP + token (`src/session.rs`
   `endpoint`), Chrome in a kill-on-close job (`src/cdp/procjob.rs`).
   `agent-eval.yml -f os=windows` and agent-check's Windows leg run Gemini
   CLI there (PowerShell shell; `run.py` resolves tools via the run's PATH
   and kills leftovers through CIM, since Windows has no `pkill`).
   `-f mode=scripted` runs only the no-model baseline (any OS, any time of
   day). The first Chrome launch on a fresh Windows VM takes 4–6 s
   (`devtools_url` ~4 s), later ones ~0.4 s (8 VMs, run 37549075986); one
   first launch out of ~50 failed its WebSocket connect, cause unknown — the
   scripted step now annotates the full error if it recurs.
5. **Browser compatibility** is part of `ci.yml`: Chrome Stable plus three
   milestones back, chrome-headless-shell and Beta (non-blocking). Each one
   runs the e2e tests with `BT_CDP_TRACE` and then `tools/cdp_check.py`.
   Before using a new CDP method or parameter, check it exists in the
   *oldest* supported milestone: `bash tools/protocol_dump.sh <chrome> p.json`.
6. **Agent eval** (`agent-eval.yml`, keys in the `main` environment):
   Gemini CLI does the five Acme tasks with each tool, one parallel job per
   tool, each on its own key slot (`GEMINI_API_KEY`, `_2`, `_3`; free quota
   is per Google Cloud project). Free-tier quota is per day, so narrow it
   with `-f tools=navigera -f tasks=purchase` while iterating.
   `purchase-live` (saucedemo.com) runs only when named. `--agent scripted` runs the same checks locally without
   a model:
   ```sh
   python3 bench/agent-eval/run.py --agent scripted --out /tmp/eval
   ```
7. **One decisive run per hypothesis.** Decide beforehand what output would
   confirm or kill the hypothesis, and make sure that output lands in an
   annotation. If two runs in a row didn't change your mind, stop and re-read
   the code path end to end instead of adding more logging.
8. Full `reps=3` bench only after CI and a narrow run are green.

## 4. Root cause before mitigation

- **Check what actually ran.** Print the exact binary + argv when a launch
  fails. (Lightpanda "engine selection" was `google-chrome serve --port …`:
  the harness passed `$CHROME_BIN` as the lightpanda binary.)
- **Read protocols, not timeouts.** The DevTools HTTP poll read to EOF;
  Lightpanda keeps the connection alive, so every poll hit the 5 s read
  timeout (EAGAIN). The fix was honoring Content-Length, not a longer wait.
- No retries, sleeps or timeouts as fixes until the cause is known.
- Don't "fix" a competitor contender beyond what's idiomatic for that
  library; record its failure instead. Live-internet scenarios
  (example.com, GitHub) are informational, never gates.
- A check that every contender fails "randomly" is a harness bug. The
  GitHub scroll gate raced CSS `scroll-behavior: smooth` (`scrollY` read
  mid-animation: `[0, 0, 724, …]`); the fix was `behavior: 'instant'`,
  found by making the gate report *why* it failed.
- Vendor downloads (Edge, Brave, Lightpanda) flake; their install steps are
  `continue-on-error` and a missing binary shows under "Skipped".
- **A failure on one Chrome milestone only: bisect it in one run.**
  `chrome-bisect.yml` runs the same test binary on one Chrome for Testing
  milestone under several env variants (`BT_DROP_FLAGS`, `BT_EXTRA_FLAGS`,
  `BT_CDP_TRANSPORT=ws|pipe`, `BT_NO_DISCOVER=1`) and annotates a pass/fail
  table. On a branch, commit `.github/bisect.env` to trigger it (never merge
  that file). Three rounds found that `--disable-features=OptimizationHints`
  segfaults Chrome 151 (runs 37461070056 → 37461788587 → 37464048004).
- A scenario step that fails dumps the page state (URL, visibility, focus,
  hovered chain, last steps, `ax`, and a log of visibility/pointer events)
  into the `cargo test failure` annotation. Read that before guessing at a
  flake. That log settled the ~5% "Next page click did nothing" flake: the
  page saw `visibilitychange` but no pointer event at all after a tab
  switch — Chrome acked `Input.dispatchMouseEvent` without delivering it.
  `click` now checks that the press arrived and resends once (bisect run
  37626179229: 40/40 with 3 resends logged; `BT_NO_CLICK_CHECK=1` turns the
  check off). Network.enable was cleared first (37623035163: 20/20 with it,
  19/20 without). `chrome-bisect.yml` annotates each variant's first full
  failure and counts click resends in the session logs.

### What A/B runs have already settled (don't redo)

| change | result | run |
|---|---|---|
| standard automation flags (go-rod/chromiumoxide set) + `--no-startup-window` + kill-based close | session −23%, cold start −33% | 37410456058 |
| adopt Chrome's initial tab instead of creating the first page | no gain: first_page −60 ms but ws_connect +70 ms — Chrome's startup is serialized on its UI thread | 37411020717 |
| current-thread tokio runtime (no cross-thread hops per round trip) | no gain: engine eval 0.458 vs 0.459 ms | 37411391276 |
| CDP over `--remote-debugging-pipe` instead of a WebSocket port | no speed gain (pipe 0.31 s cold vs master's WebSocket 0.31 s); kept as default because it opens no TCP port and Chrome exits with its driver (WebSocket leaks the browser: `killed_session_server_takes_its_browser_down`) | 37465364738 |
| chrome-headless-shell instead of Chrome (`bt-shell`) | session 0.66 → 0.32 s, cold 0.31 → 0.10 s, browser PSS 356 → 228 MB; used when no system Chrome is installed (cache lookup), or via `$CHROME_BIN` | 37465364738 |
| new ops, dialog/popup/navigation tracking, text `ax` | no session cost: 0.65 → 0.66 s vs master | 37465364738 |
| `Network.enable` per tab + `ax`/`screenshot` wait for in-flight fetch/XHR (500 ms quiet, ≤3 s) | no session cost: 0.51 vs 0.49 s, cold 0.23 s both | 37619224427 |
| that wait + skill line "chain sure steps, end with `ax`" (agent eval, purchase+account ×2) | median turns 18 → 10, tokens 288K → 167K, 4/4 both | 37619227942 vs 37619231246 |
| throwaway profile on tmpfs (`/dev/shm`; Chrome makes ~210 `fdatasync` calls per cold start) + kill the browser's whole process group + delete the profile while Chrome dies, reap in the background (`src/cdp/profile.rs`, `Browser::close`; `BT_PROFILE_DIR=<dir>` puts the profile elsewhere) | cold 0.26 → 0.24 s (gorod 0.27), session 0.57 → 0.53 s, close 18 → 2 ms; launch itself unchanged on the runner's disk (locally, on a slower disk: cold 288 → 234 ms) | 37690824030 |

## 5. Measurement rules

- **A 10×+ gap is a measurement bug until proven otherwise.** The "27× more
  driver CPU than go-rod" was `wait4` — its rusage is RUSAGE_BOTH and includes
  every child the driver reaped (Chrome). Drivers that reap Chrome were
  charged Chrome's CPU and RSS; go-rod/chromiumoxide were not.
- Driver CPU = the process's own `utime+stime`, read from the zombie's
  `/proc/<pid>/stat` after `waitid(WNOWAIT)`; driver RSS = its own VmHWM.
  Never `ru_maxrss` / `wait4` totals for a process that spawns browsers.
- Compare like with like: same work, same teardown semantics, same polling
  granularity (≤5 ms or blocking waits), best-of-N wall.
- Browser-side numbers cover the whole browser process tree (all renderer,
  GPU and utility processes, including ones a launcher stub orphaned), summed
  as PSS, not RSS (RSS counts shared pages once per process).
- Identify processes by `/proc/<pid>/comm`, never `argv[0]`: Chromium
  rewrites child command lines into one space-joined string.
- Mind the observer effect: `smaps_rollup` takes the target's mmap lock, so
  read it sparingly (the ladder reads PSS every 250 ms, CPU every 50 ms).
- Every number in README must cite the run id it came from. Regenerate the
  README table from the run's `publish.json` rather than retyping it.
- Agent eval: judge success from the site's recorded state (`/__state`)
  and the final answer, never from the agent's own claim. Read each run's
  `shell_steps` (command + output tail) before blaming a tool: the first
  runs blamed nobody for a 503 from Gemini's own web fetch, and found that
  Gemini CLI strips the shell environment in GitHub Actions (hence
  `PASS_ENV` in `run.py`). When the run's model hits its daily quota,
  Gemini CLI silently retries on the next model of its fallback chain
  (`gemini-3.8-flash`, 20 free requests a day, then a 429): runs that hit
  a quota or switched models are marked `infra` and excluded from pass
  rates, and the smoke test fails when the model already falls back. A run
  that times out names the command it was stuck in (`in_flight`, and
  `stuck in: …` in its failure reasons). Keep the model,
  prompt template, settings, Chrome and site identical across tools. Each
  tool gets the skill its own vendor ships.
- For agents, **tokens matter more than milliseconds**. An `ax` that takes
  100 ms but is half the size is the better trade, because LLM thinking time
  between steps is seconds.

## 6. Repo hygiene and history

- **Releasing:** bump `version` in `Cargo.toml` on master, then Actions →
  release → Run workflow with that version (the GitHub app works). The run
  checks the version, builds five targets and creates tag + release itself.
  Agents: never publish a release (or push a tag) yourself — dispatch with
  `-f publish=false` for a dry run and leave the real one to the owner.
- Master history is curated (reset to a single commit on 2026-10-06). Work on
  a branch, squash to meaningful commits, no "debug"/"temp"/"diagnostic"
  commits on master. Don't force-push master unless the owner asks.
- Outputs go to `bench/ladder/out/` (gitignored). Never commit cookie jars,
  reports or page dumps from other tools run in this checkout.
- Keep `.claude/skills/navigera/SKILL.md`, `docs/navigera.md`,
  `llms.txt` and the `OPS` table in `src/protocol.rs` (`--help`) in step
  with the CLI whenever ops or flags change. The skill is compiled into the
  binary, so a stale skill ships with the next release.

## Map

| path | what |
|---|---|
| `src/cdp/` | from-scratch CDP engine: transport, client (+`BT_CDP_TRACE`), browser, page (ops), `events.rs` (dialogs, popups, navigation and fetch/XHR state), `ax.rs` (snapshot tree), `attach.rs` (`--attach` endpoint discovery, `--profile` browser), `procjob.rs` (Windows kill-on-close job) |
| `src/proc.rs` | detached spawn (session servers, `--profile` browsers) |
| `src/protocol.rs` | CLI parsing + JSON-lines protocol + `Driver` |
| `src/session.rs` | named sessions (`--session`, `start`): Unix socket; loopback TCP + token file on Windows |
| `src/timing.rs` | `BT_TIMINGS=1` phase timings printed at shutdown |
| `tests/serve_roundtrip.rs` | serve protocol + named session e2e |
| `tests/site_scenarios.rs` | realistic end-to-end flow on the Acme site (SPA, iframes, shadow DOM, dialogs, popups, login, upload, slow load) |
| `tests/attach.rs` | `--attach` / `--profile`: the browser and the user's tabs survive `quit`, cookies persist, one-shot attach refused |
| `tests/edge_cases.rs` | one regression test per reproduced bug (hung loads, browser death, slow popups, styled checkboxes, cross-origin iframes, caller-relative paths); pages in `tests/fixtures/edge_site.py` |
| `bench/site/server.py` | Acme Supply: deterministic local shop with state at `/__state` |
| `bench/agent-eval/` | LLM agent eval (Gemini CLI) across navigera / playwright-cli / agent-browser |
| `tools/` | `cdp_check.py`, `protocol_dump.sh`, `cft_matrix.py` (Chrome for Testing matrix) |
| `.claude/skills/navigera/SKILL.md` | the agent guide, embedded in the binary (`navigera skill`) |
| `bench/ladder/` | driver ladder: `ladder.py` (harness), `contenders/` (incl. `cli_agent.py` for the agent CLI scenario), `annotate.py` |
| `.github/workflows/` | `ci.yml` (push/PR/weekly: tests, CDP check, Chrome matrix, Windows, install.sh/install.ps1), `bench.yml` (manual), `agent-eval.yml` (manual/weekly, parallel per tool), `agent-check.yml` (push/PR/nightly natural purchase checks, Linux + Windows), `vendor.yml`, `release.yml` (one-tap: dispatch with `version`, or a `v*` tag) |
