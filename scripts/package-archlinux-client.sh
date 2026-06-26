#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${GPUSTAT4CLUSTER_ARCH_OUT_DIR:-$ROOT_DIR/dist}"
VERSION="${GPUSTAT4CLUSTER_ARCH_VERSION:-}"
REVISION="${GPUSTAT4CLUSTER_ARCH_REVISION:-1}"
SKIP_BUILD="${GPUSTAT4CLUSTER_ARCH_SKIP_BUILD:-0}"
TMP_DIR=""
ARCH=""

log() { printf '[arch-package] %s\n' "$*"; }
fail() { printf '[arch-package][error] %s\n' "$*" >&2; exit 1; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"; }

cleanup() {
  local status=$?
  [[ -n "$TMP_DIR" ]] && rm -rf "$TMP_DIR"
  exit "$status"
}
trap cleanup EXIT INT TERM

load_rust_module_if_needed() {
  if command -v cargo >/dev/null 2>&1; then
    return 0
  fi
  if [[ -f /opt/shell_related/z00_lmod.sh ]]; then
    # shellcheck disable=SC1091
    source /opt/shell_related/z00_lmod.sh
    module load compiler/rust
  fi
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
  esac
}

detect_version() {
  if [[ -n "$VERSION" ]]; then
    return 0
  fi
  VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/crates/client-cli/Cargo.toml" | head -1)"
  [[ -n "$VERSION" ]] || fail "could not detect package version"
}

build_release_binaries() {
  if [[ "$SKIP_BUILD" == "1" ]]; then
    log "skipping release build because GPUSTAT4CLUSTER_ARCH_SKIP_BUILD=1"
    return 0
  fi
  load_rust_module_if_needed
  require_cmd cargo
  log "building native release client binaries"
  (
    cd "$ROOT_DIR"
    cargo build --locked --release -p gpustat4cluster-client-backend
    cargo build --locked --release -p gpustat4cluster-client-cli
  )
}

write_install_script() {
  local path="$1"
  cat >"$path" <<'INSTALL'
post_install() {
  getent group gpustat4cluster >/dev/null 2>&1 || groupadd -r gpustat4cluster || true
  id -u gpustat4cluster >/dev/null 2>&1 || useradd -r -g gpustat4cluster -d /var/lib/gpustat4cluster -s /usr/bin/nologin -c "gpustat4cluster service user" gpustat4cluster || true
  install -d -o gpustat4cluster -g gpustat4cluster -m 0755 /var/lib/gpustat4cluster /var/log/gpustat4cluster /run/gpustat4cluster || true
  install -d -o root -g root -m 0755 /etc/gpustat4cluster || true
  if [ ! -f /etc/gpustat4cluster/client.toml ] && [ -f /etc/gpustat4cluster/client.toml.example ]; then
    cp /etc/gpustat4cluster/client.toml.example /etc/gpustat4cluster/client.toml
  fi
  if ! command -v gpustat >/dev/null 2>&1; then
    if [ ! -e /usr/local/bin/gpustat ] && [ ! -L /usr/local/bin/gpustat ]; then
      ln -s /usr/local/bin/gpustat4cluster-client /usr/local/bin/gpustat || true
    else
      echo "gpustat4cluster: warning: /usr/local/bin/gpustat already exists but is not runnable; leaving it untouched" >&2
    fi
  fi
  if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    if [ "${GPUSTAT4CLUSTER_ARCH_START:-1}" = "1" ]; then
      systemctl enable --now gpustat4cluster-client.service || echo "gpustat4cluster: warning: failed to enable/start gpustat4cluster-client.service; check journalctl -u gpustat4cluster-client" >&2
    else
      systemctl enable gpustat4cluster-client.service || true
    fi
  fi
}

post_upgrade() {
  post_install
}

pre_remove() {
  if command -v systemctl >/dev/null 2>&1; then
    systemctl disable --now gpustat4cluster-client.service >/dev/null 2>&1 || true
  fi
  if [ "$(readlink /usr/local/bin/gpustat 2>/dev/null || true)" = "/usr/local/bin/gpustat4cluster-client" ]; then
    rm -f /usr/local/bin/gpustat
  fi
}

post_remove() {
  if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    systemctl reset-failed >/dev/null 2>&1 || true
  fi
}
INSTALL
}

stage_package_root() {
  local pkgroot="$1"
  mkdir -p \
    "$pkgroot/usr/local/bin" \
    "$pkgroot/usr/lib/systemd/system" \
    "$pkgroot/etc/gpustat4cluster"

  cp "$ROOT_DIR/target/release/gpustat4cluster" "$pkgroot/usr/local/bin/gpustat4cluster-client"
  cp "$ROOT_DIR/target/release/gpustat4cluster-client-backend" "$pkgroot/usr/local/bin/gpustat4cluster-client-backend"
  cp "$ROOT_DIR/packaging/systemd/gpustat4cluster-client.service" "$pkgroot/usr/lib/systemd/system/gpustat4cluster-client.service"
  cp "$ROOT_DIR/dist/etc/gpustat4cluster/client.toml.example" "$pkgroot/etc/gpustat4cluster/client.toml.example"
  cp "$ROOT_DIR/dist/etc/gpustat4cluster/client.toml.example" "$pkgroot/etc/gpustat4cluster/client.toml"

  chmod 0755 "$pkgroot/usr/local/bin/gpustat4cluster-client" "$pkgroot/usr/local/bin/gpustat4cluster-client-backend"
  chmod 0644 "$pkgroot/usr/lib/systemd/system/gpustat4cluster-client.service" "$pkgroot/etc/gpustat4cluster/client.toml" "$pkgroot/etc/gpustat4cluster/client.toml.example"
}

write_pkginfo() {
  local pkgroot="$1"
  local size builddate
  size="$(du -sk "$pkgroot" | awk '{print $1 * 1024}')"
  builddate="${SOURCE_DATE_EPOCH:-$(date +%s)}"
  cat >"$pkgroot/.PKGINFO" <<PKGINFO
pkgname = gpustat4cluster-client
pkgbase = gpustat4cluster-client
pkgver = ${VERSION}-${REVISION}
pkgdesc = gpustat4cluster client backend and CLI
url = https://github.com/hiuyu/gpustat4cluster
builddate = $builddate
packager = gpustat4cluster maintainers <root@localhost>
size = $size
arch = $ARCH
license = MIT
depend = glibc
depend = gcc-libs
depend = systemd
backup = etc/gpustat4cluster/client.toml
PKGINFO
}

package_client() {
  require_cmd tar
  require_cmd zstd
  TMP_DIR="$(mktemp -d)"
  local pkgroot="$TMP_DIR/pkgroot"
  mkdir -p "$pkgroot" "$OUT_DIR"
  stage_package_root "$pkgroot"
  write_pkginfo "$pkgroot"
  write_install_script "$pkgroot/.INSTALL"
  chmod 0644 "$pkgroot/.PKGINFO" "$pkgroot/.INSTALL"
  local artifact="$OUT_DIR/gpustat4cluster-client-${VERSION}-${REVISION}-${ARCH}.pkg.tar.zst"
  log "building Arch Linux package $artifact"
  tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner --zstd -C "$pkgroot" -cf "$artifact" .
}

main() {
  detect_arch
  detect_version
  build_release_binaries
  package_client
  log "built Arch Linux package under $OUT_DIR"
}

main "$@"
