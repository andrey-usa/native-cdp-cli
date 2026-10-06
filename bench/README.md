# Headless-browser benchmark: RustWright vs Playwright, Chrome vs Lightpanda

> Historical record. Realtime browser driving moved to `browser-tool`
> (`src/bin/browser-tool.rs`): same RustWright+Chromium engine, zero Node.js.
> `browser-tool eval --expression …` replaces a Playwright `page.evaluate`,
> `browser-tool serve` (JSON lines on stdin/stdout) replaces a Playwright
> session — one warm browser, many commands, no relaunch cost.

Measured for this repo's CDP sources (eBay SRP, Car-Part.com search, Hollander
Parts category) to answer: **which browser stack is faster for harvesting used
parts?**

## Harnesses

| Harness | Engine | Command |
| --- | --- | --- |
| RustWright + Chrome | in-process CDP, Chromium launched by RustWright | `cargo run --release --example bench_browser -- --engine chrome` |
| RustWright + Lightpanda | in-process CDP, `lightpanda serve` started directly | `cargo run --release --example bench_browser -- --engine lightpanda` |
| Playwright + Chrome | node driver, `channel: 'chrome'`, `--headless=new` | `node bench/pw-bench.mjs --engine chrome` |
| Playwright + Lightpanda | `chromium.connectOverCDP('http://127.0.0.1:9222')` | `node bench/pw-bench.mjs --engine lightpanda` |

Both harnesses use the **same URLs** (built by the production URL code via
`--print-urls`), the **same `PROBE_JS`** and the **same 250 ms poll pacing**, so
`ready_ms` (document has a body) and `cards_ms` (first probe with extractable
cards) are comparable. Results were stored in `bench/out/*.json` (gitignored).

## Results (Windows 11, Chrome 154, Lightpanda 0.4.1 in WSL2 Fedora, 2026-09-29)

| Stack | Startup | eBay SRP | Car-Part search | Hollander category |
| --- | --- | --- | --- | --- |
| **RustWright + Chrome** | 689 ms (launch) | **52 cards @ 3052 ms**, 21 cards @ 3263 ms (2nd run) | **75 rows @ 1703 ms / 765 ms** | page ready 1445 ms / 708 ms |
| **RustWright + Lightpanda** | 1079 – 4735 ms (WSL cold start) | 0 cards run 1 (bot page) → **50 cards @ 3031 ms** run 2; warm-only run: 50 cards @ 1986 ms, 104 cards @ 592 ms | 0 (Cloudflare "Just a moment...") | page ready 506 ms / 1215 ms |
| **Playwright + Chrome** | 371 ms (launch) | 0 – eBay `Error Page` every run | 0 (Cloudflare challenge) | page ready 3080 ms |
| **Playwright + Lightpanda** | 1144 ms (attach) | 0 – `Error Page` / `Pardon Our Interruption...` | 0 (Cloudflare challenge) | page ready 1786 ms |

`cards` = eBay `.s-item` cards / Car-Part `quoteForm.cgi` rows found by the probe.

## What this says

1. **Startup:** Playwright+Chrome is fastest (~0.4 s) — `playwright-core` adds no
   driver download and Chrome is already installed; RustWright+Chrome is ~0.7 s
   (RustWright launches Chromium itself, in-process CDP, no Node driver).
   RustWright+Lightpanda pays 1 – 4.7 s because each session boots a fresh
   `lightpanda serve` inside WSL.
2. **Per-page speed:** Lightpanda is the fastest renderer when it gets through —
   592 ms for a full eBay SRP page and 0.5 – 1.2 s for the Hollander category,
   versus 0.8 – 3.3 s for Chrome (it never renders pixels, lays out no layout,
   runs JS with `--load-resources` defaults off). RustWright's CDP client is
   ~1 – 3 ms per `Runtime.evaluate`, so client overhead is negligible next to
   navigation.
3. **Getting the data is the bottleneck, not raw speed.** eBay's bot wall served
   its `Error Page` to *every* Playwright-driven session (Chrome **and**
   Lightpanda, launched or attached) and to Chrome's legacy `--headless`, while
   RustWright+Chrome returned 52/21 cards per run and Lightpanda returned 50/104
   cards once the session had a warm-up navigation. Car-Part.com answered
   Cloudflare `Just a moment...` to every Playwright and Lightpanda session, but
   RustWright+Chrome harvested **75 `quoteForm.cgi` rows in 0.8 – 1.7 s**.
4. **Verdict:** for this repo's job — *search for new parts* — **RustWright +
   Chrome headless is the fastest *useful* method** (only stack that gets eBay
   *and* Car-Part reliably). Playwright+Chrome starts marginally quicker but
   loses every blocked page, so its end-to-end time is dominated by poll
   timeouts. **RustWright + Lightpanda** is the fastest engine per page and works
   for eBay, but needs a warm-up navigation and cannot do Car-Part at all, so it
   is not a drop-in replacement today.

## Notes / limitations

* Lightpanda emits neither `Page.loadEventFired` nor
  `Network.responseReceived`, so RustWright's `wait_until: load` never resolves;
  `BrowserSession` uses `commit` + a 500 ms cap and lets the DOM poll decide.
* Lightpanda has no official Windows build — run the Linux binary via WSL
  (`LIGHTPANDA_BIN`, `LIGHTPANDA_WSL_DISTRO`) or a container. Killing
  `wsl.exe`/the shim also ends the Linux-side server, so no orphan processes.
* Playwright needs `headless: false` + `--headless=new` in `args` and
  `ignoreDefaultArgs: ['--enable-automation']` to get close to RustWright's
  command line; even then it was served eBay's bot page.
* Single-machine, single-network A/B; absolute times move with network weather,
  the blocking results are the reproducible part (10+ runs).
