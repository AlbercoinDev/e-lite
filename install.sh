#!/bin/sh
# e-lite installer / updater for Debian-based Linux (x86_64).
#
#   curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh
#
# Options (pass after `sh -s --`):
#   --with-core | --no-core   install/update Bitcoin Core, or skip it (default: ask)
#   --with-tor | --without-tor install/update the bundled Tor (default: yes)
#   --yes                     non-interactive
#   --prefix DIR              install prefix (default: ~/.local/bin)
#   --system                  install the .deb (requires root)
#   --version X.Y.Z           pin the e-lite release
#   --core-version X.Y.Z      pin the Bitcoin Core version
#   --force                   continue on non-Debian systems
#
# The same command installs or updates. It replaces the binary atomically and
# never touches configuration, databases or the Bitcoin Core datadir.

set -eu

REPO="AlbercoinDev/e-lite"
PREFIX="${E_LITE_PREFIX:-$HOME/.local/bin}"
CORE_MODE="ask"
TOR_MODE="yes"
ASSUME_YES=0
SYSTEM=0
FORCE=0
PIN_VERSION=""
CORE_VERSION=""
ASSET="e-lite_x86_64-unknown-linux-gnu.tar.gz"
# Tor expert bundle (GPLv3 binary, downloaded from the Tor Project at install
# time; e-lite does not redistribute it).
TOR_BUNDLE_VERSION="16.0"
TOR_BUNDLE_SHA256="cc950ebe9acb80188325c3a2cd8817def7dd7237d6fdcd861bb87b234a0cb2fd"
TOR_BUNDLE_URL="https://dist.torproject.org/torbrowser/${TOR_BUNDLE_VERSION}/tor-expert-bundle-linux-x86_64-${TOR_BUNDLE_VERSION}.tar.gz"

usage() {
  cat <<'EOF'
e-lite installer / updater (Debian 12+, x86_64)

  curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh

Options (after `sh -s --`):
  --with-core | --no-core   install/update Bitcoin Core, or skip it (default: ask)
  --with-tor | --without-tor install/update the bundled Tor (default: yes)
  --yes                     non-interactive
  --prefix DIR              install prefix (default: ~/.local/bin)
  --system                  install the .deb (requires root)
  --version X.Y.Z           pin the e-lite release
  --core-version X.Y.Z      pin the Bitcoin Core version
  --force                   continue on non-Debian systems
EOF
}

log() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --with-core) CORE_MODE="yes" ;;
    --no-core) CORE_MODE="no" ;;
    --with-tor) TOR_MODE="yes" ;;
    --without-tor|--no-tor) TOR_MODE="no" ;;
    --yes|-y) ASSUME_YES=1 ;;
    --system) SYSTEM=1; PREFIX="/usr/local/bin" ;;
    --force) FORCE=1 ;;
    --prefix) shift; [ $# -gt 0 ] || die "--prefix needs a value"; PREFIX="$1" ;;
    --prefix=*) PREFIX="${1#--prefix=}" ;;
    --version) shift; [ $# -gt 0 ] || die "--version needs a value"; PIN_VERSION="$1" ;;
    --version=*) PIN_VERSION="${1#--version=}" ;;
    --core-version) shift; [ $# -gt 0 ] || die "--core-version needs a value"; CORE_VERSION="$1" ;;
    --core-version=*) CORE_VERSION="${1#--core-version=}" ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
  shift
done

# ---------------------------------------------------------------- environment
[ "$(uname -s)" = "Linux" ] || die "e-lite supports Linux only"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) ;;
  *) die "unsupported architecture: $ARCH (only x86_64 is published)" ;;
esac
if [ ! -f /etc/debian_version ] && [ "$FORCE" -eq 0 ]; then
  die "this build targets Debian 12+ (no /etc/debian_version found; use --force to override)"
fi
command -v curl >/dev/null 2>&1 || die "curl is required"
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT TERM

fetch() { curl -fsSL --retry 3 --connect-timeout 15 -o "$2" "$1"; }

# --------------------------------------------------------------- current state
CURRENT=""
if [ -x "$PREFIX/e-lite" ]; then
  CURRENT="$("$PREFIX/e-lite" --version 2>/dev/null | awk '{print $2}' || true)"
fi

if [ -n "$PIN_VERSION" ]; then
  TAG="v$PIN_VERSION"
  BASE="https://github.com/$REPO/releases/download/$TAG"
  TARGET="$PIN_VERSION"
else
  TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/' || true)"
  if [ -n "$TAG" ]; then
    BASE="https://github.com/$REPO/releases/download/$TAG"
    TARGET="${TAG#v}"
  else
    BASE="https://github.com/$REPO/releases/latest/download"
    TARGET="latest"
  fi
fi

if [ "$TARGET" != "latest" ] && [ "$CURRENT" = "$TARGET" ] && [ "$SYSTEM" -eq 0 ]; then
  log "e-lite is already up to date ($CURRENT)"
else
  log "e-lite: installing/updating${TARGET:+ to $TARGET}${CURRENT:+ (current: $CURRENT)}"
  if [ "$SYSTEM" -eq 1 ]; then
    [ "$(id -u)" -eq 0 ] || die "--system requires root"
    DEB="e-lite_amd64.deb"
    fetch "$BASE/$DEB" "$TMP/$DEB"
    fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS"
    ( cd "$TMP" && grep "  $DEB\$" SHA256SUMS | sha256sum -c - >/dev/null ) \
      || die "checksum verification failed for $DEB"
    dpkg -i "$TMP/$DEB"
    CURRENT="$TARGET"
  else
    fetch "$BASE/$ASSET" "$TMP/$ASSET"
    fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS"
    ( cd "$TMP" && grep "  $ASSET\$" SHA256SUMS | sha256sum -c - >/dev/null ) \
      || die "checksum verification failed for $ASSET"
    mkdir -p "$TMP/pkg"
    tar -xzf "$TMP/$ASSET" -C "$TMP/pkg"
    [ -x "$TMP/pkg/e-lite" ] || die "unexpected archive layout (missing e-lite binary)"
    mkdir -p "$PREFIX" 2>/dev/null || die "cannot create $PREFIX"
    [ -w "$PREFIX" ] || die "$PREFIX is not writable (use --prefix or --system)"
    install -m 0755 "$TMP/pkg/e-lite" "$PREFIX/.e-lite.new"
    mv -f "$PREFIX/.e-lite.new" "$PREFIX/e-lite"
    CURRENT="$("$PREFIX/e-lite" --version | awk '{print $2}')"
  fi
  log "e-lite: installed $CURRENT at $PREFIX/e-lite"
fi

if [ "$SYSTEM" -eq 0 ]; then
  case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *) warn "$PREFIX is not in PATH; add: export PATH=\"$PREFIX:\$PATH\"" ;;
  esac
fi

# ------------------------------------------------------------- Bitcoin Core
core_latest() {
  if [ -n "$CORE_VERSION" ]; then
    printf '%s' "$CORE_VERSION"
    return 0
  fi
  v="$(curl -fsSL https://bitcoincore.org/en/download/ 2>/dev/null \
       | grep -oE 'bitcoin-core-[0-9]+\.[0-9]+(\.[0-9]+)?' | head -n 1 | sed 's/bitcoin-core-//' || true)"
  if [ -z "$v" ]; then
    v="$(curl -fsSL https://bitcoincore.org/bin/ 2>/dev/null \
         | grep -oE 'bitcoin-core-[0-9]+\.[0-9]+(\.[0-9]+)?/' | head -n 1 | sed 's|bitcoin-core-||; s|/||' || true)"
  fi
  printf '%s' "$v"
}

core_installed_version() {
  for c in "$PREFIX/bitcoind" /usr/local/bin/bitcoind /usr/bin/bitcoind; do
    if [ -x "$c" ]; then
      "$c" -version 2>/dev/null | head -n 1 \
        | sed -n 's/.*version v\{0,1\}\([0-9][0-9]*\.[0-9][0-9]*\(\.[0-9][0-9]*\)*\).*/\1/p'
      return 0
    fi
  done
  return 1
}

# Compare versions treating "32.1" and "32.1.0" as equal.
same_version() {
  a="$1"; b="$2"
  [ "$a" = "$b" ] && return 0
  pa="$(printf '%s' "$a" | awk -F. '{printf "%d.%d.%d", $1, $2, $3+0}')"
  pb="$(printf '%s' "$b" | awk -F. '{printf "%d.%d.%d", $1, $2, $3+0}')"
  [ "$pa" = "$pb" ]
}

install_core() {
  want="$1"
  [ -n "$want" ] || { warn "could not determine the latest Bitcoin Core version; skipping"; return 0; }

  if [ -x /usr/bin/bitcoind ] && [ "$PREFIX" != "/usr/bin" ] && [ "$PREFIX" != "/usr/local/bin" ]; then
    warn "Bitcoin Core from apt detected at /usr/bin/bitcoind; update it with apt to avoid mixed installs"
    return 0
  fi

  have="$(core_installed_version || true)"
  if [ -n "$have" ] && same_version "$have" "$want"; then
    log "Bitcoin Core is already up to date ($have)"
    return 0
  fi

  log "Bitcoin Core: installing/updating${have:+ from $have} to $want"
  tarball="bitcoin-$want-x86_64-linux-gnu.tar.gz"
  url="https://bitcoincore.org/bin/bitcoin-core-$want/$tarball"
  fetch "$url" "$TMP/$tarball" || { warn "cannot download $url"; return 0; }
  if fetch "https://bitcoincore.org/bin/bitcoin-core-$want/SHA256SUMS" "$TMP/core.SHA256SUMS"; then
    ( cd "$TMP" && grep "  $tarball\$" core.SHA256SUMS | sha256sum -c - >/dev/null ) \
      || { warn "checksum verification failed for Bitcoin Core; skipping"; return 0; }
  else
    warn "no SHA256SUMS published for Bitcoin Core $want; skipping for safety"
    return 0
  fi
  mkdir -p "$TMP/core"
  tar -xzf "$TMP/$tarball" -C "$TMP/core"
  mkdir -p "$PREFIX"
  install -m 0755 "$TMP/core/bitcoin-$want/bin/bitcoind" "$PREFIX/bitcoind"
  install -m 0755 "$TMP/core/bitcoin-$want/bin/bitcoin-cli" "$PREFIX/bitcoin-cli"
  log "Bitcoin Core $want installed at $PREFIX (datadir and bitcoin.conf untouched)"
  log "restart bitcoind yourself for the new version to take effect"
}

case "$CORE_MODE" in
  yes) install_core "$(core_latest)" ;;
  no) : ;;
  ask)
    if [ "$ASSUME_YES" -eq 1 ]; then
      :
    elif (: </dev/tty) 2>/dev/null; then
      latest="$(core_latest)"
      have="$(core_installed_version || true)"
      if [ -n "$latest" ] && ! same_version "$have" "$latest"; then
        if [ -n "$have" ]; then state="Bitcoin Core $have installed"; else state="Bitcoin Core not installed"; fi
        printf '%s. Install/update to the latest official %s? [y/N] ' "$state" "$latest" >/dev/tty
        ans=""
        read -r ans </dev/tty || ans=""
        case "$ans" in
          y|Y|yes|YES) install_core "$latest" ;;
          *) log "Bitcoin Core: skipped" ;;
        esac
      fi
    else
      log "Bitcoin Core: not prompted (no terminal); use --with-core to install it"
    fi
    ;;
  *) die "bad core mode" ;;
esac

# ----------------------------------------------------------------------- tor
install_tor() {
  [ "$TOR_MODE" = "yes" ] || { log "Tor: skipped"; return 0; }
  libdir="$(dirname "$PREFIX")/lib/e-lite/tor"
  if [ -x "$libdir/tor" ] && [ "$(cat "$libdir/VERSION" 2>/dev/null || true)" = "$TOR_BUNDLE_VERSION" ]; then
    log "Tor is already up to date ($TOR_BUNDLE_VERSION)"
    return 0
  fi
  log "Tor: installing/updating to $TOR_BUNDLE_VERSION (official expert bundle)"
  tgz="tor-expert-$TOR_BUNDLE_VERSION.tar.gz"
  if ! fetch "$TOR_BUNDLE_URL" "$TMP/$tgz"; then
    warn "cannot download the Tor bundle; install tor with your package manager or rerun with --with-tor"
    return 0
  fi
  actual="$(sha256sum "$TMP/$tgz" | awk '{print $1}')"
  if [ "$actual" != "$TOR_BUNDLE_SHA256" ]; then
    warn "Tor bundle checksum mismatch (expected $TOR_BUNDLE_SHA256, got $actual); skipping"
    return 0
  fi
  mkdir -p "$TMP/torbundle"
  tar -xzf "$TMP/$tgz" -C "$TMP/torbundle" tor || { warn "bad Tor bundle layout; skipping"; return 0; }
  mkdir -p "$libdir"
  # Only the daemon and its bundled libraries are needed; skip the pluggable
  # transports (bridges) to keep the install small.
  cp -a "$TMP/torbundle/tor/tor" "$libdir/tor"
  for lib in "$TMP/torbundle/tor/"lib*.so*; do
    [ -f "$lib" ] && cp -a "$lib" "$libdir/"
  done
  printf '%s\n' "$TOR_BUNDLE_VERSION" > "$libdir/VERSION"
  chmod 0755 "$libdir/tor" 2>/dev/null || true
  log "Tor $TOR_BUNDLE_VERSION installed at $libdir"
}
install_tor

# ---------------------------------------------------------------------- done
log ""
log "Next steps:"
log "  e-lite doctor --network <regtest|signet|testnet4|mainnet> --core-datadir <path>"
log "  e-lite run    --network <...> --core-datadir <path> --data-dir <path>"
log "    (run waits for Core, starts it with manage=auto, and auto-initializes)"
log "All-in-one test + report: bash scripts/e-lite-all-in-one.sh --help"
log "Docs: https://github.com/$REPO/blob/main/docs/GETTING-STARTED.md"
