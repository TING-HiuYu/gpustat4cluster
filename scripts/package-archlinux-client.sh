#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${CLUSTAT_ARCH_OUT_DIR:-$ROOT_DIR/dist}"
VERSION="${CLUSTAT_ARCH_VERSION:-}"
REVISION="${CLUSTAT_ARCH_REVISION:-1}"
SKIP_BUILD="${CLUSTAT_ARCH_SKIP_BUILD:-0}"
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
    log "skipping release build because CLUSTAT_ARCH_SKIP_BUILD=1"
    return 0
  fi
  load_rust_module_if_needed
  require_cmd cargo
  log "building native release client binaries"
  (
    cd "$ROOT_DIR"
    cargo build --locked --release -p clustat-client-backend
    cargo build --locked --release -p clustat-client-cli
  )
}

write_install_script() {
  local path="$1"
  cat >"$path" <<'INSTALL'
post_install() {
  getent group clustat >/dev/null 2>&1 || groupadd -r clustat || true
  id -u clustat >/dev/null 2>&1 || useradd -r -g clustat -d /var/lib/clustat -s /usr/bin/nologin -c "clustat service user" clustat || true
  install -d -o clustat -g clustat -m 0755 /var/lib/clustat /var/log/clustat /run/clustat || true
  install -d -o root -g root -m 0755 /etc/clustat || true
  if [ ! -f /etc/clustat/client.toml ] && [ -f /etc/clustat/client.toml.example ]; then
    cp /etc/clustat/client.toml.example /etc/clustat/client.toml
  fi
  if ! command -v gpustat >/dev/null 2>&1; then
    if [ ! -e /usr/local/bin/gpustat ] && [ ! -L /usr/local/bin/gpustat ]; then
      ln -s /usr/local/bin/clustat /usr/local/bin/gpustat || true
    else
      echo "clustat: warning: /usr/local/bin/gpustat already exists but is not runnable; leaving it untouched" >&2
    fi
  fi
  if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    if [ "${CLUSTAT_ARCH_START:-1}" = "1" ]; then
      systemctl enable --now clustat-client.service || echo "clustat: warning: failed to enable/start clustat-client.service; check journalctl -u clustat-client" >&2
    else
      systemctl enable clustat-client.service || true
    fi
  fi
}

post_upgrade() {
  post_install
}

pre_remove() {
  if command -v systemctl >/dev/null 2>&1; then
    systemctl disable --now clustat-client.service >/dev/null 2>&1 || true
  fi
  if [ "$(readlink /usr/local/bin/gpustat 2>/dev/null || true)" = "/usr/local/bin/clustat" ]; then
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
    "$pkgroot/etc/clustat"

  cp "$ROOT_DIR/target/release/clustat" "$pkgroot/usr/local/bin/clustat"
  cp "$ROOT_DIR/target/release/clustat-backend" "$pkgroot/usr/local/bin/clustat-backend"
  cp "$ROOT_DIR/packaging/systemd/clustat-client.service" "$pkgroot/usr/lib/systemd/system/clustat-client.service"
  cp "$ROOT_DIR/dist/etc/clustat/client.toml.example" "$pkgroot/etc/clustat/client.toml.example"
  cp "$ROOT_DIR/dist/etc/clustat/client.toml.example" "$pkgroot/etc/clustat/client.toml"

  chmod 0755 "$pkgroot/usr/local/bin/clustat" "$pkgroot/usr/local/bin/clustat-backend"
  chmod 0644 "$pkgroot/usr/lib/systemd/system/clustat-client.service" "$pkgroot/etc/clustat/client.toml" "$pkgroot/etc/clustat/client.toml.example"
}

write_pkginfo() {
  local pkgroot="$1"
  local size builddate
  size="$(du -sk "$pkgroot" | awk '{print $1 * 1024}')"
  builddate="${SOURCE_DATE_EPOCH:-$(date +%s)}"
  cat >"$pkgroot/.PKGINFO" <<PKGINFO
pkgname = clustat
pkgbase = clustat
pkgver = ${VERSION}-${REVISION}
pkgdesc = clustat client backend and CLI
url = https://github.com/hiuyu/clustat
builddate = $builddate
packager = clustat maintainers <root@localhost>
size = $size
arch = $ARCH
license = MIT
depend = glibc
depend = gcc-libs
depend = systemd
backup = etc/clustat/client.toml
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
  local artifact="$OUT_DIR/clustat-client-${VERSION}-${REVISION}-${ARCH}.pkg.tar.zst"
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
