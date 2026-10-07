#!/usr/bin/env node
// Playwright benchmark: Chrome (headless, node driver) or a Lightpanda CDP server.
//
// DEPRECATED: kept only so the old engine A/B in bench/out/ stays reproducible.
// Realtime browser driving is now `navigera` (src/bin/navigera.rs):
// same RustWright+Chromium engine, zero Node.js — `navigera eval …`
// replaces `page.evaluate`, `navigera serve` replaces a Playwright session.
// Mirrors examples/bench_browser.rs (RustWright): same URLs, same PROBE_JS and
// same poll pacing, so the two engines are directly comparable.
//
//   node pw-bench.mjs --engine chrome --runs 2 --url <u> [--url <u>]
//   node pw-bench.mjs --engine lightpanda --lightpanda-port 9222
//
// Env: LIGHTPANDA_BIN (binary, default `lightpanda`), LIGHTPANDA_WSL_DISTRO
// (run it through `wsl.exe -d <distro>`), LIGHTPANDA_HOST (default 127.0.0.1).
import { spawn } from 'node:child_process';
import { chromium } from 'playwright-core';

/** Must stay identical to PROBE_JS in examples/bench_browser.rs. */
const PROBE_JS = `() => ({
  title: document.title,
  html_len: document.documentElement ? document.documentElement.outerHTML.length : 0,
  text_len: document.body ? (document.body.innerText || '').length : 0,
  links: document.querySelectorAll('a[href]').length,
  s_item: document.querySelectorAll('.s-item, .s-item-card, .s-card').length,
  itm_links: document.querySelectorAll('a[href*="/itm/"]').length,
  quote_links: document.querySelectorAll('a[href*="quoteForm.cgi"]').length
})`;

// Same as BrowserSession::launch for the chrome engine.
const CHROME_ARGS = [
  '--disable-blink-features=AutomationControlled',
  '--no-first-run',
  '--no-default-browser-check',
  '--disable-infobars',
  '--window-size=1440,900',
];

// Production eBay pacing is 10 × 1500 ms (`load_srp`); the bench polls faster
// (250 ms) to resolve engine latency.
const MAX_TRIES = 40;
const RETRY_SLEEP_MS = 250;

const argv = process.argv.slice(2);
const has = (name) => argv.includes(name);
const flag = (name, fallback) => {
  const i = argv.indexOf(name);
  return i >= 0 && i + 1 < argv.length ? argv[i + 1] : fallback;
};

const engine = String(flag('--engine', 'chrome')).toLowerCase();
const runs = Number(flag('--runs', '1'));
const headless = !has('--headed');
const timeoutMs = Number(flag('--timeout-ms', '40000'));
const lightpandaPort = Number(flag('--lightpanda-port', '9222'));
const userAgent = flag('--user-agent', '');
const label = flag('--label', `playwright-${engine}`);

let urls = [];
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === '--url' && i + 1 < argv.length) urls.push(argv[i + 1]);
}
if (urls.length === 0) {
  urls = [
    'https://www.ebay.com/sch/i.html?_nkw=600-10070&_sacat=0&_sop=15&LH_ItemCondition=3000',
  ];
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitForCdp(port, deadlineMs = 30000) {
  const deadline = Date.now() + deadlineMs;
  let lastError = 'unknown';
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/json/version`);
      const payload = await response.json();
      if (payload.webSocketDebuggerUrl) return payload;
      lastError = 'no webSocketDebuggerUrl';
    } catch (error) {
      lastError = String(error);
    }
    await sleep(250);
  }
  throw new Error(`Lightpanda CDP not reachable on ${port}: ${lastError}`);
}

/** Start `lightpanda serve` (directly, or through wsl.exe on Windows). */
function startLightpanda(port) {
  const host = process.env.LIGHTPANDA_HOST || '127.0.0.1';
  const bin = process.env.LIGHTPANDA_BIN || 'lightpanda';
  const distro = process.env.LIGHTPANDA_WSL_DISTRO || '';
  const serve = ['serve', '--host', host, '--port', String(port)];
  const [cmd, args] = distro
    ? ['wsl.exe', ['-d', distro, '-e', bin, ...serve]]
    : [bin, serve];
  return spawn(cmd, args, { stdio: ['ignore', 'ignore', 'inherit'] });
}

function summary(navs) {
  const ok = navs.filter((n) => !n.error && n.cards > 0);
  const total = navs.reduce((acc, n) => acc + n.total_ms, 0);
  const avgOk = ok.reduce((acc, n) => acc + n.total_ms, 0) / (ok.length || 1);
  return {
    navs: navs.length,
    productive_navs: ok.length,
    total_ms: total,
    avg_productive_ms: Math.round(avgOk),
  };
}

async function main() {
  let proc = null;
  let browser = null;
  let context = null;
  let launchMs = 0;

  const launchStart = Date.now();
  if (engine === 'lightpanda') {
    proc = startLightpanda(lightpandaPort);
    await waitForCdp(lightpandaPort);
    browser = await chromium.connectOverCDP(`http://127.0.0.1:${lightpandaPort}`);
    context =
      browser.contexts()[0] ||
      (await browser.newContext().catch(() => null)) ||
      (await browser.contexts()[0]);
  } else {
    browser = await chromium.launch({
      channel: 'chrome',
      // `headless: true` makes playwright pass the legacy `--headless` flag,
      // which eBay's bot wall rejects; launch windowless and force Chrome's new
      // headless mode, mirroring RustWright's `--headless=new`.
      headless: false,
      args: headless ? [...CHROME_ARGS, '--headless=new'] : CHROME_ARGS,
      // RustWright launches a plain Chromium command line; playwright would add
      // `--enable-automation`, another automation tell.
      ignoreDefaultArgs: ['--enable-automation'],
    });
    // Prefer the default context RustWright would use (playwright's fresh
    // context adds its own automation setup).
    context = browser.contexts()[0] || (await browser.newContext());
  }
  launchMs = Date.now() - launchStart;
  console.error(
    `[bench] engine=${engine} ${engine === 'lightpanda' ? 'attach' : 'launch'} ${launchMs} ms`,
  );

  const totalStart = Date.now();
  const navs = [];
  for (let run = 1; run <= Math.max(runs, 1); run++) {
    for (const url of urls) {
      const page = await context.newPage();
      if (userAgent) {
        // Lightpanda sends `Lightpanda/1.0` by default; sites with bot walls
        // (eBay) only answer a real browser UA.
        const cdp = await context.newCDPSession(page).catch(() => null);
        if (cdp) {
          await cdp
            .send('Emulation.setUserAgentOverride', { userAgent })
            .catch((e) => console.error(`[bench] UA override failed: ${e}`));
        } else {
          console.error('[bench] UA override unavailable (no CDP session)');
        }
      }
      const navStart = Date.now();
      let error = null;
      const navWait = engine === 'lightpanda'
        ? { waitUntil: 'commit', timeout: 3000 }
        : { waitUntil: 'load', timeout: timeoutMs };
      try {
        await page.goto(url, navWait);
      } catch (e) {
        error = String(e).split('\n')[0];
        // Lightpanda emits no load/lifecycle events, so a commit wait can time
        // out while the document is already there — the probe decides.
        if (engine === 'lightpanda') error = null;
      }
      const navMs = Date.now() - navStart;

      let probe = null;
      let cards = 0;
      let readyMs = null;
      let cardsMs = null;
      for (let attempt = 0; attempt < MAX_TRIES; attempt++) {
        try {
          // PROBE_JS is a function expression; playwright needs an expression to
          // evaluate, so call it: `(() => ({…}))()`.
          probe = await page.evaluate(`(${PROBE_JS})()`);
          const elapsed = Date.now() - navStart;
          if (readyMs === null && probe.text_len > 200) readyMs = elapsed;
          cards = Math.max(probe.s_item, probe.itm_links, probe.quote_links);
          if (cards > 0) {
            cardsMs = elapsed;
            break;
          }
        } catch (e) {
          error = `probe: ${String(e).split('\n')[0]}`;
          break;
        }
        if (attempt % 4 === 3) {
          await page
            .evaluate('(() => { window.scrollTo(0, 2200); return true; })()')
            .catch(() => {});
        }
        await sleep(RETRY_SLEEP_MS);
      }
      const totalMs = Date.now() - navStart;

      console.error(
        `[bench] run ${run} nav ${navMs} ms, ready ${readyMs} ms, cards ${cardsMs} ms, total ${totalMs} ms · ${cards} cards · title=${JSON.stringify(probe && probe.title)}`,
      );
      navs.push({
        run,
        url,
        nav_ms: navMs,
        ready_ms: readyMs,
        cards_ms: cardsMs,
        dry_ms: totalMs,
        total_ms: totalMs,
        cards,
        error,
        probe,
      });
      await page.close().catch(() => {});
    }
  }
  const totalMs = Date.now() - totalStart;

  if (browser) await browser.close().catch(() => {});
  if (proc) proc.kill();

  console.log(
    JSON.stringify(
      {
        engine,
        label,
        headless,
        launch_ms: launchMs,
        navs,
        total_ms: totalMs,
        summary: summary(navs),
      },
      null,
      2,
    ),
  );
}

main().catch((error) => {
  console.error(`[bench] fatal: ${error && error.stack ? error.stack : error}`);
  process.exit(1);
});
