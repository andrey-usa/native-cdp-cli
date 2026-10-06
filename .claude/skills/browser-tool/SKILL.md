---
name: browser-tool
description: Drive a real headless Chrome from the shell with browser-tool (native CDP, no Node) — browse, read pages via a compact accessibility snapshot, click/fill by ref, screenshot. Use for any web browsing, scraping or UI check instead of writing Playwright/Puppeteer/Python wrapper scripts.
---

# browser-tool

One Rust binary that speaks the Chrome DevTools Protocol. Use it **directly
from the shell, one command per step** — no wrapper scripts.

## The loop

```bash
B="browser-tool --session work"        # any name; one warm browser per name

$B start                               # once: detached server + headless Chrome
$B goto --url https://example.com
$B ax                                  # look: compact a11y snapshot with refs
$B click --ref 12                      # act on a ref from the last `ax`
$B fill --ref 31 --value "hello"
$B ax                                  # look again to verify
$B quit                                # done: browser + server exit
```

Every command prints exactly one JSON object and exits 0 on `"ok": true`,
1 otherwise (`"error"` says why). The browser, tabs, cookies and page state
persist between commands of the same session. An idle session shuts itself
down after 30 min (`--idle-timeout-s` on `start`; 0 = never).

Why sessions: each shell call is a separate process, so a stdin pipe to
`serve` can't survive between tool calls. `--session` puts the warm browser
behind a Unix socket instead; that's what removes the need for wrappers.

## `ax` — read the page

```json
{"ok":true,"result":[
  {"ref":3,"role":"RootWebArea","name":"Example Domain"},
  {"ref":7,"role":"heading","name":"Example Domain"},
  {"ref":9,"role":"StaticText","name":"This domain is for use in illustrative examples…"},
  {"ref":12,"role":"link","name":"More information..."}]}
```

- Interactive nodes (buttons, links, textboxes, checkboxes, options, …) are
  always listed; other nodes only when they carry text. Wrapper divs and text
  that repeats its parent's label are dropped. Textboxes show `value`.
- `ref` is valid until the page navigates or re-renders that node. After a
  `goto` or a click that changes the page, take a fresh `ax`.
- `ax --all` returns every node (debugging only — much larger).
- Prefer `ax` over screenshots for structure, and over ad-hoc
  `eval("document.querySelectorAll(...)")` for discovering elements.

## Commands

| command | args | result |
|---|---|---|
| `start` | — | `{session, socket, log, pid}` |
| `goto` | `--url <u>` `[--wait load\|domcontentloaded\|commit]` | `{tab, tabs, url}` |
| `ax` | `[--all]` `[--max-depth n]` | `[{ref, role, name, value?}]` |
| `click` | `--ref n` or `--selector <css>` | `{tab, tabs, url}` |
| `fill` | (`--ref n` or `--selector <css>`) `--value <text>` | `{tab, tabs, url}` |
| `text` | `--selector <css>` | `string \| null` |
| `eval` | `--expression "<js>"` (arrow fns are called) | JSON value |
| `title` / `url` | — | string / `{tab, tabs, url}` |
| `screenshot` | `[--path f.png]` `[--full-page]` | `{path, bytes}` |
| `tab-new` / `tab-list` / `tab-select --index n` / `tab-close [--index n]` | | tab inventory |
| `quit` | — | `{bye: true}` |

## Tips

- Batch several reads into one `eval` when `ax` doesn't carry what you need:
  `$B eval --expression "() => [...document.querySelectorAll('h2')].map(h => h.textContent)"`.
- JS-heavy SPA: `goto --wait commit`, then poll with `ax` until content appears.
- `click`/`fill` with `--selector` wait (up to `--timeout-ms`, default 35 s)
  for the element to appear, so no polling loops for late-rendering content.
- Clicks are real (trusted) mouse events at the element's centre. If the
  element has no visible box or is covered, the tool falls back to
  DOM-dispatched events instead of clicking whatever covers it.
- `$BROWSER_TOOL_SESSION=work` lets you drop `--session work` from each call.
- Programs (not agents) can still hold a pipe open:
  `browser-tool serve` reads one JSON command per stdin line, e.g.
  `{"id":1,"op":"goto","url":"https://example.com"}`; the socket speaks the
  same protocol.

## Engines

`--engine chrome` (default; Chrome/Chromium/Edge/Brave via `--chromium <path>`)
or `--engine lightpanda` (single tab; binary via `$LIGHTPANDA_BIN` or PATH).
