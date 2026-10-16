#!/usr/bin/env python3
"""Agent chat against the REAL Claude Code (Phase 21 C1 smoke test).

Starts a holder and a runner (no gateway: the runner API on its local socket
is enough), then drives a chat with the real `claude` in its structured mode
on an API key: a streamed answer, a permission round trip, AskUserQuestion
answered with "Other…" through the PreToolUse hook (spike S-C1), a plan
(ExitPlanMode as a permission), an interrupt, a resume after a stop, the
handoff to the terminal and back, and finally checks that the API key
appears nowhere in the chat's files.

    python3 chat-smoke.py --runner ./tmuxedo-runner --claude-dir /opt/claude \\
        --key-file /path/to/key [--model haiku]

The key is read from a file (mode 600) and never printed; HOME is a throwaway
directory, so a developer's own Claude login and settings are never touched.
Costs a few cents of API usage. Exit status 0 when every check passed.

Without --key-file it runs the CLI CONTRACT checks only, with a made-up
(invalid) key: the real binary accepts the structured-mode argv and the
generated hook settings, the session starts, the turn fails with the CLI's
own authentication error, shown in the chat, and the key is masked.
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

ap = argparse.ArgumentParser()
ap.add_argument("--runner", required=True)
ap.add_argument("--claude-dir", required=True, help="directory holding the real `claude` binary")
ap.add_argument("--key-file", help="a file holding a real API key; without it, the contract checks only")
ap.add_argument("--model", default="haiku")
ap.add_argument("--keep", action="store_true", help="keep the temp dir (for a look at the log)")
a = ap.parse_args()

key = open(a.key_file).read().strip() if a.key_file else "sk-ant-api03-" + "Contract0Check" * 6 + "AA"
if not key:
    sys.exit("empty key file")
tmp = tempfile.mkdtemp(prefix="chat-smoke-")
home = os.path.join(tmp, "home")
work = os.path.join(home, "work")
os.makedirs(work)
state = os.path.join(tmp, "state")
hsock = os.path.join(tmp, "h.sock")
rsock = os.path.join(tmp, "r.sock")
cfg = os.path.join(tmp, "runner.toml")
with open(cfg, "w") as f:
    f.write("server = 'http://127.0.0.1:9'\ntoken = 'amr.smokesmokesmoke.smokesmokesmokesmokesmokesmokesmoke'\nauto_update = false\n")
env = dict(os.environ)
env.update({
    "HOME": home, "PATH": f"{a.claude_dir}:{env.get('PATH', '/usr/bin:/bin')}",
    "ANTHROPIC_API_KEY": key, "ANTHROPIC_MODEL": a.model, "DISABLE_AUTOUPDATER": "1",
    "TMUXEDO_STATE_DIR": state, "TMUXEDO_HOLDER_SOCKET": hsock, "TMUXEDO_RUNNER_SOCKET": rsock,
    "TMUXEDO_CONFIG": cfg, "TMUXEDO_RUNNER_TOKEN": "amr.smokesmokesmoke.smokesmokesmokesmokesmokesmokesmoke",
})
logs = open(os.path.join(tmp, "processes.log"), "w")
procs = [subprocess.Popen([a.runner, "holder", "--socket", hsock], env=env, stdout=logs, stderr=logs)]
time.sleep(0.5)
procs.append(subprocess.Popen([a.runner, "run", "--no-start-holder", "--server", "ws://127.0.0.1:9/gw/runner", "--shell", "sh"],
                              env=env, stdout=logs, stderr=logs))

results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}  {name}{(' — ' + detail) if detail else ''}", flush=True)
    return ok


def call(method, params=None, timeout=60):
    deadline = time.time() + 20
    while True:
        try:
            s = socket.socket(socket.AF_UNIX)
            s.connect(rsock)
            break
        except OSError:
            if time.time() > deadline:
                raise
            time.sleep(0.2)
    s.settimeout(timeout)
    s.sendall((json.dumps({"id": "1", "method": method, "params": params or {}}) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
    s.close()
    return json.loads(buf)


def events(sid, since=1):
    r = call("chat.read", {"session_id": sid, "from_seq": since, "limit": 1000})
    return r["result"]["events"]


def wait(sid, pred, timeout=180, since=1):
    deadline = time.time() + timeout
    while time.time() < deadline:
        for e in events(sid, since):
            if pred(e):
                return e
        time.sleep(1)
    return None


def turn_done(sid, n, timeout=240):
    return wait(sid, lambda e: e["type"] == "turn.done" and e.get("turnId") == f"t{n}", timeout)


def text_of(sid, n):
    return " ".join(e["text"] for e in events(sid) if e["type"] == "message.done" and e.get("turnId") == f"t{n}")


def leaks():
    out = []
    for root, _dirs, files in os.walk(os.path.join(state, "chat")):
        for fn in files:
            with open(os.path.join(root, fn), "rb") as f:
                if key.encode() in f.read():
                    out.append(fn)
    return out


def contract():
    """The real binary, a made-up key: argv, settings and the error path."""
    r = call("chat.start", {"cwd": work, "text": "Reply with exactly the word: pong"})
    if not check("chat.start", "result" in r, json.dumps(r.get("error", ""))):
        return
    sid = r["result"]["session"]["id"]
    opened = wait(sid, lambda e: e["type"] == "session.opened", 30)
    check("session.opened (login api_key)", opened is not None and opened.get("login") == "api_key")
    # The CLI retries an authentication failure for about three minutes before it gives up.
    d = wait(sid, lambda e: e["type"] == "turn.done", 300)
    check("the turn ends failed (the key is not real), with the CLI's reason", d is not None and d["status"] == "failed",
          json.dumps(d)[:400] if d else "no turn.done")
    retries = [e for e in events(sid) if e["type"] == "notice" and e.get("code") == "api_retry"]
    check("its retries show as one notice", len(retries) == 1, f"{len(retries)} notices")
    stderr = ""
    for root, _dirs, files in os.walk(os.path.join(state, "chat")):
        if "stderr.log" in files:
            stderr = open(os.path.join(root, "stderr.log"), errors="replace").read()
    bad = [w for w in ("unknown option", "error: option", "unknown argument", "unexpected argument") if w in stderr.lower()]
    check("the CLI accepted the structured-mode argv and settings", not bad, stderr[-300:])
    print("events:", " ".join(e["type"] for e in events(sid)), flush=True)
    info = call("chat.get", {"session_id": sid})["result"]["session"]
    print("session:", json.dumps({k: info.get(k) for k in ("status", "model", "login", "hooksSeen")}), flush=True)
    check("system/init reported the model", bool(info.get("model")))
    check("the CLI ran tmuxedo's hooks (generated settings, TMUXEDO_CHAT_ID)", (info.get("hooksSeen") or 0) > 0, str(info.get("hooksSeen")))
    check("the key is in no chat file", not leaks(), ", ".join(leaks()))
    call("chat.send", {"session_id": sid, "text": "again"})
    d2 = wait(sid, lambda e: e["type"] == "turn.done" and e.get("turnId") == "t2", 300)
    check("a second message gets its own turn (resumed if the CLI had exited)", d2 is not None, json.dumps(d2)[:300] if d2 else "")
    call("chat.close", {"session_id": sid})


def full():
    r = call("chat.start", {"cwd": work, "text": "Reply with exactly the word: pong"})
    if not check("chat.start on an API key", "result" in r, json.dumps(r.get("error", ""))):
        return
    sid = r["result"]["session"]["id"]
    opened = wait(sid, lambda e: e["type"] == "session.opened", 30)
    check("session.opened says api_key", opened is not None and opened.get("login") == "api_key")
    d = turn_done(sid, 1)
    check("turn 1 completed", d is not None and d["status"] == "completed", json.dumps(d)[:300] if d else "no turn.done")
    check("streamed answer contains pong", "pong" in text_of(sid, 1).lower(), text_of(sid, 1)[:200])
    check("usage reported", bool(d and d.get("usage", {}).get("outputTokens")))

    # A permission round trip (the PermissionRequest hook hold).
    call("chat.send", {"session_id": sid, "text": "Use the Bash tool to run exactly this command: echo smoke-ok-1234. Then say done."})
    need = wait(sid, lambda e: e["type"] == "needs.opened", 120)
    ok = check("permission item raised", bool(need) and need["item"]["kind"] == "permission",
               json.dumps(need["item"])[:300] if need else "none")
    if ok:
        r = call("chat.respond", {"session_id": sid, "need_id": need["item"]["id"], "response": {"kind": "allow_once"}, "by": "smoke"})
        check("answered by id", r.get("result", {}).get("outcome") == "accepted", json.dumps(r)[:200])
        t = wait(sid, lambda e: e["type"] == "tool.done" and "smoke-ok-1234" in (e.get("output") or ""), 120)
        check("the tool ran and its output came back", t is not None)
    d = turn_done(sid, 2)
    check("turn 2 completed", d is not None and d["status"] == "completed")

    # Spike S-C1: AskUserQuestion answered through the PreToolUse hook, with "Other…".
    seq = events(sid)[-1]["seq"] + 1
    call("chat.send", {"session_id": sid, "text": "Use the AskUserQuestion tool to ask me one question: whether I prefer tea or coffee "
                                                  "(two options). After I answer, reply with: You chose <my answer>."})
    q = wait(sid, lambda e: e["type"] == "needs.opened" and e["item"]["kind"] == "question", 120, since=seq)
    ok = check("S-C1: AskUserQuestion became a question item", q is not None,
               json.dumps(q["item"])[:300] if q else "no question item (the agent may have asked in text)")
    if ok:
        qt = q["item"]["questions"][0]["text"]
        r = call("chat.respond", {"session_id": sid, "need_id": q["item"]["id"],
                                  "response": {"kind": "answers", "answers": {qt: "hot chocolate"}}})
        check("S-C1: answer accepted", r.get("result", {}).get("outcome") == "accepted", json.dumps(r)[:200])
    d = turn_done(sid, 3)
    check("S-C1: the agent saw the Other answer", "chocolate" in text_of(sid, 3).lower(), text_of(sid, 3)[:200])

    # A plan: ExitPlanMode arrives as a permission.
    r = call("chat.mode", {"session_id": sid, "mode": "plan_first"})
    check("mode switch accepted", r.get("result", {}).get("mode") == "plan_first", json.dumps(r)[:200])
    time.sleep(2)
    seq = events(sid)[-1]["seq"] + 1
    call("chat.send", {"session_id": sid, "text": "Plan how you would create a file named hello.txt containing hi. Present the plan for approval."})
    p = wait(sid, lambda e: e["type"] == "needs.opened", 180, since=seq)
    ok = check("plan item raised (ExitPlanMode)", bool(p) and p["item"]["kind"] == "plan",
               json.dumps(p["item"])[:300] if p else "none")
    if ok:
        r = call("chat.respond", {"session_id": sid, "need_id": p["item"]["id"],
                                  "response": {"kind": "keep_planning", "message": "do not create anything; stop here"}})
        check("keep planning accepted", r.get("result", {}).get("outcome") == "accepted")
    d = turn_done(sid, 4)
    check("turn 4 ended", d is not None, json.dumps(d)[:200] if d else "")
    check("no hello.txt created", not os.path.exists(os.path.join(work, "hello.txt")))

    # Interrupt.
    call("chat.mode", {"session_id": sid, "mode": "ask_first"})
    time.sleep(2)
    call("chat.send", {"session_id": sid, "text": "Count slowly from 1 to 400, one number per line, in plain text."})
    started = wait(sid, lambda e: e["type"] == "turn.started" and e.get("turnId") == "t5", 60)
    time.sleep(4)
    r = call("chat.interrupt", {"session_id": sid})
    check("interrupt sent", r.get("result", {}).get("interrupted") is True, json.dumps(r)[:200])
    d = turn_done(sid, 5, 120)
    check("turn 5 interrupted", d is not None and d["status"] == "interrupted", json.dumps(d)[:200] if d else "no turn.done")
    # After an interrupt the session goes on (the CLI may have exited: it resumes by id).
    call("chat.send", {"session_id": sid, "text": "Reply with exactly the word: again"})
    d = turn_done(sid, 6)
    check("resumed after the interrupt, same conversation", d is not None and "again" in text_of(sid, 6).lower(), text_of(sid, 6)[:200])

    # Chat -> terminal -> chat on one session.
    r = call("chat.handoff", {"session_id": sid, "to": "terminal"}, timeout=60)
    pane = r.get("result", {}).get("paneId")
    check("handed off to a terminal pane", bool(pane), json.dumps(r)[:200])
    time.sleep(8)
    deadline = time.time() + 60
    back = None
    while time.time() < deadline:
        back = call("chat.handoff", {"session_id": sid, "to": "chat"})
        if "result" in back:
            break
        time.sleep(2)
    check("back to chat", back is not None and "result" in back, json.dumps(back)[:200])
    call("chat.send", {"session_id": sid, "text": "What was the very first word I asked you to reply with? One word."})
    d = turn_done(sid, 7)
    check("the same conversation after the terminal", "pong" in text_of(sid, 7).lower(), text_of(sid, 7)[:200])

    # The key is nowhere in the chat's files (masked before anything is written).
    check("the API key is in no chat file", not leaks(), ", ".join(leaks()))
    types = {}
    for e in events(sid):
        types[e["type"]] = types.get(e["type"], 0) + 1
    print("event types:", json.dumps(types, sort_keys=True))
    call("chat.close", {"session_id": sid})


try:
    if a.key_file:
        full()
    else:
        contract()
finally:
    for p in procs[::-1]:
        p.terminate()
    for p in procs:
        try:
            p.wait(5)
        except subprocess.TimeoutExpired:
            p.kill()
    if a.keep:
        print("kept", tmp)
    else:
        shutil.rmtree(tmp, ignore_errors=True)

print(f"{sum(results)}/{len(results)} checks passed")
sys.exit(0 if all(results) else 1)
