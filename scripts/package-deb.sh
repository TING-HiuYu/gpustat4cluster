#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${CLUSTAT_DEB_OUT_DIR:-$ROOT_DIR/dist}"
VERSION="${CLUSTAT_DEB_VERSION:-}"
REVISION="${CLUSTAT_DEB_REVISION:-1}"
SKIP_BUILD="${CLUSTAT_DEB_SKIP_BUILD:-0}"
MULTIARCH="${CLUSTAT_DEB_MULTIARCH:-0}"
ARM64_TARGET="${CLUSTAT_ARM64_TARGET:-aarch64-unknown-linux-gnu}"
TMP_DIR=""
ARCH=""
PKG_ARCH=""

log() { printf '[deb-package] %s\n' "$*"; }
fail() { printf '[deb-package][error] %s\n' "$*" >&2; exit 1; }

cleanup() {
  local status=$?
  [[ -n "$TMP_DIR" ]] && rm -rf "$TMP_DIR"
  exit "$status"
}
trap cleanup EXIT INT TERM

require_cmd() { command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"; }

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
  if [[ "$MULTIARCH" == "1" ]]; then
    PKG_ARCH="all"
  else
    PKG_ARCH="$ARCH"
  fi
}

detect_version() {
  if [[ -n "$VERSION" ]]; then
    return 0
  fi
  VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/crates/server/Cargo.toml" | head -1)"
  [[ -n "$VERSION" ]] || fail "could not detect package version"
}

build_native_release_binaries() {
  if [[ "$SKIP_BUILD" == "1" ]]; then
    log "skipping native release build because CLUSTAT_DEB_SKIP_BUILD=1"
    return 0
  fi

  load_rust_module_if_needed
  require_cmd cargo

  log "building native release server/client binaries"
  (
    cd "$ROOT_DIR"
    cargo build --locked --release -p clustat-server --features nvml
    cargo build --locked --release -p clustat-client-backend
    cargo build --locked --release -p clustat-client-cli
  )
}

build_arm64_release_binaries() {
  if [[ "$SKIP_BUILD" == "1" ]]; then
    return 0
  fi

  load_rust_module_if_needed
  require_cmd cargo

  log "building arm64 server/client/backend for target $ARM64_TARGET"
  (
    cd "$ROOT_DIR"
    case "$ARM64_TARGET" in
      aarch64-unknown-linux-gnu)
        require_cmd aarch64-linux-gnu-gcc
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-aarch64-linux-gnu-gcc}"
        ;;
      aarch64-unknown-linux-musl)
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-rust-lld}"
        ;;
    esac
    cargo build --locked --release --target "$ARM64_TARGET" -p clustat-server --features nvml
    cargo build --locked --release --target "$ARM64_TARGET" -p clustat-client-backend
    cargo build --locked --release --target "$ARM64_TARGET" -p clustat-client-cli
  )
}

write_arch_selector() {
  local path="$1" role="$2"
  cat >"$path" <<SELECTOREOF
#!/bin/sh
set -e

ROLE="$role"
ARCH="\
\$(dpkg --print-architecture 2>/dev/null || true)"
if [ -z "\$ARCH" ]; then
    case "\$(uname -m)" in
        x86_64|amd64) ARCH="amd64" ;;
        aarch64|arm64) ARCH="arm64" ;;
    esac
fi

SRC_DIR="/usr/lib/clustat/\$ARCH/bin"
if [ ! -d "\$SRC_DIR" ]; then
    echo "clustat: unsupported architecture \$ARCH; available binaries are under /usr/lib/clustat" >&2
    exit 1
fi

install -d -m 0755 /usr/local/bin
case "\$ROLE" in
    server)
        ln -sfn "\$SRC_DIR/clustat-server" /usr/local/bin/clustat-server
        ;;
    client)
        ln -sfn "\$SRC_DIR/clustat" /usr/local/bin/clustat
        ln -sfn "\$SRC_DIR/clustat-backend" /usr/local/bin/clustat-backend
        ;;
    *)
        echo "clustat: invalid role \$ROLE" >&2
        exit 1
        ;;
esac
SELECTOREOF
  chmod 0755 "$path"
}

write_postinst() {
  local path="$1" service="$2" role="$3" multiarch="$4"
  cat >"$path" <<POSTINSTEOF
#!/bin/sh
set -e

USER_NAME="clustat"
GROUP_NAME="clustat"
SERVICE_NAME="$service"
ROLE="$role"
CONFIG_NAME="$role.toml"

if ! getent group "\$GROUP_NAME" >/dev/null 2>&1; then
    addgroup --system "\$GROUP_NAME" >/dev/null
fi

if ! id -u "\$USER_NAME" >/dev/null 2>&1; then
    adduser --system --ingroup "\$GROUP_NAME" --home /var/lib/clustat \\
        --no-create-home --disabled-login --gecos "clustat service user" "\$USER_NAME" >/dev/null
fi

install -d -o "\$USER_NAME" -g "\$GROUP_NAME" -m 0755 /var/lib/clustat /var/log/clustat
install -d -o "\$USER_NAME" -g "\$GROUP_NAME" -m 0755 /run/clustat 2>/dev/null || true
install -d -o root -g root -m 0755 /etc/clustat

if [ "$multiarch" = "1" ]; then
    /usr/lib/clustat/select-binary "$role"
fi

if [ "$role" = "client" ] && ! which gpustat >/dev/null 2>&1; then
    if [ ! -e /usr/local/bin/gpustat ] && [ ! -L /usr/local/bin/gpustat ]; then
        ln -s /usr/local/bin/clustat /usr/local/bin/gpustat
    else
        echo "clustat: warning: /usr/local/bin/gpustat already exists but is not runnable; leaving it untouched" >&2
    fi
fi

if [ ! -f "/etc/clustat/\$CONFIG_NAME" ] && [ -f "/etc/clustat/\$CONFIG_NAME.example" ]; then
    cp "/etc/clustat/\$CONFIG_NAME.example" "/etc/clustat/\$CONFIG_NAME"
fi

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    if [ "\${CLUSTAT_DEB_START:-1}" = "1" ]; then
        systemctl enable --now "\$SERVICE_NAME" || echo "clustat: warning: failed to enable/start \$SERVICE_NAME; check journalctl -u \$SERVICE_NAME" >&2
    else
        systemctl enable "\$SERVICE_NAME" || true
    fi
fi

exit 0
POSTINSTEOF
  chmod 0755 "$path"
}

write_prerm() {
  local path="$1" service="$2" role="$3" multiarch="$4"
  cat >"$path" <<PRERMEOF
#!/bin/sh
set -e

SERVICE_NAME="$service"

if [ "\$1" = "remove" ] || [ "\$1" = "deconfigure" ]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl disable --now "\$SERVICE_NAME" >/dev/null 2>&1 || true
    fi
    if [ "$role" = "client" ] && [ "\$(readlink /usr/local/bin/gpustat 2>/dev/null || true)" = "/usr/local/bin/clustat" ]; then
        rm -f /usr/local/bin/gpustat
    fi
    if [ "$multiarch" = "1" ]; then
        case "$role" in
            server) rm -f /usr/local/bin/clustat-server ;;
            client)
                rm -f /usr/local/bin/clustat /usr/local/bin/clustat-backend
                ;;
        esac
    fi
fi

exit 0
PRERMEOF
  chmod 0755 "$path"
}

write_postrm() {
  local path="$1"
  cat >"$path" <<'POSTRMEOF'
#!/bin/sh
set -e

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    systemctl reset-failed >/dev/null 2>&1 || true
fi

exit 0
POSTRMEOF
  chmod 0755 "$path"
}

write_control() {
  local path="$1" package="$2" description="$3"
  cat >"$path" <<CONTROLEOF
Package: $package
Version: ${VERSION}-${REVISION}
Section: admin
Priority: optional
Architecture: $PKG_ARCH
Maintainer: clustat maintainers <root@localhost>
Depends: libc6, libgcc-s1, systemd | systemd-tmpfiles, adduser
Description: $description
 clustat provides low-latency GPU status collection and display across a cluster.
 This package installs the $description and enables the matching systemd service.
 Runtime dependencies are libc6 and libgcc-s1 for the bundled GNU/Linux binaries.
CONTROLEOF
  if [[ "$package" == "clustat-server" ]]; then
    cat >>"$path" <<'CONTROLEOF'
 The server dynamically loads NVIDIA NVML at runtime; provide libnvidia-ml.so or set runtime.nvml_lib_path in server.toml.
CONTROLEOF
  fi
}

install_common_files() {
  local pkgdir="$1" role="$2"
  mkdir -p \
    "$pkgdir/DEBIAN" \
    "$pkgdir/etc/clustat" \
    "$pkgdir/lib/systemd/system" \
    "$pkgdir/usr/local/bin"
  chmod 0755 "$pkgdir" "$pkgdir/DEBIAN"

  local role_config="$ROOT_DIR/dist/etc/clustat/${role}.toml.example"
  [[ -f "$role_config" ]] || fail "missing role config: $role_config"
  cp "$role_config" "$pkgdir/etc/clustat/${role}.toml.example"
  cp "$role_config" "$pkgdir/etc/clustat/${role}.toml"
  cat >"$pkgdir/DEBIAN/conffiles" <<CONFFILESEOF
/etc/clustat/${role}.toml
CONFFILESEOF
}

normalize_package_permissions() {
  local pkgdir="$1"
  find "$pkgdir" -type d -exec chmod 0755 {} +
  find "$pkgdir/etc/clustat" -type f -exec chmod 0644 {} +
  find "$pkgdir/lib/systemd/system" -type f -exec chmod 0644 {} +
  if [[ -d "$pkgdir/usr/local/bin" ]]; then
    find "$pkgdir/usr/local/bin" -type f -exec chmod 0755 {} +
  fi
  if [[ -d "$pkgdir/usr/lib/clustat" ]]; then
    find "$pkgdir/usr/lib/clustat" -type f -exec chmod 0755 {} +
  fi
  find "$pkgdir/DEBIAN" -type f ! -name postinst ! -name prerm ! -name postrm -exec chmod 0644 {} +
  chmod 0755 "$pkgdir/DEBIAN/postinst" "$pkgdir/DEBIAN/prerm" "$pkgdir/DEBIAN/postrm"
}

native_binary_path() {
  local bin="$1"
  case "$bin" in
    server) printf '%s/target/release/clustat-server\n' "$ROOT_DIR" ;;
    clustat-client-backend) printf '%s/target/release/clustat-backend\n' "$ROOT_DIR" ;;
    clustat-client) printf '%s/target/release/clustat\n' "$ROOT_DIR" ;;
  esac
}

arm64_binary_path() {
  local bin="$1" env_name=""
  case "$bin" in
    server) env_name="CLUSTAT_ARM64_SERVER_BIN" ;;
    clustat-client-backend) env_name="CLUSTAT_ARM64_CLIENT_BACKEND_BIN" ;;
    clustat-client) env_name="CLUSTAT_ARM64_CLIENT_BIN" ;;
  esac
  local override="${!env_name:-}"
  if [[ -n "$override" ]]; then
    printf '%s\n' "$override"
    return 0
  fi
  case "$bin" in
    server) printf '%s/target/%s/release/clustat-server\n' "$ROOT_DIR" "$ARM64_TARGET" ;;
    clustat-client-backend) printf '%s/target/%s/release/clustat-backend\n' "$ROOT_DIR" "$ARM64_TARGET" ;;
    clustat-client) printf '%s/target/%s/release/clustat\n' "$ROOT_DIR" "$ARM64_TARGET" ;;
  esac
}

copy_single_arch_server() {
  local pkgdir="$1"
  cp "$(native_binary_path server)" "$pkgdir/usr/local/bin/clustat-server"
}

copy_single_arch_client() {
  local pkgdir="$1"
  cp "$(native_binary_path clustat-client-backend)" "$pkgdir/usr/local/bin/clustat-backend"
  cp "$(native_binary_path clustat-client)" "$pkgdir/usr/local/bin/clustat"
}

copy_multiarch_server() {
  local pkgdir="$1"
  mkdir -p "$pkgdir/usr/lib/clustat/amd64/bin" "$pkgdir/usr/lib/clustat/arm64/bin"
  cp "$(native_binary_path server)" "$pkgdir/usr/lib/clustat/amd64/bin/clustat-server"
  local arm_server
  arm_server="$(arm64_binary_path server)"
  [[ -x "$arm_server" ]] || fail "missing arm64 server binary: $arm_server. Build it on arm64 or set CLUSTAT_ARM64_SERVER_BIN=/path/to/clustat-server"
  cp "$arm_server" "$pkgdir/usr/lib/clustat/arm64/bin/clustat-server"
  write_arch_selector "$pkgdir/usr/lib/clustat/select-binary" server
}

copy_multiarch_client() {
  local pkgdir="$1"
  mkdir -p "$pkgdir/usr/lib/clustat/amd64/bin" "$pkgdir/usr/lib/clustat/arm64/bin"
  cp "$(native_binary_path clustat-client)" "$pkgdir/usr/lib/clustat/amd64/bin/clustat"
  cp "$(native_binary_path clustat-client-backend)" "$pkgdir/usr/lib/clustat/amd64/bin/clustat-backend"
  cp "$(arm64_binary_path clustat-client)" "$pkgdir/usr/lib/clustat/arm64/bin/clustat"
  cp "$(arm64_binary_path clustat-client-backend)" "$pkgdir/usr/lib/clustat/arm64/bin/clustat-backend"
  write_arch_selector "$pkgdir/usr/lib/clustat/select-binary" client
}

package_server() {
  local pkgdir="$TMP_DIR/clustat-server"
  local deb="$OUT_DIR/clustat-server_${VERSION}-${REVISION}_${PKG_ARCH}.deb"
  install_common_files "$pkgdir" "server"
  if [[ "$MULTIARCH" == "1" ]]; then
    copy_multiarch_server "$pkgdir"
  else
    copy_single_arch_server "$pkgdir"
  fi
  cp "$ROOT_DIR/packaging/systemd/clustat-server.service" "$pkgdir/lib/systemd/system/clustat-server.service"
  write_control "$pkgdir/DEBIAN/control" "clustat-server" "clustat server daemon"
  write_postinst "$pkgdir/DEBIAN/postinst" "clustat-server.service" "server" "$MULTIARCH"
  write_prerm "$pkgdir/DEBIAN/prerm" "clustat-server.service" "server" "$MULTIARCH"
  write_postrm "$pkgdir/DEBIAN/postrm"
  normalize_package_permissions "$pkgdir"
  dpkg-deb --build --root-owner-group "$pkgdir" "$deb" >/dev/null \
    || fail "failed to build $deb"
  printf '%s\n' "$deb"
}

package_client() {
  local pkgdir="$TMP_DIR/clustat-client"
  local deb="$OUT_DIR/clustat-client_${VERSION}-${REVISION}_${PKG_ARCH}.deb"
  install_common_files "$pkgdir" "client"
  if [[ "$MULTIARCH" == "1" ]]; then
    copy_multiarch_client "$pkgdir"
  else
    copy_single_arch_client "$pkgdir"
  fi
  cp "$ROOT_DIR/packaging/systemd/clustat-client.service" "$pkgdir/lib/systemd/system/clustat-client.service"
  write_control "$pkgdir/DEBIAN/control" "clustat-client" "clustat client backend and CLI"
  write_postinst "$pkgdir/DEBIAN/postinst" "clustat-client.service" "client" "$MULTIARCH"
  write_prerm "$pkgdir/DEBIAN/prerm" "clustat-client.service" "client" "$MULTIARCH"
  write_postrm "$pkgdir/DEBIAN/postrm"
  normalize_package_permissions "$pkgdir"
  dpkg-deb --build --root-owner-group "$pkgdir" "$deb" >/dev/null \
    || fail "failed to build $deb"
  printf '%s\n' "$deb"
}

main() {
  require_cmd dpkg-deb
  require_cmd sed
  detect_arch
  detect_version
  TMP_DIR="$(mktemp -d)"
  mkdir -p "$OUT_DIR"
  build_native_release_binaries
  if [[ "$MULTIARCH" == "1" ]]; then
    build_arm64_release_binaries
  fi

  local server_deb client_deb
  server_deb="$(package_server)"
  client_deb="$(package_client)"

  log "built server deb: $server_deb"
  log "built client deb: $client_deb"
}

main "$@"
