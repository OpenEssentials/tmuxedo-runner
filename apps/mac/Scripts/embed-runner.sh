#!/bin/bash
# Xcode build phase: put the tmuxedo-runner binary into Contents/Helpers/ and sign it as ours.
#
# The app BUNDLES the Rust runner; it never reimplements it (docs/design/mac-pairing.md §1).
# One universal binary (arm64 + x86_64) plays the holder and the runner.
#
# Where the binary comes from, first match wins:
#   1. TMUXEDO_RUNNER_BIN   a prebuilt binary (CI builds it from this commit with cargo and passes it in);
#   2. cargo on PATH        build the repository's runner/ (the nearest parent folder holding
#                           runner/Cargo.toml) at THIS commit for both architectures and lipo them;
#   3. the download         <TMUXEDO_DL_SERVER, default https://tmuxedo.dev>/dl/manifest.json,
#                           darwin-aarch64 + darwin-x86_64, each checked against the manifest's
#                           sha256 before use, then lipo'd.
# The result must offer the features this app needs (`version --json`), or the build fails:
# an app whose runner cannot be supervised must never ship.
#
# Signed with the build's own identity (EXPANDED_CODE_SIGN_IDENTITY), hardened runtime and a
# secure timestamp, BEFORE Xcode signs the app, so notarisation sees a consistent bundle and the
# app's launch-time check (RunnerBinary.checkSignature: same team) passes.
set -euo pipefail

say() { echo "embed-runner: $*"; }
die() { echo "error: embed-runner: $*" >&2; exit 1; }

MAC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The repository root: the nearest parent of the app folder that holds runner/ (mac/ here,
# apps/mac in the public repository).
REPO="$MAC_DIR"
while [ "$REPO" != / ] && [ ! -f "$REPO/runner/Cargo.toml" ]; do REPO="$(dirname "$REPO")"; done
DEST_DIR="${TARGET_BUILD_DIR:?}/${CONTENTS_FOLDER_PATH:?}/Helpers"
DEST="$DEST_DIR/tmuxedo-runner"
WORK="${DERIVED_FILE_DIR:-${TMPDIR:-/tmp}}/tmuxedo-runner"
REQUIRED="managed signer-fd holder-build holder-info"
mkdir -p "$DEST_DIR" "$WORK"

from_cargo() {
  local targets="aarch64-apple-darwin x86_64-apple-darwin" outs=()
  if command -v rustup >/dev/null 2>&1; then rustup target add $targets >/dev/null 2>&1 || true; fi
  for t in $targets; do
    say "cargo build --release --target $t (runner at $(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo '?'))"
    (cd "$REPO/runner" && env -u SDKROOT -u MACOSX_DEPLOYMENT_TARGET cargo build --release --locked --target "$t" -p tmuxedo-runner) \
      || return 1
    outs+=("$REPO/runner/target/$t/release/tmuxedo-runner")
  done
  lipo -create "${outs[@]}" -output "$WORK/tmuxedo-runner"
}

from_download() {
  local server="${TMUXEDO_DL_SERVER:-https://tmuxedo.dev}" outs=()
  server="${server%/}"
  say "downloading the runner from $server/dl/"
  curl -fsSL "$server/dl/manifest.json" -o "$WORK/manifest.json" || die "cannot fetch $server/dl/manifest.json"
  for key in darwin-aarch64 darwin-x86_64; do
    local line want url
    line="$(grep "\"$key\"" "$WORK/manifest.json" | head -n 1)"
    want="$(printf '%s\n' "$line" | sed -n 's/.*"sha256": *"\([0-9a-f]\{64\}\)".*/\1/p')"
    url="$(printf '%s\n' "$line" | sed -n 's/.*"url": *"\([^"]*\)".*/\1/p')"
    [ -n "$want" ] || die "no $key build in the manifest"
    case "$url" in http://* | https://*) ;; /*) url="$server$url" ;; "") url="$server/dl/tmuxedo-runner-$key" ;; *) url="$server/dl/$url" ;; esac
    curl -fsSL "$url" -o "$WORK/$key" || die "download of $key failed"
    [ "$(shasum -a 256 "$WORK/$key" | cut -d' ' -f1)" = "$want" ] || die "$key does not match the manifest's sha256"
    outs+=("$WORK/$key")
  done
  lipo -create "${outs[@]}" -output "$WORK/tmuxedo-runner"
}

if [ -n "${TMUXEDO_RUNNER_BIN:-}" ]; then
  [ -x "$TMUXEDO_RUNNER_BIN" ] || die "TMUXEDO_RUNNER_BIN=$TMUXEDO_RUNNER_BIN is not an executable"
  say "using $TMUXEDO_RUNNER_BIN"
  cp "$TMUXEDO_RUNNER_BIN" "$WORK/tmuxedo-runner"
elif command -v cargo >/dev/null 2>&1 && [ "${TMUXEDO_RUNNER_FROM:-}" != "download" ]; then
  from_cargo || die "cargo build failed"
else
  from_download
fi

chmod 0755 "$WORK/tmuxedo-runner"
info="$("$WORK/tmuxedo-runner" version --json)" || die "the runner binary does not run on this Mac"
say "runner: $info"
for f in $REQUIRED; do
  case "$info" in *"\"$f\""*) ;; *) die "the runner lacks the '$f' feature this app needs (too old; build it from this commit or deploy it)" ;; esac
done

cp "$WORK/tmuxedo-runner" "$DEST"
ID="${EXPANDED_CODE_SIGN_IDENTITY:-}"
if [ "${CODE_SIGNING_ALLOWED:-YES}" != "NO" ] && [ -n "$ID" ] && [ "$ID" != "-" ]; then
  /usr/bin/codesign -f -s "$ID" -o runtime --timestamp --identifier dev.tmuxedo.mac.runner "$DEST"
  say "signed with $ID"
else
  # Unsigned development build: the app accepts an unsigned runner only while it is unsigned itself.
  /usr/bin/codesign -f -s - "$DEST" 2>/dev/null || true
  say "ad-hoc (unsigned build)"
fi
