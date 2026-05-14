#!/usr/bin/env bash
# infistreamer installer: prompts for configuration, installs Docker + Compose on any
# major Linux distro (optionally Tailscale), and brings the stack up.
set -euo pipefail

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; NC='\033[0m'
log()  { echo -e "${GREEN}==>${NC} $*"; }
warn() { echo -e "${YELLOW}!  ${NC} $*"; }
die()  { echo -e "${RED}xx ${NC} $*" >&2; exit 1; }
ask()  { echo -ne "${YELLOW}? ${NC}$*"; }

cd "$(dirname "$0")"

[ "$(uname -s)" = "Linux" ] || die "this installer targets Linux; on macOS/Windows install Docker Desktop and run: docker compose up -d --build"

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  command -v sudo >/dev/null 2>&1 || die "please run as root or install sudo"
  SUDO="sudo"
fi

INTERACTIVE=0; [ -t 0 ] && INTERACTIVE=1

# ============================================================================
# All prompts happen up front, so the rest of the install runs unattended.
# ============================================================================

# ---- Configuration / admin account ----
if [ ! -f .env ]; then
  log "Creating .env from .env.example"
  cp .env.example .env

  if command -v openssl >/dev/null 2>&1; then
    sed -i "s|^SECRET_KEY=.*|SECRET_KEY=$(openssl rand -hex 48)|" .env
    log "Generated a random SECRET_KEY"
  else
    warn "openssl not found — set SECRET_KEY in .env manually (>= 64 chars)"
  fi

  admin_user="${ADMIN_USERNAME:-}"
  admin_pass="${ADMIN_PASSWORD:-}"
  if [ "$INTERACTIVE" = 1 ]; then
    [ -n "$admin_user" ] || { ask "Admin username [admin]: "; read -r admin_user; }
    admin_user="${admin_user:-admin}"
    while [ -z "$admin_pass" ]; do
      ask "Admin password: "; read -rs admin_pass; echo
    done
  fi
  if [ -n "$admin_user" ] && [ -n "$admin_pass" ]; then
    # rewrite the ADMIN_* lines safely (passwords may contain sed-special chars)
    grep -v -E '^ADMIN_(USERNAME|PASSWORD)=' .env > .env.tmp && mv .env.tmp .env
    printf 'ADMIN_USERNAME=%s\n' "$admin_user" >> .env
    printf 'ADMIN_PASSWORD=%s\n' "$admin_pass" >> .env
    log "Admin account configured: ${admin_user}"
  else
    warn "Set ADMIN_USERNAME / ADMIN_PASSWORD in .env before exposing this service"
  fi
else
  log ".env already exists — leaving it untouched"
fi

# ---- Tailscale (optional) ----
# Non-interactive: set INSTALL_TAILSCALE=yes|no and optionally TAILSCALE_AUTHKEY=tskey-...
want_ts="${INSTALL_TAILSCALE:-}"
if [ -z "$want_ts" ] && [ "$INTERACTIVE" = 1 ]; then
  ask "Install Tailscale for secure remote access? [y/N] "; read -r ans
  case "$ans" in [Yy]*) want_ts=yes ;; *) want_ts=no ;; esac
fi
ts_key="${TAILSCALE_AUTHKEY:-}"
if [ "$want_ts" = "yes" ] && [ -z "$ts_key" ] && [ "$INTERACTIVE" = 1 ]; then
  echo "  Tailscale auth: [1] auth key (best for headless servers)  [2] browser login"
  ask "Choose [1/2]: "; read -r m
  if [ "$m" = "1" ]; then
    ask "Paste Tailscale auth key: "; read -rs ts_key; echo
  fi
fi

# ============================================================================
# Unattended from here on.
# ============================================================================

# ---- Docker ----
if command -v docker >/dev/null 2>&1; then
  log "Docker already installed: $(docker --version)"
else
  log "Installing Docker (Ubuntu/Debian/Fedora/CentOS/RHEL/Arch/openSUSE)..."
  curl -fsSL https://get.docker.com | $SUDO sh \
    || die "Docker install failed — see https://docs.docker.com/engine/install/"
  $SUDO systemctl enable --now docker 2>/dev/null || true
  if [ -n "$SUDO" ] && [ -n "${SUDO_USER:-${USER:-}}" ]; then
    $SUDO usermod -aG docker "${SUDO_USER:-$USER}" || true
    warn "added ${SUDO_USER:-$USER} to the 'docker' group — log out/in for it to take effect"
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

# ---- Tailscale install + auth ----
if [ "$want_ts" = "yes" ]; then
  if command -v tailscale >/dev/null 2>&1; then
    log "Tailscale already installed: $(tailscale version | head -n1)"
  else
    log "Installing Tailscale..."
    curl -fsSL https://tailscale.com/install.sh | $SUDO sh \
      || warn "Tailscale install failed — skipping"
  fi
  if command -v tailscale >/dev/null 2>&1; then
    $SUDO systemctl enable --now tailscaled 2>/dev/null || true
    if [ -n "$ts_key" ]; then
      log "Authenticating Tailscale with the provided auth key..."
      $SUDO tailscale up --authkey="$ts_key" --hostname=infistreamer \
        || warn "tailscale up failed — run 'sudo tailscale up' manually"
    else
      # Browser login blocks until you authenticate, so run it in the background and
      # surface the login URL — the installer keeps going (this is the old hang).
      log "Starting Tailscale browser login (the installer continues; finish auth in a browser)..."
      ts_log="$(mktemp)"
      $SUDO tailscale up --hostname=infistreamer >"$ts_log" 2>&1 &
      for _ in $(seq 1 20); do
        grep -qE 'https://login\.tailscale\.com/[^[:space:]]+' "$ts_log" 2>/dev/null && break
        sleep 0.5
      done
      ts_url="$(grep -oE 'https://login\.tailscale\.com/[^[:space:]]+' "$ts_log" 2>/dev/null | head -n1 || true)"
      if [ -n "$ts_url" ]; then
        log "Authenticate Tailscale here: ${ts_url}"
      else
        warn "Could not capture the Tailscale login URL — run 'sudo tailscale up' manually"
      fi
    fi
    ts_ip="$($SUDO tailscale ip -4 2>/dev/null | head -n1 || true)"
    [ -n "$ts_ip" ] && log "Tailscale IP: ${ts_ip}"
  fi
else
  log "Skipping Tailscale"
fi

# ---- Launch ----
mkdir -p data
log "Building and starting infistreamer..."
$SUDO $COMPOSE up -d --build

HOST_PORT="$(grep -E '^HOST_PORT=' .env | cut -d= -f2 || true)"; HOST_PORT="${HOST_PORT:-8080}"
log "infistreamer is up — open http://localhost:${HOST_PORT}"
log "Logs: $SUDO $COMPOSE logs -f    |    Stop: $SUDO $COMPOSE down"
