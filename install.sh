#!/usr/bin/env bash
# infistreamer installer: installs Docker + Compose on any major Linux distro,
# prepares configuration, and brings the stack up.
set -euo pipefail

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; NC='\033[0m'
log()  { echo -e "${GREEN}==>${NC} $*"; }
warn() { echo -e "${YELLOW}!  ${NC} $*"; }
die()  { echo -e "${RED}xx ${NC} $*" >&2; exit 1; }

cd "$(dirname "$0")"

[ "$(uname -s)" = "Linux" ] || die "this installer targets Linux; on macOS/Windows install Docker Desktop and run: docker compose up -d --build"

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  command -v sudo >/dev/null 2>&1 || die "please run as root or install sudo"
  SUDO="sudo"
fi

# ---- Docker ----
if command -v docker >/dev/null 2>&1; then
  log "Docker already installed: $(docker --version)"
else
  log "Installing Docker via get.docker.com (supports Ubuntu/Debian/Fedora/CentOS/RHEL/Arch/openSUSE)..."
  curl -fsSL https://get.docker.com | $SUDO sh \
    || die "Docker install failed — see https://docs.docker.com/engine/install/"
  $SUDO systemctl enable --now docker 2>/dev/null || true
  if [ -n "$SUDO" ] && [ -n "${SUDO_USER:-${USER:-}}" ]; then
    $SUDO usermod -aG docker "${SUDO_USER:-$USER}" || true
    warn "added $(echo "${SUDO_USER:-$USER}") to the 'docker' group — log out/in for it to take effect"
  fi
fi

# ---- Compose ----
if docker compose version >/dev/null 2>&1; then
  log "Docker Compose plugin present: $(docker compose version | head -n1)"
elif command -v docker-compose >/dev/null 2>&1; then
  warn "using legacy docker-compose binary"
else
  die "Docker Compose not found — install the 'docker-compose-plugin' package"
fi
COMPOSE="docker compose"; docker compose version >/dev/null 2>&1 || COMPOSE="docker-compose"

# ---- Tailscale (optional) ----
# Non-interactive: set INSTALL_TAILSCALE=yes|no, and optionally TAILSCALE_AUTHKEY=tskey-...
# Interactive: prompts when run from a terminal.
install_tailscale() {
  local want="${INSTALL_TAILSCALE:-}"
  if [ -z "$want" ]; then
    if [ -t 0 ]; then
      read -rp "$(echo -e "${YELLOW}? ${NC}")Install Tailscale for secure remote access? [y/N] " ans
      case "$ans" in [Yy]*) want=yes ;; *) want=no ;; esac
    else
      want=no
    fi
  fi
  [ "$want" = "yes" ] || { log "Skipping Tailscale"; return; }

  if command -v tailscale >/dev/null 2>&1; then
    log "Tailscale already installed: $(tailscale version | head -n1)"
  else
    log "Installing Tailscale..."
    curl -fsSL https://tailscale.com/install.sh | $SUDO sh \
      || { warn "Tailscale install failed — skipping"; return; }
  fi
  $SUDO systemctl enable --now tailscaled 2>/dev/null || true

  # Decide auth method: pre-supplied key, or browser login.
  local key="${TAILSCALE_AUTHKEY:-}"
  if [ -z "$key" ] && [ -t 0 ]; then
    echo "  Tailscale authentication:"
    echo "    1) Auth key  (paste a tskey-... key — best for headless servers)"
    echo "    2) Browser login (opens a URL you authenticate in a browser)"
    read -rp "$(echo -e "${YELLOW}? ${NC}")Choose [1/2]: " m
    if [ "$m" = "1" ]; then
      read -rsp "$(echo -e "${YELLOW}? ${NC}")Paste Tailscale auth key: " key; echo
    fi
  fi

  if [ -n "$key" ]; then
    log "Bringing Tailscale up with auth key..."
    $SUDO tailscale up --authkey="$key" --hostname=infistreamer \
      || warn "tailscale up failed — run 'sudo tailscale up' manually"
  else
    log "Bringing Tailscale up — open the URL below to authenticate:"
    $SUDO tailscale up --hostname=infistreamer \
      || warn "tailscale up failed — run 'sudo tailscale up' manually"
  fi
  local ts_ip; ts_ip="$($SUDO tailscale ip -4 2>/dev/null | head -n1 || true)"
  [ -n "$ts_ip" ] && log "Tailscale IP: ${ts_ip} — infistreamer will be reachable at http://${ts_ip}:${HOST_PORT:-8080}"
}
install_tailscale

# ---- Configuration ----
if [ ! -f .env ]; then
  log "Creating .env from .env.example"
  cp .env.example .env
  if command -v openssl >/dev/null 2>&1; then
    SECRET="$(openssl rand -hex 48)"
    sed -i "s|^SECRET_KEY=.*|SECRET_KEY=${SECRET}|" .env
    log "Generated a random SECRET_KEY"
  else
    warn "openssl not found — set SECRET_KEY in .env manually (>= 64 chars)"
  fi
  warn "EDIT .env NOW: set ADMIN_USERNAME / ADMIN_PASSWORD before exposing this service"
else
  log ".env already exists — leaving it untouched"
fi

mkdir -p data

# ---- Launch ----
log "Building and starting infistreamer..."
$SUDO $COMPOSE up -d --build

HOST_PORT="$(grep -E '^HOST_PORT=' .env | cut -d= -f2 || true)"; HOST_PORT="${HOST_PORT:-8080}"
log "infistreamer is up — open http://localhost:${HOST_PORT}"
log "Logs: $SUDO $COMPOSE logs -f    |    Stop: $SUDO $COMPOSE down"
