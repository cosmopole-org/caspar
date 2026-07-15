#!/usr/bin/env bash
# =============================================================================
# build-dist.sh — Build all artifacts and populate dist/ for Docker
#
# Run this script before `docker build` to ensure all binaries and libraries
# in dist/ are fresh. The Dockerfile copies from dist/ instead of building
# inside the container, keeping Docker builds to ~30s instead of ~45 min.
#
# Usage:
#   ./build-dist.sh [OPTIONS]
#
# Options:
#   --skip-node      Skip cargo build for caspar-node/keygen (use existing binary)
#   --skip-ctl       Skip cargo build for casparctl
#   --wasmedge-ver V Override WasmEdge version to install (default: 0.14.0)
#   --disable-vm K   Disable a VM plugin for this node build (repeatable, or
#                    comma-separated keys). Equivalent to `casparctl vms disable`.
#   --enable-vm K    Re-enable a previously disabled VM plugin (repeatable).
#   --help           Show this help
#
# After this script succeeds, run:
#   docker build -f node/Dockerfile -t caspar-node:latest .
# =============================================================================

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE_DIR="$REPO_DIR/node"
CTL_DIR="$REPO_DIR/cmd/casparctl"
DIST_DIR="$REPO_DIR/dist"
WASMEDGE_VERSION="0.14.0"
WASMEDGE_HOME="${WASMEDGE_HOME:-$HOME/.wasmedge}"
QUESTDB_VERSION="8.3.1"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'
info() { echo -e "${CYAN}[build-dist]${NC} $*"; }
ok()   { echo -e "${GREEN}[build-dist]${NC} $*"; }
warn() { echo -e "${YELLOW}[build-dist]${NC} $*"; }
die()  { echo -e "${RED}[build-dist] FATAL:${NC} $*" >&2; exit 1; }
step() { echo -e "\n${BOLD}${CYAN}━━━ $* ━━━${NC}"; }

# Portable arch token: amd64 / arm64 / armhf — works on any Linux/WSL distro
_arch() {
  case "$(uname -m)" in
    x86_64)        echo amd64 ;;
    aarch64|arm64) echo arm64 ;;
    armv7l)        echo armhf ;;
    *)             uname -m   ;;
  esac
}

# ─── Args ────────────────────────────────────────────────────────────────────
SKIP_NODE=false
SKIP_CTL=false
DISABLE_VMS=()
ENABLE_VMS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-node)    SKIP_NODE=true ;;
    --skip-ctl)     SKIP_CTL=true ;;
    --wasmedge-ver) shift; WASMEDGE_VERSION="$1" ;;
    --disable-vm)   shift; IFS=',' read -ra _keys <<< "$1"; DISABLE_VMS+=("${_keys[@]}") ;;
    --enable-vm)    shift; IFS=',' read -ra _keys <<< "$1"; ENABLE_VMS+=("${_keys[@]}") ;;
    --help|-h)
      sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "Unknown argument: $1" ;;
  esac
  shift
done

DIST_START=$SECONDS

echo ""
echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║          Caspar dist/ Build Script                          ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""

# =============================================================================
# Step 1 — Dependency checks
# =============================================================================
step "Step 1: Check dependencies"

command -v curl &>/dev/null || die "curl not found"
ok "curl available"

# Auto-install Rust if cargo is missing so build-dist.sh stays self-sufficient
# when invoked from run-nodes.sh on a host that hasn't been pre-provisioned
# (notably the GitHub Actions runner when the workflow's "Install Rust" step
# was skipped via skip_build=true). The non-interactive rustup install drops
# the toolchain in $HOME/.cargo and updates PATH for the rest of this shell.
if ! command -v cargo &>/dev/null; then
  info "cargo not found — installing rustup (stable toolchain)…"
  curl -sSf --proto '=https' --tlsv1.2 https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path
  export PATH="$HOME/.cargo/bin:$PATH"
  command -v cargo &>/dev/null \
    || die "rustup install completed but cargo still not on PATH"
fi
ok "cargo $(cargo --version 2>/dev/null | head -1)"

# =============================================================================
# Step 2 — WasmEdge runtime library
# =============================================================================
step "Step 2: WasmEdge runtime ($WASMEDGE_VERSION)"

# Detect installed WasmEdge — the shared lib naming convention is
# libwasmedge.so.<SOVERSION> where SOVERSION differs from the project version.
WASMEDGE_LIB_FILE=""
for search_dir in "$WASMEDGE_HOME/lib" "/usr/local/lib" "/usr/lib"; do
  # Skip absent dirs BEFORE find: under `set -euo pipefail` a find on a
  # missing path exits 1 and aborts the whole script — which killed every
  # run on a virgin machine (no ~/.wasmedge yet) before the self-install
  # branch below could ever execute.
  [[ -d "$search_dir" ]] || continue
  found=$(find "$search_dir" -maxdepth 2 -name "libwasmedge.so.*.*" 2>/dev/null | head -1 || true)
  if [[ -n "$found" ]]; then
    # Verify the corresponding include dir exists — wasmedge-sys needs headers too.
    found_inc="${search_dir%/lib*}/include"
    [[ -d "$found_inc" ]] || continue
    WASMEDGE_LIB_FILE="$found"
    ok "Found WasmEdge library: $WASMEDGE_LIB_FILE ($(ls -lh "$WASMEDGE_LIB_FILE" | awk '{print $5}'))"
    break
  fi
done

if [[ -z "$WASMEDGE_LIB_FILE" ]]; then
  info "WasmEdge $WASMEDGE_VERSION not found — installing to $WASMEDGE_HOME …"
  # Use the version-tagged installer URL for reproducibility
  curl -sSf "https://raw.githubusercontent.com/WasmEdge/WasmEdge/$WASMEDGE_VERSION/utils/install.sh" \
    | bash -s -- -v "$WASMEDGE_VERSION"

  WASMEDGE_LIB_FILE=$(find "$WASMEDGE_HOME/lib" -maxdepth 2 -name "libwasmedge.so.*.*" 2>/dev/null | head -1)
  [[ -n "$WASMEDGE_LIB_FILE" ]] || die "WasmEdge install succeeded but library not found"
  ok "Installed WasmEdge: $WASMEDGE_LIB_FILE"
fi

# Export so cargo build can find it
WASMEDGE_LIB_DIR="$(dirname "$WASMEDGE_LIB_FILE")"
export WASMEDGE_LIB_DIR
export LD_LIBRARY_PATH="$WASMEDGE_LIB_DIR:${LD_LIBRARY_PATH:-}"
export PATH="$(dirname "$WASMEDGE_LIB_DIR")/bin:$PATH"

# =============================================================================
# Step 3 — Build casparctl (needed first: it drives the VM plugin selection)
# =============================================================================
step "Step 3: Build casparctl"

CTL_BIN="$CTL_DIR/target/release/casparctl"
if $SKIP_CTL && [[ -f "$CTL_BIN" ]]; then
  ok "Skipping casparctl build (--skip-ctl), binary already present"
else
  info "Running cargo build --release (casparctl)…"
  CTL_START=$SECONDS
  cd "$CTL_DIR"
  cargo build --release 2>&1 | grep -E "^error|Compiling casparctl|Finished" || true
  cd "$REPO_DIR"
  ok "casparctl built in $((SECONDS - CTL_START))s"
  [[ -f "$CTL_BIN" ]] || die "casparctl binary not found"
fi

# =============================================================================
# Step 4 — VM plugin selection + registration codegen
# =============================================================================
step "Step 4: Sync VM plugins (vms/ → generated registration crate)"

if [[ -f "$CTL_BIN" ]]; then
  # Apply one-shot selection flags first.
  for key in "${DISABLE_VMS[@]:-}"; do
    [[ -n "$key" ]] || continue
    "$CTL_BIN" vms disable "$key" --vms-dir "$REPO_DIR/vms" || die "failed to disable VM '$key'"
  done
  for key in "${ENABLE_VMS[@]:-}"; do
    [[ -n "$key" ]] || continue
    "$CTL_BIN" vms enable "$key" --vms-dir "$REPO_DIR/vms" || die "failed to enable VM '$key'"
  done
  # Regenerate the node's plugin registration code from the current selection
  # so the binary carries exactly the VM types the admin picked.
  "$CTL_BIN" vms sync --vms-dir "$REPO_DIR/vms" --node-dir "$NODE_DIR" \
    || die "casparctl vms sync failed"
  ok "VM plugin registration is in sync"
else
  warn "casparctl binary unavailable — skipping VM plugin sync (using committed registration)"
fi

# =============================================================================
# Step 5 — Build caspar-node workspace
# =============================================================================
step "Step 5: Build caspar-node workspace"

if $SKIP_NODE; then
  [[ -f "$NODE_DIR/target/release/caspar-node" ]] \
    || die "--skip-node given but binary not found at $NODE_DIR/target/release/caspar-node"
  ok "Skipping node build (--skip-node)"
else
  info "Running cargo build --release (node workspace)…"
  BUILD_START=$SECONDS
  cd "$NODE_DIR"
  cargo build --release 2>&1 | grep -E "^error|Compiling caspar|Finished" || true
  cd "$REPO_DIR"
  ok "Node workspace built in $((SECONDS - BUILD_START))s"
fi

for bin in caspar-node caspar-keygen; do
  [[ -f "$NODE_DIR/target/release/$bin" ]] || die "Expected binary missing: $NODE_DIR/target/release/$bin"
done

# casparctl was already built in Step 3; nothing to do here beyond noting
# whether its binary is available for the dist copy.
if [[ ! -f "$CTL_BIN" ]]; then
  SKIP_CTL_COPY=true
fi

# =============================================================================
# Step 6 — Obtain QuestDB jar
# =============================================================================
step "Step 6: QuestDB jar"

QUESTDB_JAR_SRC="/opt/questdb/questdb.jar"

if [[ ! -f "$QUESTDB_JAR_SRC" ]]; then
  info "QuestDB jar not found at $QUESTDB_JAR_SRC — downloading $QUESTDB_VERSION…"
  mkdir -p /opt/questdb
  curl -fsSL \
    "https://github.com/questdb/questdb/releases/download/$QUESTDB_VERSION/questdb-$QUESTDB_VERSION-no-jre-bin.tar.gz" \
    -o /tmp/questdb.tar.gz
  tar -xzf /tmp/questdb.tar.gz -C /opt/questdb --strip-components=1
  mv /opt/questdb/questdb.jar "$QUESTDB_JAR_SRC" 2>/dev/null || true
  rm -f /tmp/questdb.tar.gz
  ok "Downloaded QuestDB $QUESTDB_VERSION"
fi

ok "QuestDB jar: $QUESTDB_JAR_SRC ($(ls -lh "$QUESTDB_JAR_SRC" | awk '{print $5}'))"

# =============================================================================
# Step 7 — Populate dist/
# =============================================================================
step "Step 7: Populate dist/"

mkdir -p "$DIST_DIR/bin" "$DIST_DIR/lib/wasmedge" "$DIST_DIR/questdb"

info "Copying binaries…"
cp "$NODE_DIR/target/release/caspar-node"   "$DIST_DIR/bin/caspar-node"
cp "$NODE_DIR/target/release/caspar-keygen" "$DIST_DIR/bin/caspar-keygen"
if [[ "${SKIP_CTL_COPY:-false}" != "true" ]]; then
  cp "$CTL_DIR/target/release/casparctl"     "$DIST_DIR/bin/casparctl"
fi
chmod +x "$DIST_DIR/bin/"*

info "Copying WasmEdge library…"
SO_BASENAME=$(basename "$WASMEDGE_LIB_FILE")
cp "$WASMEDGE_LIB_FILE" "$DIST_DIR/lib/wasmedge/$SO_BASENAME"
# Recreate symlinks
(
  cd "$DIST_DIR/lib/wasmedge"
  # Extract major version for symlink (e.g. libwasmedge.so.0.1.0 → libwasmedge.so.0)
  SO_MAJOR=$(echo "$SO_BASENAME" | grep -oP 'libwasmedge\.so\.\d+')
  ln -sf "$SO_BASENAME" "$SO_MAJOR"
  ln -sf "$SO_MAJOR" libwasmedge.so
)
ok "WasmEdge: $SO_BASENAME → symlinks created"

info "Copying QuestDB jar…"
cp "$QUESTDB_JAR_SRC" "$DIST_DIR/questdb/questdb.jar"

# =============================================================================
# Summary
# =============================================================================
ELAPSED=$((SECONDS - DIST_START))

echo ""
echo -e "${BOLD}${GREEN}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${GREEN}║                   dist/ Build Complete                     ║${NC}"
echo -e "${BOLD}${GREEN}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  ${BOLD}dist/ contents:${NC}"
find "$DIST_DIR" -type f | sort | while read -r f; do
  size=$(ls -lh "$f" | awk '{print $5}')
  printf "    %-45s %s\n" "${f#$REPO_DIR/}" "$size"
done
echo ""
echo -e "  ${BOLD}Total time:${NC} ${ELAPSED}s"
echo ""
echo -e "  ${BOLD}Next step:${NC}"
echo "    docker build -f node/Dockerfile -t caspar-node:latest ."
echo ""
