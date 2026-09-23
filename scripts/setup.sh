#!/usr/bin/env bash
# desk setup helper: builds desk, wires it to Cloudflare (Tunnel + Access + optional TURN),
# installs the systemd user services and creates your login. Safe to re-run.
#
#   ./scripts/setup.sh
#
# Nothing is exposed to the internet until Cloudflare Access is verified to protect your hostname.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/desk"
STATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/desk/state"
CONFIG="$CONFIG_DIR/config.toml"
CF_CONFIG="$CONFIG_DIR/cloudflared.yml"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
AGENT="$REPO/agent/target/release/desk-agent"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
step() { printf '\n\033[1;37;40m %s \033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }
die()  { printf '\n\033[31m✗ %s\033[0m\n' "$*" >&2; exit 1; }
ask()  { local reply; read -r -p "  $1 " reply; printf '%s' "$reply"; }
confirm() { local r; read -r -p "  $1 [Y/n] " r; [[ -z "$r" || "$r" =~ ^[Yy] ]]; }
json() { node -e "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{const j=JSON.parse(d);console.log(($1)(j)??'')})"; }

[[ $EUID -ne 0 ]] || die "Run this as your normal user, not root (it will ask for sudo only if needed)."

bold "desk setup"
echo "  Repo:   $REPO"
echo "  Config: $CONFIG"

# ---------------------------------------------------------------------------
step "1/9  Checking requirements"
# ---------------------------------------------------------------------------
[[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]] || die "desk currently supports Hyprland only. Run this from inside your Hyprland session."
ok "Hyprland session"

missing=()
for bin in cargo node npm cloudflared curl pkg-config cc gst-inspect-1.0 wl-copy wl-paste notify-send hyprctl systemctl; do
  command -v "$bin" >/dev/null 2>&1 && ok "$bin" || missing+=("$bin")
done
if command -v rustc >/dev/null 2>&1; then
  RUST_MINOR="$(rustc --version | sed -E 's/rustc 1\.([0-9]+).*/\1/')"
  if ((RUST_MINOR < 85)); then
    missing+=("rust>=1.85")
    warn "rustc $(rustc --version | cut -d' ' -f2) is too old; install a current toolchain with https://rustup.rs"
  fi
fi
if command -v node >/dev/null 2>&1 && (( $(node -p 'process.versions.node.split(".")[0]') < 18 )); then
  missing+=("node>=18")
fi
for el in pipewiresrc pulsesrc opusenc h264parse videoconvert; do
  gst-inspect-1.0 "$el" >/dev/null 2>&1 && ok "GStreamer $el" || missing+=("gstreamer:$el")
done
[[ -e /usr/lib/xdg-desktop-portal-hyprland || -e /usr/libexec/xdg-desktop-portal-hyprland \
   || -n "$(command -v xdg-desktop-portal-hyprland 2>/dev/null)" ]] \
  && ok "xdg-desktop-portal-hyprland" || missing+=("xdg-desktop-portal-hyprland")

if ((${#missing[@]})); then
  echo
  warn "Missing: ${missing[*]}"
  cat <<'EOF'

  Arch:          sudo pacman -S --needed base-devel pkgconf cmake rustup nodejs npm cloudflared curl \
                   wl-clipboard libnotify gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
                   gst-plugin-pipewire xdg-desktop-portal-hyprland && rustup default stable
  Debian/Ubuntu: sudo apt install build-essential pkg-config cmake nodejs npm curl wl-clipboard libnotify-bin \
                   libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-{base,good,bad} \
                   gstreamer1.0-pipewire gstreamer1.0-pulseaudio
                 Rust 1.85+: https://rustup.rs (distro cargo is often too old)
                 cloudflared: https://pkg.cloudflare.com
  Encoders:      NVIDIA works out of the box (gst-plugins-bad nvcodec).
                 AMD/Intel: install gst-plugin-va (Arch) / gstreamer1.0-plugins-bad + VA driver.
                 No GPU encoder: install gst-plugins-ugly (x264enc).
EOF
  die "Install the missing packages and run this script again."
fi

# ---------------------------------------------------------------------------
step "2/9  Input device access (/dev/uinput)"
# ---------------------------------------------------------------------------
if [[ -w /dev/uinput ]]; then
  ok "/dev/uinput is writable"
else
  warn "/dev/uinput is not writable by you. desk needs it to inject the remote mouse and keyboard."
  echo "  This installs a udev rule giving the logged-in user access (needs sudo):"
  echo "    $REPO/deploy/99-desk-uinput.rules → /etc/udev/rules.d/"
  if confirm "Install it now?"; then
    sudo install -m 644 "$REPO/deploy/99-desk-uinput.rules" /etc/udev/rules.d/99-desk-uinput.rules
    echo uinput | sudo tee /etc/modules-load.d/desk-uinput.conf >/dev/null
    sudo modprobe uinput
    sudo udevadm control --reload
    sudo udevadm trigger --name-match=uinput
    sleep 1
    [[ -w /dev/uinput ]] && ok "/dev/uinput is writable" || warn "Still not writable: log out and back in, then re-run."
  else
    warn "Skipped: remote viewing will work, remote control will not."
  fi
fi

# ---------------------------------------------------------------------------
step "3/9  Building desk"
# ---------------------------------------------------------------------------
(cd "$REPO/agent" && cargo build --release)
ok "agent built: $AGENT"
(cd "$REPO/web" && npm ci --silent && npm run build --silent)
ok "web client built: $REPO/web/dist"

# ---------------------------------------------------------------------------
step "4/9  Headless monitor picker"
# ---------------------------------------------------------------------------
# Hyprland's portal normally asks you to click a monitor. desk answers that dialog itself,
# but only for its own requests; every other app still gets the normal picker.
mkdir -p "$HOME/.local/bin" "$HOME/.config/hypr"
install -m 755 "$REPO/scripts/desk-picker" "$HOME/.local/bin/desk-picker"
XDPH="$HOME/.config/hypr/xdph.conf"
if grep -qs 'desk-picker' "$XDPH"; then
  ok "xdph.conf already uses desk-picker"
elif grep -qs 'custom_picker_binary' "$XDPH"; then
  warn "$XDPH already sets custom_picker_binary; not touching it."
  warn "Point it at $HOME/.local/bin/desk-picker manually (it falls back to hyprland-share-picker)."
else
  [[ -f "$XDPH" ]] && cp "$XDPH" "$XDPH.bak.$(date +%s)"
  printf '\n# Added by desk setup: answers screen-share requests from desk-agent, otherwise shows the normal picker.\nscreencopy {\n    custom_picker_binary = %s\n}\n' \
    "$HOME/.local/bin/desk-picker" >> "$XDPH"
  systemctl --user restart xdg-desktop-portal-hyprland 2>/dev/null || true
  ok "configured $XDPH"
fi

# ---------------------------------------------------------------------------
step "5/9  Your settings"
# ---------------------------------------------------------------------------
REUSE=0
if [[ -f "$CONFIG" ]] && confirm "Existing config found at $CONFIG. Keep it (skip Cloudflare setup)?"; then
  REUSE=1
  HOST="$(sed -n 's|^public_origin *= *"https://\([^"/]*\)".*|\1|p' "$CONFIG")"
  ok "keeping config for https://$HOST"
else
  echo "  desk will be reachable at a hostname on a domain you have on Cloudflare."
  HOST="$(ask "Hostname (e.g. desk.example.com):")"
  [[ "$HOST" =~ ^[A-Za-z0-9.-]+\.[A-Za-z]{2,}$ ]] || die "That doesn't look like a hostname."
  EMAIL="$(ask "Email address allowed to log in (Cloudflare sends the login code here):")"
  [[ "$EMAIL" == *@*.* ]] || die "That doesn't look like an email address."
fi

if ((REUSE == 0)); then
  # -------------------------------------------------------------------------
  step "6/9  Cloudflare Tunnel"
  # -------------------------------------------------------------------------
  if [[ ! -f "$HOME/.cloudflared/cert.pem" ]]; then
    echo "  Log in to Cloudflare and pick the zone for $HOST (a browser opens):"
    cloudflared tunnel login
  fi
  TUNNEL_NAME="desk-${HOST%%.*}"
  TUNNEL_ID="$(cloudflared tunnel list --output json 2>/dev/null | json "j=>(j.find(t=>t.name==='$TUNNEL_NAME')||{}).id")"
  if [[ -z "$TUNNEL_ID" ]]; then
    cloudflared tunnel create "$TUNNEL_NAME" >/dev/null
    TUNNEL_ID="$(cloudflared tunnel list --output json | json "j=>(j.find(t=>t.name==='$TUNNEL_NAME')||{}).id")"
  fi
  [[ -n "$TUNNEL_ID" ]] || die "Could not create or find tunnel $TUNNEL_NAME."
  CREDS="$HOME/.cloudflared/$TUNNEL_ID.json"
  [[ -f "$CREDS" ]] || die "Tunnel credentials $CREDS not found (was the tunnel created on another machine?)."
  mkdir -p "$CONFIG_DIR"
  cat > "$CF_CONFIG" <<EOF
# Tunnel for desk (written by scripts/setup.sh). Kept separate from ~/.cloudflared/config.yml.
tunnel: $TUNNEL_ID
credentials-file: $CREDS

ingress:
  - hostname: $HOST
    service: http://127.0.0.1:8750
    originRequest:
      httpHostHeader: $HOST
  - service: http_status:404
EOF
  ok "tunnel $TUNNEL_NAME ($TUNNEL_ID)"

  # -------------------------------------------------------------------------
  step "7/9  Cloudflare Access (login in front of desk)"
  # -------------------------------------------------------------------------
  cat <<EOF
  In the Cloudflare dashboard (https://one.dash.cloudflare.com), Zero Trust:

    1. First time only: choose a team name and the Free plan.
    2. Access controls → Applications → Add an application → Self-hosted
         Name:             desk
         Public hostname:  $HOST
         Session duration: 24 hours
    3. Add a policy:  Action Allow   Include → Emails → $EMAIL
    4. Login methods: One-time PIN (enabled by default)
    5. Save.

  The tunnel is NOT running yet, so nothing is exposed while you do this.
EOF
  read -r -p "  Press Enter once the Access application is saved… " _

  # Route DNS to *this* tunnel (explicit --config so ~/.cloudflared/config.yml can't interfere).
  if ! cloudflared tunnel --config "$CF_CONFIG" route dns "$TUNNEL_ID" "$HOST" 2>/tmp/desk-dns.err; then
    if grep -qi 'already exists' /tmp/desk-dns.err && confirm "A DNS record for $HOST already exists. Replace it?"; then
      cloudflared tunnel --config "$CF_CONFIG" route dns --overwrite-dns "$TUNNEL_ID" "$HOST"
    else
      cat /tmp/desk-dns.err >&2; die "Could not create the DNS record."
    fi
  fi
  ok "DNS $HOST → tunnel"

  # Verify Access is in front, and read the team domain + AUD tag from its login redirect.
  echo "  Verifying that Cloudflare Access protects https://$HOST …"
  LOCATION=""
  for _ in $(seq 1 24); do
    read -r CODE LOCATION < <(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "https://$HOST/" || true)
    [[ "$CODE" == 302 && "$LOCATION" == https://*.cloudflareaccess.com/* ]] && break
    LOCATION=""; sleep 5
  done
  [[ -n "$LOCATION" ]] || die "https://$HOST is not redirecting to Cloudflare Access. Check the application's hostname and re-run. (The tunnel is not running, so nothing is exposed.)"
  TEAM_DOMAIN="$(sed -E 's|https://([^/]+)/.*|\1|' <<<"$LOCATION")"
  AUD="$(sed -nE 's|.*[?&]kid=([0-9a-f]{64}).*|\1|p' <<<"$LOCATION")"
  [[ -n "$AUD" ]] || die "Could not read the AUD tag from the Access redirect."
  ok "Access active: team $TEAM_DOMAIN"

  # -------------------------------------------------------------------------
  step "8/9  Relay for strict networks (optional)"
  # -------------------------------------------------------------------------
  cat <<'EOF'
  Some networks (mobile hotspots, school/work Wi-Fi) block direct connections. A Cloudflare
  TURN key lets desk relay through Cloudflare there (first 1,000 GB/month free).
  Create one at: Cloudflare dashboard → Realtime → TURN Server → Create. Leave empty to skip.
EOF
  TURN_ID="$(ask "TURN Token ID:")"
  TURN_TOKEN=""
  if [[ -n "$TURN_ID" ]]; then
    read -r -s -p "  TURN API Token (hidden): " TURN_TOKEN; echo
    curl -sf -X POST "https://rtc.live.cloudflare.com/v1/turn/keys/$TURN_ID/credentials/generate-ice-servers" \
      -H "Authorization: Bearer $TURN_TOKEN" -H 'content-type: application/json' -d '{"ttl":60}' >/dev/null \
      && ok "TURN key works" || die "Cloudflare rejected that TURN key."
  fi

  mkdir -p "$STATE_DIR"
  umask 077
  {
    cat <<EOF
# Written by scripts/setup.sh — see agent/config.example.toml for all options.
listen = "127.0.0.1:8750"
public_origin = "https://$HOST"
state_dir = "$STATE_DIR"
web_dir = "$REPO/web/dist"
encoder = "auto"
udp_port = 0

[access]
team_domain = "$TEAM_DOMAIN"
aud = "$AUD"
allowed_email = "$EMAIL"
EOF
    if [[ -n "$TURN_ID" ]]; then
      printf '\n[turn]\nkey_id = "%s"\napi_token = "%s"\n' "$TURN_ID" "$TURN_TOKEN"
    fi
  } > "$CONFIG"
  chmod 600 "$CONFIG"
  umask 022
  ok "wrote $CONFIG"
fi

echo
"$AGENT" doctor --config "$CONFIG" || warn "Some checks failed (see above). desk may not work fully until they pass."

# ---------------------------------------------------------------------------
step "9/9  Login, services"
# ---------------------------------------------------------------------------
if [[ -f "$STATE_DIR/auth.json" ]] && ! confirm "A desk password already exists. Replace it (and the authenticator code)?"; then
  ok "keeping existing password"
else
  echo "  Choose a desk password (12+ characters), then scan the QR code with an authenticator app."
  "$AGENT" setup --config "$CONFIG"
fi

mkdir -p "$UNIT_DIR"
CLOUDFLARED="$(command -v cloudflared)"
for unit in desk-agent desk-tunnel; do
  sed -e "s|@REPO@|$REPO|g" -e "s|@CONFIG@|$CONFIG|g" -e "s|@CF_CONFIG@|$CF_CONFIG|g" -e "s|@CLOUDFLARED@|$CLOUDFLARED|g" \
    "$REPO/deploy/$unit.service.in" > "$UNIT_DIR/$unit.service"
done
systemctl --user daemon-reload
systemctl --user enable --now desk-tunnel.service >/dev/null
systemctl --user import-environment WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE XDG_CURRENT_DESKTOP XDG_RUNTIME_DIR
systemctl --user restart desk-agent.service

# desk-agent needs the running Hyprland session, so Hyprland starts it at every login.
HOOK_CMD='systemctl --user import-environment WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE XDG_CURRENT_DESKTOP XDG_RUNTIME_DIR && systemctl --user restart desk-agent'
HYPR_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/hypr"
if grep -rqs 'restart desk-agent' "$HYPR_DIR"; then
  ok "Hyprland already starts desk-agent at login"
elif [[ -f "$HYPR_DIR/hyprland.lua" ]]; then
  warn "Your Hyprland uses a Lua config. Add this inside your hl.on(\"hyprland.start\", …) handler:"
  echo "    hl.exec_cmd(\"$HOOK_CMD\")"
  read -r -p "  Press Enter once added… " _
else
  cp "$HYPR_DIR/hyprland.conf" "$HYPR_DIR/hyprland.conf.bak.$(date +%s)"
  printf '\n# desk: start the remote-desktop agent with this session'"'"'s environment\nexec-once = %s\n' "$HOOK_CMD" >> "$HYPR_DIR/hyprland.conf"
  ok "added exec-once to $HYPR_DIR/hyprland.conf"
fi
sleep 4
systemctl --user is-active --quiet desk-agent && ok "desk-agent running" || die "desk-agent failed: journalctl --user -u desk-agent"
systemctl --user is-active --quiet desk-tunnel && ok "desk-tunnel running" || die "desk-tunnel failed: journalctl --user -u desk-tunnel"

if [[ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != yes ]] && confirm "Start the tunnel at boot, before you log in (enable linger)?"; then
  loginctl enable-linger "$USER" && ok "linger enabled"
fi

CODE="$(curl -s -o /dev/null -w '%{http_code}' "https://$HOST/api/me" || true)"
[[ "$CODE" == 302 ]] && ok "public check: unauthenticated requests are redirected to Cloudflare Access" \
  || warn "unexpected public response ($CODE) — check the Access application."

echo
bold "Done. Open https://$HOST"
echo "  1. Enter your email → Cloudflare emails you a code."
echo "  2. Enter your desk password + authenticator code."
echo "  Logs: journalctl --user -u desk-agent -f"
echo
echo "  Note: desk can only stream a logged-in Hyprland session. For access after a reboot,"
echo "  enable autologin in your display manager (or greetd) so Hyprland starts on boot."
