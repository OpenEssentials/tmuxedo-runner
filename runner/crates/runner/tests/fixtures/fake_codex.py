#!/usr/bin/env python3
"""A scripted stand-in for the Codex CLI's app-server, for the agent chat tests.

`codex app-server`: JSON-RPC over stdio, one JSON object per line, as far as
docs/design/agent-chat.md §2.2 documents it. The field names are the same
from-memory reading as src/chat/codex.rs: this is NOT a recording of a real
CLI. What a turn does depends on the first word of the message:

  hello        streamed text (deltas) and a final message
  tool         `echo hi` behind item/commandExecution/requestApproval
  risky        `git push origin main` behind the same request
  edit NAME    a file change (creating NAME in the cwd) behind
               item/fileChange/requestApproval
  slow N       N deltas, 150 ms apart (interrupts)
  hang         a command that never returns: the turn ends when interrupted
  think        a reasoning item, then a message
  fail         the turn fails with an error
  junk         a line that is not JSON, an unknown notification, an unknown
               request, a 5 MiB line, then a normal message
  exit         a command starts, then the process prints to stderr and exits 3

Every request and answer it reads is appended to $FAKE_LOG/codex-rpc.log, and
each start's argv to $FAKE_LOG/codex-argv.log.
"""
import json
import os
import sys
import threading
import time

LOCK = threading.Lock()
LOG = os.environ.get("FAKE_LOG", "")


def log(name, obj):
    if LOG:
        with open(os.path.join(LOG, name), "a") as f:
            f.write(json.dumps(obj) + "\n")


def raw(text):
    with LOCK:
        sys.stdout.write(text + "\n")
        sys.stdout.flush()


def out(obj):
    raw(json.dumps(obj))


def note(method, params):
    out({"method": method, "params": params})


class Server:
    def __init__(self):
        self.thread = None
        self.turns = 0
        self.asks = 100
        self.stop = threading.Event()
        self.answers = {}
        self.answered = threading.Condition()

    def ask(self, method, params):
        """A server request; blocks until the client answers or interrupts."""
        self.asks += 1
        rid = self.asks
        out({"id": rid, "method": method, "params": params})
        with self.answered:
            while rid not in self.answers and not self.stop.is_set():
                self.answered.wait(0.05)
            return self.answers.pop(rid, {"decision": "cancel"}).get("decision")

    def message(self, turn, item, text, deltas=()):
        note("item/started", {"turnId": turn, "item": {"type": "agentMessage", "id": item, "text": ""}})
        for d in deltas:
            note("item/agentMessage/delta", {"turnId": turn, "itemId": item, "delta": d})
        note("item/completed", {"turnId": turn, "item": {"type": "agentMessage", "id": item, "text": text}})

    def end(self, turn, status="completed", error=None):
        t = {"id": turn, "status": status}
        if error:
            t["error"] = {"message": error}
        note("turn/completed", {"threadId": self.thread, "turn": t})

    def command(self, turn, item, command, output):
        base = {"type": "commandExecution", "id": item, "command": command, "cwd": os.getcwd()}
        note("item/started", {"turnId": turn, "item": dict(base, status="inProgress")})
        d = self.ask(
            "item/commandExecution/requestApproval",
            {"threadId": self.thread, "turnId": turn, "itemId": item, "command": command, "cwd": os.getcwd()},
        )
        if self.stop.is_set() or d == "cancel":
            # Withdrawn: the turn stops, as an interrupt would stop it.
            return self.end(turn, "interrupted")
        if d in ("accept", "acceptForSession"):
            note("item/commandExecution/outputDelta", {"turnId": turn, "itemId": item, "delta": output})
            done = dict(base, status="completed", aggregatedOutput=output, exitCode=0, durationMs=12)
            note("item/completed", {"turnId": turn, "item": done})
            self.message(turn, item + "-m", "done")
        else:
            note("item/completed", {"turnId": turn, "item": dict(base, status="declined")})
            self.message(turn, item + "-m", "skipped")
        self.end(turn)

    def turn(self, turn, text):
        words = text.split()
        word = words[0] if words else ""
        note("turn/started", {"threadId": self.thread, "turn": {"id": turn, "status": "inProgress"}})
        item = turn + "-i1"
        if word == "tool":
            self.command(turn, item, "echo hi", "hi\n")
        elif word == "risky":
            self.command(turn, item, "git push origin main", "pushed\n")
        elif word == "edit":
            name = words[1]
            path = os.path.join(os.getcwd(), name)
            change = {"path": path, "kind": {"type": "add"}, "diff": "+new\n"}
            base = {"type": "fileChange", "id": item, "changes": [change]}
            note("item/started", {"turnId": turn, "item": dict(base, status="inProgress")})
            d = self.ask(
                "item/fileChange/requestApproval",
                {"threadId": self.thread, "turnId": turn, "itemId": item, "reason": "write outside the sandbox"},
            )
            if d == "accept":
                with open(path, "w") as f:
                    f.write("new\n")
            status = "completed" if d == "accept" else "declined"
            note("item/completed", {"turnId": turn, "item": dict(base, status=status)})
            self.end(turn)
        elif word == "slow":
            note("item/started", {"turnId": turn, "item": {"type": "agentMessage", "id": item, "text": ""}})
            for i in range(int(words[1])):
                if self.stop.wait(0.15):
                    return self.end(turn, "interrupted")
                note("item/agentMessage/delta", {"turnId": turn, "itemId": item, "delta": "w%d " % i})
            note("item/completed", {"turnId": turn, "item": {"type": "agentMessage", "id": item, "text": "slow done"}})
            self.end(turn)
        elif word == "hang":
            cmd = {"type": "commandExecution", "id": item, "command": "sleep 1000", "status": "inProgress"}
            note("item/started", {"turnId": turn, "item": cmd})
            self.stop.wait()
            self.end(turn, "interrupted")
        elif word == "think":
            note("item/started", {"turnId": turn, "item": {"type": "reasoning", "id": item}})
            time.sleep(0.05)
            done = {"type": "reasoning", "id": item, "summary": ["Weighing the options."], "content": []}
            note("item/completed", {"turnId": turn, "item": done})
            self.message(turn, item + "-m", "thought it through")
            self.end(turn)
        elif word == "fail":
            note("error", {"error": {"message": "model overloaded"}})
            self.end(turn, "failed", "model overloaded")
        elif word == "junk":
            raw("this is not json")
            note("future/thing", {"x": 1})
            out({"id": "q-1", "method": "tool/requestUserInput", "params": {"questions": []}})
            raw(json.dumps({"method": "item/completed", "params": {"pad": "x" * (5 * 1024 * 1024)}}))
            raw('{"method": 7, "params": [1, 2], "id": {"a": 1}}')
            self.message(turn, item, "after junk")
            self.end(turn)
        elif word == "exit":
            cmd = {"type": "commandExecution", "id": item, "command": "make", "status": "inProgress"}
            note("item/started", {"turnId": turn, "item": cmd})
            time.sleep(0.2)
            sys.stderr.write("fatal: the app-server lost its mind\n")
            sys.stderr.flush()
            os._exit(3)
        else:
            self.message(turn, item, "Hello from Codex", deltas=("Hello ", "from ", "Codex"))
            self.end(turn)

    def handle(self, m):
        method, rid, p = m.get("method"), m.get("id"), m.get("params") or {}
        log("codex-rpc.log", {k: m[k] for k in ("id", "method", "params", "result", "error") if k in m})
        if method is None:
            with self.answered:
                self.answers[rid] = m.get("result") or {"decision": "decline"}
                self.answered.notify_all()
        elif method == "initialize":
            out({"id": rid, "result": {"userAgent": "fake-codex/0.170.0"}})
        elif method == "thread/start":
            self.thread = "thr-%d" % os.getpid()
            out({"id": rid, "result": {"thread": {"id": self.thread}, "model": "fake-model"}})
            note("thread/started", {"thread": {"id": self.thread}})
        elif method == "thread/resume":
            if p.get("threadId") == "thr-gone":
                out({"id": rid, "error": {"code": -32600, "message": "no rollout found for thread"}})
                return
            self.thread = p.get("threadId")
            out({"id": rid, "result": {"thread": {"id": self.thread}, "model": "fake-model"}})
        elif method == "turn/start":
            self.turns += 1
            turn = "turn-%d" % self.turns
            self.stop.clear()
            out({"id": rid, "result": {"turn": {"id": turn, "status": "inProgress"}}})
            text = p["input"][0]["text"]
            threading.Thread(target=self.turn, args=(turn, text), daemon=True).start()
        elif method == "turn/interrupt":
            out({"id": rid, "result": {}})
            self.stop.set()


def main():
    argv = sys.argv[1:]
    if "--version" in argv:
        print("codex-cli 0.170.0")
        return
    log("codex-argv.log", argv)
    if argv[:1] != ["app-server"]:
        sys.stderr.write("fake codex: only app-server is scripted\n")
        sys.exit(2)
    server = Server()
    for line in sys.stdin:
        line = line.strip()
        if line:
            server.handle(json.loads(line))


if __name__ == "__main__":
    main()
