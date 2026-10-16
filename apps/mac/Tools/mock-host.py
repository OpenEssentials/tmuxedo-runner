#!/usr/bin/env python3
"""A stand-in for tmuxedo's pairing API and its runner gateway, for looking at a build by
hand and for the live runner test (Tests/LiveRunnerTests.swift). Standard library only.

    python3 Tools/mock-host.py [port]                    # default 4918
    defaults write dev.tmuxedo.mac.profile.verify serverURL http://127.0.0.1:4918
    PROFILE=verify fastlane mac try                       # pair the profile copy against it

Same shapes as web/src/app/api/pair/* and the gateway's /gw/runner (docs/protocol.md). It does
NOT verify signatures (no ECDSA in the standard library); it RECORDS every signed field so a
test can verify them, and it plays the gateway's half of the runner proof: after the runner's
`hello` it sends `{t:"challenge", nonce}` and holds `welcome` until a `proof` arrives.

Admin (localhost, no auth):
    POST /_admin/approve {"pairingId", "method"?}   approve as if from the phone
    POST /_admin/deny    {"pairingId"}
    POST /_admin/token   {"token", "publicKey"}      register a key-bound runner token directly
    POST /_admin/close   {"code", "reason"}          close the runner socket (4003 = revoked)
    GET  /_admin/state                               everything, as JSON
"""
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 4918
lock = threading.Lock()
state = {"attempts": {}, "tokens": {}, "proofs": [], "hellos": [], "frames": [], "push": {}, "revoked": [], "runner": None}


def b64u(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


# ── WebSocket (RFC 6455), just enough for the runner socket ──────────────────

def ws_send(sock, text, opcode=1):
    data = text.encode() if isinstance(text, str) else text
    head = bytes([0x80 | opcode])
    n = len(data)
    if n < 126:
        head += bytes([n])
    elif n < 65536:
        head += bytes([126]) + struct.pack(">H", n)
    else:
        head += bytes([127]) + struct.pack(">Q", n)
    sock.sendall(head + data)


def ws_recv(sock):
    def exact(n):
        b = b""
        while len(b) < n:
            c = sock.recv(n - len(b))
            if not c:
                raise ConnectionError("eof")
            b += c
        return b
    h = exact(2)
    op, n = h[0] & 0x0F, h[1] & 0x7F
    masked = h[1] & 0x80
    if n == 126:
        n = struct.unpack(">H", exact(2))[0]
    elif n == 127:
        n = struct.unpack(">Q", exact(8))[0]
    mask = exact(4) if masked else b"\0\0\0\0"
    data = bytes(b ^ mask[i % 4] for i, b in enumerate(exact(n)))
    return op, data


def ws_close(sock, code, reason=""):
    try:
        ws_send(sock, struct.pack(">H", code) + reason.encode(), opcode=8)
    except OSError:
        pass


def serve_runner(handler, token):
    sock = handler.connection
    with lock:
        key = state["tokens"].get(token, {}).get("publicKey")
        state["runner"] = sock
    nonce = b64u(os.urandom(32))
    proven = key is None
    hello = None
    try:
        while True:
            op, data = ws_recv(sock)
            if op == 8:
                return
            if op == 9:
                ws_send(sock, data, opcode=10)
                continue
            if op != 1:
                continue
            frame = json.loads(data)
            with lock:
                state["frames"].append(frame.get("t"))
            if frame.get("t") == "hello" and hello is None:
                hello = frame
                with lock:
                    state["hellos"].append(frame)
                if not proven:
                    ws_send(sock, json.dumps({"t": "challenge", "nonce": nonce}))
                    # Like the gateway: no proof within 10 s closes 4003 proof_failed.
                    sock.settimeout(10)
                    continue
            elif frame.get("t") == "proof" and not proven:
                lookup = token.split(".")[1]
                with lock:
                    state["proofs"].append({"nonce": nonce, "lookup": lookup, "sig": frame.get("sig"), "publicKey": key})
                proven = True
                sock.settimeout(None)
            elif frame.get("t") == "ping":
                ws_send(sock, json.dumps({"t": "pong", "at": int(time.time() * 1000)}))
                continue
            elif not proven and hello is None:
                ws_close(sock, 4003, "proof_failed")
                return
            if proven and hello is not None and frame.get("t") in ("hello", "proof"):
                ws_send(sock, json.dumps({"t": "welcome", "machineId": "m-mock", "userId": "u-mock", "lastEventSeq": 0}))
    except socket.timeout:
        ws_close(sock, 4003, "proof_failed")
    except (ConnectionError, OSError, ValueError):
        return


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def send(self, code, body=None):
        data = b"" if body is None else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        n = int(self.headers.get("content-length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def bearer(self):
        a = self.headers.get("authorization", "")
        return a[7:].strip() if a.startswith("Bearer ") else ""

    def do_GET(self):
        if self.path == "/_admin/state":
            with lock:
                return self.send(200, {k: v for k, v in state.items() if k != "runner"})
        if self.path.startswith("/gw/runner"):
            token = self.bearer()
            with lock:
                known = token in state["tokens"] and token not in state["revoked"]
            if not known:
                return self.send(401, {"error": "unauthorized"})
            accept = base64.b64encode(hashlib.sha1((self.headers["Sec-WebSocket-Key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.send_header("Sec-WebSocket-Protocol", "tmuxedo.v1")
            self.end_headers()
            self.wfile.flush()
            serve_runner(self, token)
            self.close_connection = True
            return
        self.send(404, {"error": {"code": "not_found", "message": "no route"}})

    def do_DELETE(self):
        return self.do_POST()

    def do_POST(self):
        b = self.body()
        p = self.path
        with lock:
            if p == "/api/pair/start":
                aid = "p" + uuid.uuid4().hex[:8]
                a = {"id": aid, "pollToken": b64u(os.urandom(32)), "appleNonce": b64u(os.urandom(32)),
                     "matchCode": "%06d" % (int.from_bytes(os.urandom(3), "big") % 1000000),
                     "publicKey": b.get("publicKey"), "proof": b.get("proof"), "deviceName": b.get("deviceName"),
                     "approved": False, "denied": False, "delivered": False, "method": None}
                state["attempts"][aid] = a
                base = f"http://127.0.0.1:{PORT}"
                return self.send(200, {"pairingId": aid, "pollToken": a["pollToken"], "url": f"{base}/pair?code=L{aid}",
                                       "appUrl": f"{base}/m/pair?code=L{aid}", "matchCode": a["matchCode"],
                                       "appleNonce": a["appleNonce"], "interval": 1, "expiresIn": 300})
            if p == "/api/pair/poll":
                a = state["attempts"].get(b.get("pairingId"))
                if not a or a["pollToken"] != b.get("pollToken"):
                    return self.send(410, {"error": "expired_token"})
                a.setdefault("polls", []).append({"ts": b.get("ts"), "signature": b.get("signature")})
                if a["denied"]:
                    return self.send(403, {"error": "access_denied"})
                if a["delivered"]:
                    return self.send(410, {"error": "expired_token"})
                if not a["approved"]:
                    return self.send(428, {"error": "authorization_pending", "opened": False})
                token = f"tmr.{a['id']}lk.{b64u(os.urandom(16))}"
                a["delivered"] = True
                state["tokens"][token] = {"publicKey": a["publicKey"]}
                return self.send(200, {"kind": "runner", "runnerToken": token, "machineId": "m-" + a["id"],
                                       "server": f"http://127.0.0.1:{PORT}", "method": a["method"] or "phone"})
            if p == "/api/pair/push-device":
                state["push"][b.get("token", "")] = self.bearer()
                return self.send(201, {"id": "d1", "platform": "macos", "provider": "apns"})
            if p == "/api/pair/machine":
                state["revoked"].append(self.bearer())
                return self.send(200, {"revoked": True})
            if p == "/_admin/approve":
                a = state["attempts"][b["pairingId"]]
                a["approved"], a["method"] = True, b.get("method", "phone")
                return self.send(200, {"ok": True})
            if p == "/_admin/deny":
                state["attempts"][b["pairingId"]]["denied"] = True
                return self.send(200, {"ok": True})
            if p == "/_admin/token":
                state["tokens"][b["token"]] = {"publicKey": b.get("publicKey")}
                return self.send(200, {"ok": True})
            if p == "/_admin/close":
                s = state["runner"]
            else:
                s = None
        if p == "/_admin/close":
            if s:
                ws_close(s, int(b.get("code", 4003)), b.get("reason", "revoked"))
            return self.send(200, {"ok": True})
        return self.send(404, {"error": {"code": "not_found", "message": "no route"}})


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"mock tmuxedo on http://127.0.0.1:{PORT}", flush=True)
    srv.serve_forever()
