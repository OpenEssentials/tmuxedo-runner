#!/bin/bash
# The holder as a real LaunchAgent, on a real Mac (CI: GitHub-hosted macOS runners).
#
#   mac/Tools/holder-e2e.sh <tmuxedo-runner binary>
#
# Proves what docs/design/mac-pairing.md §1 promises, with launchd itself:
#   1. the holder runs as its own LaunchAgent and reports its build fingerprint;
#   2. a terminal (pane) started in it survives an APP UPDATE: the bundled binary is replaced
#      on disk (atomic rename, different bytes) and the holder process, its pid and the pane's
#      process are all still there, and the holder's build still equals the new binary's, so
#      the app's HolderPolicy keeps it (no restart);
#   3. `launchctl kickstart -k` (the app's restart, used only when the holder build changed)
#      replaces the holder process, and launchd's KeepAlive brings a killed holder back.
# Uses its own label, socket and app directory: nothing of a real installation is touched.
set -euo pipefail

BIN="${1:?usage: holder-e2e.sh <tmuxedo-runner>}"
LABEL="dev.tmuxedo.mac.e2e-holder.$$"
T="$(mktemp -d /tmp/tmx-e2e.XXXXXX)"
APPDIR="$T/tmuxedo.app/Contents/Helpers"
SOCK="$T/run/holder.sock"
PLIST="$T/$LABEL.plist"
DOMAIN="gui/$(id -u)"
mkdir -p "$APPDIR" "$T/run"
cp "$BIN" "$APPDIR/tmuxedo-runner"
R="$APPDIR/tmuxedo-runner"

pass() { echo "ok - $*"; }
fail() { echo "FAIL - $*" >&2; exit 1; }
cleanup() {
  launchctl bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || launchctl bootout "user/$(id -u)/$LABEL" >/dev/null 2>&1 || true
  [ -n "${PANE_PID:-}" ] && kill "$PANE_PID" 2>/dev/null || true
  rm -rf "$T"
}
trap cleanup EXIT

cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array>
    <string>$R</string><string>holder</string><string>--wait</string><string>--socket</string><string>$SOCK</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>$T/holder.log</string>
</dict></plist>
EOF

if ! launchctl bootstrap "$DOMAIN" "$PLIST" 2>/dev/null; then
  DOMAIN="user/$(id -u)"
  launchctl bootstrap "$DOMAIN" "$PLIST" || fail "launchctl bootstrap refused the holder"
fi
pass "holder bootstrapped in $DOMAIN"

info() { "$R" holder-info --socket "$SOCK"; }
field() { python3 -c "import json,sys; print(json.loads(sys.argv[1]).get(sys.argv[2]))" "$1" "$2"; }
wait_running() {
  for _ in $(seq 1 100); do
    i="$(info)"; [ "$(field "$i" running)" = "True" ] && { echo "$i"; return 0; }
    sleep 0.1
  done
  return 1
}

I1="$(wait_running)" || fail "the holder never answered on $SOCK"
PID1="$(field "$I1" pid)"
BUILD="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['holderBuild'])" "$("$R" version --json)")"
[ "$(field "$I1" build)" = "$BUILD" ] || fail "holder build $(field "$I1" build) is not the binary's $BUILD"
pass "1. holder pid $PID1 reports build $BUILD"

# A terminal in the holder, through its own socket protocol (u32 BE length | type | JSON).
PANE_PID="$(python3 - "$SOCK" "$T/pane.pid" <<'PY'
import json, socket, struct, sys, time
sock, pidfile = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX); s.connect(sock)
def send(o):
    b = json.dumps(o).encode(); s.sendall(struct.pack(">I", len(b) + 1) + b"\x01" + b)
def recv():
    n = struct.unpack(">I", s.recv(4, socket.MSG_WAITALL))[0]
    body = b""
    while len(body) < n: body += s.recv(n - len(body))
    return json.loads(body[1:]) if body[0] == 1 else None
send({"op": "hello", "proto": 1, "client": "e2e"}); assert recv()["op"] == "hello"
send({"op": "spawn", "rid": 1, "spec": {"id": "e2e:p1", "argv": ["/bin/sh", "-c", f"echo $$ > {pidfile}; exec sleep 100000"],
      "cwd": None, "env": {}, "cols": 80, "rows": 24}})
while True:
    m = recv()
    if m and m.get("op") in ("ok", "error") and m.get("rid") == 1: break
assert m["op"] == "ok", m
for _ in range(100):
    try:
        print(open(pidfile).read().strip()); break
    except FileNotFoundError: time.sleep(0.05)
PY
)"
[ -n "$PANE_PID" ] && kill -0 "$PANE_PID" || fail "the pane did not start"
pass "a terminal runs in the holder (pid $PANE_PID)"

# 2. An app update: the bundled binary replaced on disk (new bytes, same holder code).
cp "$R" "$T/new-runner"
codesign -f -s - "$T/new-runner" >/dev/null 2>&1 || true
mv -f "$T/new-runner" "$R"
sleep 1
I2="$(info)"
[ "$(field "$I2" pid)" = "$PID1" ] || fail "the holder was restarted by an app update"
kill -0 "$PANE_PID" || fail "the terminal died in an app update"
NEWBUILD="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['holderBuild'])" "$("$R" version --json)")"
[ "$(field "$I2" build)" = "$NEWBUILD" ] || fail "after the update the holder build differs: the app would restart it"
pass "2. after an app update: same holder pid $PID1, terminal alive, build equal (HolderPolicy: keep)"

# 3. The app's restart (only when the holder build changed) and launchd's KeepAlive.
launchctl kickstart -k "$DOMAIN/$LABEL" || fail "kickstart -k refused"
for _ in $(seq 1 100); do
  I3="$(info)"; P3="$(field "$I3" pid)"
  [ "$(field "$I3" running)" = "True" ] && [ "$P3" != "$PID1" ] && break
  sleep 0.1
done
[ "$P3" != "$PID1" ] || fail "kickstart -k did not replace the holder"
pass "3a. kickstart -k: new holder pid $P3"
kill -9 "$P3"
for _ in $(seq 1 150); do
  I4="$(info)"; P4="$(field "$I4" pid)"
  [ "$(field "$I4" running)" = "True" ] && [ "$P4" != "$P3" ] && break
  sleep 0.1
done
[ "$P4" != "$P3" ] || fail "launchd did not bring a killed holder back (KeepAlive)"
pass "3b. KeepAlive: a killed holder came back as pid $P4"
echo "holder-e2e: all checks passed"
