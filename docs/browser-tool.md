# `browser-tool` — Node-free realtime browser driver

`playwright-cli` (the `playwright cli` / MCP terminal commands) drags a
Node.js driver process into every browser action. This repo replaces it with
**`browser-tool`**, a Rust binary in `src/bin/browser-tool.rs` that drives the
same RustWright + Chromium stack the production agent already uses — no Node,
no `node_modules`, no per-command browser relaunch.

```text
# build once
cargo build --release --bin browser-tool     # target/release/browser-tool.exe
```

## Two modes

### 1. Serve mode — one warm browser, realtime commands (the model contract)

```text
browser-tool serve [--headed] [--engine chrome|lightpanda] [--chromium <path>]
```

The browser launches once. Every following line on **stdin** is a JSON object;
one JSON response line is written to **stdout** (logs go to stderr):

```json
{"id":1,"op":"tab-new","url":"https://www.ebay.com/sch/i.html?_nkw=600-10070"}
{"id":2,"op":"eval","expression":"() => document.title","timeout_ms":25000}
{"id":3,"op":"fill","selector":"#kw","value":"600-10070"}
{"id":4,"op":"goto","url":"https://example.com"}
{"id":5,"op":"quit"}
```

Response shape — every reply echoes the request `id`:

```json
{"id":2,"ok":true,"result":"600-10070 for sale | eBay","elapsed_ms":3}
{"id":9,"ok":false,"error":"click \"#missing\": … click: no element matches selector within 9750 ms …","elapsed_ms":9760}
```

| op | fields | result |
| --- | --- | --- |
| `open` | `url?` | `{tab, tabs, url}` (session is already open in serve mode) |
| `goto` | `url`, `wait?` (`load`/`domcontentloaded`/`commit`) | `{tab, tabs, url}` |
| `click` | `selector` or `ref`, `timeout_ms?` | `{tab, tabs, url}` — waits for the selector, then a trusted CDP mouse click at the element's centre (DOM events if it has no hittable box) |
| `fill` | `selector` or `ref`, `value`, `timeout_ms?` | `{tab, tabs, url}` — waits for the selector; native value setter + `input`/`change` |
| `text` | `selector`, `timeout_ms?` | `string \| null` |
| `eval` | `expression`, `timeout_ms?` | JSON value returned by the JS expression |
| `title` | — | `string` |
| `ax` | `max_depth?`, `all?`, `timeout_ms?` | compact `[{ref, role, name, value?}]` (interactive + named nodes; `ref` feeds `click`/`fill`); `all:true` → every non-ignored node as `{id, role, name, backendNodeId}` |
| `url` | — | `{tab, tabs, url}` |
| `tab-list` | — | `{tabs:[{index,target,active}], active, url}` |
| `tab-new` | `url?` | tab inventory (new tab becomes active) |
| `tab-select` | `index` | `{tab, tabs, url}` |
| `tab-close` / `close-page` | `index?` (default: active) | `{tab, tabs, url}` |
| `screenshot` | `path?`, `full_page?` | `{path, bytes}` |
| `quit` | — | `{"bye":true}`, then the browser shuts down and the process exits |

Malformed lines and unknown ops return `ok:false` with the session left
running; EOF on stdin or `quit` closes the browser and exits 0.

**Model workflow:** `tab-new` → `goto` → observe with `ax` (one round trip,
token-efficient) or `eval`/`text`/`title` →
act with `click`/`fill` → verify with `eval` → `quit`. Results always carry
`{tab, tabs, url}` so the model knows which tab it is driving.

### Named sessions — serve over a Unix socket (for agents)

An agent's shell runs each tool call as its own process, so it can't keep
`serve`'s stdin open. `--session <name>` moves the warm browser behind a
socket (`$TMPDIR/browser-tool-<name>.sock`; a value with `/` is a path):

```text
browser-tool --session work start                  # detached server; idle shutdown after 30 min (--idle-timeout-s)
browser-tool --session work goto --url https://example.com
browser-tool --session work ax
browser-tool --session work click --ref 12
browser-tool --session work quit
```

Each call prints one JSON response (`ok`, `result`/`error`) and exits 0/1.
The socket speaks the serve protocol verbatim. `$BROWSER_TOOL_SESSION`
supplies a default session for client calls.

### 2. One-shot mode — one command per process

```text
browser-tool eval --expression "() => document.title" --pretty
browser-tool goto --url https://example.com
browser-tool fill --selector "#kw" --value "600-10070"
browser-tool text --selector "title"
browser-tool click --selector ".s-item__link" --timeout-ms 10000
browser-tool tab-list | tab-new --url <url> | tab-select --index 1
browser-tool screenshot --path shot.png --full-page
```

Global flags (`--engine`, `--headed`, `--chromium`, `--timeout-ms`,
`--pretty`) work before *or* after the command. Set `BT_TIMINGS=1` to get a
`BT_TIMINGS {"devtools_url": ms, "ws_connect": ms, "first_page": ms,
"close": ms, …}` line on stderr at shutdown (where launch time goes). Exit code 0 on `ok:true`,
1 on a failed op, 2 on bad arguments. Use **serve mode** for anything more
than one action — it pays the ~1 s browser launch once instead of per command.

## Engines

* `--engine chrome` (default): RustWright launches Chrome/Edge/Chromium —
  the same fast, bot-wall-passing path validated in `bench/README.md`.
* `--engine lightpanda`: browser-tool starts `lightpanda serve` directly
  (`$LIGHTPANDA_BIN` or `lightpanda` on `PATH`, `--chromium` overrides) and
  connects to its CDP endpoint. Lightpanda is single-tab (`tab-new` returns
  a clear error) and has no load events, so `goto` uses a short `commit`
  wait; confirm content by polling with `eval`.
  Note: some Lightpanda builds want an X server even for `serve`;
  browser-tool wraps it with `xvfb-run` when
  `$DISPLAY` is unset and xvfb is available.

## Playwright CLI → browser-tool mapping

| Playwright CLI | browser-tool |
| --- | --- |
| `npx playwright cli open <url>` | serve: `{"op":"tab-new","url":…}` or one-shot `goto` |
| `page.goto` / `goto <url>` | `{"op":"goto","url":…}` |
| `page.evaluate` / `eval` | `{"op":"eval","expression":…}` |
| `fill` / `type` | `{"op":"fill","selector":…,"value":…}` |
| `click` | `{"op":"click","selector":…}` |
| `snapshot` (read DOM) | `{"op":"eval","expression":"() => document.documentElement.outerHTML"}` |
| `screenshot` | `{"op":"screenshot","path":…}` |
| `tab-list` / `tab-new` / `tab-close` | same names as ops |
| `close` | `{"op":"quit"}` |
| `--json` output | always JSON (serve) / `--pretty` (one-shot) |

Node.js (`bench/`) remains only as the historical engine A/B recorded in
`bench/README.md`; nothing in the runtime path depends on it.
