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
| compiler / clippy file:line | ci.yml (problem matcher) | the error itself |
| `ladder table N/M` | bench.yml | the markdown results table |
| `ladder json N/M` | bench.yml | `publish.json` (chart-ready results; join chunks in order) |
| `ladder log tail N/M` | bench.yml (on failure) | last 30 KB of ladder.log |

If something you need isn't there, add an annotation for it (see
`bench/ladder/annotate.py`) — don't push a debug commit to find out.

## 3. Fast loop

1. **Local first** when you have Chrome and crates.io: `cargo test` runs the
   browser e2e tests (serve protocol, `ax` refs, trusted clicks, named
   sessions) against the local Chrome; `cargo clippy -- -D warnings` is the
   CI lint gate.
2. **No local browser/registry?** Push a *branch* and dispatch narrow runs:
   ```sh
   gh workflow run ci.yml    --ref my-branch                      # ~3 min
   gh workflow run bench.yml --ref my-branch -f reps=1 \
       -f only=bt-serve,gorod -f scenarios=                        # ~3 min, session gate only
   ```
   `only` takes any contender names (`bt-serve bt-edge bt-brave bt-lightpanda
   playwright puppeteer chromiumoxide chromedp gorod`); `scenarios` is any of
   `eval,cold,realworld,browse` (empty = the session gate only).
3. **One decisive run per hypothesis.** Decide beforehand what output would
   confirm or kill the hypothesis, and make sure that output lands in an
   annotation. If two runs in a row didn't change your mind, stop and re-read
   the code path end to end instead of adding more logging.
4. Full `reps=3` bench only after CI and a narrow run are green.

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

## 6. Repo hygiene and history

- Master history is curated (reset to a single commit on 2026-10-06). Work on
  a branch, squash to meaningful commits, no "debug"/"temp"/"diagnostic"
  commits on master. Don't force-push master unless the owner asks.
- Outputs go to `bench/ladder/out/` (gitignored). Never commit cookie jars,
  reports or page dumps from other tools run in this checkout.
- Keep `.claude/skills/browser-tool/SKILL.md` and `docs/browser-tool.md` in
  step with the CLI whenever ops or flags change.

## Map

| path | what |
|---|---|
| `src/cdp/` | from-scratch CDP engine (transport, client, browser, page) |
| `src/protocol.rs` | CLI parsing + JSON-lines protocol + `Driver` |
| `src/session.rs` | named Unix-socket sessions (`--session`, `start`) |
| `tests/serve_roundtrip.rs` | browser e2e tests (run in CI with real Chrome) |
| `bench/ladder/` | driver ladder: `ladder.py` (harness), `contenders/`, `annotate.py` |
| `.github/workflows/` | `ci.yml` (every push/PR), `bench.yml` (manual dispatch) |
