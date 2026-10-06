---
name: browser-tool
description: Drive a real headless Chrome from the shell with browser-tool — one command per step against a warm session. Read pages as a compact accessibility tree with [ref=N], then click/fill/select/press by ref, wait, screenshot, handle tabs, iframes and dialogs. Use for web browsing, scraping, form filling and UI checks instead of writing Playwright/Puppeteer/Python scripts.
---

# browser-tool

A single native binary that drives Chrome over the DevTools Protocol. Use it
**directly from the shell, one command per step**. Don't write wrapper scripts.

## Setup (once)

```bash
browser-tool --version || curl -fsSL https://raw.githubusercontent.com/andrey-usa/native-cdp-cli/master/install.sh | sh
```

It finds Chrome/Chromium on its own (system install, or Playwright/Puppeteer
caches). Otherwise pass `--chromium <path>` to `start`, or set `$CHROME_BIN`.

## The loop: start, look, act, look again

```bash
browser-tool -s work start                    # once: warm headless browser named "work"
browser-tool -s work goto https://example.com
browser-tool -s work --raw ax                 # look: page as a tree with [ref=N]
browser-tool -s work click 12                 # act on a ref from the last ax
browser-tool -s work fill 31 "hello world"
browser-tool -s work press Enter
browser-tool -s work --raw ax                 # look again to verify
browser-tool -s work quit                     # done
```

- Every command prints one JSON line: `{"ok":true,"result":…}` or
  `{"ok":false,"error":"…"}` (exit code 1). Add `--raw` to print only the
  result. Strings are printed as plain text, which is best for `ax` and `eval`.
- Actions return `{tab, tabs, url, title}`, so you can see where you ended up.
- The browser, its tabs and its cookies persist between commands of the same
  session. `-s <name>` is short for `--session <name>`. `$BROWSER_TOOL_SESSION`
  sets a default session. An idle session shuts down after 30 min.

## Reading a page: `ax`

```text
page "Checkout — Acme" http://shop.test/checkout
- banner:
  - link "Cart 2" [ref=9] url=/cart
- main:
  - heading "Checkout" [level=1]
  - textbox "Full name" [ref=21]
  - combobox "Country" [ref=22] value="Select…" options: "Canada", "Mexico", …
  - group "Shipping speed":
    - radio "Standard (5–8 days)" [ref=24]
    - radio "Express (1–2 days)" [checked, ref=25]
  - Iframe "Card payment":
    - document "Card details":
      - textbox "Card number" [ref=41]
  - button "Place order" [ref=30]
```

- `[ref=N]` marks things you can act on. Pass the number straight to
  `click`/`fill`/`select`/…; `@12`, `e12` and `ref=12` also work.
- Nesting shows what belongs together, for example which "Add to cart" button
  sits in which product. Text inside iframes and open shadow DOM is included.
- Content inside collapsed or hidden parts (closed `<details>`, menus that
  open on hover, inactive tabs) is **not** in the tree until you open it:
  click the summary or toggle, or `hover` the menu, then run `ax` again.
- Refs go stale when the page navigates or re-renders. Take a fresh `ax`
  before acting on an old one.
- Big page? Scope it with `ax --selector "#results"` or `ax 57` (a ref), and
  use `--limit <lines>` (default 2000).
- To extract many items, use one `eval` that returns JSON:
  `browser-tool -s work --raw eval "() => [...document.querySelectorAll('.item')].map(e => e.innerText)"`

## Acting

| command | what it does |
|---|---|
| `goto <url> [--wait load\|domcontentloaded\|commit]` | navigate (`example.com` gets `https://`) |
| `click <ref>` / `--selector <css>` / `--text "<visible text>"` | real mouse click; waits for the element and for any page load it triggers |
| `fill <ref> "<value>"` | set an input/textarea value (replaces existing text) |
| `type "<text>" [--ref N]` | type key by key (autocomplete, key listeners) |
| `press <key> [--ref N]` | `Enter`, `Tab`, `Escape`, `ArrowDown`, `Control+a`, … |
| `select <ref> "<option>"` | pick a `<select>` option by label or value |
| `hover <ref>` | move the mouse over an element (hover menus) |
| `scroll [--by 800 \| --to bottom] [<ref>]` | scroll the page (infinite lists load more) or bring a ref into view |
| `upload <ref> <file>…` | set an `<input type=file>` |
| `wait --text "Saved" \| --selector <css> \| --url <part> \| --gone <css> \| --js "<expr>" \| --ms 500` | wait for a condition (default up to 5 s) |
| `eval "<js>"` | run JavaScript in the page; arrow functions are called; result printed as JSON |
| `back` / `forward` / `reload` | history |
| `screenshot [file.png] [--full-page]` | PNG of the viewport or the whole page |
| `tab-new [url]`, `tab-list`, `tab-select <i>`, `tab-close [<i>]` | tabs |
| `dialog --accept\|--dismiss [--prompt-text t]` | how future `alert`/`confirm`/`prompt` dialogs are answered |

Element commands wait up to 5 s for the element to appear. Pass
`--timeout-ms <ms>` to wait longer.

## What the tool handles for you

- **New tabs:** a link with `target=_blank` or `window.open` opens a tab
  that the session adopts and switches to. The response says
  `"new_tabs":[1]`. Use `tab-select 0` to go back.
- **Dialogs:** `alert`, `confirm` and `prompt` are accepted and reported in the
  response as `"dialogs":[{type, message, accepted}]`. Run `dialog --dismiss`
  first if you want "Cancel".
- **Navigation:** a click that loads a new page returns after that page is
  ready. The next command never reads the old page. A page that never
  finishes loading is used as it is after about 5 s, and the response says
  `"loading": true`: `wait --text …` for what you need.
- **Covered elements:** if a toast or overlay covers the target, the click
  waits up to 3 s. If it is still covered, a DOM click is sent instead and the
  response says `synthetic_click: "element is covered by …"`. A cookie banner
  usually needs its own click first. Styled checkboxes and radios are
  clicked through their label automatically.
- **Iframes:** refs work inside any iframe, cross-origin included.
  `--selector` and `--text` only search the top page (and its open shadow
  roots), so act on iframe content by ref.
- **Files:** `upload ./doc.txt` and `screenshot shot.png` use paths relative
  to your current directory.

## When something fails

| error | do this |
|---|---|
| `no element matches selector … / no element with text …` | `ax` to see what is really there; act by ref |
| `ref N not found (stale …)` | take a fresh `ax` |
| `timed out … waiting for load` | the page has a slow resource: `goto <url> --wait domcontentloaded`, then `wait --text …` |
| `select: element is … not a <select>` | custom dropdown: `click` it, `ax`, then click the option by ref |
| `no browser-tool session at …` | run `browser-tool -s <name> start` first |
| text you expect is missing from `ax` | it may be collapsed (`<details>`, accordion, tab) or appear after scrolling: expand it or `scroll`, then `ax` again |

## More

`browser-tool help <command>` prints usage for one command. `browser-tool skill`
prints this guide for the installed version, and `browser-tool install-skill`
copies it to `./.agents/skills` (`--claude` for `./.claude/skills`).
