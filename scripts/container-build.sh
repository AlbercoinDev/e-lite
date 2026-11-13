#!/usr/bin/env bash
# Runs INSIDE debian:12 (see scripts/package.sh). Builds the release binary and
# writes the .deb and tarball into /out. Expects /src (source) and /target
# (cargo target cache) to be mounted, plus VERSION and DEB_VERSION env vars.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  build-essential clang libclang-dev pkg-config curl ca-certificates dpkg-dev >/dev/null

if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"

export CARGO_TARGET_DIR=/target
export RUSTFLAGS="-C target-cpu=generic"
cd /src
cargo build --release --locked -p e-lite
strip --strip-unneeded /target/release/e-lite

rm -rf /tmp/pkg /tmp/tarroot
mkdir -p /tmp/pkg/DEBIAN /tmp/pkg/usr/bin /tmp/pkg/usr/share/doc/e-lite
install -m 0755 /target/release/e-lite /tmp/pkg/usr/bin/e-lite
for f in README.md LICENSE CHANGELOG.md config.example.toml public-servers.toml; do
  install -m 0644 "/src/$f" /tmp/pkg/usr/share/doc/e-lite/"$f"
done
install -d -m 0755 /tmp/pkg/usr/share/e-lite
install -m 0644 /src/public-servers.toml /tmp/pkg/usr/share/e-lite/public-servers.toml

cat > /tmp/pkg/DEBIAN/control <<EOF
Package: e-lite
Version: $DEB_VERSION
Section: net
Priority: optional
Architecture: amd64
Depends: libc6 (>= 2.36)
Maintainer: AlbercoinDev <albercoin@users.noreply.github.com>
Homepage: https://github.com/AlbercoinDev/e-lite
Description: Electrum 1.4-1.7 server for Bitcoin Core
 Query history, balance and UTXOs of any scriptPubKey without wallet
 registration. Confirmed chain only. No systemd unit is installed.
EOF

dpkg-deb --build --root-owner-group /tmp/pkg "/out/e-lite_${VERSION}_amd64.deb" >/dev/null

mkdir -p /tmp/tarroot
cp /target/release/e-lite /tmp/tarroot/e-lite
for f in README.md LICENSE CHANGELOG.md config.example.toml public-servers.toml; do
  cp "/src/$f" /tmp/tarroot/"$f"
done
tar -czf "/out/e-lite-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz" -C /tmp/tarroot .
