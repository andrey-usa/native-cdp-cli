#!/usr/bin/env python3
"""Agent eval: how a real, generic coding agent copes with each browser tool.

A real agent (Gemini CLI, headless, `--approval-mode=yolo`) gets a task on
the local Acme Supply site (bench/site/server.py) and one browser tool:

  skilled  the tool is installed and its own Agent Skill is in the workspace
           (`.agents/skills/<tool>/SKILL.md`, the skill each vendor ships)
  onboard  only the tool's name and repository URL: the agent must install
           it from its own docs, then do a short task

Everything else is identical across tools: same model, prompt template,
settings, Chrome binary, site and checks. Success is judged from the site's
recorded state (orders, tickets, logins) plus the final answer, never from
the agent's own claim. The run also records model requests, tokens, tool
calls, how many shell commands used the tool, and off-tool workarounds
(curl against the site, ad-hoc Playwright/Puppeteer scripts).

  run.py --tools browser-tool,agent-browser,playwright-cli --tasks all \
         --mode skilled --model gemini-3.5-flash-lite --out out/

`--agent scripted` replays known-good browser-tool command plans instead of
calling a model: it validates the site, the checks and this harness without
an API key (CI runs it on every eval, and locally).
"""
import argparse
import json
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
SITE = REPO / "bench" / "site" / "server.py"

TOOLS = {
    "browser-tool": {
        "cmd": "browser-tool", "skill": "browser-tool",
        "repo": "https://github.com/andrey-usa/native-cdp-cli",
    },
    "agent-browser": {
        "cmd": "agent-browser", "skill": "agent-browser",
        "repo": "https://github.com/vercel-labs/agent-browser",
    },
    "playwright-cli": {
        "cmd": "playwright-cli", "skill": "playwright-cli",
        "repo": "https://github.com/microsoft/playwright-cli",
    },
}

SKILLED_PROMPT = """You are in {workdir}. The command-line browser tool `{cmd}` is installed, and its usage guide is available to you as the skill "{skill}". Read that skill first, then use `{cmd}` (through shell commands) for every browser interaction. Do not drive a browser through other libraries or scripts, and do not fetch the website with curl, wget or similar.

The website is running at {base}.

Task: {task}

When you are done, end your reply with one line exactly in this form:
FINAL ANSWER: <answer>"""

ONBOARD_PROMPT = """You are in {workdir}. Use the browser automation tool {name} ({repo}) to complete the task below. It is not installed yet: install it by following its own documentation, then use it (through shell commands) for every browser interaction. A Chrome binary is available at $CHROME_BIN. Do not drive a browser through other libraries or scripts, and do not fetch the website with curl, wget or similar.

The website is running at {base}.

Task: {task}

When you are done, end your reply with one line exactly in this form:
FINAL ANSWER: <answer>"""

GEMINI_SETTINGS = {
    "security": {"auth": {"selectedType": "gemini-api-key"}, "folderTrust": {"enabled": False}},
    "model": {"maxSessionTurns": 40},
    "general": {"checkpointing": {"enabled": False}, "enableAutoUpdate": False,
                "enableAutoUpdateNotification": False, "maxAttempts": 10},
    "privacy": {"usageStatisticsEnabled": False},
    "telemetry": {"enabled": False},
    "tools": {"shell": {"inactivityTimeout": 240, "enableInteractiveShell": False},
              "exclude": ["google_web_search"]},
    "experimental": {"enableAgents": False},
    "skills": {"enabled": True},
    "context": {"fileName": ["AGENTS.md", "GEMINI.md"]},
}


# --- site -------------------------------------------------------------------

class Site:
    def __init__(self):
        self.proc = subprocess.Popen([sys.executable, str(SITE), "--port", "0"],
                                     stdout=subprocess.PIPE, text=True)
        line = self.proc.stdout.readline().strip()
        if not line.startswith("LISTENING "):
            raise RuntimeError(f"site did not start: {line!r}")
        self.base = line.split(" ", 1)[1]

    def get(self, path: str):
        with urllib.request.urlopen(self.base + path, timeout=10) as r:
            return json.load(r)

    def close(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


# --- workspace --------------------------------------------------------------

def npm_root() -> str:
    try:
        return subprocess.run(["npm", "root", "-g"], capture_output=True, text=True, timeout=30).stdout.strip()
    except Exception:
        return ""


def prepare(tool: str, mode: str, root: Path, args) -> tuple[Path, dict]:
    work, home = root / "work", root / "home"
    work.mkdir(parents=True)
    (home / ".gemini").mkdir(parents=True)
    settings = json.loads(json.dumps(GEMINI_SETTINGS))
    settings["model"]["name"] = args.model
    if mode == "skilled":
        settings["tools"]["exclude"].append("web_fetch")
    (home / ".gemini" / "settings.json").write_text(json.dumps(settings, indent=1))

    chrome = os.environ["CHROME_BIN"]
    real_home = os.environ.get("HOME", "/root")
    env = {
        **os.environ,
        "HOME": str(home), "GEMINI_CLI_HOME": str(home), "GEMINI_CLI_TRUST_WORKSPACE": "true",
        # Toolchains stay where the runner installed them.
        "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", f"{real_home}/.rustup"),
        "CHROME_BIN": chrome,
        "AGENT_BROWSER_EXECUTABLE_PATH": chrome, "AGENT_BROWSER_ARGS": "--no-sandbox",
        "PLAYWRIGHT_MCP_EXECUTABLE_PATH": chrome, "PLAYWRIGHT_MCP_SANDBOX": "false",
        "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD": "1",
        "DO_NOT_TRACK": "1", "DISABLE_TELEMETRY": "1", "CI": "true",
    }
    if mode == "onboard":
        # Whatever the agent installs lands in this run's own prefix.
        prefix = home / ".npm-global"
        env["NPM_CONFIG_PREFIX"] = str(prefix)
        env["CARGO_HOME"] = str(home / ".cargo")
        env["PATH"] = os.pathsep.join([str(prefix / "bin"), str(home / ".cargo" / "bin"),
                                       str(home / ".local" / "bin"), env.get("PATH", "")])
        if args.bt_bin_dir:
            env["PATH"] = os.pathsep.join(p for p in env["PATH"].split(os.pathsep)
                                          if Path(p).resolve() != Path(args.bt_bin_dir).resolve())
        return work, env

    skills = work / ".agents" / "skills"
    skills.mkdir(parents=True)
    if tool == "browser-tool":
        if args.bt_bin_dir:
            env["PATH"] = os.pathsep.join([args.bt_bin_dir, env.get("PATH", "")])
        subprocess.run(["browser-tool", "install-skill", "--dir", str(skills)], env=env, check=True,
                       capture_output=True)
    elif tool == "agent-browser":
        src = Path(npm_root()) / "agent-browser" / "skills" / "agent-browser"
        shutil.copytree(src, skills / "agent-browser")
    elif tool == "playwright-cli":
        subprocess.run(["playwright-cli", "install", "--skills=agents"], cwd=work, env=env,
                       capture_output=True, timeout=300)
        if not (skills / "playwright-cli" / "SKILL.md").exists():
            raise RuntimeError("playwright-cli install --skills=agents wrote no skill")
        (work / ".playwright").mkdir(exist_ok=True)
        (work / ".playwright" / "cli.config.json").write_text(json.dumps({"browser": {
            "browserName": "chromium",
            "launchOptions": {"executablePath": chrome, "headless": True, "chromiumSandbox": False}}}))
    return work, env


# --- agents -----------------------------------------------------------------

def run_gemini(prompt: str, work: Path, env: dict, args) -> dict:
    cmd = [args.gemini, "-p", prompt, "-m", args.model, "--approval-mode=yolo",
           "--skip-trust", "-o", "stream-json"]
    started = time.time()
    proc = subprocess.Popen(cmd, cwd=work, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, start_new_session=True)
    try:
        out, err = proc.communicate(timeout=args.run_timeout)
        timed_out = False
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        out, err = proc.communicate()
        timed_out = True
    wall = time.time() - started
    events = []
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    text = "".join(e.get("content", "") for e in events
                   if e.get("type") == "message" and e.get("role") == "assistant")
    shells = [e.get("parameters", {}).get("command", "") for e in events
              if e.get("type") == "tool_use" and e.get("tool_name") == "run_shell_command"]
    tool_names = [e.get("tool_name") for e in events if e.get("type") == "tool_use"]
    result = next((e for e in reversed(events) if e.get("type") == "result"), {})
    stats = result.get("stats") or {}
    models = stats.get("models") or {}
    requests = 0
    for m in models.values() if isinstance(models, dict) else []:
        requests += ((m.get("api") or {}).get("totalRequests") or 0) if isinstance(m, dict) else 0
    errors = [e.get("message", "") for e in events if e.get("type") == "error"]
    return {
        "exit": proc.returncode, "timed_out": timed_out, "wall_s": round(wall, 1),
        "text": text, "shell_commands": shells, "tools_used": tool_names,
        "requests": requests or None, "models": list(models) if isinstance(models, dict) else [],
        "tokens_in": stats.get("input_tokens"), "tokens_out": stats.get("output_tokens"),
        "tokens_total": stats.get("total_tokens"), "tool_calls": stats.get("tool_calls"),
        "errors": errors[-3:], "stderr_tail": err[-1500:] if proc.returncode else "",
    }


def scripted_plan(task: str, base: str) -> list[list[str]]:
    """Known-good browser-tool plans (refs found by --text / selectors)."""
    s = ["-s", "eval"]
    plans = {
        "lookup": [["start"], ["goto", f"{base}/shop/p/103"], ["wait", "--selector", "#stock"],
                   ["--raw", "ax", "--selector", "#detail"]],
        "purchase": [["start"], ["goto", f"{base}/shop?q=brass+sprocket"], ["click", "--text", "Accept all"],
                     ["click", "--text", "Add Brass Sprocket to cart"],
                     ["wait", "--js", "document.querySelector('#cart-count').textContent === '1'"],
                     ["click", "--text", "Add Brass Sprocket to cart"],
                     ["wait", "--js", "document.querySelector('#cart-count').textContent === '2'"],
                     ["goto", f"{base}/shop?q=titanium+widget"], ["click", "--text", "Add Titanium Widget to cart"],
                     ["wait", "--js", "document.querySelector('#cart-count').textContent === '3'"],
                     ["goto", f"{base}/cart"], ["fill", "--text", "Coupon code", "SAVE10"],
                     ["click", "--text", "Apply coupon"], ["wait", "--text", "Discount (SAVE10)"],
                     ["click", "--text", "Proceed to checkout"], ["fill", "--selector", "#name", "Ada Lovelace"],
                     ["select", "--selector", "#country", "Canada"],
                     ["click", "--selector", "input[value=express]"], ["click", "--text", "Continue to payment"],
                     ["eval", "() => { const d = document.querySelector('#payframe').contentDocument;"
                              " for (const [id, v] of [['card','4242 4242 4242 4242'],['exp','12/30'],['cvc','123']])"
                              " { const el = d.getElementById(id); el.value = v; el.dispatchEvent(new Event('input')); } return true; }"],
                     ["click", "--text", "Place order"], ["wait", "--url", "/orders/"],
                     ["--raw", "eval", "document.querySelector('#order-id').textContent"]],
        "account": [["start"], ["goto", f"{base}/account"], ["fill", "--selector", "#email", "demo@acme.test"],
                    ["fill", "--selector", "#password", "hunter2"], ["press", "Enter", "--selector", "#password"],
                    ["click", "--text", "Next page"], ["--raw", "ax", "--selector", "table"]],
        "docs": [["start"], ["goto", f"{base}/shop"], ["click", "--text", "Docs"],
                 ["click", "--text", "Returns FAQ"], ["--raw", "ax", "--selector", "#returns-faq"]],
        "support": [["start"], ["goto", f"{base}/support"],
                    ["eval", "() => { const d = document.querySelector('iframe').contentDocument;"
                             " d.getElementById('category').value = 'Billing';"
                             " d.querySelector('input[value=high]').checked = true;"
                             " d.getElementById('message').value = 'Charged twice for order A-1042';"
                             " d.getElementById('send').click(); return true; }"],
                    ["wait", "--ms", "500"],
                    ["--raw", "eval", "document.querySelector('iframe').contentDocument.body.innerText"]],
    }
    return [s + p for p in plans[task]] + [s + ["quit"]]


def run_scripted(task: str, base: str, work: Path, env: dict) -> dict:
    started = time.time()
    shells, outputs = [], []
    for argv in scripted_plan(task, base):
        p = subprocess.run(["browser-tool", *argv], cwd=work, env=env, capture_output=True, text=True, timeout=120)
        shells.append("browser-tool " + " ".join(argv))
        outputs.append(p.stdout)
        if p.returncode != 0:
            return {"exit": p.returncode, "timed_out": False, "wall_s": round(time.time() - started, 1),
                    "text": "", "shell_commands": shells, "tools_used": [], "requests": 0,
                    "errors": [p.stdout[-500:] + p.stderr[-500:]], "stderr_tail": p.stderr[-800:]}
    last = outputs[-2] if len(outputs) > 1 else ""
    return {"exit": 0, "timed_out": False, "wall_s": round(time.time() - started, 1),
            "text": "FINAL ANSWER: " + " ".join(last.split())[-600:], "shell_commands": shells,
            "tools_used": ["run_shell_command"] * len(shells), "requests": 0, "errors": []}


# --- checks -----------------------------------------------------------------

def final_answer(text: str) -> str:
    m = re.findall(r"FINAL ANSWER:\s*(.+)", text)
    return m[-1].strip() if m else text.strip()[-400:]


def check(task: dict, answer: str, site: Site) -> tuple[bool, list[str]]:
    c, why = task["check"], []
    state = site.get("/__state")
    if "answer_product" in c:
        item = site.get("/api/products?q=" + urllib.request.quote(c["answer_product"].lower()))["items"][0]
        if f"{item['price']:.2f}" not in answer:
            why.append(f"price {item['price']:.2f} not in answer")
        if not re.search(rf"(?<!\d){item['stock']}(?!\d)", answer):
            why.append(f"stock {item['stock']} not in answer")
    for needle in c.get("answer_contains", []):
        if needle.lower() not in answer.lower():
            why.append(f"{needle!r} not in answer")
    if "login" in c and c["login"] not in state["logins"]:
        why.append("never logged in")
    if "order" in c:
        want = c["order"]
        orders = state["orders"]
        if len(orders) != 1:
            why.append(f"{len(orders)} orders placed (want 1)")
        if orders:
            o = orders[-1]
            got = {i["name"]: i["qty"] for i in o["items"]}
            if got != want["items"]:
                why.append(f"items {got}")
            for k in ("coupon", "country", "speed", "name"):
                if str(o.get(k)) != want[k]:
                    why.append(f"{k}={o.get(k)!r}")
            if o["id"] not in answer:
                why.append(f"order id {o['id']} not in answer")
    if "ticket" in c:
        want = c["ticket"]
        tickets = state["tickets"]
        if not tickets:
            why.append("no ticket created")
        else:
            t = tickets[-1]
            for k in ("category", "priority"):
                if t.get(k) != want[k]:
                    why.append(f"{k}={t.get(k)!r}")
            if want["message_contains"] not in (t.get("message") or ""):
                why.append("message differs")
            if t["id"] not in answer:
                why.append(f"ticket id {t['id']} not in answer")
    return not why, why


OFF_TOOL = re.compile(r"\b(curl|wget|httpie)\b.*127\.0\.0\.1|require\(['\"](playwright|puppeteer)|"
                      r"from playwright|import (playwright|puppeteer)|chromedp|selenium", re.I)


# --- main -------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tools", default="browser-tool,agent-browser,playwright-cli")
    ap.add_argument("--tasks", default="all")
    ap.add_argument("--mode", choices=["skilled", "onboard"], default="skilled")
    ap.add_argument("--agent", choices=["gemini", "scripted"], default="gemini")
    ap.add_argument("--model", default="gemini-3.5-flash-lite")
    ap.add_argument("--gemini", default="gemini")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--run-timeout", type=int, default=900)
    ap.add_argument("--bt-bin-dir", default=str(REPO / "target" / "release"),
                    help="directory holding the browser-tool binary under test")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    tasks = json.loads((HERE / "tasks.json").read_text())["tasks"]
    if args.tasks != "all":
        wanted = set(args.tasks.split(","))
        tasks = [t for t in tasks if t["id"] in wanted]
    if args.mode == "onboard":
        tasks = [t for t in tasks if t["id"] in ("lookup", "docs")][:1] or tasks[:1]
    tools = [t for t in args.tools.split(",") if t]
    if args.agent == "scripted":
        tools = ["browser-tool"]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    runs = []
    for rep in range(args.reps):
        for task in tasks:
            for tool in tools:
                root = Path(tempfile.mkdtemp(prefix=f"eval-{tool}-{task['id']}-"))
                site = Site()
                rec = {"tool": tool, "task": task["id"], "rep": rep, "mode": args.mode, "agent": args.agent}
                try:
                    work, env = prepare(tool, args.mode, root, args)
                    spec = TOOLS[tool]
                    template = SKILLED_PROMPT if args.mode == "skilled" else ONBOARD_PROMPT
                    prompt = template.format(workdir=work, cmd=spec["cmd"], skill=spec["skill"], name=tool,
                                             repo=spec["repo"], base=site.base, task=task["prompt"])
                    res = run_gemini(prompt, work, env, args) if args.agent == "gemini" \
                        else run_scripted(task["id"], site.base, work, env)
                    answer = final_answer(res["text"])
                    ok, why = check(task, answer, site)
                    shells = res["shell_commands"]
                    rec.update(res)
                    rec.update({
                        "ok": ok, "why": why, "answer": answer[-300:],
                        "tool_commands": sum(1 for c in shells if spec["cmd"] in c),
                        "off_tool_commands": [c[:200] for c in shells if OFF_TOOL.search(c)],
                        "shell_count": len(shells),
                    })
                except Exception as e:  # harness/setup problem: record, keep going
                    rec.update({"ok": False, "why": [f"harness: {e}"]})
                finally:
                    site.close()
                    # Each tool's own daemon/browser must not leak into the next run.
                    for pat in ("agent-browser", "playwright-cli", "cli-daemon", "browser-tool"):
                        subprocess.run(["pkill", "-f", f"{pat}.*{root.name}"], capture_output=True)
                rec.pop("text", None)
                runs.append(rec)
                status = "PASS" if rec["ok"] else "FAIL " + "; ".join(rec.get("why", []))[:200]
                print(f"[{args.mode}] {tool:15} {task['id']:9} rep{rep}: {status} "
                      f"(requests={rec.get('requests')}, shell={rec.get('shell_count')}, "
                      f"wall={rec.get('wall_s')}s)", flush=True)
                (out / "runs.jsonl").open("a").write(json.dumps(rec) + "\n")

    # Summary table per tool.
    def med(xs):
        xs = [x for x in xs if isinstance(x, (int, float))]
        return statistics.median(xs) if xs else None

    lines = [f"### Agent eval — {args.mode} ({args.agent}, model {args.model if args.agent == 'gemini' else '—'})", "",
             "| tool | passed | median requests | median shell cmds | median tokens | median wall | off-tool cmds |",
             "|---|---|---|---|---|---|---|"]
    summary = []
    for tool in tools:
        rs = [r for r in runs if r["tool"] == tool]
        passed = sum(1 for r in rs if r["ok"])
        row = {"tool": tool, "passed": passed, "runs": len(rs),
               "median_requests": med([r.get("requests") for r in rs]),
               "median_shell": med([r.get("shell_count") for r in rs]),
               "median_tokens": med([r.get("tokens_total") for r in rs]),
               "median_wall_s": med([r.get("wall_s") for r in rs]),
               "off_tool": sum(len(r.get("off_tool_commands") or []) for r in rs)}
        summary.append(row)
        fmt = lambda v, f="{:.0f}": "—" if v is None else f.format(v)
        lines.append(f"| `{tool}` | {passed}/{len(rs)} | {fmt(row['median_requests'])} | {fmt(row['median_shell'])} | "
                     f"{fmt(row['median_tokens'])} | {fmt(row['median_wall_s'], '{:.0f}s')} | {row['off_tool']} |")
    lines += ["", "| task | " + " | ".join(f"`{t}`" for t in tools) + " |",
              "|---|" + "---|" * len(tools)]
    for task in tasks:
        cells = []
        for tool in tools:
            rs = [r for r in runs if r["tool"] == tool and r["task"] == task["id"]]
            cells.append(" ".join("✓" if r["ok"] else "✗" for r in rs) or "—")
        lines.append(f"| {task['id']} | " + " | ".join(cells) + " |")
    fails = [r for r in runs if not r["ok"]]
    if fails:
        lines += ["", "Failures:"]
        for r in fails:
            lines.append(f"- `{r['tool']}` {r['task']}: {'; '.join(r.get('why', []))[:240]}"
                         + (" (timed out)" if r.get("timed_out") else "")
                         + (f" — {r['errors'][-1][:160]}" if r.get("errors") else ""))
    table = "\n".join(lines) + "\n"
    (out / f"table-{args.mode}.md").write_text(table)
    (out / f"summary-{args.mode}.json").write_text(json.dumps({"mode": args.mode, "agent": args.agent,
                                                               "model": args.model, "tools": summary,
                                                               "runs": runs}, indent=1))
    print(table)
    return 0


if __name__ == "__main__":
    sys.exit(main())
