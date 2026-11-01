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
  -v "$ROOT/scripts/container-build.sh":/container-build.sh:ro \
  -v e-lite-cargo-target:/target \
  -e VERSION="$VERSION" \
  -e DEB_VERSION="$DEB_VERSION" \
  -w /src \
  debian:12 bash /container-build.sh

cp "dist/e-lite_${VERSION}_amd64.deb" dist/e-lite_amd64.deb
cp "dist/e-lite-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz" dist/e-lite_x86_64-unknown-linux-gnu.tar.gz
cp install.sh dist/install.sh

(
  cd dist
  sha256sum \
    "e-lite_${VERSION}_amd64.deb" \
    e-lite_amd64.deb \
    "e-lite-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
    e-lite_x86_64-unknown-linux-gnu.tar.gz \
    install.sh > SHA256SUMS
)

echo "== artifacts =="
ls -lh dist
echo "== checksums =="
cat dist/SHA256SUMS
