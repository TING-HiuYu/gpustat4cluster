#!/usr/bin/env bash
set -euo pipefail

ROLE=""
VERSION="latest"
LIBC="gnu"
DRY_RUN=0

PREFIX="${PREFIX:-/usr/local/bin}"
ETC_DIR="${ETC_DIR:-/etc/gpustat4cluster}"
SYSTEMD_DIR="${SYSTEMD_DIR:-/etc/systemd/system}"
TMP_DIR="${TMP_DIR:-/tmp/gpustat4cluster-install}"
REPO="${REPO:-TING-HiuYu/gpustat4cluster}"
LOCAL_TARBALL_DIR="${LOCAL_TARBALL_DIR:-}"
ROOT_DIR="${ROOT_DIR:-/}"
SERVICE_USER="${SERVICE_USER:-gpustat4cluster}"
SERVICE_GROUP="${SERVICE_GROUP:-gpustat4cluster}"

require_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "missing command: $1" >&2; exit 1; }; }

as_root() {
  if [[ "$(id -u)" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

usage() {
  cat <<USAGE
Usage: $0 [--role server|client|both] [--version TAG|latest] [--libc gnu|musl] [--dry-run]

Options:
  --role ROLE      Non-interactive mode, ROLE=server|client|both
  --version VER    Release tag (default: latest)
  --libc LIBC      gnu|musl (default: gnu)
  --dry-run        Print planned operations only
USAGE
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --role) ROLE="${2:-}"; shift 2 ;;
      --version) VERSION="${2:-}"; shift 2 ;;
      --libc) LIBC="${2:-}"; shift 2 ;;
      --dry-run) DRY_RUN=1; shift ;;
      -h|--help) usage; exit 0 ;;
      *) echo "Unknown argument: $1" >&2; usage; exit 1 ;;
    esac
  done
}

choose_role_interactive() {
  echo "Select role to install:"
  echo "  1) server"
  echo "  2) client"
  echo "  3) both"
  read -r -p "Enter choice [1-3]: " choice
  case "$choice" in
    1) ROLE="server" ;;
    2) ROLE="client" ;;
    3) ROLE="both" ;;
    *) echo "Invalid selection" >&2; exit 1 ;;
  esac
}

validate() {
  case "$ROLE" in server|client|both) ;; *) echo "Invalid role: $ROLE" >&2; exit 1;; esac
  case "$LIBC" in gnu|musl) ;; *) echo "Invalid libc: $LIBC" >&2; exit 1;; esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64) ARCH="amd64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
  esac
}

ensure_gnu_deps() {
  [[ "$LIBC" != "gnu" ]] && return 0
  local deps=(ca-certificates curl tar)
  if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update -y && sudo apt-get install -y "${deps[@]}"
  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y "${deps[@]}"
  elif command -v yum >/dev/null 2>&1; then
    sudo yum install -y "${deps[@]}"
  elif command -v zypper >/dev/null 2>&1; then
    sudo zypper --non-interactive install "${deps[@]}"
  else
    echo "Warning: no supported package manager found; ensure curl/tar/ca-certificates are installed." >&2
  fi
}

create_service_user() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] ensure system user/group: ${SERVICE_USER}:${SERVICE_GROUP}"
    return 0
  fi

  local nologin_shell="/usr/sbin/nologin"
  if [[ ! -x "$nologin_shell" ]]; then
    nologin_shell="/sbin/nologin"
  fi

  if ! getent group "$SERVICE_GROUP" >/dev/null 2>&1; then
    as_root groupadd --system "$SERVICE_GROUP"
  fi

  if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    as_root useradd \
      --system \
      --gid "$SERVICE_GROUP" \
      --home-dir /var/lib/gpustat4cluster \
      --create-home \
      --shell "$nologin_shell" \
      "$SERVICE_USER"
  fi
}

asset_name() {
  local role="$1"
  local ver="$2"
  echo "gpustat4cluster-${role}-${ver}-linux-${ARCH}-${LIBC}.tar.gz"
}

resolve_tag() {
  if [[ "$VERSION" == "latest" ]]; then
    local api="https://api.github.com/repos/${REPO}/releases/latest"
    TAG="$(curl -fsSL "$api" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
    [[ -z "$TAG" ]] && { echo "Failed to resolve latest release tag" >&2; exit 1; }
  else
    TAG="$VERSION"
  fi
}

install_one_tar() {
  local tarball="$1"
  mkdir -p "$TMP_DIR"
  local url="https://github.com/${REPO}/releases/download/${TAG}/${tarball}"
  local local_tar="$TMP_DIR/$tarball"

  if [[ -n "$LOCAL_TARBALL_DIR" && -f "$LOCAL_TARBALL_DIR/$tarball" ]]; then
    local_tar="$LOCAL_TARBALL_DIR/$tarball"
  fi

  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] download: $url"
    echo "[dry-run] extract to $ROOT_DIR"
    return 0
  fi

  if [[ ! -f "$local_tar" || "$local_tar" == "$TMP_DIR/$tarball" ]]; then
    echo "Downloading $url"
    curl -fL "$url" -o "$local_tar"
  else
    echo "Using local tarball: $local_tar"
  fi
  as_root mkdir -p "$ROOT_DIR" "$PREFIX" "$ETC_DIR" "$SYSTEMD_DIR"
  as_root tar -xzf "$local_tar" -C "$ROOT_DIR"
}

write_server_config() {
  local cfg="$ETC_DIR/server.toml"
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] init config: $cfg"
    return 0
  fi

  if [[ ! -f "$cfg" ]]; then
    as_root tee "$cfg" >/dev/null <<'EOF'
[connecting]
port_range = [30000, 40000]
multicast_addr = "239.0.0.1:4000"
udp_port = 0
tcp_port = 0
udp_mtu = 0
heartbeat_interval = 5
connection_idle_timeout = 10
max_connections = 64
discover_wait_secs = 5
multicast_retry_limit = 5
# Optional: one or more local IPv4 addresses used as multicast outbound interfaces.
# multicast_outbound_ip = ["192.0.2.10"]

[log]
max_size = "5mb"

[services]
cache_ttl_ms = 40
collector_interval_ms = 25
latency_display = true

[runtime]
# Leave unset to use the dynamic loader default: libnvidia-ml.so.
# Set this when the host only provides a versioned runtime library.
# nvml_lib_path = "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1"
EOF
  fi

  as_root chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "$ETC_DIR"
  as_root chmod 0755 "$ETC_DIR"
  as_root chmod 0644 "$cfg"
}

write_client_config() {
  local cfg="$ETC_DIR/client.toml"
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] init config: $cfg"
    return 0
  fi

  if [[ ! -f "$cfg" ]]; then
    as_root tee "$cfg" >/dev/null <<'EOF'
[connecting]
port_range = [30000, 40000]
multicast_addr = "239.0.0.1:4000"
protocol = "udp" # or "tcp"
udp_mtu = 0
heartbeat_interval = 5
connection_idle_timeout = 10
max_connections = 64
discover_wait_secs = 5
multicast_retry_limit = 5
# Optional: one or more local IPv4 addresses used as multicast outbound interfaces.
# multicast_outbound_ip = ["192.0.2.11"]

[log]
max_size = "5mb"

[services]
cache_ttl_ms = 40
collector_interval_ms = 25
latency_display = true
EOF
  fi

  as_root chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "$ETC_DIR"
  as_root chmod 0755 "$ETC_DIR"
  as_root chmod 0644 "$cfg"
}

write_default_config() {
  if [[ "$ROLE" == "server" || "$ROLE" == "both" ]]; then
    write_server_config
  fi
  if [[ "$ROLE" == "client" || "$ROLE" == "both" ]]; then
    write_client_config
  fi
}

install_systemd() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] systemctl daemon-reload"
    echo "[dry-run] enable/start services based on role"
    return 0
  fi

  if ! command -v systemctl >/dev/null 2>&1 || [[ "$(ps -p 1 -o comm=)" != "systemd" ]]; then
    echo "Warning: systemd is not active; skip daemon-reload/enable." >&2
    return 0
  fi

  as_root systemctl daemon-reload
  if [[ "$ROLE" == "server" || "$ROLE" == "both" ]]; then
    as_root systemctl enable --now gpustat4cluster-server.service || true
  fi
  if [[ "$ROLE" == "client" || "$ROLE" == "both" ]]; then
    as_root systemctl enable --now gpustat4cluster-client.service || true
  fi
}

main() {
  parse_args "$@"
  [[ -z "$ROLE" ]] && choose_role_interactive
  validate
  detect_arch
  require_cmd curl
  require_cmd tar
  if [[ "$DRY_RUN" -eq 0 && "$(id -u)" -ne 0 ]]; then
    require_cmd sudo
  fi

  if [[ "$DRY_RUN" -eq 0 ]]; then
    ensure_gnu_deps
  fi

  resolve_tag
  create_service_user

  if [[ "$ROLE" == "server" || "$ROLE" == "both" ]]; then
    install_one_tar "$(asset_name server "$TAG")"
  fi
  if [[ "$ROLE" == "client" || "$ROLE" == "both" ]]; then
    install_one_tar "$(asset_name client "$TAG")"
  fi

  write_default_config
  install_systemd
  echo "Install completed: role=$ROLE tag=$TAG arch=$ARCH libc=$LIBC"
}

main "$@"
