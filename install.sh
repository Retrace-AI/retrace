#!/usr/bin/env bash
#
# Retrace installer — https://github.com/Retrace-AI/retrace
#
#   curl -fsSL https://raw.githubusercontent.com/Retrace-AI/retrace/main/install.sh | bash
#
# Installs the Retrace CLI: a local-first, provider-agnostic coding agent.
# macOS only for v1 (uses launchd + Seatbelt sandbox).

set -euo pipefail

REPO="Retrace-AI/retrace"
RETRACE_HOME="${RETRACE_HOME:-$HOME/.retrace}"
BIN_DIR="${RETRACE_BIN_DIR:-$HOME/.local/bin}"
PLIST_LABEL="com.retrace.responses-proxy"
PLIST_DEST="$HOME/Library/LaunchAgents/${PLIST_LABEL}.plist"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33m warning:\033[0m %s\n' "$1" >&2; }
die()  { printf '\033[1;31m error:\033[0m %s\n' "$1" >&2; exit 1; }

# --- preflight -------------------------------------------------------------
[ "$(uname -s)" = "Darwin" ] || die "Retrace v1 supports macOS only. Linux/Windows are on the roadmap."
command -v node >/dev/null 2>&1 || die "Node.js is required (for the local proxy). Install it (e.g. 'brew install node') and re-run."
command -v curl >/dev/null 2>&1 || die "curl is required."
NODE_BIN="$(command -v node)"
NODE_DIR="$(dirname "$NODE_BIN")"

ARCH="$(uname -m)"   # arm64 or x86_64
case "$ARCH" in
  arm64|aarch64) ASSET_ARCH="aarch64" ;;
  x86_64|amd64)  ASSET_ARCH="x86_64" ;;
  *) die "Unsupported architecture: $ARCH" ;;
esac

say "Installing Retrace to $RETRACE_HOME"
mkdir -p "$RETRACE_HOME/bin" "$BIN_DIR" "$HOME/Library/LaunchAgents"

# --- resolve latest release tarball ----------------------------------------
say "Finding the latest release..."
TAG="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
  | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')" || true
[ -n "${TAG:-}" ] || die "No published release found yet for ${REPO}."
ASSET="retrace-macos-${ASSET_ARCH}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
say "Downloading ${ASSET} (${TAG})..."
curl -fsSL "$URL" -o "$TMP/retrace.tar.gz" || die "Download failed: $URL"
tar -xzf "$TMP/retrace.tar.gz" -C "$TMP"

# Tarball layout: retrace-bin, runtime/*, config-skeleton/*, launchd template
say "Installing binary and runtime..."
install -m 0755 "$TMP/retrace-bin"                 "$RETRACE_HOME/bin/retrace-bin"
install -m 0644 "$TMP/runtime/responses-chat-proxy.mjs" "$RETRACE_HOME/responses-chat-proxy.mjs"
install -m 0755 "$TMP/runtime/retrace-admin.mjs"   "$RETRACE_HOME/bin/retrace-admin.mjs"
install -m 0755 "$TMP/runtime/retrace"             "$BIN_DIR/retrace"
install -m 0755 "$TMP/runtime/retrace-admin"       "$BIN_DIR/retrace-admin"

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

# --- launchd proxy agent ----------------------------------------------------
say "Setting up the local proxy service..."
sed -e "s#__NODE__#${NODE_BIN}#g" \
    -e "s#__NODE_DIR__#${NODE_DIR}#g" \
    -e "s#__HOME__#${HOME}#g" \
    "$TMP/launchd/com.retrace.responses-proxy.plist.template" > "$PLIST_DEST"
launchctl bootout   "gui/$(id -u)/${PLIST_LABEL}" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST_DEST" 2>/dev/null || launchctl load "$PLIST_DEST" 2>/dev/null || true

# --- PATH hint --------------------------------------------------------------
case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *) warn "$BIN_DIR is not on your PATH. Add this to your shell profile:"
     printf '\n    export PATH="%s:$PATH"\n\n' "$BIN_DIR" ;;
esac

cat <<DONE

  Retrace installed.

  Start it:      retrace
  First step:    inside Retrace, run  /model  ->  "Add custom model"
                 and enter your provider's URL + API key.

  Manage models: retrace-admin models list
  Uninstall:     launchctl bootout gui/$(id -u)/${PLIST_LABEL}; rm -rf "$RETRACE_HOME" "$BIN_DIR/retrace" "$BIN_DIR/retrace-admin" "$PLIST_DEST"

DONE
