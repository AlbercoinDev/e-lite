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

# Build cache. Locally it defaults to the named Docker volume; CI points
# E_LITE_TARGET_DIR at a directory restored from actions/cache, and optionally
# mounts the cargo/rustup homes so the container does not reinstall the
# toolchain on every run.
MOUNTS=(
  -v "$ROOT":/src:ro
  -v "$ROOT/dist":/out
  -v "$ROOT/scripts/container-build.sh":/container-build.sh:ro
)
if [ -n "${E_LITE_TARGET_DIR:-}" ]; then
  mkdir -p "$E_LITE_TARGET_DIR"
  MOUNTS+=(-v "$E_LITE_TARGET_DIR":/target)
else
  MOUNTS+=(-v e-lite-cargo-target:/target)
fi
if [ -n "${E_LITE_CARGO_HOME:-}" ]; then
  mkdir -p "$E_LITE_CARGO_HOME"
  MOUNTS+=(-v "$E_LITE_CARGO_HOME":/root/.cargo)
fi
if [ -n "${E_LITE_RUSTUP_HOME:-}" ]; then
  mkdir -p "$E_LITE_RUSTUP_HOME"
  MOUNTS+=(-v "$E_LITE_RUSTUP_HOME":/root/.rustup)
fi

docker run --rm \
  "${MOUNTS[@]}" \
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
