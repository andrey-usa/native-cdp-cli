#!/usr/bin/env python3
"""Agent-style CLI scenario: one process per step against a warm browser.

This is how an AI agent with a shell tool drives a browser: every action is
a separate command, and a background session keeps the browser warm between
them. It runs the same canonical session as the rest of the ladder (listing
page: title, count, 500-card extract; fill + click filter; visible count;
screenshot; detail page in a new tab: title, rows) plus one accessibility
snapshot, the agent's usual way to look at a page.

Tools:
  bt   browser-tool --session <name> <op>   (session server: `start` / `quit`)
  ab   agent-browser --session <name> --json <op>   (Vercel Labs; native Rust
       CDP daemon that starts on the first command)

Prints one JSON line: {tool, wall_s, steps:[{op, ms}], snapshot_bytes,
counts, extract_out}. Env: BROWSER_TOOL, CHROME_BIN, LADDER_BASE,
AGENT_BROWSER (binary, default `agent-browser`).
"""
import argparse
import json
import os
import subprocess
import sys
import time

# Same expressions as bt_serve.py, written as IIFEs so a tool that evaluates
# the expression as-is (rather than calling a returned function) gets values.
TITLE = "(() => document.title)()"
CARDS = "(() => document.querySelectorAll('.card').length)()"
EXTRACT = ("(() => Array.from(document.querySelectorAll('.card')).map(c => ({"
           "t: c.querySelector('.t').textContent, "
           "p: c.querySelector('.p').textContent, "
           "v: c.dataset.vendor})))()")
VISIBLE = "(() => document.querySelectorAll('.card:not(.hidden)').length)()"
ROWS = "(() => document.querySelectorAll('#rows tr').length)()"


def run(argv: list[str], env: dict) -> tuple[float, str]:
    t0 = time.perf_counter()
    p = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=120)
    ms = (time.perf_counter() - t0) * 1000.0
    if p.returncode != 0:
        raise RuntimeError(f"{argv[:4]}... exited {p.returncode}\nstdout: {p.stdout[-1500:]}\n"
                           f"stderr: {p.stderr[-1500:]}")
    return ms, p.stdout


def result_of(tool: str, out: str):
    doc = json.loads(out.strip().splitlines()[-1])
    if tool == "bt":
        if not doc.get("ok"):
            raise RuntimeError(f"browser-tool error: {doc.get('error')}")
        return doc.get("result")
    if not doc.get("success", False):
        raise RuntimeError(f"agent-browser error: {doc.get('error')}")
    data = doc.get("data") or {}
    return data.get("result", data)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tool", choices=["bt", "ab"], required=True)
    ap.add_argument("--extract-out", required=True)
    ap.add_argument("--shot-out", required=True)
    ap.add_argument("--session", default=f"ladder-{os.getpid()}")
    args = ap.parse_args()

    base = os.environ["LADDER_BASE"]
    env = dict(os.environ)
    s = args.session
    if args.tool == "bt":
        exe = os.environ["BROWSER_TOOL"]

        def cmd(*a):
            return [exe, "--session", s, *a]

        def ev(js):
            return cmd("eval", "--expression", js)

        plan = [
            ("start", cmd("start", "--chromium", env["CHROME_BIN"])),
            ("goto", cmd("goto", "--url", f"{base}/page_a.html")),
            ("eval title", ev(TITLE)),
            ("eval count", ev(CARDS)),
            ("eval extract", ev(EXTRACT)),
            ("snapshot", cmd("ax")),
            ("fill", cmd("fill", "--selector", "#q", "--value", "widget")),
            ("click", cmd("click", "--selector", "#search")),
            ("eval visible", ev(VISIBLE)),
            ("screenshot", cmd("screenshot", "--path", args.shot_out)),
            ("tab new", cmd("tab-new", "--url", f"{base}/page_b.html")),
            ("eval title b", ev(TITLE)),
            ("eval rows", ev(ROWS)),
            ("close", cmd("quit")),
        ]
    else:
        exe = os.environ.get("AGENT_BROWSER", "agent-browser")
        env.setdefault("AGENT_BROWSER_EXECUTABLE_PATH", env.get("CHROME_BIN", ""))
        # Same sandbox setting every other contender launches Chrome with.
        env.setdefault("AGENT_BROWSER_ARGS", "--no-sandbox")

        def cmd(*a):
            return [exe, "--session", s, "--json", *a]

        def ev(js):
            return cmd("eval", js)

        plan = [
            ("goto", cmd("open", f"{base}/page_a.html")),  # also starts daemon + browser
            ("eval title", ev(TITLE)),
            ("eval count", ev(CARDS)),
            ("eval extract", ev(EXTRACT)),
            ("snapshot", cmd("snapshot")),
            ("fill", cmd("fill", "#q", "widget")),
            ("click", cmd("click", "#search")),
            ("eval visible", ev(VISIBLE)),
            ("screenshot", cmd("screenshot", args.shot_out)),
            ("tab new", cmd("tab", "new", f"{base}/page_b.html")),
            ("eval title b", ev(TITLE)),
            ("eval rows", ev(ROWS)),
            ("close", cmd("close")),
        ]

    steps, values, snapshot_bytes = [], {}, 0
    t0 = time.perf_counter()
    try:
        for name, argv in plan:
            ms, out = run(argv, env)
            steps.append({"op": name, "ms": round(ms, 2)})
            if name.startswith("eval"):
                values[name] = result_of(args.tool, out)
            elif name == "snapshot":
                snapshot_bytes = len(out.encode())
                result_of(args.tool, out)  # raises on a failed snapshot
            elif name not in ("start", "close"):
                result_of(args.tool, out)
    finally:
        wall = time.perf_counter() - t0
    with open(args.extract_out, "w", encoding="utf-8") as f:
        json.dump(values.get("eval extract"), f)
    counts = {
        "title_a": values.get("eval title"),
        "cards_a": values.get("eval count"),
        "visible": values.get("eval visible"),
        "title_b": values.get("eval title b"),
        "rows_b": values.get("eval rows"),
    }
    print(json.dumps({"tool": args.tool, "wall_s": wall, "steps": steps,
                      "snapshot_bytes": snapshot_bytes, "counts": counts,
                      "extract_out": args.extract_out}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
