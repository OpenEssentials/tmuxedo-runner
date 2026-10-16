#!/usr/bin/env python3
"""A scripted stand-in for an agent that speaks the Agent Client Protocol, for
the agent chat tests. Installed as `agent` and `cursor-agent`, so the Cursor
plugin's `agent acp` starts it; nothing here is Cursor-specific.

JSON-RPC 2.0 over stdio, one JSON object per line. The field names are the same
from-memory reading of the ACP specification as src/chat/acp.rs: this is NOT a
recording of a real agent. What a turn does depends on the first word of the
message:

  hello        streamed text (chunks), then the turn ends
  tool         `echo hi` behind session/request_permission
  risky        `git push origin main` behind the same request
  edit NAME    a diff creating NAME in the cwd, behind a request offering
               allow_once and reject_once only
  slow N       N chunks, 150 ms apart (cancel)
  hang         a command that never returns: the turn ends when cancelled
  think        thought chunks, then a message
  plan         a plan with three entries, then a message
  fail         the prompt is answered with an error
  junk         a line that is not JSON, an unknown notification, an unknown
               request, a 5 MiB line, a response to nothing, then a message
  exit         a call starts, then the process prints to stderr and exits 3

FAKE_ACP_NO_LOAD=1: the agent does not advertise session/load.
Every message it reads is appended to $FAKE_LOG/acp-rpc.log, and each start's
argv to $FAKE_LOG/acp-argv.log.
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
    obj["jsonrpc"] = "2.0"
    raw(json.dumps(obj))


class Agent:
    def __init__(self):
        self.session = None
        self.asks = 100
        self.stop = threading.Event()
        self.answers = {}
        self.answered = threading.Condition()
        self.calls = 0

    def update(self, u):
        out({"method": "session/update", "params": {"sessionId": self.session, "update": u}})

    def say(self, *chunks):
        for c in chunks:
            self.update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": c}})

    def ask(self, call, options):
        """A permission request; blocks until the client answers."""
        self.asks += 1
        rid = self.asks
        out({
            "id": rid,
            "method": "session/request_permission",
            "params": {"sessionId": self.session, "toolCall": call, "options": options},
        })
        with self.answered:
            while rid not in self.answers:
                self.answered.wait(0.05)
            o = self.answers.pop(rid).get("outcome") or {}
        if o.get("outcome") != "selected":
            return None
        return o.get("optionId")

    def command(self, command, output):
        self.calls += 1
        cid = "call-%d" % self.calls
        call = {"toolCallId": cid, "title": command, "kind": "execute", "status": "pending",
                "rawInput": {"command": command}}
        self.update(dict(call, sessionUpdate="tool_call"))
        picked = self.ask({"toolCallId": cid}, [
            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
            {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
            {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
        ])
        if picked is None:
            return "cancelled"
        if picked in ("allow", "always"):
            text = {"type": "content", "content": {"type": "text", "text": output}}
            self.update({"sessionUpdate": "tool_call_update", "toolCallId": cid, "status": "in_progress",
                         "content": [text]})
            self.update({"sessionUpdate": "tool_call_update", "toolCallId": cid, "status": "completed",
                         "rawOutput": {"exitCode": 0}})
            self.say("done")
        else:
            self.update({"sessionUpdate": "tool_call_update", "toolCallId": cid, "status": "failed"})
            self.say("skipped")
        return "end_turn"

    def turn(self, rid, text):
        words = text.split()
        word = words[0] if words else ""
        reason = "end_turn"
        if word == "tool":
            reason = self.command("echo hi", "hi\n")
        elif word == "risky":
            reason = self.command("git push origin main", "pushed\n")
        elif word == "edit":
            path = os.path.join(os.getcwd(), words[1])
            self.calls += 1
            cid = "call-%d" % self.calls
            diff = {"type": "diff", "path": path, "oldText": None, "newText": "new\n"}
            self.update({"sessionUpdate": "tool_call", "toolCallId": cid, "title": "Write " + words[1],
                         "kind": "edit", "status": "pending", "locations": [{"path": path}], "content": [diff]})
            picked = self.ask({"toolCallId": cid, "kind": "edit", "locations": [{"path": path}]}, [
                {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                {"optionId": "no", "name": "Reject", "kind": "reject_once"},
            ])
            if picked == "yes":
                with open(path, "w") as f:
                    f.write("new\n")
            status = "completed" if picked == "yes" else "failed"
            self.update({"sessionUpdate": "tool_call_update", "toolCallId": cid, "status": status})
        elif word == "slow":
            for i in range(int(words[1])):
                if self.stop.wait(0.15):
                    reason = "cancelled"
                    break
                self.say("w%d " % i)
        elif word == "hang":
            self.update({"sessionUpdate": "tool_call", "toolCallId": "call-hang", "title": "sleep 1000",
                         "kind": "execute", "status": "in_progress", "rawInput": {"command": "sleep 1000"}})
            self.stop.wait()
            reason = "cancelled"
        elif word == "think":
            for c in ("Weighing ", "the options."):
                self.update({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": c}})
            time.sleep(0.05)
            self.say("thought it through")
        elif word == "plan":
            self.update({"sessionUpdate": "plan", "entries": [
                {"content": "Read the code", "priority": "high", "status": "completed"},
                {"content": "Change it", "priority": "medium", "status": "in_progress"},
                {"content": "Test it", "priority": "low", "status": "pending"},
            ]})
            self.say("planned")
        elif word == "fail":
            out({"id": rid, "error": {"code": -32603, "message": "model overloaded"}})
            return
        elif word == "junk":
            raw("this is not json")
            out({"method": "future/thing", "params": {"x": 1}})
            out({"id": "q-1", "method": "cursor/ask_question", "params": {"questions": []}})
            raw(json.dumps({"method": "session/update", "params": {"pad": "x" * (5 * 1024 * 1024)}}))
            out({"id": 9999, "result": {}})
            self.say("after junk")
        elif word == "exit":
            self.update({"sessionUpdate": "tool_call", "toolCallId": "call-exit", "title": "make",
                         "kind": "execute", "status": "in_progress"})
            time.sleep(0.2)
            sys.stderr.write("fatal: the agent lost its mind\n")
            sys.stderr.flush()
            os._exit(3)
        else:
            self.say("Hello ", "from ", "ACP")
        out({"id": rid, "result": {"stopReason": reason}})

    def handle(self, m):
        method, rid, p = m.get("method"), m.get("id"), m.get("params") or {}
        log("acp-rpc.log", {k: m[k] for k in ("id", "method", "params", "result", "error", "jsonrpc") if k in m})
        if method is None:
            with self.answered:
                self.answers[rid] = m.get("result") or {}
                self.answered.notify_all()
        elif method == "initialize":
            load = os.environ.get("FAKE_ACP_NO_LOAD") != "1"
            out({"id": rid, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": load},
                                       "authMethods": []}})
        elif method == "session/new":
            self.session = "acp-sess-%d" % os.getpid()
            out({"id": rid, "result": {"sessionId": self.session, "modes": {
                "currentModeId": "ask", "availableModes": [{"id": "agent"}, {"id": "plan"}, {"id": "ask"}]}}})
        elif method == "session/load":
            if p.get("sessionId") == "acp-gone":
                out({"id": rid, "error": {"code": -32002, "message": "no such session"}})
                return
            self.session = p.get("sessionId")
            # The old conversation is replayed before the answer.
            self.update({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "old question"}})
            self.say("old answer")
            out({"id": rid, "result": None})
        elif method == "session/set_mode":
            out({"id": rid, "result": {}})
        elif method == "session/prompt":
            self.stop.clear()
            text = p["prompt"][0]["text"]
            threading.Thread(target=self.turn, args=(rid, text), daemon=True).start()
        elif method == "session/cancel":
            self.stop.set()
            # Open permission requests are answered `cancelled` by the client.


def main():
    argv = sys.argv[1:]
    if "--version" in argv:
        print("2026.10.10-fake")
        return
    if argv[:1] == ["status"]:
        print(json.dumps({"isAuthenticated": False}))
        return
    log("acp-argv.log", argv)
    if argv[:1] != ["acp"]:
        sys.stderr.write("fake agent: only acp is scripted\n")
        sys.exit(2)
    agent = Agent()
    for line in sys.stdin:
        line = line.strip()
        if line:
            agent.handle(json.loads(line))


if __name__ == "__main__":
    main()
