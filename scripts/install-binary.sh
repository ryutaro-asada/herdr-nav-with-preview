#!/usr/bin/env bash
# Download a prebuilt herdr-nav-preview binary from GitHub Releases into
# target/release/, so installing the plugin does not require a Rust toolchain.
# Exits non-zero if no matching prebuilt binary can be fetched, letting the
# manifest fall back to a local `cargo build`.
set -euo pipefail

REPO="ryutaro-asada/herdr-nav-with-preview"
BIN="herdr-nav-preview"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
[ -n "${VERSION}" ] || { echo "install-binary: no version in Cargo.toml" >&2; exit 1; }
TAG="v${VERSION}"

os="$(uname -s)"; arch="$(uname -m)"; target=""; ext=""
case "${os}" in
  Darwin) case "${arch}" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64)        target="x86_64-apple-darwin" ;; esac ;;
  Linux) case "${arch}" in
      x86_64)        target="x86_64-unknown-linux-gnu" ;;
      aarch64|arm64) target="aarch64-unknown-linux-gnu" ;; esac ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT) target="x86_64-pc-windows-msvc"; ext=".exe" ;;
esac
[ -n "${target}" ] || { echo "install-binary: no prebuilt for ${os}/${arch}; building from source" >&2; exit 1; }

asset="${BIN}-${target}${ext}"
url="https://github.com/${REPO}/releases/download/${TAG}/${asset}"
mkdir -p target/release
out="target/release/${BIN}${ext}"

echo "install-binary: fetching ${url}" >&2
if command -v curl >/dev/null 2>&1; then
  curl -fSL --retry 3 -o "${out}" "${url}"
elif command -v wget >/dev/null 2>&1; then
  wget -O "${out}" "${url}"
else
  echo "install-binary: no curl/wget; building from source" >&2; exit 1
fi
chmod +x "${out}" 2>/dev/null || true
echo "install-binary: installed ${out}" >&2
