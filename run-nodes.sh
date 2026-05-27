#!/usr/bin/env bash
# run-nodes.sh — unified Caspar node runner
#
# Usage:
#   ./run-nodes.sh [single|triple] [OPTIONS]
#
# Modes:
#   single  — run only node1
#   triple  — run node1 + node2 + node3 (default)
#
# Options:
#   --no-docker      Run nodes as local binaries instead of docker containers.
#                    (Default: docker. Local mode supports triple-node without
#                    port or data-dir conflicts — each node has its own ports.)
#   --no-questdb     Skip QuestDB startup (manage it separately)
#   --fresh          Wipe /tmp/caspar/* before starting (clean-slate run)
#   --no-gvisor      Skip gVisor (runsc) install / configuration. By default
#                    gVisor is installed and registered with Docker so all
#                    caspar VMs run sandboxed.
#   --no-firecracker Skip Firecracker install and network setup. By default
#                    Firecracker is installed and its host bridge is configured
#                    so microVM-backed workloads can run immediately.
#   --rebuild        Force rebuild even if binaries / image already exist.
#                    Docker mode: re-runs build-dist.sh + docker build.
#                    Local mode:  re-runs cargo build --release.
#                    (alias: --rebuild-image)
#   --foreground     (docker mode) keep tailing container logs until Ctrl-C
#                    instead of returning immediately
#   --skip-deploy    Skip WASM creature build & deployment. By default
#                    run-nodes.sh clones decillionai-server (if absent), builds
#                    all WASM creatures with TinyGo, and deploys them so the
#                    cluster is fully ready before the script exits.
#   --help           show this help
#
# Companion script:
#   ./stop-nodes.sh  — gracefully shuts down everything started by this script

set -euo pipefail

# ─── PATH: native Linux docker locations first ───────────────────────────────
# Put native Linux paths BEFORE the Windows-mounted paths that WSL appends.
# This prevents Windows Docker Desktop's docker.exe (reachable via /mnt/c/...)
# from being used instead of a native Docker CE install.
export PATH="/snap/bin:/usr/local/bin:/usr/bin:/usr/sbin:${PATH:-}"

# _native_docker: returns true only if a native Linux docker binary exists.
# Rejects .exe wrappers and Windows-mount paths (/mnt/...).
_native_docker_exists() {
  local p
  p=$(command -v docker 2>/dev/null) || return 1
  # Reject Windows-mount paths and .exe wrappers
  [[ "$p" == /mnt/* ]] && return 1
  [[ "$p" == *.exe  ]] && return 1
  return 0
}

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE_DIR="$REPO_DIR/node"
BINARY="$NODE_DIR/target/release/caspar-node"
DATA_ROOT="/tmp/caspar"
# Use the pre-built jar from dist/ if /opt/questdb/questdb.jar is absent
QUESTDB_JAR="${QUESTDB_JAR:-}"
[[ -z "$QUESTDB_JAR" ]] && [[ -f "/opt/questdb/questdb.jar" ]] && QUESTDB_JAR="/opt/questdb/questdb.jar"
[[ -z "$QUESTDB_JAR" ]] && [[ -f "$REPO_DIR/dist/questdb/questdb.jar" ]] && QUESTDB_JAR="$REPO_DIR/dist/questdb/questdb.jar"
[[ -z "$QUESTDB_JAR" ]] && QUESTDB_JAR="/opt/questdb/questdb.jar"  # keep original as default for download logic
QUESTDB_DATA="$DATA_ROOT/questdb"
QUESTDB_PORT=8812
DOCKER_IMAGE="caspar-node:latest"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
info()  { echo -e "${CYAN}[caspar]${NC} $*"; }
ok()    { echo -e "${GREEN}[caspar]${NC} $*"; }
warn()  { echo -e "${YELLOW}[caspar]${NC} $*"; }
die()   { echo -e "${RED}[caspar] FATAL:${NC} $*" >&2; exit 1; }

# ─── Environment detection ────────────────────────────────────────────────────
# Detect WSL: /proc/version contains "Microsoft" or "WSL"
_is_wsl() { grep -qi 'microsoft\|wsl' /proc/version 2>/dev/null; }
# Detect systemd as PID 1 (works bare-metal AND WSL2 with systemd enabled)
_has_systemd() { [[ "$(ps -p 1 -o comm= 2>/dev/null)" == "systemd" ]]; }
# Return the active package manager token: apt | dnf | yum | pacman | unknown
_pkg_mgr() {
  command -v apt-get &>/dev/null && { echo apt;    return; }
  command -v dnf     &>/dev/null && { echo dnf;    return; }
  command -v yum     &>/dev/null && { echo yum;    return; }
  command -v pacman  &>/dev/null && { echo pacman; return; }
  echo unknown
}
# Normalise uname -m → Docker/apt arch token (amd64 / arm64 / armhf)
_arch() {
  case "$(uname -m)" in
    x86_64)        echo amd64 ;;
    aarch64|arm64) echo arm64 ;;
    armv7l)        echo armhf ;;
    *)             uname -m   ;;
  esac
}
# Install packages via the appropriate manager (Debian/Ubuntu, RHEL/Fedora, Arch)
_pkg_install() {
  case "$(_pkg_mgr)" in
    apt)    apt-get install -y -qq "$@" ;;
    dnf)    dnf     install -y -q  "$@" ;;
    yum)    yum     install -y -q  "$@" ;;
    pacman) pacman  -S --noconfirm "$@" ;;
    *)      warn "Unknown package manager — install manually: $*"; return 1 ;;
  esac
}
_pkg_update() {
  case "$(_pkg_mgr)" in
    apt)    apt-get update -qq ;;
    dnf|yum) : ;;  # dnf/yum update on demand; skip explicit refresh
    pacman) pacman -Sy --noconfirm ;;
    *) : ;;
  esac
}

# ─── Arg parsing ─────────────────────────────────────────────────────────────
MODE="triple"
USE_DOCKER=true
START_QUESTDB=true
FRESH=false
SETUP_GVISOR=true
SETUP_FIRECRACKER=true
REBUILD_IMAGE=false
FOREGROUND=false
DEPLOY_CREATURES=true

for arg in "$@"; do
  case "$arg" in
    single)            MODE="single" ;;
    triple)            MODE="triple" ;;
    --no-docker)       USE_DOCKER=false ;;
    --no-questdb)      START_QUESTDB=false ;;
    --fresh)           FRESH=true ;;
    --no-gvisor)       SETUP_GVISOR=false ;;
    --no-firecracker)  SETUP_FIRECRACKER=false ;;
    --rebuild|--rebuild-image) REBUILD_IMAGE=true ;;
    --foreground)      FOREGROUND=true ;;
    --skip-deploy)     DEPLOY_CREATURES=false ;;
    --help|-h)
      sed -n '2,34p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "Unknown argument: $arg" ;;
  esac
done

NODES=(1)
[[ "$MODE" == "triple" ]] && NODES=(1 2 3)

declare -A NODE_TCP=([1]=8074 [2]=8174 [3]=8274)

# ─── Sudo pre-check ──────────────────────────────────────────────────────────
# If Docker is not yet installed we will need root. Validate sudo credentials
# NOW (interactively) so subsequent _as_root calls never hang waiting for a
# password on a background/piped invocation.
if $USE_DOCKER && ! _native_docker_exists; then
  if [[ "$EUID" -ne 0 ]] && command -v sudo &>/dev/null; then
    echo -e "${CYAN}[caspar]${NC} Docker CE not installed — root access is needed."
    echo -e "${CYAN}[caspar]${NC} Please enter your sudo password when prompted:"
    sudo -v || die "sudo authentication failed. Run 'sudo -v' first, then re-run this script."
    # Keep sudo alive in the background for the duration of the script
    ( while true; do sudo -n true; sleep 50; done ) &
    _SUDO_KEEPALIVE_PID=$!
    trap 'kill $_SUDO_KEEPALIVE_PID 2>/dev/null' EXIT
    ok "sudo credentials cached"
  fi
fi

# ─── Helper: run a function body as root ─────────────────────────────────────
# Usage: _as_root <function_name>
# Calls the named function directly if already root, otherwise exports its
# definition and re-invokes it through sudo bash.  Colour helper functions
# (info/ok/warn) are exported alongside so output remains consistent.
_as_root() {
  local fn="$1"
  if [[ "$EUID" -eq 0 ]]; then
    "$fn"
    return
  fi
  command -v sudo &>/dev/null || { warn "Cannot run $fn: need root and sudo is not available."; return 1; }

  # Write all function definitions to a temp file so heredocs (e.g. <<'PYEOF'
  # in _setup_gvisor) are preserved correctly — `bash -c "..."` misparses
  # heredocs embedded inside a string and silently drops later definitions.
  local tmpscript
  tmpscript=$(mktemp /tmp/caspar_root_XXXXXX.sh)
  # shellcheck disable=SC2064
  trap "rm -f '$tmpscript'" RETURN

  # Dump every currently-defined function, then call the requested one.
  declare -f              >> "$tmpscript"
  echo "${fn}"            >> "$tmpscript"
  chmod 700 "$tmpscript"

  sudo bash "$tmpscript"
}

# ─── gVisor setup ─────────────────────────────────────────────────────────────
#
# Snap-docker compatibility notes:
#   • Snap docker reads its daemon.json from /var/snap/docker/<rev>/config/daemon.json
#     NOT from /etc/docker/daemon.json — we detect this and write to the right path.
#   • Snap docker uses strict confinement: external runtime binaries (runsc) may be
#     blocked unless the snap has the right interfaces. We register runsc and warn if
#     confinement might prevent execution. For full gVisor support, Docker CE (apt)
#     is recommended over snap docker.
#   • Restart is done via `snap restart docker` instead of `systemctl restart docker`.
_setup_gvisor() {
  set -e
  info "Installing gVisor (runsc)…"

  if [[ "$(_pkg_mgr)" == apt ]]; then
    # ── Debian / Ubuntu path (official apt repo) ──────────────────────────────
    _pkg_update
    _pkg_install apt-transport-https ca-certificates curl gnupg

    local keyring="/usr/share/keyrings/gvisor-archive-keyring.gpg"
    [[ -f "$keyring" ]] \
      || curl -fsSL https://gvisor.dev/archive.key | gpg --dearmor --yes -o "$keyring"

    local arch; arch=$(_arch)
    echo "deb [arch=${arch} signed-by=${keyring}] https://storage.googleapis.com/gvisor/releases release main" \
      > /etc/apt/sources.list.d/gvisor.list

    _pkg_update
    _pkg_install runsc
  else
    # ── Non-Debian: direct binary download from gVisor release CDN ───────────
    local hw_arch; hw_arch=$(uname -m)   # x86_64 or aarch64
    local runsc_url="https://storage.googleapis.com/gvisor/releases/release/latest/${hw_arch}/runsc"
    info "Non-apt system — downloading runsc binary for ${hw_arch}…"
    curl -fsSL "$runsc_url" -o /usr/local/bin/runsc
    chmod +x /usr/local/bin/runsc
    ok "runsc installed: $(runsc --version 2>&1 | head -1)"
  fi

  # ── Detect whether Docker is snap-based or CE (apt) ─────────────────────────
  local daemon_json snap_rev snap_config_dir
  snap_rev=$(snap list docker 2>/dev/null | awk 'NR>1{print $3}' | head -1)
  if [[ -n "$snap_rev" ]]; then
    # Snap docker: config lives inside the snap revision directory
    snap_config_dir="/var/snap/docker/${snap_rev}/config"
    daemon_json="${snap_config_dir}/daemon.json"
    mkdir -p "$snap_config_dir"
    warn "Snap docker detected (rev ${snap_rev}) — writing runtime to ${daemon_json}"
    warn "Note: snap strict confinement may prevent runsc execution."
    warn "For full gVisor support, install Docker CE via apt instead of snap."
  else
    # Docker CE (apt) — standard path
    daemon_json="/etc/docker/daemon.json"
    mkdir -p /etc/docker
  fi

  # ── Merge runsc runtime into the correct daemon.json ────────────────────────
  # --network=host: sandbox shares host netstack (no NAT, full reachability)
  # --platform=ptrace: works without /dev/kvm
  python3 - "$daemon_json" <<'PYEOF'
import json, os, sys, tempfile
path = sys.argv[1]
try:
    cfg = json.loads(open(path).read().strip() or '{}')
except FileNotFoundError:
    cfg = {}
cfg.setdefault('runtimes', {})['runsc'] = {
    'path': 'runsc',
    'runtimeArgs': ['--network=host', '--platform=ptrace'],
}
cfg_dir = os.path.dirname(path)
fd, tmp = tempfile.mkstemp(dir=cfg_dir, prefix='.daemon.')
with os.fdopen(fd, 'w') as f:
    json.dump(cfg, f, indent=2); f.write('\n')
os.replace(tmp, path)
print(f'  wrote {path}')
PYEOF

  # ── Restart Docker and wait for daemon to come back ─────────────────────────
  if [[ -n "${snap_rev:-}" ]]; then
    # Snap docker restart
    snap restart docker 2>/dev/null \
      && { local i; for i in $(seq 1 20); do docker info >/dev/null 2>&1 && break; sleep 0.5; done; } \
      || warn "snap restart docker failed — gVisor runtime registered but Docker not reloaded"
  elif systemctl is-active --quiet docker 2>/dev/null; then
    systemctl restart docker
    local i
    for i in $(seq 1 20); do docker info >/dev/null 2>&1 && break; sleep 0.5; done
  fi

  ok "gVisor (runsc) installed and registered with Docker (${daemon_json})"
}

# ─── WSL2 DNS fix (run as root) ──────────────────────────────────────────────
# WSL2's auto-generated /etc/resolv.conf uses 10.255.255.254 as a virtual DNS
# relay. On some corporate / VPN networks this relay fails to resolve external
# hostnames. This function permanently switches to 8.8.8.8 / 1.1.1.1 by
# disabling WSL's auto-generation and writing a static resolv.conf.
_fix_wsl_dns() {
  set -e
  # Only run inside WSL
  grep -qi 'microsoft\|wsl' /proc/version 2>/dev/null || return 0

  local current_ns
  current_ns=$(awk '/^nameserver/{print $2; exit}' /etc/resolv.conf 2>/dev/null)
  # If already using a non-relay DNS, skip
  [[ "$current_ns" != "10.255.255.254" ]] && [[ "$current_ns" != "172.16."* ]] \
    && [[ -n "${current_ns:-}" ]] && { ok "WSL2 DNS already set to $current_ns — skipping fix"; return 0; }

  # Disable WSL auto-generation of resolv.conf
  local wsl_conf="/etc/wsl.conf"
  if ! grep -q 'generateResolvConf' "$wsl_conf" 2>/dev/null; then
    {
      grep -v 'generateResolvConf' "$wsl_conf" 2>/dev/null || true
      printf '\n[network]\ngenerateResolvConf = false\n'
    } > /tmp/_wsl_conf_new
    mv /tmp/_wsl_conf_new "$wsl_conf"
  else
    sed -i 's/generateResolvConf *= *true/generateResolvConf = false/' "$wsl_conf"
  fi

  # Unlink if it's a symlink (WSL manages it as one)
  [[ -L /etc/resolv.conf ]] && rm /etc/resolv.conf

  # Write static resolv.conf
  printf '# Static DNS set by run-nodes.sh (overrides broken WSL relay)\nnameserver 8.8.8.8\nnameserver 1.1.1.1\n' \
    > /etc/resolv.conf

  ok "WSL2 DNS fixed: /etc/resolv.conf → 8.8.8.8 / 1.1.1.1"
}

# ─── Docker CE auto-installation ─────────────────────────────────────────────
# Called (as root) when USE_DOCKER=true but docker is not in PATH.
# Supports Debian/Ubuntu (apt) and RHEL/Fedora/Amazon (dnf/yum).
# Also handles WSL2 environments where systemd may not be running.
_install_docker() {
  set -e
  info "Docker not found — installing Docker CE…"

  # ── Fix WSL2 DNS first so apt-get can reach download.docker.com ─────────────
  _fix_wsl_dns

  # ── Detect package manager ──────────────────────────────────────────────────
  if command -v apt-get &>/dev/null; then
    # Debian / Ubuntu / Raspbian
    apt-get update -qq
    apt-get install -y -qq \
      apt-transport-https ca-certificates curl gnupg lsb-release

    # Load /etc/os-release for $ID
    . /etc/os-release
    local keyring="/usr/share/keyrings/docker-archive-keyring.gpg"
    [[ -f "$keyring" ]] \
      || curl -fsSL "https://download.docker.com/linux/${ID}/gpg" \
           | gpg --dearmor --yes -o "$keyring"

    local arch; arch=$(dpkg --print-architecture)
    local codename; codename=$(lsb_release -cs)
    echo "deb [arch=${arch} signed-by=${keyring}] \
https://download.docker.com/linux/${ID} ${codename} stable" \
      > /etc/apt/sources.list.d/docker.list

    apt-get update -qq
    apt-get install -y -qq \
      docker-ce docker-ce-cli containerd.io \
      docker-buildx-plugin docker-compose-plugin

  elif command -v dnf &>/dev/null || command -v yum &>/dev/null; then
    # RHEL / Fedora / Amazon Linux
    local pkg_mgr; command -v dnf &>/dev/null && pkg_mgr=dnf || pkg_mgr=yum
    $pkg_mgr install -y -q yum-utils
    yum-config-manager --add-repo \
      https://download.docker.com/linux/centos/docker-ce.repo
    $pkg_mgr install -y -q \
      docker-ce docker-ce-cli containerd.io \
      docker-buildx-plugin docker-compose-plugin
  else
    die "Unsupported package manager — install Docker manually: https://docs.docker.com/engine/install/"
  fi

  # ── WSL2: switch to iptables-legacy so Docker CE daemon can start ───────────
  # Ubuntu 22.04/24.04 defaults to nftables; dockerd requires iptables-legacy
  # to configure bridge NAT rules inside WSL2 where nftables isn't available.
  if _is_wsl && command -v update-alternatives &>/dev/null; then
    update-alternatives --set iptables  /usr/sbin/iptables-legacy  >/dev/null 2>&1 || true
    update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy >/dev/null 2>&1 || true
    ok "Configured iptables-legacy for Docker CE on WSL2"
  fi

  # ── Start the daemon ────────────────────────────────────────────────────────
  # systemd path (bare-metal, LXC, WSL2 with systemd enabled)
  if systemctl is-system-running 2>/dev/null | grep -qE 'running|degraded|starting'; then
    systemctl enable docker --quiet 2>/dev/null || true
    systemctl start  docker         2>/dev/null || true
    local i; for i in $(seq 1 30); do docker info >/dev/null 2>&1 && break; sleep 1; done
  else
    # No systemd (plain WSL2, containers, CI) — launch dockerd directly
    if ! docker info >/dev/null 2>&1; then
      nohup dockerd --host=unix:///var/run/docker.sock \
            > /tmp/dockerd.log 2>&1 &
      local i; for i in $(seq 1 40); do docker info >/dev/null 2>&1 && break; sleep 1; done
    fi
  fi

  # ── Allow the invoking (non-root) user to run docker ────────────────────────
  if [[ -n "${SUDO_USER:-}" ]]; then
    usermod -aG docker "$SUDO_USER" 2>/dev/null || true
  fi

  docker info >/dev/null 2>&1 || die "Docker daemon still not reachable after installation"
  ok "Docker CE installed and running: $(docker --version)"
}

# ─── Snap → Docker CE migration ──────────────────────────────────────────────
# Removes snap docker and installs Docker CE (apt) in its place.
# Required for full gVisor (runsc) support — snap's strict confinement blocks
# external runtime binaries and uses a non-standard daemon.json location.
_migrate_snap_to_docker_ce() {
  set -e
  info "Removing snap docker and installing Docker CE (apt)…"

  # Stop and remove snap docker
  snap stop docker 2>/dev/null || true
  snap remove docker 2>/dev/null || true
  # Remove leftover snap socket/pid files
  rm -f /run/snap.docker/docker.sock /run/snap.docker/docker.pid 2>/dev/null || true

  # Install Docker CE via the official apt repo (reuses _install_docker logic)
  _install_docker

  ok "Migrated from snap docker to Docker CE"
}

# ─── Firecracker setup ────────────────────────────────────────────────────────
FC_VERSION="v1.10.1"
FC_ARCH=$(uname -m)   # x86_64 or aarch64

_install_firecracker() {
  set -e
  local fc_version="${FC_VERSION:-v1.10.1}"
  local fc_arch="${FC_ARCH:-$(uname -m)}"

  info "Installing Firecracker ${fc_version} (${fc_arch})…"
  _pkg_update
  case "$(_pkg_mgr)" in
    apt)    _pkg_install curl libelf-dev e2fsprogs ;;
    dnf)    _pkg_install curl elfutils-libelf-devel e2fsprogs ;;
    yum)    _pkg_install curl elfutils-libelf-devel e2fsprogs ;;
    pacman) _pkg_install curl libelf e2fsprogs ;;
    *)      warn "Cannot auto-install Firecracker deps — ensure curl + e2fsprogs are present" ;;
  esac

  mkdir -p /opt/firecracker/{vms,kernel,rootfs,snapshots}

  # Binary
  if ! command -v firecracker &>/dev/null; then
    local tgz="firecracker-${fc_version}-${fc_arch}.tgz"
    curl -fsSL \
      "https://github.com/firecracker-microvm/firecracker/releases/download/${fc_version}/${tgz}" \
      -o "/tmp/${tgz}"
    tar -xzf "/tmp/${tgz}" -C /tmp
    mv "/tmp/release-${fc_version}-${fc_arch}/firecracker-${fc_version}-${fc_arch}" \
       /usr/local/bin/firecracker
    chmod +x /usr/local/bin/firecracker
    rm -rf "/tmp/${tgz}" "/tmp/release-${fc_version}-${fc_arch}"
    ok "Installed: $(firecracker --version 2>&1 | head -1)"
  else
    ok "Firecracker binary already present: $(firecracker --version 2>&1 | head -1)"
  fi

  # Guest kernel
  if [[ ! -f /opt/firecracker/kernel/vmlinux ]]; then
    info "Downloading guest kernel for ${fc_arch}…"
    curl -fsSL \
      "https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/${fc_arch}/kernels/vmlinux.bin" \
      -o /opt/firecracker/kernel/vmlinux
    chmod +x /opt/firecracker/kernel/vmlinux
    ok "Guest kernel ready ($(ls -lh /opt/firecracker/kernel/vmlinux | awk '{print $5}'))"
  else
    ok "Guest kernel already present"
  fi

  # Guest rootfs (Alpine-based ext4 image)
  # Requires loop device support — may not be available in WSL2 without a custom kernel.
  if [[ ! -f /opt/firecracker/rootfs/rootfs.ext4 ]]; then
    # WSL2 guard: check for usable loop devices before attempting mount
    if _is_wsl && ! ls /dev/loop* &>/dev/null 2>&1; then
      warn "WSL detected: no /dev/loop* devices found — skipping rootfs build."
      warn "To enable loop devices in WSL2, add to /etc/wsl.conf:"
      warn "  [boot]"
      warn "  command = modprobe loop"
      warn "Firecracker binary + kernel are installed; rootfs must be provided manually."
    else
      info "Building guest rootfs (Alpine 3.20 / ${fc_arch})…"
      _pkg_install e2fsprogs 2>/dev/null || true
      local alpine_url="https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/${fc_arch}/alpine-minirootfs-3.20.0-${fc_arch}.tar.gz"
      curl -fsSL "$alpine_url" -o /tmp/alpine-minirootfs.tar.gz
      dd if=/dev/zero of=/opt/firecracker/rootfs/rootfs.ext4 bs=1M count=128 status=none
      mkfs.ext4 -q /opt/firecracker/rootfs/rootfs.ext4
      local mnt; mnt=$(mktemp -d)
      mount -o loop /opt/firecracker/rootfs/rootfs.ext4 "$mnt"
      tar -xzf /tmp/alpine-minirootfs.tar.gz -C "$mnt"
      printf '#!/bin/sh\nmount -t proc proc /proc\nmount -t sysfs sysfs /sys\nmount -t devtmpfs devtmpfs /dev 2>/dev/null||true\nexec /bin/sh\n' \
        > "$mnt/sbin/init"
      chmod +x "$mnt/sbin/init"
      umount "$mnt"; rmdir "$mnt"
      rm -f /tmp/alpine-minirootfs.tar.gz
      ok "Guest rootfs ready ($(ls -lh /opt/firecracker/rootfs/rootfs.ext4 | awk '{print $5}'))"
    fi
  else
    ok "Guest rootfs already present"
  fi

  [[ -e /dev/kvm ]] \
    && ok "/dev/kvm available — hardware-accelerated microVMs enabled" \
    || warn "/dev/kvm not available — Firecracker will use ptrace platform (slower cold-start)"
}

_setup_firecracker_network() {
  set -e
  local bridge="br0"
  local bridge_cidr="172.16.0.1/24"
  local host_iface
  host_iface=$(ip route show default 2>/dev/null | awk '/default/{print $5; exit}')

  # WSL2: restricted networking — bridge and iptables may not fully work.
  # We attempt best-effort but never hard-fail.
  if _is_wsl; then
    warn "WSL detected: bridge networking / iptables rules may be restricted."
    warn "Firecracker microVM networking may not work inside WSL2 without a custom kernel."
  fi

  for tool in ip iptables sysctl; do
    command -v "$tool" &>/dev/null || { warn "$tool not found; skipping Firecracker network setup"; return 1; }
  done

  if ! ip link show "$bridge" &>/dev/null; then
    ip link add name "$bridge" type bridge 2>/dev/null \
      || { warn "Could not create bridge $bridge (WSL restriction?) — skipping"; return 0; }
    ip addr add "$bridge_cidr" dev "$bridge" 2>/dev/null || true
    ip link set "$bridge" up 2>/dev/null || true
  fi

  if [[ -n "$host_iface" ]]; then
    iptables -t nat -C POSTROUTING -o "$host_iface" -j MASQUERADE 2>/dev/null \
      || iptables -t nat -A POSTROUTING -o "$host_iface" -j MASQUERADE 2>/dev/null || true
    iptables -C FORWARD -i "$bridge" -o "$host_iface" -j ACCEPT 2>/dev/null \
      || iptables -A FORWARD -i "$bridge" -o "$host_iface" -j ACCEPT 2>/dev/null || true
    iptables -C FORWARD -i "$host_iface" -o "$bridge" \
        -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null \
      || iptables -A FORWARD -i "$host_iface" -o "$bridge" \
           -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || true
  fi

  # ip_forward: read-only in WSL2 — attempt but don't fail
  if [[ "$(cat /proc/sys/net/ipv4/ip_forward 2>/dev/null)" != "1" ]]; then
    sysctl -qw net.ipv4.ip_forward=1 2>/dev/null \
      || warn "Could not enable ip_forward (read-only in WSL?) — microVM routing may not work"
  fi
}

# ─── Docker daemon DNS auto-fix (Docker CE only) ─────────────────────────────
# When Docker CE's daemon has DNS issues (resolv.conf / daemon.json not set up),
# this writes the daemon DNS config and reloads dockerd.
# For Docker Desktop, fix your host's DNS instead (see README).

_fix_docker_ce_dns() {
  local daemon_json="/etc/docker/daemon.json"
  mkdir -p /etc/docker
  python3 - "$daemon_json" <<'PYEOF'
import json, sys, os, tempfile
path = sys.argv[1]
try:
    cfg = json.loads(open(path).read().strip() or '{}')
except (FileNotFoundError, json.JSONDecodeError):
    cfg = {}
cfg['dns'] = ['8.8.8.8', '1.1.1.1']
d = os.path.dirname(path) or '.'
fd, tmp = tempfile.mkstemp(dir=d, prefix='.daemon.')
with os.fdopen(fd, 'w') as f:
    json.dump(cfg, f, indent=2); f.write('\n')
os.replace(tmp, path)
print(f'Updated {path}')
PYEOF
  if systemctl is-active --quiet docker 2>/dev/null; then
    systemctl reload docker 2>/dev/null || systemctl restart docker 2>/dev/null || true
  elif [[ -f /var/run/docker.pid ]]; then
    kill -HUP "$(cat /var/run/docker.pid)" 2>/dev/null || true
  fi
  sleep 2
}

# Verify Docker daemon can reach Docker Hub; offer DNS fix hint if not.
_ensure_docker_dns() {
  $USE_DOCKER || return 0

  local probe_out
  probe_out=$(docker pull hello-world:latest 2>&1) && {
    docker rmi hello-world:latest >/dev/null 2>&1 || true
    return 0
  }

  # Only act on clear DNS errors; skip auth / rate-limit errors
  if echo "$probe_out" | grep -qiE 'no such host|lookup .+ on .+:[0-9]+|dial tcp.*i/o timeout'; then
    warn "Docker daemon DNS issue detected."
    warn "Fix: ensure /etc/resolv.conf has a working nameserver (e.g. 8.8.8.8) and retry."
    # Auto-fix for Docker CE (not Docker Desktop — fix host DNS instead)
    if ! _is_wsl || command -v dockerd &>/dev/null; then
      warn "Attempting Docker CE daemon DNS fix…"
      _as_root _fix_docker_ce_dns
    fi
  else
    # Non-DNS error (e.g. TLS, auth) — show once and continue
    info "Docker registry test: $(echo "$probe_out" | tail -1)"
  fi
}

# ─── TinyGo + Go 1.23 installation (runs as root) ────────────────────────────
# TinyGo is the WASM compiler for Caspar creature modules.
# TinyGo ≤ 0.34 requires Go ≤ 1.23, so we install Go 1.23 to /usr/local/go123
# and symlink tinygo to /usr/local/bin so it is in every user's PATH.
TINYGO_VERSION="0.34.0"

_install_tinygo() {
  set -e
  command -v tinygo &>/dev/null && {
    ok "TinyGo already installed: $(tinygo version 2>&1 | head -1)"
    return 0
  }

  info "Installing TinyGo ${TINYGO_VERSION}…"
  local hw_arch; hw_arch=$(uname -m)
  local tg_arch
  case "$hw_arch" in
    x86_64)        tg_arch="amd64" ;;
    aarch64|arm64) tg_arch="arm64" ;;
    *) warn "TinyGo: unsupported arch ${hw_arch} — skipping"; return 0 ;;
  esac

  _pkg_update; _pkg_install curl tar ca-certificates

  # ── Go 1.23 (required by TinyGo ≤ 0.34) ─────────────────────────────────
  if [[ ! -x "/usr/local/go123/bin/go" ]]; then
    info "Installing Go 1.23 (required by TinyGo ≤ 0.34)…"
    local go_tgz="go1.23.9.linux-${tg_arch}.tar.gz"
    curl -fsSL "https://go.dev/dl/${go_tgz}" -o "/tmp/${go_tgz}"
    tar -xzf "/tmp/${go_tgz}" -C /tmp
    mv /tmp/go /usr/local/go123
    rm -f "/tmp/${go_tgz}"
    ok "Go 1.23 installed at /usr/local/go123"
  else
    ok "Go 1.23 already present: $(/usr/local/go123/bin/go version)"
  fi

  # ── TinyGo binary ─────────────────────────────────────────────────────────
  local tg_tgz="tinygo${TINYGO_VERSION}.linux-${tg_arch}.tar.gz"
  curl -fsSL \
    "https://github.com/tinygo-org/tinygo/releases/download/v${TINYGO_VERSION}/${tg_tgz}" \
    -o "/tmp/${tg_tgz}"
  tar -xzf "/tmp/${tg_tgz}" -C /usr/local
  rm -f "/tmp/${tg_tgz}"
  ln -sf /usr/local/tinygo/bin/tinygo /usr/local/bin/tinygo

  ok "TinyGo installed: $(GOROOT=/usr/local/go123 /usr/local/bin/tinygo version 2>&1 | head -1)"
}

# ─── Python creature-deploy dependencies (runs as root) ──────────────────────
_install_python_deps() {
  python3 -c "from Crypto.PublicKey import RSA" 2>/dev/null && return 0
  info "Installing pycryptodome…"
  pip3 install pycryptodome --quiet \
    || warn "pycryptodome install failed — creature deployment may not work"
}

# ─── Clone / verify decillionai-server repo ───────────────────────────────────
# Sets global DECILLIONAI_SERVER_DIR.  Returns 1 on failure.
DECILLIONAI_SERVER_DIR=""
_ensure_decillionai_server() {
  local server_dir; server_dir="$(dirname "$REPO_DIR")/decillionai-server"
  if [[ ! -d "$server_dir/.git" ]]; then
    info "Cloning decillionai-server…"
    git clone --depth=1 \
      https://github.com/DecillionAI/decillionai-server.git \
      "$server_dir" 2>&1 \
      || { warn "git clone failed — skipping creature deployment"; return 1; }
    ok "Cloned decillionai-server → $server_dir"
  else
    ok "decillionai-server present: $server_dir"
  fi
  DECILLIONAI_SERVER_DIR="$server_dir"
}

# ─── Build WASM creatures via decillionai-server/build-all.sh ────────────────
_build_creatures() {
  local server_dir="$1"
  local wasm_dir="$server_dir/wasm"
  local wasm_count; wasm_count=$(find "$wasm_dir" -name "*.wasm" 2>/dev/null | wc -l)
  if [[ $wasm_count -ge 6 ]]; then
    ok "WASM creatures already built (${wasm_count} .wasm files)"
    return 0
  fi
  [[ -f "$server_dir/build-all.sh" ]] \
    || { warn "build-all.sh not found in $server_dir — skipping build"; return 1; }
  info "Building WASM creatures via build-all.sh (~2-3 min)…"
  export PATH="/usr/local/go123/bin:/usr/local/tinygo/bin:${PATH}"
  export GOROOT="/usr/local/go123"
  bash "$server_dir/build-all.sh" \
    || { warn "build-all.sh failed — creature deployment may be incomplete"; return 1; }
  ok "WASM creatures built: $(find "$wasm_dir" -name '*.wasm' 2>/dev/null | wc -l) files"
}

# ─── Application-level readiness probe ───────────────────────────────────────
# TCP-open is necessary but not sufficient; wait until the node actually
# responds to a /auths/getServerPublicKey request before deploying.
_probe_node_app_ready() {
  # $1 = TCP port.  Returns 0 when the node responds, 1 on timeout/error.
  local port="$1"
  python3 -c "
import sys, socket, struct, uuid
port = int('$port')
def lp(x): b=x.encode(); return struct.pack('>I',len(b))+b
pkt = str(uuid.uuid4())
body = lp('') + lp('') + lp('/auths/getServerPublicKey') + lp(pkt) + b'{}'
frame = struct.pack('>I',len(body)) + body
s = socket.socket(); s.settimeout(5)
try:
    s.connect(('127.0.0.1', port))
    s.sendall(frame)
    hdr = b''
    while len(hdr) < 4:
        c = s.recv(4 - len(hdr))
        if not c: sys.exit(1)
        hdr += c
    length = struct.unpack('>I', hdr)[0]
    data = b''
    while len(data) < length:
        c = s.recv(length - len(data))
        if not c: sys.exit(1)
        data += c
    sys.exit(0 if length > 0 else 1)
except Exception:
    sys.exit(1)
finally:
    try: s.close()
    except: pass
" 2>/dev/null
}

_wait_nodes_app_ready() {
  local max_wait=180
  info "Waiting for node(s) to be application-ready (up to ${max_wait}s)…"
  for n in "${NODES[@]}"; do
    local port=${NODE_TCP[$n]}
    local elapsed=0
    while ! _probe_node_app_ready "$port"; do
      sleep 2; elapsed=$((elapsed + 2))
      if [[ $elapsed -ge $max_wait ]]; then
        warn "node$n did not become application-ready after ${max_wait}s — deploy may fail"
        break
      fi
    done
    _probe_node_app_ready "$port" && ok "node$n application-ready" \
      || warn "node$n not responding to probes — continuing anyway"
  done
}

# ─── Deploy WASM creatures to the running Caspar nodes ───────────────────────
_deploy_creatures() {
  local server_dir="$1"
  local deploy_script="$server_dir/bench/deploy.py"
  [[ -f "$deploy_script" ]] \
    || { warn "deploy.py not found at $deploy_script"; return 1; }
  python3 -c "from Crypto.PublicKey import RSA" 2>/dev/null \
    || { warn "pycryptodome not available — skipping deployment"; return 1; }
  info "Deploying WASM creatures to node(s)…"
  mkdir -p "$DATA_ROOT"
  python3 "$deploy_script" 2>&1 | tee "$DATA_ROOT/deploy.log"
  local report="${HOME:-/root}/deployment_report.json"
  if [[ -f "$report" ]]; then
    local ok_count
    ok_count=$(python3 -c "
import json
try:
    reps = json.load(open('$report'))
    print(sum(r.get('ok_count', 0) for r in reps if isinstance(r, dict)))
except Exception:
    print(0)
" 2>/dev/null || echo 0)
    ok "Creature deployment complete — ${ok_count} WASM modules deployed"
  else
    warn "deploy.py finished but deployment_report.json not found"
  fi
}

# ─── Dependency checks ───────────────────────────────────────────────────────
check_dep() {
  local name="$1" cmd="$2" hint="$3"
  if ! command -v "$cmd" &>/dev/null; then
    die "Missing dependency: $name\n  Install: $hint"
  fi
}

info "Checking dependencies…"

if $USE_DOCKER; then
  # ── Snap docker → Docker CE migration ────────────────────────────────────────
  # Docker CE (apt) is required for full gVisor support.
  # If snap docker is the only docker present, replace it with Docker CE.
  _docker_bin=$(command -v docker 2>/dev/null || true)
  # _docker_ce_installed: distro-agnostic check for Docker CE (apt/rpm)
  _docker_ce_installed() {
    command -v dpkg &>/dev/null && dpkg -l docker-ce &>/dev/null 2>&1 && return 0
    command -v rpm  &>/dev/null && rpm -q docker-ce  &>/dev/null 2>&1 && return 0
    return 1
  }
  if snap list docker &>/dev/null 2>&1 && ! _docker_ce_installed; then
    warn "Snap docker detected — migrating to Docker CE (apt) for gVisor compatibility…"
    _as_root _migrate_snap_to_docker_ce
    export PATH="/usr/bin:/usr/local/bin:$PATH"
    unset _docker_bin
  fi
  unset _docker_bin

  # ── Auto-install Docker CE if no NATIVE docker exists ────────────────────
  # _native_docker_exists rejects Windows .exe wrappers accessible via WSL PATH
  if ! _native_docker_exists; then
    warn "Native docker not found — auto-installing Docker CE…"
    _as_root _install_docker
    export PATH="/usr/bin:/usr/local/bin:$PATH"
  fi
  check_dep "docker" "docker" "https://docs.docker.com/engine/install/"

  # ── Fix socket permissions (Docker CE fresh install: user not yet in group) ──
  if ! docker info >/dev/null 2>&1 && sudo docker info >/dev/null 2>&1; then
    warn "Docker socket not accessible to $(whoami) — fixing group membership…"
    getent group docker &>/dev/null || sudo groupadd docker 2>/dev/null || true
    sudo chown root:docker /var/run/docker.sock 2>/dev/null || true
    sudo chmod 660 /var/run/docker.sock 2>/dev/null || true
    sudo usermod -aG docker "$(whoami)" 2>/dev/null || true
    # Re-exec this script under the docker group (avoids needing a new login session)
    exec sg docker -c "bash $(printf '%q' "$0") $(printf '%q ' "$@")" \
      || die "Could not re-exec with docker group. Log out and back in, then re-run."
  fi

  # Ensure the daemon is reachable; start dockerd if needed (handles WSL2 no-systemd)
  if ! docker info >/dev/null 2>&1; then
    warn "Docker daemon not reachable — attempting to start it…"
    if command -v systemctl &>/dev/null && systemctl is-system-running 2>/dev/null | grep -qE 'running|degraded'; then
      sudo systemctl start docker 2>/dev/null || true
    else
      sudo nohup dockerd --host=unix:///var/run/docker.sock \
           > /tmp/dockerd.log 2>&1 &
    fi
    _di=0; while [[ $_di -lt 30 ]]; do docker info >/dev/null 2>&1 && break; sleep 1; _di=$((_di+1)); done
    docker info >/dev/null 2>&1 \
      || die "Docker daemon is not reachable. Check /tmp/dockerd.log for details."
  fi

  # ── Auto-fix Docker daemon DNS if Docker Hub is unreachable ──────────────
  # Detects 10.255.255.254 relay failures (common on corporate/VPN networks)
  # and reconfigures the daemon to use 8.8.8.8 / 1.1.1.1 with an auto-restart.
  _ensure_docker_dns
else
  check_dep "Rust/cargo" "cargo" "curl https://sh.rustup.rs -sSf | sh"
fi

if $START_QUESTDB; then
  _java_hint() {
    case "$(_pkg_mgr)" in
      apt)    echo "apt-get install -y default-jre" ;;
      dnf|yum) echo "dnf install -y java-11-openjdk" ;;
      pacman) echo "pacman -S jre-openjdk" ;;
      *)      echo "install Java 11+ for your distro" ;;
    esac
  }
  check_dep "java" "java" "$(_java_hint)"
  if [[ ! -f "$QUESTDB_JAR" ]]; then
    if command -v curl &>/dev/null; then
      warn "QuestDB jar not found at $QUESTDB_JAR — downloading…"
      mkdir -p /opt/questdb
      QDB_VER="8.3.1"
      curl -fsSL \
        "https://github.com/questdb/questdb/releases/download/$QDB_VER/questdb-$QDB_VER-no-jre-bin.tar.gz" \
        -o /tmp/questdb.tar.gz
      tar -xzf /tmp/questdb.tar.gz -C /opt/questdb --strip-components=1
      mv /opt/questdb/questdb.jar "$QUESTDB_JAR" 2>/dev/null || true
      rm -f /tmp/questdb.tar.gz
      ok "QuestDB downloaded to $QUESTDB_JAR"
    else
      die "QuestDB jar not found: $QUESTDB_JAR"
    fi
  fi

  jver=$(java -version 2>&1 | grep -oP '(?<=version ")[0-9]+' | head -1)
  [[ -z "$jver" ]] && jver=$(java -version 2>&1 | grep -oP '"[0-9]+\.' | grep -oP '[0-9]+')
  if [[ "${jver:-0}" -lt 11 ]]; then
    warn "Java $jver found but QuestDB needs Java 11+. Skipping QuestDB."
    START_QUESTDB=false
  fi
fi

# ─── gVisor (runsc) — default ON ──────────────────────────────────────────────
if ! $SETUP_GVISOR; then
  info "Skipping gVisor setup (--no-gvisor)"
elif ! command -v docker &>/dev/null; then
  warn "Docker not installed — skipping gVisor setup"
elif command -v runsc &>/dev/null && docker info 2>/dev/null | grep -q runsc; then
  ok "gVisor (runsc) already installed and registered with Docker"
else
  _as_root _setup_gvisor
  docker info 2>/dev/null | grep -q runsc \
    && ok "gVisor registered with Docker" \
    || warn "gVisor setup completed but runsc not visible in docker info"
fi

# ─── Firecracker — default ON ─────────────────────────────────────────────────
if ! $SETUP_FIRECRACKER; then
  info "Skipping Firecracker setup (--no-firecracker)"
else
  if command -v firecracker &>/dev/null && [[ -f /opt/firecracker/kernel/vmlinux ]]; then
    ok "Firecracker already installed: $(firecracker --version 2>&1 | head -1)"
  else
    _as_root _install_firecracker
  fi

  info "Configuring Firecracker host network (bridge + NAT)…"
  _as_root _setup_firecracker_network \
    && ok "Firecracker network ready (br0 172.16.0.1/24)" \
    || warn "Firecracker network setup failed — microVM networking may not work"
fi

ok "All dependency checks passed"

# ─── Fresh start ─────────────────────────────────────────────────────────────
if $FRESH; then
  warn "--fresh: wiping $DATA_ROOT and stale deployment/bench reports"
  rm -rf "$DATA_ROOT"
  # Remove stale reports so bench-all.sh doesn't think deployment is done
  rm -f "${HOME:-/root}/deployment_report.json" \
        "${HOME:-/root}/workflow_results.json"  \
        "${HOME:-/root}/workflow_report.md"
fi

mkdir -p "$DATA_ROOT"

# ─── Stop existing processes / containers ───────────────────────────────────
stop_existing() {
  local pids
  pids=$(ps -eo pid,cmd 2>/dev/null | awk '/caspar-node/ && !/awk/ && !/grep/ && !/run-nodes/ {print $1}')
  if [[ -n "$pids" ]]; then
    info "Stopping existing caspar-node processes: $pids"
    for p in $pids; do kill "$p" 2>/dev/null || true; done
    sleep 2
    for p in $pids; do kill -9 "$p" 2>/dev/null || true; done
  fi

  if command -v docker &>/dev/null; then
    for n in 1 2 3; do
      if docker ps -a --format '{{.Names}}' 2>/dev/null | grep -q "^caspar-node${n}$"; then
        info "Removing existing container: caspar-node${n}"
        docker rm -f "caspar-node${n}" >/dev/null 2>&1 || true
      fi
    done
  fi

  local jpids
  jpids=$(ps -eo pid,cmd 2>/dev/null | awk '/questdb/ && !/awk/ && !/grep/ {print $1}')
  if [[ -n "$jpids" ]]; then
    info "Stopping existing QuestDB processes: $jpids"
    for p in $jpids; do kill "$p" 2>/dev/null || true; done
    sleep 1
  fi
}
stop_existing

# ─── Build / fetch the artifact we need ──────────────────────────────────────
# Ensure ~/.cargo/bin is in PATH so build-dist.sh can find cargo if needed
[[ -d "$HOME/.cargo/bin" ]] && export PATH="$HOME/.cargo/bin:$PATH"

if $USE_DOCKER; then
  if $REBUILD_IMAGE || ! docker image inspect "$DOCKER_IMAGE" >/dev/null 2>&1; then
    # dist/ pre-built check: if all required artifacts already exist and we're not
    # doing a forced rebuild, skip the expensive build-dist.sh step (~45 min).
    _dist_ready() {
      [[ -f "$REPO_DIR/dist/bin/caspar-node"       ]] || return 1
      [[ -f "$REPO_DIR/dist/bin/caspar-keygen"      ]] || return 1
      [[ -f "$REPO_DIR/dist/bin/casparctl"           ]] || return 1
      [[ -f "$REPO_DIR/dist/questdb/questdb.jar"    ]] || return 1
      # At least one wasmedge .so file must be present
      ls "$REPO_DIR/dist/lib/wasmedge/libwasmedge.so"* >/dev/null 2>&1 || return 1
      return 0
    }

    if $REBUILD_IMAGE; then
      info "--rebuild: force-rebuilding $DOCKER_IMAGE (build-dist.sh + docker build)…"
      bash "$REPO_DIR/build-dist.sh"
    elif _dist_ready; then
      info "dist/ already populated — skipping build-dist.sh, running docker build…"
    else
      info "Docker image $DOCKER_IMAGE not present — building (runs build-dist.sh + docker build)…"
      bash "$REPO_DIR/build-dist.sh"
    fi
    # Pass --no-firecracker flag through to the Dockerfile so the image build
    # skips the Firecracker binary + kernel downloads when they are not needed.
    _fc_build_arg="true"; $SETUP_FIRECRACKER || _fc_build_arg="false"
    docker build -f "$REPO_DIR/node/Dockerfile" \
      --build-arg "INSTALL_FIRECRACKER=${_fc_build_arg}" \
      -t "$DOCKER_IMAGE" "$REPO_DIR"
  fi
  ok "Docker image ready: $DOCKER_IMAGE"
else
  build_needed=false
  if $REBUILD_IMAGE; then
    build_needed=true
    info "--rebuild: forcing cargo build --release…"
  elif [[ ! -f "$BINARY" ]]; then
    build_needed=true
    info "Binary not found — building (this takes ~3 min first time)…"
  elif [[ "$BINARY" -ot "$NODE_DIR/src/main.rs" ]]; then
    build_needed=true
    info "Source newer than binary — rebuilding…"
  fi
  if $build_needed; then
    cd "$NODE_DIR"
    cargo build --release 2>&1 | grep -E "^error|Compiling caspar|Finished" || true
    cd "$REPO_DIR"
  fi
  [[ -f "$BINARY" ]] || die "Build failed: binary not found at $BINARY"
  ok "Binary ready: $BINARY ($(ls -lh "$BINARY" | awk '{print $5}'))"

  # Local mode: caspar-node hardcodes /app/scripts/shardchain.sh. Create the
  # directory and symlink the script from the repo so the binary can find it.
  # This is a no-op if the symlink already exists.
  local _shard_script="$REPO_DIR/node/scripts/shardchain.sh"
  if [[ -f "$_shard_script" ]]; then
    mkdir -p /app/scripts 2>/dev/null || true
    ln -sf "$_shard_script" /app/scripts/shardchain.sh 2>/dev/null \
      && ok "Linked /app/scripts/shardchain.sh → $_shard_script" \
      || warn "Could not create /app/scripts/shardchain.sh — shard init may fail"
  fi
fi

# ─── Helper: wait for a TCP port ────────────────────────────────────────────
wait_for_port() {
  local host="$1" port="$2" name="$3" timeout="${4:-30}"
  local elapsed=0
  while ! python3 -c "import socket; s=socket.socket(); s.settimeout(1); s.connect(('$host',$port)); s.close()" 2>/dev/null; do
    sleep 1; elapsed=$((elapsed+1))
    [[ $elapsed -ge $timeout ]] && return 1
  done
  return 0
}

# ─── Start QuestDB ───────────────────────────────────────────────────────────
QUESTDB_PID=""
if $START_QUESTDB; then
  mkdir -p "$QUESTDB_DATA"
  info "Starting QuestDB on port $QUESTDB_PORT…"
  java -jar "$QUESTDB_JAR" -m io.questdb/io.questdb.ServerMain \
       -d "$QUESTDB_DATA" >> "$DATA_ROOT/questdb.log" 2>&1 &
  QUESTDB_PID=$!
  if wait_for_port localhost $QUESTDB_PORT QuestDB 45; then
    ok "QuestDB ready (pid $QUESTDB_PID)"
  else
    warn "QuestDB did not come up within 45s — nodes may log telemetry errors but will function"
  fi
fi

# ─── Per-node config ──────────────────────────────────────────────────────────
# NODE LAYOUT:
#  node1: TCP=8074  WS=8076  FED=8077  CHAIN=8078  ENTITY=8079  VM=8080  TEL=9099
#  node2: TCP=8174  WS=8176  FED=8177  CHAIN=8178  ENTITY=8179  VM=8180  TEL=9199
#  node3: TCP=8274  WS=8276  FED=8277  CHAIN=8278  ENTITY=8279  VM=8280  TEL=9299

# _generate_node_config: create .env + babble key for a node from scratch.
# Safe to call on an existing node — exits immediately if .env already exists.
_generate_node_config() {
  local n="$1"
  local node_dir="$DATA_ROOT/node${n}"
  local env_file="$node_dir/.env"

  [[ -f "$env_file" ]] && return 0   # already configured

  info "Generating fresh config for node${n}…"
  mkdir -p "$node_dir"/{storage,db,applet,search,store_logs,telemetry,babble}

  # ── Babble secp256k1 consensus key ──────────────────────────────────────────
  # caspar-keygen writes to $HOME/.babble/{priv_key,key.pub}.
  # We override HOME so each node gets its own key.
  local keygen_bin="$REPO_DIR/dist/bin/caspar-keygen"
  if [[ -x "$keygen_bin" ]]; then
    local tmp_home; tmp_home=$(mktemp -d)
    HOME="$tmp_home" "$keygen_bin" >/dev/null 2>&1 || true
    if [[ -f "$tmp_home/.babble/priv_key" ]]; then
      cp "$tmp_home/.babble/priv_key" "$node_dir/babble/priv_key"
      # key.pub is needed by shardchain.sh — copy it alongside priv_key
      [[ -f "$tmp_home/.babble/key.pub" ]] && \
        cp "$tmp_home/.babble/key.pub" "$node_dir/babble/key.pub"
    else
      # keygen failed (race on $HOME/.babble?) — fall back to random key
      python3 -c "import secrets; print(secrets.token_hex(32), end='')" \
        > "$node_dir/babble/priv_key" 2>/dev/null \
        || dd if=/dev/urandom bs=32 count=1 2>/dev/null | xxd -p | tr -d '\n' \
           > "$node_dir/babble/priv_key"
    fi
    rm -rf "$tmp_home"
  else
    warn "caspar-keygen not found at $keygen_bin — generating random babble key"
    python3 -c "import secrets; print(secrets.token_hex(32), end='')" \
      > "$node_dir/babble/priv_key"
  fi

  # ── peers.genesis.json (required by shardchain.sh in local / --no-docker mode) ─
  # shardchain.sh copies this file into each chain shard directory as peers.json
  # so that Babble can discover validators on startup.  Without it babble.init()
  # fails with "No such file or directory" and creature deployment times out.
  #
  # For IS_HEAD (single-node / triple-node head): the file lists only this node.
  # Non-head nodes in triple mode use peersMode=3 and fetch from node1's entity
  # API (via the ENTITY_API_URL override set in local_start_node).
  local pub_key_file="$node_dir/babble/key.pub"
  if [[ -f "$pub_key_file" ]]; then
    local tcp_port_early=${NODE_TCP[$n]}
    local chain_port_early=$((tcp_port_early + 4))
    local pub_key_upper
    pub_key_upper=$(tr '[:lower:]' '[:upper:]' < "$pub_key_file")
    python3 - "$node_dir/babble/peers.genesis.json" \
              "127.0.0.1:${chain_port_early}" \
              "0X${pub_key_upper}" \
              "node${n}" <<'PEERSPY'
import json, sys
out_path, net_addr, pub_key_hex, moniker = sys.argv[1:]
with open(out_path, 'w') as f:
    json.dump([{"NetAddr": net_addr, "PubKeyHex": pub_key_hex, "Moniker": moniker}], f)
    f.write('\n')
PEERSPY
    ok "node${n} babble peers.genesis.json generated"
  fi

  # ── RSA identity key (OWNER_PRIVATE_KEY, PKCS#8 PEM) ───────────────────────
  # Prefer openssl (always present) over pycryptodome.
  local priv_pem=""
  if command -v openssl &>/dev/null; then
    priv_pem=$(openssl genrsa 2048 2>/dev/null \
               | openssl pkcs8 -topk8 -nocrypt 2>/dev/null)
  fi
  if [[ -z "$priv_pem" ]]; then
    priv_pem=$(python3 -c "
from Crypto.PublicKey import RSA
print(RSA.generate(2048).export_key().decode(), end='')
" 2>/dev/null) || true
  fi
  [[ -z "$priv_pem" ]] && die "Cannot generate RSA key for node${n}: install openssl or pycryptodome"

  # ── Port layout (matches the NODE LAYOUT above) ──────────────────────────────
  local tcp_port=${NODE_TCP[$n]}            # 8074 / 8174 / 8274
  local ws_port=$((tcp_port + 2))           # 8076 / 8176 / 8276
  local fed_port=$((tcp_port + 3))          # 8077 / 8177 / 8277
  local chain_port=$((tcp_port + 4))        # 8078 / 8178 / 8278
  local entity_port=$((tcp_port + 5))       # 8079 / 8179 / 8279
  local vm_port=$((tcp_port + 6))           # 8080 / 8180 / 8280
  local tel_port=$((9099 + (n - 1) * 100))  # 9099 / 9199 / 9299

  local is_head root_node
  [[ $n -eq 1 ]] && is_head="true" || is_head="false"
  root_node="localhost:${NODE_TCP[1]}"

  # Docker mounts $node_dir as /app/data inside the container, so all paths
  # inside .env use the container prefix.  Local mode overrides them via
  # the env vars set in local_start_node.
  cat > "$env_file" <<EOF
OWNER_ID=owner-node${n}
OWNER_PRIVATE_KEY="${priv_pem}"
STORAGE_ROOT_PATH=/app/data/storage
BASE_DB_PATH=/app/data/db
APPLET_DB_PATH=/app/data/applet
SEARCH_INDEX_PATH=/app/data/search
STORE_LOGS_DB=/app/data/store_logs
CLIENT_WS_API_PORT=${ws_port}
CLIENT_TCP_API_PORT=${tcp_port}
FEDERATION_API_PORT=${fed_port}
BLOCKCHAIN_API_PORT=${chain_port}
ENTITY_API_PORT=${entity_port}
VM_API_PORT=${vm_port}
ORIGIN=http://localhost:${tcp_port}
IPADDR=127.0.0.1
ROOT_NODE=${root_node}
IS_HEAD=${is_head}
AdminPassword=admin123
VM_EXEC_COST_PER_SECOND=0
VM_RAM_COST_PER_MB_PER_MINUTE=0
VM_CPU_CORE_COST_PER_MINUTE=0
VM_DISK_COST_PER_GB_PER_MINUTE=0
TELEMETRY_API_PORT=${tel_port}
TELEMETRY_DB_PATH=/app/data/telemetry
BABBLE_DIR=/app/data/babble
BABBLE_DATA_DIR=/app/data/babble
EOF

  ok "node${n} config generated (TCP=${tcp_port}, IS_HEAD=${is_head})"
}

ensure_node_config() {
  local n="$1"
  local node_dir="$DATA_ROOT/node${n}"
  mkdir -p "$node_dir"/{storage,db,applet,search,store_logs,telemetry,babble}
  # Generate .env + babble key if not already present (fresh environment).
  _generate_node_config "$n"
}

# ─── Local-mode launch ───────────────────────────────────────────────────────
local_start_node() {
  local n="$1"
  local node_dir="$DATA_ROOT/node${n}"
  local env_file="$node_dir/.env"
  local log_file="$node_dir/node.log"

  [[ -f "$env_file" ]] && { set -a; source "$env_file"; set +a; }

  # caspar-node links against libwasmedge.so.0 which ships in dist/lib/wasmedge/.
  # Prepend that directory to LD_LIBRARY_PATH so the dynamic linker finds it
  # without needing a system-wide ldconfig run.
  local _wasm_lib="$REPO_DIR/dist/lib/wasmedge"
  local _ld_path="${_wasm_lib}${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"

  info "Starting node$n locally (TCP=${NODE_TCP[$n]})…"
  # Override every /app/data/* Docker container path to the actual node directory.
  # Also set ENTITY_API_URL so shardchain.sh (peersMode=3 on non-head nodes) fetches
  # peers from node1's local entity API instead of the external production server.
  STORAGE_ROOT_PATH="$node_dir/storage" \
  BASE_DB_PATH="$node_dir/db" \
  APPLET_DB_PATH="$node_dir/applet" \
  SEARCH_INDEX_PATH="$node_dir/search" \
  STORE_LOGS_DB="$node_dir/store_logs" \
  TELEMETRY_DB_PATH="$node_dir/telemetry" \
  BABBLE_DIR="$node_dir/babble" \
  BABBLE_DATA_DIR="$node_dir/babble" \
  ENTITY_API_URL="http://127.0.0.1" \
  LD_LIBRARY_PATH="$_ld_path" \
  "$BINARY" >> "$log_file" 2>&1 &
  echo $! > "$node_dir/caspar.pid"
  echo $!
}

# ─── Docker-mode launch ──────────────────────────────────────────────────────
# Path translation: .env host paths (/tmp/caspar/nodeN/) → container (/app/data/)
# --network host: ports match local mode exactly; no NAT or port-mapping needed.
docker_start_node() {
  local n="$1"
  local node_dir="$DATA_ROOT/node${n}"
  local env_file="$node_dir/.env"
  local docker_env="$node_dir/.env.docker"
  local container="caspar-node${n}"

  if [[ ! -f "$env_file" ]]; then
    warn "No $env_file for node$n — container may fail to start without keys"
    touch "$env_file"
  fi

  sed "s|${DATA_ROOT}/node${n}|/app/data|g" "$env_file" > "$docker_env"

  info "Starting node$n in docker (container=$container, TCP=${NODE_TCP[$n]})…"

  local docker_args=(
    --name "$container"
    --network host
    --restart unless-stopped
    -v "$docker_env":/app/.env:ro
    -v "$node_dir":/app/data
  )
  [[ -S /var/run/docker.sock ]] \
    && docker_args+=( -v /var/run/docker.sock:/var/run/docker.sock )

  docker run -d "${docker_args[@]}" "$DOCKER_IMAGE" >/dev/null
  echo "$container"
}

# ─── Launch nodes ────────────────────────────────────────────────────────────
declare -a STARTED_PIDS=()
declare -a STARTED_CONTAINERS=()

for n in "${NODES[@]}"; do
  ensure_node_config "$n"
  if $USE_DOCKER; then
    container=$(docker_start_node "$n")
    STARTED_CONTAINERS+=("$container")
  else
    pid=$(local_start_node "$n")
    STARTED_PIDS+=("$pid")
  fi
done

# ─── Wait for nodes to accept connections ────────────────────────────────────
info "Waiting for node(s) to accept connections…"
all_up=true
for n in "${NODES[@]}"; do
  port=${NODE_TCP[$n]}
  if wait_for_port localhost "$port" "node$n" 120; then
    ok "node$n up on TCP port $port"
  else
    $USE_DOCKER \
      && warn "node$n (port $port): timeout — check: docker logs caspar-node${n}" \
      || warn "node$n (port $port): timeout — check: tail $DATA_ROOT/node${n}/node.log"
    all_up=false
  fi
done

# ─── Clone, build, and deploy WASM creatures ─────────────────────────────────
# run-nodes.sh owns the full setup so the cluster is ready for benchmarking
# the moment the script exits. bench-all.sh auto-detects this and skips the
# deploy step when deployment_report.json already contains successful entries.
#
# The TCP-port check above is a fast initial probe; some nodes (especially
# on a freshly-built Docker image) take longer than that to accept connections.
# We therefore always proceed with the clone/build/deploy pipeline and rely on
# _wait_nodes_app_ready (180 s) as the definitive readiness gate right before
# creatures are pushed to the nodes.
if $DEPLOY_CREATURES; then
  if ! $all_up; then
    warn "One or more node(s) did not respond on TCP within the initial 120 s window."
    warn "Proceeding with creature build — node(s) may still be initialising."
    warn "_wait_nodes_app_ready will verify readiness (up to 180 s) before deploying."
  fi
  if _ensure_decillionai_server; then
    _as_root _install_tinygo
    _as_root _install_python_deps
    # Make Go 1.23 / TinyGo visible in the current shell after root install
    export PATH="/usr/local/go123/bin:/usr/local/tinygo/bin:${PATH}"
    export GOROOT="/usr/local/go123"
    if _build_creatures "$DECILLIONAI_SERVER_DIR"; then
      _wait_nodes_app_ready
      _deploy_creatures "$DECILLIONAI_SERVER_DIR"
    fi
  fi
else
  info "Skipping creature build/deploy (--skip-deploy)"
fi

# ─── Summary ─────────────────────────────────────────────────────────────────
echo ""
$all_up && ok "All ${#NODES[@]} node(s) running." || warn "Some node(s) may not have started correctly."

echo ""
echo "  Mode:    $MODE (${#NODES[@]} node(s)) — $($USE_DOCKER && echo 'docker' || echo 'local')"
$USE_DOCKER  && echo "  Image:   $DOCKER_IMAGE"
$USE_DOCKER  || echo "  Binary:  $BINARY"
echo "  Data:    $DATA_ROOT"
$START_QUESTDB && echo "  QuestDB: localhost:$QUESTDB_PORT (http: localhost:9000)"
echo ""
if $USE_DOCKER; then
  echo "  Containers:"
  for c in "${STARTED_CONTAINERS[@]}"; do echo "    $c → docker logs -f $c"; done
  echo ""
  echo "  Stop all:  $REPO_DIR/stop-nodes.sh"
else
  echo "  Logs:"
  for n in "${NODES[@]}"; do echo "    node$n → $DATA_ROOT/node${n}/node.log"; done
  echo ""
  echo "  Stop all:  $REPO_DIR/stop-nodes.sh   (or Ctrl-C in this terminal)"
fi
echo ""

# ─── Foreground vs detached ──────────────────────────────────────────────────
if $USE_DOCKER && ! $FOREGROUND; then
  info "Containers running detached. Run with --foreground to tail logs."
  exit 0
fi

cleanup() {
  echo ""
  info "Shutting down…"
  for p in "${STARTED_PIDS[@]:-}";      do [[ -n "$p" ]] && kill "$p" 2>/dev/null || true; done
  for c in "${STARTED_CONTAINERS[@]:-}"; do [[ -n "$c" ]] && docker stop --time 10 "$c" >/dev/null 2>&1 || true; done
  [[ -n "$QUESTDB_PID" ]] && kill "$QUESTDB_PID" 2>/dev/null || true
  exit 0
}
trap cleanup INT TERM

info "Press Ctrl-C to stop everything."
if $USE_DOCKER; then
  docker logs -f --tail=20 "${STARTED_CONTAINERS[0]}" &
  wait $!
else
  wait
fi
