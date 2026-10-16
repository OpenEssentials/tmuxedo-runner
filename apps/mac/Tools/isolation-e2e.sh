#!/bin/bash
# Separate-user mode, end to end, on a real Mac (CI: GitHub-hosted macOS runners, whose user
# has passwordless sudo, so the administrator steps run unattended). docs/design/mac-isolation.md §5.
#
#   mac/Tools/isolation-e2e.sh <tmuxedo.app>
#
# The CI user plays the Mac's OWNER. With the app's own helper binary (the same code the
# privileged helper runs over XPC, here as `sudo tmuxedo-helper <op>`), it proves:
#   1. the user is created hidden, standard (not admin), passwordless, with its own 0750 home;
#   2. as that user: the owner's home, ~/Documents, ~/.ssh and login keychain are unreadable,
#      sudo fails, and nothing can be written outside its home (system folders, the owner's);
#   3. folder grants work (read, write, the owner can still edit what the terminals wrote),
#      keep everything else closed (siblings, entries created later, links planted by a
#      terminal), inside and outside the home, and revoke cleanly (even the terminals' files);
#   4. the holder and runner run as that user from root-owned binaries; the runner's plist (its
#      token) and its log cannot be read or forged by a terminal; the dashboard socket is
#      reachable by the owner;
#   5. the kill switch ends every process of the user; setup is idempotent (mode switching);
#   6. uninstall removes the user, its home, its group, its daemons and every ACL entry.
set -uo pipefail

APP="$(cd "${1:?usage: isolation-e2e.sh <tmuxedo.app>}" && pwd)"
cd / # the separate user cannot enter the caller's folder (and says so on every command otherwise)
H="$APP/Contents/MacOS/tmuxedo-helper"
R="$APP/Contents/Helpers/tmuxedo-runner"
OWNER="$(id -un)"
T="$(mktemp -d /tmp/tmx-iso.XXXXXX)"
STATE="$T/grants.json"
G="$HOME/tmx-e2e-git"          # a projects folder inside the owner's home
OUT="/Users/Shared/tmx-e2e-$$" # a project folder outside it
FAILS=0

pass() { echo "ok - $*"; }
fail() { echo "FAIL - $*" >&2; FAILS=$((FAILS + 1)); }
die() { echo "FATAL - $*" >&2; exit 1; }
as_tmx() { sudo -n -u tmuxedo -- "$@"; }
cannot() { if as_tmx "$@" >/dev/null 2>&1; then fail "tmuxedo COULD: $*"; else pass "tmuxedo cannot: $*"; fi; }
can() { if as_tmx "$@" >/dev/null 2>&1; then pass "tmuxedo can: $*"; else fail "tmuxedo could NOT: $*"; fi; }
helper() { sudo -n "$H" "$@" --owner "$OWNER" --runner "$R"; }
acl() { "$H" acl "$@" --state "$STATE"; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }

cleanup() {
  acl unlock >/dev/null 2>&1 || true
  helper uninstall >/dev/null 2>&1 || true
  chmod -R u+rwX "$G" "$OUT" 2>/dev/null || true
  rm -rf "$G" "$OUT" "$T" "$HOME/.tmx-e2e-dotfile" "$HOME/Documents/tmx-e2e-secret.txt" "$HOME/.ssh/tmx_e2e_key"
}
trap cleanup EXIT

[ -x "$H" ] || die "no helper at $H"
[ -x "$R" ] || die "no runner at $R"

# ── fixtures, as the owner ──
mkdir -p "$HOME/Documents" "$HOME/.ssh" "$G/proj/src" "$G/other" "$OUT"
chmod 755 "$G" "$G/proj" "$G/other" "$OUT"
echo "owner secret" >"$HOME/Documents/tmx-e2e-secret.txt"
echo "PRIVATE KEY" >"$HOME/.ssh/tmx_e2e_key"; chmod 600 "$HOME/.ssh/tmx_e2e_key"
echo "export TOKEN=dotfile" >"$HOME/.tmx-e2e-dotfile"; chmod 644 "$HOME/.tmx-e2e-dotfile"
echo "code" >"$G/proj/src/a.txt"; echo "env SECRET" >"$G/proj/.env"; chmod 600 "$G/proj/.env"
echo "other project" >"$G/other/b.txt"; chmod 644 "$G/other/b.txt"
echo "shared" >"$OUT/c.txt"

# ── 1. the user ──
helper setup >"$T/setup.json" || { cat "$T/setup.json"; die "setup failed"; }
helper setup >"$T/setup2.json" || die "a second setup (idempotent) failed"
[ "$(json 'd["status"]["userExists"]' <"$T/setup.json")" = "True" ] || { cat "$T/setup.json"; die "no user after setup"; }
id tmuxedo >/dev/null 2>&1 && pass "1. user tmuxedo exists (uid $(id -u tmuxedo))" || die "id tmuxedo"
[ "$(dscl . -read /Users/tmuxedo IsHidden 2>/dev/null | awk '{print $2}')" = "1" ] && pass "1. hidden from the login window" || fail "IsHidden"
ADM="$(dseditgroup -o checkmember -m tmuxedo admin 2>&1)"
echo "$ADM" | grep -q '^yes' && fail "tmuxedo is an admin: $ADM" || pass "1. not an administrator ($ADM)"
as_tmx id -Gn | tr ' ' '\n' | grep -qx admin && fail "admin in its groups" || pass "1. admin is not among its groups"
dscl . -read /Users/tmuxedo Password 2>/dev/null | grep -q 'Password: \*' && pass "1. no password (cannot log in or su)" || fail "it has a password"
[ "$(stat -f '%Su %Lp' /Users/tmuxedo)" = "tmuxedo 750" ] && pass "1. its home is its own, 0750" || fail "home: $(stat -f '%Su %Lp' /Users/tmuxedo)"
dseditgroup -o checkmember -m "$OWNER" tmuxedo | grep -q '^yes' && pass "1. the owner is in group tmuxedo" || fail "owner not in group"

# Why the home lock exists: before it, a standard user can read the owner's world-readable files.
if as_tmx cat "$HOME/.tmx-e2e-dotfile" >/dev/null 2>&1; then
  echo "note - before the home lock, tmuxedo CAN read ~/.tmx-e2e-dotfile (staff + 0644): the lock is required"
fi

# ── 2. the boundary ──
acl lock >/dev/null || fail "home lock"
cannot ls "$HOME"
cannot cat "$HOME/.tmx-e2e-dotfile"
cannot cat "$HOME/Documents/tmx-e2e-secret.txt"
cannot ls "$HOME/.ssh"
cannot cat "$HOME/.ssh/tmx_e2e_key"
cannot cat "$G/proj/src/a.txt"
cannot /bin/cat "$HOME/Library/Keychains/login.keychain-db"
# `security` may exit 0 even when it cannot open the file: judge by what it prints.
KC="$(as_tmx /usr/bin/security dump-keychain "$HOME/Library/Keychains/login.keychain-db" 2>&1)"; KCRC=$?
if [ $KCRC -ne 0 ] || ! echo "$KC" | grep -q '^class:'; then
  pass "tmuxedo cannot dump the owner's login keychain (rc $KCRC: $(echo "$KC" | head -1))"
else
  fail "tmuxedo dumped the owner's login keychain: $(echo "$KC" | head -5)"
fi
cannot /usr/bin/sudo -n true
cannot /usr/bin/touch /Library/tmx-e2e-write
cannot /usr/bin/touch /Applications/tmx-e2e-write
cannot /usr/bin/touch /etc/tmx-e2e-write
cannot /usr/bin/touch /usr/local/tmx-e2e-write
cannot /usr/bin/touch "$HOME/tmx-e2e-write"
cannot /usr/bin/touch "$G/proj/tmx-e2e-write"
cannot /usr/bin/touch "$OUT/tmx-e2e-write"
can /usr/bin/touch /Users/tmuxedo/tmx-e2e-own
cannot /bin/cp "$R" /Library/"Application Support"/tmuxedo/bin/x 2>/dev/null

# ── 3. grants ──
acl grant "$G/proj" >/dev/null || fail "grant $G/proj"
can cat "$G/proj/src/a.txt"
can cat "$G/proj/.env"
can /bin/sh -c "echo agent >\"$G/proj/src/new.txt\""
can /bin/mkdir "$G/proj/made-by-agent"
echo "owner edit" >>"$G/proj/src/new.txt" 2>/dev/null && pass "3. the owner can edit what the terminals wrote" || fail "owner cannot edit new.txt"
cannot ls "$HOME"
cannot ls "$G"
cannot cat "$G/other/b.txt"
cannot cat "$HOME/.tmx-e2e-dotfile"
cannot cat "$HOME/Documents/tmx-e2e-secret.txt"
cannot ls "$HOME/.ssh"
mkdir "$G/later"; echo "later" >"$G/later/d.txt"; chmod 755 "$G/later"; chmod 644 "$G/later/d.txt"
cannot cat "$G/later/d.txt"
echo "new dotfile" >"$HOME/.tmx-e2e-later"; chmod 644 "$HOME/.tmx-e2e-later"
cannot cat "$HOME/.tmx-e2e-later"; rm -f "$HOME/.tmx-e2e-later"
# A terminal plants links in the granted folder, then the owner grants again: nothing leaks.
as_tmx /bin/ln -s "$HOME/.ssh/tmx_e2e_key" "$G/proj/key-link" 2>/dev/null
as_tmx /bin/ln "$G/other/b.txt" "$G/proj/hard-link" 2>/dev/null && fail "tmuxedo hard-linked a file it cannot reach" || pass "3. no hard link to an unreachable file"
acl grant "$OUT" >/dev/null || fail "grant $OUT"
cannot cat "$G/proj/key-link"
cannot cat "$HOME/.ssh/tmx_e2e_key"
[ -z "$(ls -le "$HOME/.ssh/tmx_e2e_key" | grep -i tmuxedo)" ] && pass "3. the key carries no entry for tmuxedo" || fail "a grant reached ~/.ssh through a link"
can cat "$OUT/c.txt"
can /usr/bin/touch "$OUT/agent.txt"
# Refused grants (the policy): the home, ~/.ssh, a system folder, someone else's folder.
acl grant "$HOME" >/dev/null 2>&1 && fail "granted the whole home" || pass "3. the home itself cannot be granted"
acl grant "$HOME/.ssh" >/dev/null 2>&1 && fail "granted ~/.ssh" || pass "3. ~/.ssh cannot be granted"
acl grant /Library >/dev/null 2>&1 && fail "granted /Library" || pass "3. /Library cannot be granted"
acl grant /Users/tmuxedo >/dev/null 2>&1 && fail "granted tmuxedo's own home" || pass "3. not someone else's folder"
# Revoke: everything in it closes, including what the terminals created there.
acl revoke "$G/proj" >/dev/null || fail "revoke $G/proj"
cannot cat "$G/proj/src/a.txt"
cannot cat "$G/proj/src/new.txt"
cannot ls "$G/proj/made-by-agent"
acl revoke "$OUT" >/dev/null || fail "revoke $OUT"
cannot cat "$OUT/c.txt"
cannot cat "$OUT/agent.txt"
cannot ls "$HOME"
# Granted again after a revoke: open again.
acl grant "$G/proj" >/dev/null || fail "grant again"
can cat "$G/proj/src/a.txt"

# ── 4. the runtime ──
helper installRuntime >"$T/install.json" || { cat "$T/install.json"; fail "installRuntime"; }
[ "$(stat -f '%Su %Lp' "/Library/Application Support/tmuxedo/bin/tmuxedo-runner")" = "root 755" ] \
  && pass "4. the runner the daemons run is root's, 0755" || fail "runner copy ownership"
cannot /bin/sh -c "echo x >> '/Library/Application Support/tmuxedo/bin/tmuxedo-runner'"
for _ in $(seq 1 50); do HP="$(pgrep -f "tmuxedo-runner holder" | head -1)"; [ -n "$HP" ] && break; sleep 0.2; done
if [ -n "${HP:-}" ] && [ "$(ps -o user= -p "$HP" | tr -d ' ')" = "tmuxedo" ]; then
  pass "4. the holder runs as tmuxedo (pid $HP)"
else
  fail "holder not running as tmuxedo"
  sudo launchctl print system/dev.tmuxedo.mac.iso-holder 2>&1 | grep -E 'state|exit|error|path' | head
  sudo tail -5 /Library/Logs/tmuxedo/holder.log 2>&1
fi
cat >"$T/runner.json" <<EOF
{"server":"http://127.0.0.1:9","token":"tmr.e2etest.notarealsecret","observeOnly":false,"confirm":true,"confirmTimeout":30}
EOF
helper startRunner --config "$T/runner.json" >"$T/start.json" || { cat "$T/start.json"; fail "startRunner"; }
RP="$(json 'd["daemon"].get("pid")' <"$T/start.json")"
if [ "$RP" != "None" ] && [ "$(ps -o user= -p "$RP" | tr -d ' ')" = "tmuxedo" ]; then
  pass "4. the runner runs as tmuxedo (pid $RP)"
else
  fail "runner not running as tmuxedo"; cat "$T/start.json"
  sudo launchctl print system/dev.tmuxedo.mac.iso-runner 2>&1 | grep -E 'state|exit|error' | head
fi
[ "$(stat -f '%Su %Lp' /Library/LaunchDaemons/dev.tmuxedo.mac.iso-runner.plist)" = "root 600" ] && pass "4. the runner's plist (its token) is root-only" || fail "runner plist mode"
cannot cat /Library/LaunchDaemons/dev.tmuxedo.mac.iso-runner.plist
for _ in $(seq 1 50); do grep -q "confirm on this Mac" /Library/Logs/tmuxedo/runner.log 2>/dev/null && break; sleep 0.2; done
grep -q "confirm on this Mac" /Library/Logs/tmuxedo/runner.log && pass "4. the runner says confirm-on-Mac is on" || fail "no confirm line in the runner log"
grep -q "tmr.e2etest" /Library/Logs/tmuxedo/runner.log && fail "the token is in the log" || pass "4. the token is not in the log"
# launchd opens the log as the user: the file is the user's, its folder root's.
[ "$(stat -f '%Su' /Library/Logs/tmuxedo)" = "root" ] && pass "4. the log folder is root's" || fail "log folder owner"
cannot /bin/rm -f /Library/Logs/tmuxedo/runner.log
cannot /bin/mv /Library/Logs/tmuxedo/runner.log /Library/Logs/tmuxedo/gone.log
cannot /usr/bin/touch /Library/Logs/tmuxedo/forged.log
for _ in $(seq 1 50); do [ -S /Users/tmuxedo/.local/state/tmuxedo-api/runner.sock ] && break; sleep 0.2; done
PING="$(python3 - <<'PY' 2>&1
import json, socket
s = socket.socket(socket.AF_UNIX); s.settimeout(5)
s.connect("/Users/tmuxedo/.local/state/tmuxedo-api/runner.sock")
s.sendall(b'{"id":1,"method":"ping","params":{}}\n')
print(s.recv(4096).decode().strip())
PY
)"
echo "$PING" | grep -q '"result"' && pass "4. the owner reads the dashboard socket (group tmuxedo)" || fail "dashboard socket: $PING"
# The holder's own socket stays the user's alone.
python3 -c "import socket; s=socket.socket(socket.AF_UNIX); s.connect('/Users/tmuxedo/.local/state/tmuxedo/holder.sock')" 2>/dev/null \
  && fail "the owner reached the holder socket directly" || pass "4. the holder socket is tmuxedo's alone"
helper runnerState >"$T/state.json"
[ "$(json 'd["daemon"]["running"]' <"$T/state.json")" = "True" ] && pass "4. runnerState: running" || { fail "runnerState"; cat "$T/state.json"; }
helper stopRunner >/dev/null
[ "$(helper runnerState | json 'd["daemon"]["running"]')" = "False" ] && pass "4. stopRunner stops it" || fail "stopRunner"
helper startRunner --config "$T/runner.json" >/dev/null || fail "restart the runner"

# ── 5. kill switch, then set up again (mode switching) ──
TU="$(id -u tmuxedo)"
# A terminal in the user's holder that ignores the hang-up and SIGTERM (spawned through the holder's
# own protocol, as the runner would; root may connect to the user's socket).
sudo -n /usr/bin/python3 - /Users/tmuxedo/.local/state/tmuxedo/holder.sock <<'PY' || echo "stray: holder spawn failed"
import json, socket, struct, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(10); s.connect(sys.argv[1])
def send(o):
    b = json.dumps(o).encode(); s.sendall(struct.pack(">I", len(b) + 1) + b"\x01" + b)
def recv():
    n = struct.unpack(">I", s.recv(4, socket.MSG_WAITALL))[0]
    body = b""
    while len(body) < n: body += s.recv(n - len(body))
    return json.loads(body[1:]) if body[0] == 1 else None
send({"op": "hello", "proto": 1, "client": "e2e"}); recv()
send({"op": "spawn", "rid": 1, "spec": {"id": "e2e:stray", "argv": ["/bin/sh", "-c", "trap '' HUP TERM; exec /bin/sleep 600"],
      "cwd": None, "env": {}, "cols": 80, "rows": 24}})
while True:
    m = recv()
    if m and m.get("op") in ("ok", "error") and m.get("rid") == 1: break
print("stray:", m.get("op"))
PY
for _ in $(seq 1 30); do pgrep -U "$TU" -x sleep >/dev/null && break; sleep 0.1; done
pgrep -U "$TU" -x sleep >/dev/null && pass "5. a terminal ignoring HUP and TERM runs as tmuxedo" \
  || { fail "could not start a stray process"; ps -U "$TU" -o pid,comm | head; }
helper stopAll >/dev/null || fail "stopAll"
sleep 1
LEFT_P="$(ps -U "$TU" -o pid=,comm= 2>/dev/null | grep -v -E ' /(System|usr/(sbin|libexec))/' || true)"
[ -z "$LEFT_P" ] && pass "5. kill switch: no process of tmuxedo is left" || fail "left: $LEFT_P"
SYS_P="$(ps -U "$TU" -o comm= 2>/dev/null | tr '\n' ' ')"
[ -n "$SYS_P" ] && echo "note - macOS per-user services restarted for tmuxedo after the kill switch: $SYS_P"
[ ! -e /Library/LaunchDaemons/dev.tmuxedo.mac.iso-runner.plist ] && [ ! -e /Library/LaunchDaemons/dev.tmuxedo.mac.iso-holder.plist ] \
  && pass "5. kill switch: both daemons removed (the token with them)" || fail "daemon plists left"
helper setup >/dev/null && helper installRuntime >/dev/null && helper startRunner --config "$T/runner.json" >/dev/null \
  && pass "5. set up again after the kill switch" || fail "set up again"

# ── 6. uninstall ──
acl unlock >/dev/null || fail "unlock"
# The owner's own files (what the terminals created is theirs and keeps its entries, see the design).
LEFT="$( (ls -led "$HOME" "$G" "$G/proj" "$G/proj/src" "$G/other" "$G/later" "$OUT"; ls -le "$HOME";
          ls -le "$G/proj/src/a.txt" "$G/proj/.env" "$G/other/b.txt" "$OUT/c.txt" "$HOME/.ssh/tmx_e2e_key") 2>/dev/null \
        | grep -c 'tmuxedo' || true)"
[ "$LEFT" = "0" ] && pass "6. no ACL entry for tmuxedo is left on the owner's files" || {
  fail "$LEFT ACL entries left"
  (ls -led "$HOME" "$G" "$G/proj" "$OUT"; ls -le "$HOME") 2>/dev/null | grep -B1 tmuxedo | head -20
}
helper uninstall >"$T/uninstall.json" || { cat "$T/uninstall.json"; fail "uninstall"; }
id tmuxedo >/dev/null 2>&1 && fail "the user is still there" || pass "6. the user is gone"
[ ! -e /Users/tmuxedo ] && pass "6. its home is gone" || fail "home left"
dscl . -read /Groups/tmuxedo >/dev/null 2>&1 && fail "the group is still there" || pass "6. its group is gone"
ls /Library/LaunchDaemons/dev.tmuxedo.mac.iso-* >/dev/null 2>&1 && fail "daemons left" || pass "6. its daemons are gone"
launchctl print system/dev.tmuxedo.mac.iso-holder >/dev/null 2>&1 && fail "holder still loaded" || pass "6. nothing loaded in launchd"
[ ! -e "/Library/Application Support/tmuxedo" ] && [ ! -e /Library/Logs/tmuxedo ] && pass "6. its folders are gone" || fail "folders left"

if [ "$FAILS" -ne 0 ]; then echo "isolation-e2e: $FAILS FAILED"; exit 1; fi
echo "isolation-e2e: all checks passed"
