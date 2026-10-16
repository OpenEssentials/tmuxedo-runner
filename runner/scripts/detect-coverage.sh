#!/bin/sh
# Line and branch coverage of the detect crate (crates/detect), and the gate:
# 100 % of lines and 100 % of branches, or exit 1. Needs a nightly toolchain
# (branch coverage is unstable) with llvm-tools, cargo-llvm-cov 0.9 and python3.
#
#   runner/scripts/detect-coverage.sh [<nightly toolchain>]   (run in runner/)
#
# Writes target/detect-coverage.json (llvm-cov export) and prints the per-file
# table and any line or branch direction that no test reaches. `#[cfg(test)]`
# modules are excluded with `coverage(off)` (crates/detect/src/lib.rs).
set -eu
toolchain=${1:-${DETECT_COVERAGE_TOOLCHAIN:-nightly-2026-10-01}}
report=target/detect-coverage.json
# Only the crate's own sources: not its tests, not the other workspace crates it links.
ignore='/tests/|crates/(emu|holder|protocol|runner)/|/\.cargo/|/rustc/|/registry/'
cargo "+$toolchain" llvm-cov --locked --branch -p tmuxedo-detect --ignore-filename-regex "$ignore" \
  --json --output-path "$report" -- --test-threads=2
if ! python3 - "$report" <<'EOF'
import json, sys
data = json.load(open(sys.argv[1]))["data"][0]
bad = []
print(f"{'file':28} {'lines':>12} {'branches':>12}")
for f in data["files"]:
    name = f["filename"].split("/crates/detect/")[-1]
    s = f["summary"]
    print(f"{name:28} {s['lines']['covered']:>5}/{s['lines']['count']:<6} {s['branches']['covered']:>5}/{s['branches']['count']:<6}")
    taken = {}
    for b in f.get("branches", []):
        k = (b[0], b[1])
        t, n = taken.get(k, (0, 0))
        taken[k] = (t + b[4], n + b[5])
    bad += [f"{name}:{l}:{c} branch true={t} false={n}" for (l, c), (t, n) in sorted(taken.items()) if t == 0 or n == 0]
    # llvm's own line rule (LineCoverageStats): a line counts the segment wrapping into it and
    # the region entries starting on it, so an untaken `?` on a run line is not a missed line.
    # Segments are [line, col, count, has_count, is_region_entry, is_gap].
    segs = f.get("segments", [])
    wrapped, i = None, 0
    for line in range(1, (segs[-1][0] if segs else 0) + 1):
        on = []
        while i < len(segs) and segs[i][0] == line:
            on.append(segs[i])
            i += 1
        starts = [s for s in on if s[3] and s[4] and not s[5]]
        skipped = bool(on) and not on[0][3] and on[0][4]
        mapped = (not skipped and ((wrapped is not None and wrapped[3]) or bool(starts))) or any(s[3] and s[4] for s in on)
        if mapped and max([wrapped[2] if wrapped is not None else 0] + [s[2] for s in starts]) == 0:
            bad.append(f"{name}:{line} line not run")
        if on:
            wrapped = on[-1]
t = data["totals"]
print(f"detect coverage: lines {t['lines']['covered']}/{t['lines']['count']}, "
      f"branches {t['branches']['covered']}/{t['branches']['count']}")
missing = t["lines"]["covered"] != t["lines"]["count"] or t["branches"]["covered"] != t["branches"]["count"]
if missing or t["branches"]["count"] == 0:
    print("\n".join(sorted(set(bad))))
    # Functions and closures that never ran (a closure on a line that otherwise ran is
    # missed in the summary but not in the per-line view).
    for fn in data.get("functions", []):
        if fn["count"] == 0 and fn.get("regions") and any("/crates/detect/src/" in x for x in fn["filenames"]):
            print(f"never run: {fn['filenames'][0].split('/crates/detect/')[-1]}:{fn['regions'][0][0]} {fn['name'][:100]}")
    print("detect coverage gate: every line and every branch must be covered", file=sys.stderr)
    sys.exit(1)
EOF
then
  # llvm-cov's own per-line view of the same run: every line it counts as not run.
  echo "lines with a zero count (llvm-cov report --text):"
  cargo "+$toolchain" llvm-cov report --text --ignore-filename-regex "$ignore" |
    awk '/^\/.*\.rs:$/ { file = $0; next } /^ *[0-9]+\| *0\|/ { print file " " $0 }' | head -n 60
  exit 1
fi
