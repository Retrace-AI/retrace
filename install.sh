#!/usr/bin/env bash
#
# Retrace installer — https://github.com/Retrace-AI/retrace
#
#   curl -fsSL https://raw.githubusercontent.com/Retrace-AI/retrace/main/install.sh | bash
#
# Installs the Retrace CLI: a local-first, provider-agnostic coding agent.
# macOS (launchd) and Linux/x86_64 (systemd --user) are supported.

set -euo pipefail

REPO="Retrace-AI/retrace"
RETRACE_HOME="${RETRACE_HOME:-$HOME/.retrace}"
BIN_DIR="${RETRACE_BIN_DIR:-$HOME/.local/bin}"
PLIST_LABEL="com.retrace.responses-proxy"
PLIST_DEST="$HOME/Library/LaunchAgents/${PLIST_LABEL}.plist"
SYSTEMD_UNIT="retrace-responses-proxy"
SYSTEMD_DIR="$HOME/.config/systemd/user"

# Browser control (Playwright MCP driving Chrome, vision/coordinate mode) is set
# up by DEFAULT: the installer checks for Chrome and installs it if absent, then
# wires the MCP. Opt out with  --no-browser  or  RETRACE_NO_BROWSER=1.
WITH_BROWSER="${RETRACE_WITH_BROWSER:-1}"
for arg in "$@"; do
  case "$arg" in
    --with-browser) WITH_BROWSER=1 ;;
    --no-browser)   WITH_BROWSER=0 ;;
  esac
done
[ "${RETRACE_NO_BROWSER:-0}" = "1" ] && WITH_BROWSER=0

say()  { printf '\033[1;36m==>\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33m warning:\033[0m %s\n' "$1" >&2; }
die()  { printf '\033[1;31m error:\033[0m %s\n' "$1" >&2; exit 1; }

# --- preflight -------------------------------------------------------------
OS="$(uname -s)"
case "$OS" in
  Darwin) PLATFORM="macos" ;;
  Linux)  PLATFORM="linux" ;;
  *) die "Unsupported OS: $OS. Retrace supports macOS and Linux." ;;
esac

command -v node >/dev/null 2>&1 || die "Node.js is required (for the local proxy). Install it and re-run."
command -v curl >/dev/null 2>&1 || die "curl is required."
command -v zsh  >/dev/null 2>&1 || die "zsh is required (the retrace launcher is a zsh script). Install it (e.g. 'apt install zsh') and re-run."
NODE_BIN="$(command -v node)"
NODE_DIR="$(dirname "$NODE_BIN")"

ARCH="$(uname -m)"   # arm64 / aarch64 / x86_64
case "$ARCH" in
  arm64|aarch64) ASSET_ARCH="aarch64" ;;
  x86_64|amd64)  ASSET_ARCH="x86_64" ;;
  *) die "Unsupported architecture: $ARCH" ;;
esac

if [ "$PLATFORM" = "linux" ] && [ "$ASSET_ARCH" != "x86_64" ]; then
  die "Linux builds are x86_64 only for now (got $ARCH)."
fi
if [ "$PLATFORM" = "linux" ] && ! command -v systemctl >/dev/null 2>&1; then
  die "systemd (systemctl) is required to run the proxy service on Linux."
fi

ASSET="retrace-${PLATFORM}-${ASSET_ARCH}.tar.gz"

say "Installing Retrace ($PLATFORM/$ASSET_ARCH) to $RETRACE_HOME"
mkdir -p "$RETRACE_HOME/bin" "$BIN_DIR"
[ "$PLATFORM" = "macos" ] && mkdir -p "$HOME/Library/LaunchAgents"
[ "$PLATFORM" = "linux" ] && mkdir -p "$SYSTEMD_DIR"

# --- resolve latest release tarball ----------------------------------------
say "Finding the latest release..."
TAG="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
  | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')" || true
[ -n "${TAG:-}" ] || die "No published release found yet for ${REPO}."
URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
say "Downloading ${ASSET} (${TAG})..."
curl -fsSL "$URL" -o "$TMP/retrace.tar.gz" || die "Download failed: $URL (no ${PLATFORM}/${ASSET_ARCH} build in ${TAG}?)"
tar -xzf "$TMP/retrace.tar.gz" -C "$TMP"

# Tarball layout: retrace-bin, runtime/*, config-skeleton/*, service template
say "Installing binary and runtime..."
install -m 0755 "$TMP/retrace-bin"                      "$RETRACE_HOME/bin/retrace-bin"
install -m 0644 "$TMP/runtime/responses-chat-proxy.mjs" "$RETRACE_HOME/responses-chat-proxy.mjs"
install -m 0755 "$TMP/runtime/retrace-admin.mjs"        "$RETRACE_HOME/bin/retrace-admin.mjs"
install -m 0755 "$TMP/runtime/retrace"                  "$BIN_DIR/retrace"
install -m 0755 "$TMP/runtime/retrace-admin"            "$BIN_DIR/retrace-admin"

# --- first-run config skeleton (never overwrite an existing one) -----------
if [ ! -f "$RETRACE_HOME/config.toml" ]; then
  say "Writing a fresh, empty config (no providers yet)..."
  cp "$TMP/config-skeleton/config.toml"   "$RETRACE_HOME/config.toml"
  cp "$TMP/config-skeleton/registry.json" "$RETRACE_HOME/registry.json"
  cp "$TMP/config-skeleton/models.json"   "$RETRACE_HOME/models.json"
  [ -f "$RETRACE_HOME/api_key" ] || printf 'placeholder-not-a-real-key\n' > "$RETRACE_HOME/api_key"
  chmod 600 "$RETRACE_HOME/api_key"
else
  say "Existing config found — leaving it untouched."
fi

# --- optional: browser control MCP (Playwright, vision/coordinate mode) -----
setup_browser_mcp() {
  local cfg="$RETRACE_HOME/config.toml"
  if grep -q '^\[mcp_servers.browser\]' "$cfg" 2>/dev/null; then
    say "Browser MCP already configured — leaving it."
    return
  fi
  # Playwright's --browser chrome drives real Google Chrome. Install it if absent.
  local have_chrome=0
  [ -d "/Applications/Google Chrome.app" ] && have_chrome=1
  command -v google-chrome >/dev/null 2>&1 && have_chrome=1
  command -v google-chrome-stable >/dev/null 2>&1 && have_chrome=1
  if [ "$have_chrome" = "0" ]; then
    say "Chrome not found — installing it for browser control..."
    if [ "$PLATFORM" = "macos" ] && command -v brew >/dev/null 2>&1; then
      brew install --cask google-chrome || warn "Chrome install via Homebrew failed."
    else
      npx -y playwright install chrome >/dev/null 2>&1 \
        || warn "Could not auto-install Chrome. Install Google Chrome manually, then re-run with --with-browser."
    fi
  fi
  say "Adding the browser control MCP (Playwright, vision mode)..."
  cat >> "$cfg" <<'TOML'

# Browser control for the model (Playwright driving Chrome).
# Vision mode exposes coordinate tools (browser_mouse_click_xy, ...), suited to
# grounding/vision models that emit x,y. Requires a vision-capable model.
[mcp_servers.browser]
command = "npx"
args = ["-y", "@playwright/mcp@latest", "--browser", "chrome", "--caps", "vision"]
TOML
}

if [ "$WITH_BROWSER" = "1" ]; then
  setup_browser_mcp
fi

# --- proxy service ---------------------------------------------------------
say "Setting up the local proxy service..."
if [ "$PLATFORM" = "macos" ]; then
  sed -e "s#__NODE__#${NODE_BIN}#g" \
      -e "s#__NODE_DIR__#${NODE_DIR}#g" \
      -e "s#__HOME__#${HOME}#g" \
      "$TMP/launchd/com.retrace.responses-proxy.plist.template" > "$PLIST_DEST"
  launchctl bootout   "gui/$(id -u)/${PLIST_LABEL}" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST_DEST" 2>/dev/null || launchctl load "$PLIST_DEST" 2>/dev/null || true
else
  sed -e "s#__NODE__#${NODE_BIN}#g" \
      -e "s#__NODE_DIR__#${NODE_DIR}#g" \
      -e "s#__HOME__#${HOME}#g" \
      "$TMP/systemd/${SYSTEMD_UNIT}.service.template" > "$SYSTEMD_DIR/${SYSTEMD_UNIT}.service"
  # Keep the user service running without an active login session.
  loginctl enable-linger "$USER" 2>/dev/null || warn "Could not enable linger; the proxy may stop when you log out."
  systemctl --user daemon-reload 2>/dev/null || true
  systemctl --user enable --now "${SYSTEMD_UNIT}.service" 2>/dev/null \
    || warn "Could not start the proxy via systemctl --user. Start it manually: systemctl --user enable --now ${SYSTEMD_UNIT}"
fi

# --- PATH hint --------------------------------------------------------------
case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *) warn "$BIN_DIR is not on your PATH. Add this to your shell profile:"
     printf '\n    export PATH="%s:$PATH"\n\n' "$BIN_DIR" ;;
esac

if [ "$PLATFORM" = "macos" ]; then
  UNINSTALL="launchctl bootout gui/$(id -u)/${PLIST_LABEL}; rm -rf \"$RETRACE_HOME\" \"$BIN_DIR/retrace\" \"$BIN_DIR/retrace-admin\" \"$PLIST_DEST\""
else
  UNINSTALL="systemctl --user disable --now ${SYSTEMD_UNIT}; rm -rf \"$RETRACE_HOME\" \"$BIN_DIR/retrace\" \"$BIN_DIR/retrace-admin\" \"$SYSTEMD_DIR/${SYSTEMD_UNIT}.service\""
fi

cat <<DONE

  Retrace installed.

  Start it:      retrace
  First step:    inside Retrace, run  /model  ->  "Add custom model"
                 and enter your provider's URL + API key.

  Manage models: retrace-admin models list
  Browser:       set up by default (Playwright + Chrome, vision mode; Chrome
                 auto-installed if missing). Skip next time with  --no-browser.
  Uninstall:     ${UNINSTALL}

DONE
