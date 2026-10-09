#!/usr/bin/env bash
# Build the release binary, then .deb, .rpm and Arch packages of it into
# dist/ with nfpm (https://nfpm.goreleaser.com).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${VERSION:-$(sed -nE 's/^version = "(.*)"/\1/p' Cargo.toml | head -n1)}"
ARCH="${ARCH:-amd64}"
export VERSION ARCH
command -v nfpm >/dev/null || { echo "nfpm not found: https://nfpm.goreleaser.com/install/" >&2; exit 1; }

if [ "${SKIP_BUILD:-0}" != 1 ]; then
    cargo build --release --locked
fi
./target/release/flatshot --version
mkdir -p dist
for fmt in deb rpm archlinux; do
    nfpm package --config packaging/nfpm.yaml --packager "$fmt" --target dist/
done
ls -l dist/
