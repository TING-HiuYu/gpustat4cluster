#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR=""
VERSION="${CLUSTAT_PACKAGE_SMOKE_VERSION:-local}"
LIBC="${CLUSTAT_PACKAGE_SMOKE_LIBC:-gnu}"
SKIP_BUILD="${CLUSTAT_PACKAGE_SMOKE_SKIP_BUILD:-0}"

log() {
  printf '[package-smoke] %s\n' "$*"
}

fail() {
  printf '[package-smoke][error] %s\n' "$*" >&2
  exit 1
}

cleanup() {
  local status=$?
  if [[ -n "$TMP_DIR" ]]; then
    rm -rf "$TMP_DIR"
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

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
    x86_64|amd64) ARCH="amd64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
  esac
}

build_release_binaries() {
  if [[ "$SKIP_BUILD" == "1" ]]; then
    log "skipping release build because CLUSTAT_PACKAGE_SMOKE_SKIP_BUILD=1"
    return 0
  fi

  load_rust_module_if_needed
  require_cmd cargo

  log "building release binaries"
  (
    cd "$ROOT_DIR"
    cargo build --locked --release -p clustat-server --features nvml
    cargo build --locked --release -p clustat-client-backend
    cargo build --locked --release -p clustat-client-cli
  )
}

package_role() {
  local role="$1"
  local pkgdir="$TMP_DIR/package-${role}"
  local outdir="$TMP_DIR/dist"
  local tarball="$outdir/clustat-${role}-${VERSION}-linux-${ARCH}-${LIBC}.tar.gz"

  mkdir -p \
    "$pkgdir/usr/local/bin" \
    "$pkgdir/etc/clustat" \
    "$pkgdir/etc/systemd/system" \
    "$outdir"

  cp "$ROOT_DIR/packaging/systemd/clustat-server.service" "$pkgdir/etc/systemd/system/"
  cp "$ROOT_DIR/packaging/systemd/clustat-client.service" "$pkgdir/etc/systemd/system/"

  if [[ "$role" == "server" ]]; then
    cp "$ROOT_DIR/dist/etc/clustat/server.toml.example" "$pkgdir/etc/clustat/"
    cp "$ROOT_DIR/dist/etc/clustat/server.toml.example" "$pkgdir/etc/clustat/server.toml"
    cp "$ROOT_DIR/target/release/clustat-server" "$pkgdir/usr/local/bin/clustat-server"
  elif [[ "$role" == "client" ]]; then
    cp "$ROOT_DIR/dist/etc/clustat/client.toml.example" "$pkgdir/etc/clustat/"
    cp "$ROOT_DIR/dist/etc/clustat/client.toml.example" "$pkgdir/etc/clustat/client.toml"
    cp "$ROOT_DIR/target/release/clustat-backend" "$pkgdir/usr/local/bin/"
    cp "$ROOT_DIR/target/release/clustat" "$pkgdir/usr/local/bin/clustat"
  else
    fail "unsupported package role: $role"
  fi

  tar -C "$pkgdir" -czf "$tarball" .
  printf '%s\n' "$tarball"
}

assert_file() {
  local path="$1"
  [[ -f "$path" ]] || fail "missing file in extracted artifact: $path"
}

assert_executable() {
  local path="$1"
  [[ -x "$path" ]] || fail "missing executable in extracted artifact: $path"
}

check_role() {
  local role="$1"
  local tarball="$2"
  local extract_dir="$TMP_DIR/extract-${role}"

  mkdir -p "$extract_dir"
  tar -xzf "$tarball" -C "$extract_dir"

  assert_file "$extract_dir/etc/systemd/system/clustat-server.service"
  assert_file "$extract_dir/etc/systemd/system/clustat-client.service"
  [[ ! -e "$extract_dir/etc/clustat/clustat.env" ]] \
    || fail "artifact should not include clustat.env"
  [[ ! -e "$extract_dir/etc/clustat/clustat.env.example" ]] \
    || fail "artifact should not include clustat.env.example"
  ! grep -R 'EnvironmentFile=-/etc/clustat/clustat.env' "$extract_dir/etc/systemd/system" \
    || fail "systemd unit should not reference clustat.env"

  if [[ "$role" == "server" ]]; then
    assert_file "$extract_dir/etc/clustat/server.toml"
    assert_file "$extract_dir/etc/clustat/server.toml.example"
    grep -q 'udp_port = 0' "$extract_dir/etc/clustat/server.toml" \
      || fail "server config does not default udp_port to auto"
    grep -q 'tcp_port = 0' "$extract_dir/etc/clustat/server.toml" \
      || fail "server config does not default tcp_port to auto"
    assert_executable "$extract_dir/usr/local/bin/clustat-server"
  else
    assert_file "$extract_dir/etc/clustat/client.toml"
    assert_file "$extract_dir/etc/clustat/client.toml.example"
    grep -q 'protocol = "udp"' "$extract_dir/etc/clustat/client.toml" \
      || fail "client config does not default to UDP"
    assert_executable "$extract_dir/usr/local/bin/clustat-backend"
    assert_executable "$extract_dir/usr/local/bin/clustat"
  fi
}

main() {
  require_cmd bash
  require_cmd tar
  require_cmd grep
  detect_arch

  TMP_DIR="$(mktemp -d)"
  build_release_binaries

  local server_tarball
  local client_tarball
  server_tarball="$(package_role server)"
  client_tarball="$(package_role client)"

  check_role server "$server_tarball"
  check_role client "$client_tarball"

  log "verified server artifact: $server_tarball"
  log "verified client artifact: $client_tarball"
  log "package artifact smoke passed"
}

main "$@"
