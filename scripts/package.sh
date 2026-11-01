#!/usr/bin/env bash
# Build the Debian release artifacts inside debian:12 (glibc 2.36 baseline).
# Output: dist/ with .deb, tarball, fixed-name aliases, SHA256SUMS, install.sh.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
[ -n "$VERSION" ] || { echo "cannot read version from Cargo.toml" >&2; exit 1; }
DEB_VERSION="${VERSION/-/\~}"

echo "== building e-lite $VERSION inside debian:12 =="
rm -rf dist
mkdir -p dist

docker run --rm \
  -v "$ROOT":/src:ro \
  -v "$ROOT/dist":/out \
  -v e-lite-cargo-target:/target \
  -e VERSION="$VERSION" \
  -e DEB_VERSION="$DEB_VERSION" \
  -w /src \
  debian:12 bash -c '
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  build-essential clang libclang-dev pkg-config curl ca-certificates dpkg-dev >/dev/null
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
. "$HOME/.cargo/env"

export CARGO_TARGET_DIR=/target
export RUSTFLAGS="-C target-cpu=generic"
cargo build --release --locked -p e-lite
strip --strip-unneeded /target/release/e-lite

rm -rf /tmp/pkg /tmp/tarroot
mkdir -p /tmp/pkg/DEBIAN /tmp/pkg/usr/bin /tmp/pkg/usr/share/doc/e-lite
install -m 0755 /target/release/e-lite /tmp/pkg/usr/bin/e-lite
for f in README.md LICENSE CHANGELOG.md config.example.toml; do
  install -m 0644 "/src/$f" /tmp/pkg/usr/share/doc/e-lite/"$f"
done
cat > /tmp/pkg/DEBIAN/control <<EOF
Package: e-lite
Version: $DEB_VERSION
Section: net
Priority: optional
Architecture: amd64
Depends: libc6 (>= 2.36)
Maintainer: AlbercoinDev <albercoin@users.noreply.github.com>
Homepage: https://github.com/AlbercoinDev/e-lite
Description: Electrum 1.4 server for Bitcoin Core
 Query history, balance and UTXOs of any scriptPubKey without wallet
 registration. Confirmed chain only. No systemd unit is installed.
EOF
dpkg-deb --build --root-owner-group /tmp/pkg "/out/e-lite_${VERSION}_amd64.deb" >/dev/null

mkdir -p /tmp/tarroot
cp /target/release/e-lite /tmp/tarroot/e-lite
for f in README.md LICENSE CHANGELOG.md config.example.toml; do cp "/src/$f" /tmp/tarroot/"$f"; done
tar -czf "/out/e-lite-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz" -C /tmp/tarroot .
'

cp "dist/e-lite_${VERSION}_amd64.deb" dist/e-lite_amd64.deb
cp "dist/e-lite-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz" dist/e-lite_x86_64-unknown-linux-gnu.tar.gz
cp install.sh dist/install.sh

( cd dist && sha256sum \
    "e-lite_${VERSION}_amd64.deb" \
    e-lite_amd64.deb \
    "e-lite-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
    e-lite_x86_64-unknown-linux-gnu.tar.gz \
    install.sh > SHA256SUMS )

echo "== artifacts =="
ls -lh dist
echo "== checksums =="
cat dist/SHA256SUMS
