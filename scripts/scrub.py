#!/usr/bin/env python3
"""The publication gate: nothing private, internal or third-party-named reaches this repository.

    python3 scripts/scrub.py              # the working tree (tracked files when in git)
    python3 scripts/scrub.py --history    # every blob reachable from any ref, under every path it had
    python3 scripts/scrub.py --root DIR   # another directory

It checks file PATHS (signing material, provisioning, distribution config that stays private) and
file CONTENTS, text and binary alike (terminal recordings are binary), against the gates below.
A hit fails the run unless `scripts/scrub-allow.txt` explains it:

    <gate> | <path glob> | <text that must appear in the matched line> | <why it is not a leak>

Every allowlist entry needs a reason, and an entry that no longer matches anything fails the
run too, so the list never outlives what it excuses.

Secret scanning proper (keys, tokens) is gitleaks' and trufflehog's job (CI runs both over the
whole history); this gate is about names, hosts and identifiers that are not secrets to a
scanner but must not be published.

Exit status: 0 clean, 1 findings, 2 usage or environment error.
"""

from __future__ import annotations

import argparse
import fnmatch
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field

# --- Gates -------------------------------------------------------------------------------------
#
# The generic gates live here. The OPERATOR'S gates (its hosts, accounts, identifiers, product
# and client names) are not published, since listing them would publish them: they come from
# `--patterns FILE` or the environment variable SCRUB_PRIVATE_PATTERNS (CI: a repository secret
# of the same name), one per line:
#
#     <gate> <python regex>        matched case-insensitively against every line
#     path:<gate> <python regex>   matched against every file path
#
# Without them only the generic gates run, and the summary says so (`--require-private` fails).

# (name, regex, flags). Matched against each line (binary files: each NUL/newline-separated run).
CONTENT_GATES: list[tuple[str, str, int]] = [
    ("private-ip", r"(?<![\d.])(?:10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3})(?![\d.])", 0),
    # Email addresses other than placeholders (checked against EMAIL_OK below).
    ("email", r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}", 0),
    # Home folders of real accounts in recordings and logs (a test's own `/home/dev` is fine).
    ("home-path", r"/(?:home|Users)/(?!(?:dev|user|me|alex|other|cov|runner|tmuxedo|aimux|aimuxer|reviewer|test|example|someone|nobody|x|u|a|b)\b)[a-z][a-z0-9_-]{1,31}/", 0),
]

# Placeholder and project addresses an email hit may be.
EMAIL_OK = re.compile(
    r"@(?:[a-z0-9-]+\.)*(?:example\.(?:com|org|net)|invalid|test|example|localhost)$"
    r"|^noreply@tmuxedo\.dev$|^git@github\.com$|@users\.noreply\.github\.com$"
    r"|@\d+x\.(?:png|jpe?g|pdf|gif|webp)$",  # asset names such as icon_16x16@2x.png
    re.I,
)

# Paths that must never be published, whatever they contain.
PATH_GATES: list[tuple[str, str]] = [
    ("signing-material", r"\.(?:p12|p8|pfx|mobileprovision|provisionprofile|cer|certSigningRequest|keychain|keychain-db|jks|keystore)$"),
    ("private-key-file", r"(?:^|/)(?:id_(?:rsa|ed25519|ecdsa)|[^/]*private[^/]*\.(?:pem|key)|[^/]*\.key)$"),
    ("env-file", r"(?:^|/)\.env(?:\.[^/]*)?$"),
    ("signing-config", r"(?:^|/)fastlane/Matchfile$|(?:^|/)Signing\.local\.xcconfig$"),
]


def load_private(path: str | None) -> int:
    """Adds the operator's gates; returns how many were loaded."""
    text = ""
    if path:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    text += "\n" + os.environ.get("SCRUB_PRIVATE_PATTERNS", "")
    n = 0
    for i, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        gate, _, rx = line.partition(" ")
        rx = rx.strip()
        if not rx:
            print(f"private pattern {i}: expected '<gate> <regex>'", file=sys.stderr)
            sys.exit(2)
        try:
            re.compile(rx)
        except re.error as e:
            print(f"private pattern {i} ({gate}): bad regex: {e}", file=sys.stderr)
            sys.exit(2)
        if gate.startswith("path:"):
            PATH_GATES.append((gate[5:], "(?i)" + rx))
        else:
            CONTENT_GATES.append((gate, rx, re.I))
        n += 1
    return n


SKIP_DIRS = {".git", "target", "node_modules", "build", "DerivedData", "dist", "__pycache__"}
ALLOW_FILE = "scripts/scrub-allow.txt"
MAX_SNIPPET = 160


@dataclass
class Allow:
    gate: str
    glob: str
    text: str
    reason: str
    line: int
    used: int = 0


@dataclass
class Report:
    findings: list[str] = field(default_factory=list)
    files: int = 0
    allowed: int = 0


def load_allow(root: str) -> list[Allow]:
    path = os.path.join(root, ALLOW_FILE)
    out: list[Allow] = []
    if not os.path.exists(path):
        return out
    with open(path, encoding="utf-8") as f:
        for n, raw in enumerate(f, 1):
            line = raw.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            parts = [p.strip() for p in line.split(" | ")]
            if len(parts) != 4 or not all(parts):
                print(f"{ALLOW_FILE}:{n}: expected '<gate> | <path glob> | <text> | <reason>'", file=sys.stderr)
                sys.exit(2)
            out.append(Allow(parts[0], parts[1], parts[2], parts[3], n))
    return out


def snippet(s: str) -> str:
    s = "".join(c if c.isprintable() else "." for c in s.strip())
    return s if len(s) <= MAX_SNIPPET else s[: MAX_SNIPPET - 1] + "…"


def allowed(allow: list[Allow], gate: str, path: str, line: str) -> bool:
    for a in allow:
        if a.gate == gate and fnmatch.fnmatchcase(path, a.glob) and a.text in line:
            a.used += 1
            return True
    return False


def scan_blob(path: str, data: bytes, allow: list[Allow], rep: Report, where: str) -> None:
    rep.files += 1
    for gate, rx in PATH_GATES:
        if re.search(rx, path, re.I):
            if allowed(allow, gate, path, path):
                rep.allowed += 1
            else:
                rep.findings.append(f"{where}{path}: [{gate}] this kind of file is never published")
    binary = b"\0" in data[:8192]
    text = data.decode("latin-1" if binary else "utf-8", errors="replace")
    lines = re.split(r"[\n\0]" if binary else r"\n", text)
    for n, line in enumerate(lines, 1):
        if not line:
            continue
        for gate, rx, flags in CONTENT_GATES:
            for m in re.finditer(rx, line, flags):
                if gate == "email" and EMAIL_OK.search(m.group(0)):
                    continue
                if allowed(allow, gate, path, line):
                    rep.allowed += 1
                    continue
                loc = f"{path}:{n}" if not binary else f"{path} (binary, run {n})"
                rep.findings.append(f"{where}{loc}: [{gate}] {snippet(line)}")


def git(root: str, *args: str) -> bytes:
    return subprocess.run(["git", "-C", root, *args], check=True, capture_output=True).stdout


def in_git(root: str) -> bool:
    try:
        return git(root, "rev-parse", "--is-inside-work-tree").strip() == b"true"
    except (subprocess.CalledProcessError, FileNotFoundError):
        return False


def scan_tree(root: str, allow: list[Allow], rep: Report) -> None:
    if in_git(root):
        paths = [p for p in git(root, "ls-files", "-z").decode().split("\0") if p]
    else:
        paths = []
        for d, dirs, files in os.walk(root):
            dirs[:] = sorted(x for x in dirs if x not in SKIP_DIRS)
            for f in sorted(files):
                paths.append(os.path.relpath(os.path.join(d, f), root))
    for p in paths:
        full = os.path.join(root, p)
        if os.path.islink(full) or not os.path.isfile(full):
            continue
        with open(full, "rb") as f:
            scan_blob(p, f.read(), allow, rep, "")


def scan_history(root: str, allow: list[Allow], rep: Report) -> None:
    """Every blob reachable from any ref, once per distinct path it was stored under."""
    if not in_git(root):
        print("--history needs a git repository", file=sys.stderr)
        sys.exit(2)
    seen: set[tuple[str, str]] = set()
    commits = git(root, "rev-list", "--all").decode().split()
    for c in commits:
        for entry in git(root, "ls-tree", "-r", "-z", c).decode().split("\0"):
            if not entry:
                continue
            meta, path = entry.split("\t", 1)
            mode, kind, sha = meta.split()
            if kind != "blob" or mode == "120000" or (sha, path) in seen:
                continue
            seen.add((sha, path))
            scan_blob(path, git(root, "cat-file", "blob", sha), allow, rep, f"{sha[:10]} ")
    print(f"history: {len(commits)} commits, {len(seen)} distinct (blob, path) pairs")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", default=os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    ap.add_argument("--history", action="store_true", help="scan every blob in the git history instead of the tree")
    ap.add_argument("--patterns", help="a file of the operator's gates (see the comment above CONTENT_GATES)")
    ap.add_argument("--require-private", action="store_true", help="fail when no operator gates were loaded")
    args = ap.parse_args()
    root = os.path.abspath(args.root)
    private = load_private(args.patterns)
    print(f"scrub: {len(CONTENT_GATES) + len(PATH_GATES)} gates, {private} of them the operator's")
    if args.require_private and private == 0:
        print("scrub: no operator gates loaded (SCRUB_PRIVATE_PATTERNS or --patterns) and --require-private is set")
        return 1
    allow = load_allow(root)
    rep = Report()
    (scan_history if args.history else scan_tree)(root, allow, rep)
    stale = [a for a in allow if a.used == 0]
    for a in stale:
        rep.findings.append(f"{ALLOW_FILE}:{a.line}: allowlist entry matches nothing any more ({a.gate} | {a.glob} | {a.text}); remove it")
    for f in rep.findings:
        print(f)
    print(f"scrub: {rep.files} files scanned, {len(rep.findings)} findings, {rep.allowed} allowed by {ALLOW_FILE}")
    return 1 if rep.findings else 0


if __name__ == "__main__":
    sys.exit(main())
