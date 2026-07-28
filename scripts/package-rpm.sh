#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${CLUSTAT_RPM_OUT_DIR:-$ROOT_DIR/dist}"
VERSION="${CLUSTAT_RPM_VERSION:-}"
REVISION="${CLUSTAT_RPM_REVISION:-1}"
SKIP_BUILD="${CLUSTAT_RPM_SKIP_BUILD:-0}"
MULTIARCH="${CLUSTAT_RPM_MULTIARCH:-1}"
ARM64_TARGET="${CLUSTAT_ARM64_TARGET:-aarch64-unknown-linux-gnu}"
TMP_DIR=""
ARCH=""

log() { printf '[rpm-package] %s\n' "$*"; }
fail() { printf '[rpm-package][error] %s\n' "$*" >&2; exit 1; }
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
    x86_64|amd64) ARCH="amd64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
  esac
}

detect_version() {
  if [[ -n "$VERSION" ]]; then
    return 0
  fi
  VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/crates/server/Cargo.toml" | head -1)"
  [[ -n "$VERSION" ]] || fail "could not detect package version"
}

build_release_binaries() {
  if [[ "$SKIP_BUILD" == "1" ]]; then
    log "skipping release build because CLUSTAT_RPM_SKIP_BUILD=1"
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

  if [[ "$MULTIARCH" == "1" ]]; then
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
  fi
}

native_binary_path() {
  case "$1" in
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

write_arch_selector() {
  local path="$1" role="$2"
  cat >"$path" <<SELECTOREOF
#!/bin/sh
set -e
ROLE="$role"
case "\$(uname -m)" in
    x86_64|amd64) ARCH="amd64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    *) echo "clustat: unsupported architecture \$(uname -m)" >&2; exit 1 ;;
esac
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

stage_common_files() {
  local pkgroot="$1" role="$2"
  mkdir -p \
    "$pkgroot/etc/clustat" \
    "$pkgroot/usr/lib/systemd/system" \
    "$pkgroot/usr/lib/clustat"

  local role_config="$ROOT_DIR/dist/etc/clustat/${role}.toml.example"
  [[ -f "$role_config" ]] || fail "missing role config: $role_config"
  cp "$role_config" "$pkgroot/etc/clustat/${role}.toml.example"
  cp "$role_config" "$pkgroot/etc/clustat/${role}.toml"
  cp "$ROOT_DIR/packaging/systemd/clustat-${role}.service" \
    "$pkgroot/usr/lib/systemd/system/clustat-${role}.service"
}

stage_role_files() {
  local pkgroot="$1" role="$2"
  stage_common_files "$pkgroot" "$role"

  if [[ "$MULTIARCH" == "1" ]]; then
    mkdir -p "$pkgroot/usr/lib/clustat/amd64/bin" "$pkgroot/usr/lib/clustat/arm64/bin"
    if [[ "$role" == "server" ]]; then
      cp "$(native_binary_path server)" "$pkgroot/usr/lib/clustat/amd64/bin/clustat-server"
      cp "$(arm64_binary_path server)" "$pkgroot/usr/lib/clustat/arm64/bin/clustat-server"
    else
      cp "$(native_binary_path clustat-client)" "$pkgroot/usr/lib/clustat/amd64/bin/clustat"
      cp "$(native_binary_path clustat-client-backend)" "$pkgroot/usr/lib/clustat/amd64/bin/clustat-backend"
      cp "$(arm64_binary_path clustat-client)" "$pkgroot/usr/lib/clustat/arm64/bin/clustat"
      cp "$(arm64_binary_path clustat-client-backend)" "$pkgroot/usr/lib/clustat/arm64/bin/clustat-backend"
    fi
    write_arch_selector "$pkgroot/usr/lib/clustat/select-binary" "$role"
  else
    mkdir -p "$pkgroot/usr/local/bin"
    if [[ "$role" == "server" ]]; then
      cp "$(native_binary_path server)" "$pkgroot/usr/local/bin/clustat-server"
    else
      cp "$(native_binary_path clustat-client)" "$pkgroot/usr/local/bin/clustat"
      cp "$(native_binary_path clustat-client-backend)" "$pkgroot/usr/local/bin/clustat-backend"
    fi
  fi
}

write_spec() {
  local spec="$1"
  local role="$2"
  local package="clustat-${role}"
  local summary="clustat ${role} daemon"
  if [[ "$role" == "client" ]]; then
    summary="clustat client backend and CLI"
  fi

  cat >"$spec" <<SPECEOF
Name:           $package
Version:        $VERSION
Release:        $REVISION%{?dist}
Summary:        $summary
License:        MIT
BuildArch:      noarch
Requires:       systemd
%global __brp_strip %{nil}
%global __brp_strip_static_archive %{nil}
%global __brp_strip_comment_note %{nil}
%global _binaries_in_noarch_packages_terminate_build 0

%description
clustat provides low-latency GPU status collection and display across a cluster.

%prep

%build

%install
rm -rf %{buildroot}
cp -a %{_sourcedir}/root/. %{buildroot}/

%post
getent group clustat >/dev/null 2>&1 || groupadd -r clustat || true
id -u clustat >/dev/null 2>&1 || useradd -r -g clustat -d /var/lib/clustat -s /sbin/nologin -c "clustat service user" clustat || true
install -d -o clustat -g clustat -m 0755 /var/lib/clustat /var/log/clustat /run/clustat || true
if [ -x /usr/lib/clustat/select-binary ]; then
    /usr/lib/clustat/select-binary $role || true
fi
SPECEOF

  if [[ "$role" == "client" ]]; then
    cat >>"$spec" <<'SPECEOF'
if ! command -v gpustat >/dev/null 2>&1; then
    if [ ! -e /usr/local/bin/gpustat ] && [ ! -L /usr/local/bin/gpustat ]; then
        ln -s /usr/local/bin/clustat /usr/local/bin/gpustat || true
    else
        echo "clustat: warning: /usr/local/bin/gpustat already exists but is not runnable; leaving it untouched" >&2
    fi
fi
SPECEOF
  fi

  cat >>"$spec" <<SPECEOF
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    if [ "\${CLUSTAT_RPM_START:-1}" = "1" ]; then
        systemctl enable --now clustat-${role}.service || echo "clustat: warning: failed to enable/start clustat-${role}.service; check journalctl -u clustat-${role}" >&2
    else
        systemctl enable clustat-${role}.service || true
    fi
fi

%preun
if [ \$1 -eq 0 ] && command -v systemctl >/dev/null 2>&1; then
    systemctl disable --now clustat-${role}.service >/dev/null 2>&1 || true
fi
SPECEOF

  if [[ "$role" == "client" ]]; then
    cat >>"$spec" <<'SPECEOF'
if [ $1 -eq 0 ] && [ "$(readlink /usr/local/bin/clustat 2>/dev/null || true)" = "/usr/lib/clustat/$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')/bin/clustat" ]; then
    rm -f /usr/local/bin/clustat
fi
if [ $1 -eq 0 ] && [ "$(readlink /usr/local/bin/clustat-backend 2>/dev/null || true)" = "/usr/lib/clustat/$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')/bin/clustat-backend" ]; then
    rm -f /usr/local/bin/clustat-backend
fi
if [ $1 -eq 0 ] && [ "$(readlink /usr/local/bin/gpustat 2>/dev/null || true)" = "/usr/local/bin/clustat" ]; then
    rm -f /usr/local/bin/gpustat
fi
SPECEOF
  elif [[ "$role" == "server" ]]; then
    cat >>"$spec" <<'SPECEOF'
if [ $1 -eq 0 ] && [ "$(readlink /usr/local/bin/clustat-server 2>/dev/null || true)" = "/usr/lib/clustat/$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')/bin/clustat-server" ]; then
    rm -f /usr/local/bin/clustat-server
fi
SPECEOF
  fi

  cat >>"$spec" <<SPECEOF

%postun
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    systemctl reset-failed >/dev/null 2>&1 || true
fi

%files
%config(noreplace) /etc/clustat/${role}.toml
/etc/clustat/${role}.toml.example
/usr/lib/systemd/system/clustat-${role}.service
SPECEOF

  if [[ "$MULTIARCH" == "1" ]]; then
    cat >>"$spec" <<'SPECEOF'
/usr/lib/clustat
SPECEOF
  else
    cat >>"$spec" <<SPECEOF
/usr/local/bin/clustat-*
SPECEOF
  fi

  cat >>"$spec" <<SPECEOF

%changelog
* Thu May 21 2026 clustat maintainers <root@localhost> - $VERSION-$REVISION
- Automated release package.
SPECEOF
}

package_role() {
  local role="$1"
  local topdir="$TMP_DIR/rpmbuild-${role}"
  local pkgroot="$topdir/SOURCES/root"
  local spec="$topdir/SPECS/clustat-${role}.spec"
  mkdir -p "$topdir/BUILD" "$topdir/BUILDROOT" "$topdir/RPMS" "$topdir/SOURCES" "$topdir/SPECS" "$topdir/SRPMS"
  stage_role_files "$pkgroot" "$role"
  find "$pkgroot" -type d -exec chmod 0755 {} +
  find "$pkgroot" -type f -exec chmod 0644 {} +
  find "$pkgroot" -path '*/bin/*' -type f -exec chmod 0755 {} +
  [[ -f "$pkgroot/usr/lib/clustat/select-binary" ]] && chmod 0755 "$pkgroot/usr/lib/clustat/select-binary"
  write_spec "$spec" "$role"
  rpmbuild --define "_topdir $topdir" -bb "$spec" >/dev/null || fail "failed to build RPM for $role"
  find "$topdir/RPMS" -type f -name '*.rpm' -exec cp {} "$OUT_DIR/" \;
}

main() {
  require_cmd rpmbuild
  detect_arch
  detect_version
  TMP_DIR="$(mktemp -d)"
  mkdir -p "$OUT_DIR"
  build_release_binaries
  package_role server
  package_role client
  log "built RPMs under $OUT_DIR"
}

main "$@"
