#!/bin/sh
# After the four builds: add install.sh and manifest.json (the same format the
# runner's self-update reads from <server>/dl/manifest.json), then SHA256SUMS
# over every release file. Run from the repo root with the binaries and SBOMs
# in dist/.
set -eu
: "${TMUXEDO_VERSION:?set TMUXEDO_VERSION to the release tag}"
cd dist
for key in linux-x86_64 linux-aarch64 darwin-x86_64 darwin-aarch64; do
  test -s "tmuxedo-runner-$key" || { echo "missing tmuxedo-runner-$key" >&2; exit 1; }
  test -s "tmuxedo-runner-$key.cdx.json" || { echo "missing tmuxedo-runner-$key.cdx.json" >&2; exit 1; }
  chmod 0755 "tmuxedo-runner-$key"
done
cp ../runner/install.sh install.sh
{
  printf '{\n  "version": "%s",\n  "files": {\n' "$TMUXEDO_VERSION"
  for key in linux-x86_64 linux-aarch64 darwin-x86_64 darwin-aarch64; do
    sum="$(sha256sum "tmuxedo-runner-$key" | cut -d' ' -f1)"
    printf '    "%s": {"url": "tmuxedo-runner-%s", "sha256": "%s"},\n' "$key" "$key" "$sum"
  done | sed '$ s/,$//'
  printf '  }\n}\n'
} > manifest.json
sha256sum tmuxedo-runner-* install.sh manifest.json > SHA256SUMS
cat manifest.json SHA256SUMS
