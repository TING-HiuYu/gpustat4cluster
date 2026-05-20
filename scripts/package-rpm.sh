#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${GPUSTAT4CLUSTER_RPM_OUT_DIR:-$ROOT_DIR/dist}"
VERSION="${GPUSTAT4CLUSTER_RPM_VERSION:-}"
REVISION="${GPUSTAT4CLUSTER_RPM_REVISION:-1}"
SKIP_BUILD="${GPUSTAT4CLUSTER_RPM_SKIP_BUILD:-0}"
MULTIARCH="${GPUSTAT4CLUSTER_RPM_MULTIARCH:-1}"
ARM64_TARGET="${GPUSTAT4CLUSTER_ARM64_TARGET:-aarch64-unknown-linux-gnu}"
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
    log "skipping release build because GPUSTAT4CLUSTER_RPM_SKIP_BUILD=1"
    return 0
  fi

  load_rust_module_if_needed
  require_cmd cargo

  log "building native release server/client binaries with KCP transport"
  (
    cd "$ROOT_DIR"
    cargo build --locked --release -p server --features "nvml kcp-transport"
    cargo build --locked --release -p gpustat4cluster-client-backend --features kcp-transport
    cargo build --locked --release -p gpustat4cluster-client-cli
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
      cargo build --locked --release --target "$ARM64_TARGET" -p server --features "nvml kcp-transport"
      cargo build --locked --release --target "$ARM64_TARGET" -p gpustat4cluster-client-backend --features kcp-transport
      cargo build --locked --release --target "$ARM64_TARGET" -p gpustat4cluster-client-cli
    )
  fi
}

native_binary_path() {
  case "$1" in
    server) printf '%s/target/release/server\n' "$ROOT_DIR" ;;
    gpustat4cluster-client-backend) printf '%s/target/release/gpustat4cluster-client-backend\n' "$ROOT_DIR" ;;
    gpustat4cluster-client) printf '%s/target/release/gpustat4cluster\n' "$ROOT_DIR" ;;
  esac
}

arm64_binary_path() {
  local bin="$1" env_name=""
  case "$bin" in
    server) env_name="GPUSTAT4CLUSTER_ARM64_SERVER_BIN" ;;
    gpustat4cluster-client-backend) env_name="GPUSTAT4CLUSTER_ARM64_CLIENT_BACKEND_BIN" ;;
    gpustat4cluster-client) env_name="GPUSTAT4CLUSTER_ARM64_CLIENT_BIN" ;;
  esac
  local override="${!env_name:-}"
  if [[ -n "$override" ]]; then
    printf '%s\n' "$override"
    return 0
  fi
  case "$bin" in
    server) printf '%s/target/%s/release/server\n' "$ROOT_DIR" "$ARM64_TARGET" ;;
    gpustat4cluster-client-backend) printf '%s/target/%s/release/gpustat4cluster-client-backend\n' "$ROOT_DIR" "$ARM64_TARGET" ;;
    gpustat4cluster-client) printf '%s/target/%s/release/gpustat4cluster\n' "$ROOT_DIR" "$ARM64_TARGET" ;;
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
    *) echo "gpustat4cluster: unsupported architecture \$(uname -m)" >&2; exit 1 ;;
esac
SRC_DIR="/usr/lib/gpustat4cluster/\$ARCH/bin"
if [ ! -d "\$SRC_DIR" ]; then
    echo "gpustat4cluster: unsupported architecture \$ARCH; available binaries are under /usr/lib/gpustat4cluster" >&2
    exit 1
fi
install -d -m 0755 /usr/local/bin
case "\$ROLE" in
    server)
        ln -sfn "\$SRC_DIR/gpustat4cluster-server" /usr/local/bin/gpustat4cluster-server
        ;;
    client)
        ln -sfn "\$SRC_DIR/gpustat4cluster-client" /usr/local/bin/gpustat4cluster-client
        ln -sfn "\$SRC_DIR/gpustat4cluster-client-backend" /usr/local/bin/gpustat4cluster-client-backend
        ;;
    *)
        echo "gpustat4cluster: invalid role \$ROLE" >&2
        exit 1
        ;;
esac
SELECTOREOF
  chmod 0755 "$path"
}

stage_common_files() {
  local pkgroot="$1" role="$2"
  mkdir -p \
    "$pkgroot/etc/gpustat4cluster" \
    "$pkgroot/usr/lib/systemd/system" \
    "$pkgroot/usr/lib/gpustat4cluster"

  local role_config="$ROOT_DIR/dist/etc/gpustat4cluster/${role}.toml.example"
  [[ -f "$role_config" ]] || fail "missing role config: $role_config"
  cp "$role_config" "$pkgroot/etc/gpustat4cluster/${role}.toml.example"
  cp "$role_config" "$pkgroot/etc/gpustat4cluster/${role}.toml"
  cp "$ROOT_DIR/packaging/systemd/gpustat4cluster-${role}.service" \
    "$pkgroot/usr/lib/systemd/system/gpustat4cluster-${role}.service"
}

stage_role_files() {
  local pkgroot="$1" role="$2"
  stage_common_files "$pkgroot" "$role"

  if [[ "$MULTIARCH" == "1" ]]; then
    mkdir -p "$pkgroot/usr/lib/gpustat4cluster/amd64/bin" "$pkgroot/usr/lib/gpustat4cluster/arm64/bin"
    if [[ "$role" == "server" ]]; then
      cp "$(native_binary_path server)" "$pkgroot/usr/lib/gpustat4cluster/amd64/bin/gpustat4cluster-server"
      cp "$(arm64_binary_path server)" "$pkgroot/usr/lib/gpustat4cluster/arm64/bin/gpustat4cluster-server"
    else
      cp "$(native_binary_path gpustat4cluster-client)" "$pkgroot/usr/lib/gpustat4cluster/amd64/bin/gpustat4cluster-client"
      cp "$(native_binary_path gpustat4cluster-client-backend)" "$pkgroot/usr/lib/gpustat4cluster/amd64/bin/gpustat4cluster-client-backend"
      cp "$(arm64_binary_path gpustat4cluster-client)" "$pkgroot/usr/lib/gpustat4cluster/arm64/bin/gpustat4cluster-client"
      cp "$(arm64_binary_path gpustat4cluster-client-backend)" "$pkgroot/usr/lib/gpustat4cluster/arm64/bin/gpustat4cluster-client-backend"
    fi
    write_arch_selector "$pkgroot/usr/lib/gpustat4cluster/select-binary" "$role"
  else
    mkdir -p "$pkgroot/usr/local/bin"
    if [[ "$role" == "server" ]]; then
      cp "$(native_binary_path server)" "$pkgroot/usr/local/bin/gpustat4cluster-server"
    else
      cp "$(native_binary_path gpustat4cluster-client)" "$pkgroot/usr/local/bin/gpustat4cluster-client"
      cp "$(native_binary_path gpustat4cluster-client-backend)" "$pkgroot/usr/local/bin/gpustat4cluster-client-backend"
    fi
  fi
}

write_spec() {
  local spec="$1" role="$2" package="gpustat4cluster-${role}" summary="gpustat4cluster ${role} daemon"
  if [[ "$role" == "client" ]]; then
    summary="gpustat4cluster client backend and CLI"
  fi

  cat >"$spec" <<SPECEOF
Name:           $package
Version:        $VERSION
Release:        $REVISION%{?dist}
Summary:        $summary
License:        MIT
BuildArch:      noarch
Requires:       systemd

%description
gpustat4cluster provides low-latency GPU status collection and display across a cluster.

%prep

%build

%install
rm -rf %{buildroot}
cp -a %{_sourcedir}/root/. %{buildroot}/

%post
getent group gpustat4cluster >/dev/null 2>&1 || groupadd -r gpustat4cluster || true
id -u gpustat4cluster >/dev/null 2>&1 || useradd -r -g gpustat4cluster -d /var/lib/gpustat4cluster -s /sbin/nologin -c "gpustat4cluster service user" gpustat4cluster || true
install -d -o gpustat4cluster -g gpustat4cluster -m 0755 /var/lib/gpustat4cluster /var/log/gpustat4cluster /run/gpustat4cluster || true
if [ -x /usr/lib/gpustat4cluster/select-binary ]; then
    /usr/lib/gpustat4cluster/select-binary $role || true
fi
SPECEOF

  if [[ "$role" == "client" ]]; then
    cat >>"$spec" <<'SPECEOF'
if ! command -v gpustat >/dev/null 2>&1; then
    if [ ! -e /usr/local/bin/gpustat ] && [ ! -L /usr/local/bin/gpustat ]; then
        ln -s /usr/local/bin/gpustat4cluster-client /usr/local/bin/gpustat || true
    else
        echo "gpustat4cluster: warning: /usr/local/bin/gpustat already exists but is not runnable; leaving it untouched" >&2
    fi
fi
SPECEOF
  fi

  cat >>"$spec" <<SPECEOF
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    if [ "\${GPUSTAT4CLUSTER_RPM_START:-1}" = "1" ]; then
        systemctl enable --now gpustat4cluster-${role}.service || echo "gpustat4cluster: warning: failed to enable/start gpustat4cluster-${role}.service; check journalctl -u gpustat4cluster-${role}" >&2
    else
        systemctl enable gpustat4cluster-${role}.service || true
    fi
fi

%preun
if [ \$1 -eq 0 ] && command -v systemctl >/dev/null 2>&1; then
    systemctl disable --now gpustat4cluster-${role}.service >/dev/null 2>&1 || true
fi
SPECEOF

  if [[ "$role" == "client" ]]; then
    cat >>"$spec" <<'SPECEOF'
if [ $1 -eq 0 ] && [ "$(readlink /usr/local/bin/gpustat4cluster-client 2>/dev/null || true)" = "/usr/lib/gpustat4cluster/$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')/bin/gpustat4cluster-client" ]; then
    rm -f /usr/local/bin/gpustat4cluster-client
fi
if [ $1 -eq 0 ] && [ "$(readlink /usr/local/bin/gpustat4cluster-client-backend 2>/dev/null || true)" = "/usr/lib/gpustat4cluster/$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')/bin/gpustat4cluster-client-backend" ]; then
    rm -f /usr/local/bin/gpustat4cluster-client-backend
fi
if [ $1 -eq 0 ] && [ "$(readlink /usr/local/bin/gpustat 2>/dev/null || true)" = "/usr/local/bin/gpustat4cluster-client" ]; then
    rm -f /usr/local/bin/gpustat
fi
SPECEOF
  elif [[ "$role" == "server" ]]; then
    cat >>"$spec" <<'SPECEOF'
if [ $1 -eq 0 ] && [ "$(readlink /usr/local/bin/gpustat4cluster-server 2>/dev/null || true)" = "/usr/lib/gpustat4cluster/$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')/bin/gpustat4cluster-server" ]; then
    rm -f /usr/local/bin/gpustat4cluster-server
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
%config(noreplace) /etc/gpustat4cluster/${role}.toml
/etc/gpustat4cluster/${role}.toml.example
/usr/lib/systemd/system/gpustat4cluster-${role}.service
SPECEOF

  if [[ "$MULTIARCH" == "1" ]]; then
    cat >>"$spec" <<'SPECEOF'
/usr/lib/gpustat4cluster
SPECEOF
  else
    cat >>"$spec" <<SPECEOF
/usr/local/bin/gpustat4cluster-*
SPECEOF
  fi

  cat >>"$spec" <<SPECEOF

%changelog
* Thu Jan 01 1970 gpustat4cluster maintainers <root@localhost> - $VERSION-$REVISION
- Automated release package.
SPECEOF
}

package_role() {
  local role="$1" topdir="$TMP_DIR/rpmbuild-${role}" pkgroot="$topdir/SOURCES/root" spec="$topdir/SPECS/gpustat4cluster-${role}.spec"
  mkdir -p "$topdir/BUILD" "$topdir/BUILDROOT" "$topdir/RPMS" "$topdir/SOURCES" "$topdir/SPECS" "$topdir/SRPMS"
  stage_role_files "$pkgroot" "$role"
  find "$pkgroot" -type d -exec chmod 0755 {} +
  find "$pkgroot" -type f -exec chmod 0644 {} +
  find "$pkgroot" -path '*/bin/*' -type f -exec chmod 0755 {} +
  [[ -f "$pkgroot/usr/lib/gpustat4cluster/select-binary" ]] && chmod 0755 "$pkgroot/usr/lib/gpustat4cluster/select-binary"
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
