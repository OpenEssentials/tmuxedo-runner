#!/bin/sh
# Build one release binary and its SBOM into dist/ (run from the repo root).
# usage: TMUXEDO_VERSION=v1.2.3 build-release.sh <rust-target> <key>
#   e.g. build-release.sh x86_64-unknown-linux-musl linux-x86_64
# Linux targets are static musl builds through cargo-zigbuild; macOS targets
# are built natively on a macOS runner of the same architecture.
set -eu
target="${1:?rust target}"
key="${2:?download key, e.g. linux-x86_64}"
: "${TMUXEDO_VERSION:?set TMUXEDO_VERSION to the release tag}"
export TMUXEDO_VERSION CARGO_TERM_COLOR=never

root="$(pwd)"
mkdir -p "$root/dist"
cd "$root/runner"
case "$target" in
  *-linux-musl) cargo zigbuild --locked --release -p tmuxedo-runner --target "$target" ;;
  *-apple-darwin) cargo build --locked --release -p tmuxedo-runner --target "$target" ;;
  *) echo "unsupported target $target" >&2; exit 2 ;;
esac
bin="$root/dist/tmuxedo-runner-$key"
cp "target/$target/release/tmuxedo-runner" "$bin"

# SBOM (CycloneDX JSON): the runner crate's dependency graph resolved for this
# target, from Cargo.lock.
cargo cyclonedx --manifest-path crates/runner/Cargo.toml --format json \
  --spec-version 1.5 --target "$target" --override-filename "tmuxedo-runner-$key"
mv "crates/runner/tmuxedo-runner-$key.json" "$root/dist/tmuxedo-runner-$key.cdx.json"
find crates -maxdepth 2 -name 'tmuxedo-runner-*.json' -delete

# Smoke test when the binary is for this machine.
here="$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m | sed 's/^arm64$/aarch64/')"
if [ "$key" = "$here" ]; then "$bin" version; fi
ls -l "$root/dist"
