#!/bin/sh
# Release build tools, pinned by version and (for downloads) by SHA-256.
# usage: install-release-tools.sh linux|macos
#   linux: zig + cargo-zigbuild (static musl builds), the two musl targets
#   both:  Rust 1.96.1 and cargo-cyclonedx (SBOMs)
set -eu
RUST_VERSION=1.96.1
ZIG_VERSION=0.15.2
ZIG_SHA256=02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239
ZIGBUILD_VERSION=0.23.4
ZIGBUILD_SHA256=6b69963040818ca1ef573d8135a95aa902e17cf7f873c32c6426642dadfe7f59
CYCLONEDX_VERSION=0.5.9

sha256_check() { # <file> <sha256>
  if command -v sha256sum >/dev/null 2>&1; then
    echo "$2  $1" | sha256sum -c -
  else
    echo "$2  $1" | shasum -a 256 -c -
  fi
}

rustup toolchain install "$RUST_VERSION" --profile minimal
rustup default "$RUST_VERSION"

case "${1:?linux or macos}" in
  linux)
    rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
    tmp="$(mktemp -d)"
    curl -fsSL -o "$tmp/zig.tar.xz" \
      "https://ziglang.org/download/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz"
    sha256_check "$tmp/zig.tar.xz" "$ZIG_SHA256"
    curl -fsSL -o "$tmp/zigbuild.tar.xz" \
      "https://github.com/rust-cross/cargo-zigbuild/releases/download/v${ZIGBUILD_VERSION}/cargo-zigbuild-x86_64-unknown-linux-gnu.tar.xz"
    sha256_check "$tmp/zigbuild.tar.xz" "$ZIGBUILD_SHA256"
    bin="${CARGO_HOME:-$HOME/.cargo}/bin"   # on PATH wherever rustup is
    mkdir -p "$HOME/.local/zig" "$bin"
    tar -xJf "$tmp/zig.tar.xz" -C "$HOME/.local/zig" --strip-components=1
    ln -sf "$HOME/.local/zig/zig" "$bin/zig"
    tar -xJf "$tmp/zigbuild.tar.xz" -C "$bin" --strip-components=1 \
      "cargo-zigbuild-x86_64-unknown-linux-gnu/cargo-zigbuild"
    rm -rf "$tmp"
    zig version
    cargo-zigbuild --version
    ;;
  macos)
    : # native builds: the host target only
    ;;
  *) echo "usage: $0 linux|macos" >&2; exit 2 ;;
esac

cargo install --locked cargo-cyclonedx --version "$CYCLONEDX_VERSION"
cargo cyclonedx --version
