#!/usr/bin/env node
// Diagnostic: does Playwright itself get blocked, or is it the launch flags?
//
// HISTORICAL — kept for the engine A/B record in bench/out/.
// Realtime driving is now `browser-tool` (src/bin/browser-tool.rs), zero Node.
//
// Starts Chrome with the same command line RustWright builds (fresh profile,
// `--headless=new`, no `--enable-automation`), then attaches playwright-core
// over CDP and navigates the eBay SRP.
//
//   node attach-test.mjs [url]
import { spawn } from 'node:child_process';
import { chromium } from 'playwright-core';

const url =
  process.argv[2] ||
  'https://www.ebay.com/sch/i.html?_nkw=600-10070&_sacat=0&_sop=15&LH_ItemCondition=3000';
const port = 9444;
const chrome = 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const profile = `${process.env.TEMP || '/tmp'}/cpf-attach-test-profile`;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const child = spawn(
  chrome,
  [
    `--user-data-dir=${profile}`,
    `--remote-debugging-port=${port}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-blink-features=AutomationControlled',
    '--headless=new',
    '--hide-scrollbars',
    '--no-sandbox',
    '--window-size=1440,900',
    'about:blank',
  ],
  { stdio: 'ignore' },
);

try {
  let ready = false;
  for (let i = 0; i < 60 && !ready; i++) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/json/version`);
      ready = (await response.json()).webSocketDebuggerUrl !== undefined;
    } catch {
      await sleep(250);
    }
  }
  if (!ready) throw new Error('chrome did not expose CDP');

  const browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`);
  const context = browser.contexts()[0];
  const page = await context.newPage();
  await page.goto(url, { waitUntil: 'load', timeout: 30000 });
  const probe = await page.evaluate(
    '(() => ({ title: document.title, s_item: document.querySelectorAll(".s-item, .s-card").length }))()',
  );
  console.log(JSON.stringify({ launched_by: 'rustwright-style argv', ...probe }));
  await browser.close();
} finally {
  child.kill();
}
