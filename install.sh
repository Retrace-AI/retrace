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

# Browser control (Retrace's own MCP: 1000x1000 normalized screenshots + click
# coordinate remapping, driving real Chrome) is set up by DEFAULT: the installer
# checks for Chrome and installs it if absent, installs the MCP's Node deps, then
# wires the MCP. Opt out with  --no-browser  or  RETRACE_NO_BROWSER=1.
WITH_BROWSER="${RETRACE_WITH_BROWSER:-1}"
DO_UNINSTALL=0
DO_REINSTALL=0
for arg in "$@"; do
  case "$arg" in
    --with-browser) WITH_BROWSER=1 ;;
    --no-browser)   WITH_BROWSER=0 ;;
    --uninstall)    DO_UNINSTALL=1 ;;
    --reinstall)    DO_UNINSTALL=1; DO_REINSTALL=1 ;;
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

# --- uninstall -------------------------------------------------------------
# Fully remove Retrace: stop the proxy service, remove the commands + config +
# bundled Node + browser profile. Safe to run even if things are half-installed.
#   curl -fsSL <install.sh> | bash -s -- --uninstall     (remove)
#   curl -fsSL <install.sh> | bash -s -- --reinstall     (remove, then install fresh)
uninstall_retrace() {
  say "Removing Retrace..."
  pkill -f responses-chat-proxy 2>/dev/null || true
  pkill -f "$RETRACE_HOME/browser-mcp" 2>/dev/null || true
  if [ "$PLATFORM" = "macos" ]; then
    launchctl bootout "gui/$(id -u)/${PLIST_LABEL}" 2>/dev/null || true
    rm -f "$PLIST_DEST"
  else
    systemctl --user disable --now "${SYSTEMD_UNIT}" 2>/dev/null || true
    rm -f "$SYSTEMD_DIR/${SYSTEMD_UNIT}.service"
  fi
  # Remove node/npm/npx shims only if they point into RETRACE_HOME (i.e. ours).
  for l in node npm npx; do
    tgt="$(readlink "$BIN_DIR/$l" 2>/dev/null || true)"
    case "$tgt" in "$RETRACE_HOME"/*) rm -f "$BIN_DIR/$l" ;; esac
  done
  rm -f "$BIN_DIR/retrace" "$BIN_DIR/retrace-admin"
  rm -rf "$RETRACE_HOME"
  # Strip the PATH block we may have added to the shell profile.
  for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
    if [ -f "$rc" ] && grep -q '# >>> retrace PATH >>>' "$rc" 2>/dev/null; then
      sed -i.retracebak '/# >>> retrace PATH >>>/,/# <<< retrace PATH <<</d' "$rc" && rm -f "$rc.retracebak"
    fi
  done
  say "Retrace removed ($RETRACE_HOME, commands, proxy service, PATH entry)."
}

if [ "$DO_UNINSTALL" = "1" ]; then
  uninstall_retrace
  if [ "$DO_REINSTALL" != "1" ]; then
    printf '\n  Done. Reinstall any time with the install command.\n\n'
    exit 0
  fi
  say "Reinstalling a fresh copy..."
fi

command -v curl >/dev/null 2>&1 || die "curl is required."
command -v zsh  >/dev/null 2>&1 || die "zsh is required (the retrace launcher is a zsh script). Install it (e.g. 'apt install zsh') and re-run."

# Node.js (>=18) powers the local proxy + browser MCP. If it is missing or too
# old, install a private copy under $RETRACE_HOME/node — no admin/sudo needed,
# so mac users no longer have to install Node by hand and re-run.
NODE_MIN_MAJOR=18
NODE_VER="v20.18.1"
node_ok() {
  command -v node >/dev/null 2>&1 || return 1
  local maj; maj="$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)"
  [ "${maj:-0}" -ge "$NODE_MIN_MAJOR" ]
}
ensure_node() {
  node_ok && return 0
  local os arch nos narch
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os" in Darwin) nos=darwin ;; Linux) nos=linux ;; *) return 1 ;; esac
  case "$arch" in arm64|aarch64) narch=arm64 ;; x86_64|amd64) narch=x64 ;; *) return 1 ;; esac
  say "Node.js not found — installing a private copy (${NODE_VER}, no admin needed)..."
  local url="https://nodejs.org/dist/${NODE_VER}/node-${NODE_VER}-${nos}-${narch}.tar.gz"
  local dest="$RETRACE_HOME/node"
  mkdir -p "$dest" "$BIN_DIR"
  curl -fsSL "$url" -o "$RETRACE_HOME/.node.tgz" || { warn "could not download Node ($url)"; return 1; }
  tar -xzf "$RETRACE_HOME/.node.tgz" -C "$dest" --strip-components=1 || { warn "could not unpack Node"; return 1; }
  rm -f "$RETRACE_HOME/.node.tgz"
  export PATH="$dest/bin:$PATH"
  ln -sf "$dest/bin/node" "$BIN_DIR/node"
  ln -sf "$dest/bin/npm"  "$BIN_DIR/npm"
  ln -sf "$dest/bin/npx"  "$BIN_DIR/npx"
  node_ok
}
ensure_node || die "Node.js ${NODE_MIN_MAJOR}+ is required and the automatic install failed. Install Node from https://nodejs.org and re-run."
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
  say "Writing config with a free, no-key model (GPT-OSS 20B via Pollinations)..."
  cp "$TMP/config-skeleton/config.toml"   "$RETRACE_HOME/config.toml"
  cp "$TMP/config-skeleton/registry.json" "$RETRACE_HOME/registry.json"
  cp "$TMP/config-skeleton/models.json"   "$RETRACE_HOME/models.json"
  [ -f "$RETRACE_HOME/api_key" ] || printf 'local-proxy\n' > "$RETRACE_HOME/api_key"
  chmod 600 "$RETRACE_HOME/api_key"
else
  say "Existing config found — leaving it untouched."
fi

# --- Retrace branded fonts (Pixelify Sans + Departure Mono) -----------------
# A terminal app can't set its own font, so we install the fonts to the user's
# font directory; they pick one in their terminal settings for the Retrace look.
if [ -d "$TMP/runtime/fonts" ]; then
  case "$(uname -s)" in Darwin) FDIR="$HOME/Library/Fonts" ;; Linux) FDIR="$HOME/.local/share/fonts" ;; *) FDIR="" ;; esac
  if [ -n "$FDIR" ]; then
    mkdir -p "$FDIR"
    cp -f "$TMP/runtime/fonts/"*.ttf "$TMP/runtime/fonts/"*.otf "$FDIR/" 2>/dev/null || true
    command -v fc-cache >/dev/null 2>&1 && fc-cache -f "$FDIR" >/dev/null 2>&1 || true
    say "Fonts installed — set your terminal font to 'Pixelify Sans' or 'Departure Mono' for the Retrace look."
  fi
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
  # Install Retrace's own browser MCP (normalizes every screenshot to 1000x1000
  # and maps click coordinates back to real pixels — see runtime/browser-mcp).
  local mcp_dir="$RETRACE_HOME/browser-mcp"
  if [ -d "$TMP/runtime/browser-mcp" ]; then
    if ! command -v npm >/dev/null 2>&1; then
      warn "npm not found — skipping the browser MCP. Install Node/npm and re-run with --with-browser."
      return
    fi
    say "Installing the Retrace browser MCP (1000x1000 normalized vision)..."
    mkdir -p "$mcp_dir"
    cp "$TMP/runtime/browser-mcp/index.mjs" "$mcp_dir/index.mjs"
    cp "$TMP/runtime/browser-mcp/package.json" "$mcp_dir/package.json"
    ( cd "$mcp_dir" && npm install --omit=dev --no-audit --no-fund >/dev/null 2>&1 ) \
      || warn "npm install for the browser MCP failed — run 'cd $mcp_dir && npm install' by hand."
  else
    warn "runtime/browser-mcp not found in the install bundle; skipping browser MCP."
    return
  fi
  cat >> "$cfg" <<TOML

# Browser control for the model. Retrace's own browser MCP: every screenshot is
# normalized to a 1000x1000 image and the server maps the model's click
# coordinates in that space back to real page pixels (folds in retina/DPR and
# viewport). Keeps screenshots tiny so they never blow the context window.
# Set RETRACE_BROWSER_HEADLESS=1 to run without a visible window.
[mcp_servers.browser]
command = "node"
args = ["$mcp_dir/index.mjs"]
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

# --- PATH setup ------------------------------------------------------------
# If BIN_DIR isn't on PATH, append it to the user's shell profile (idempotently)
# so `retrace` is found in new shells. The current shell still needs a reload,
# so we flag that and print a highlighted notice at the end.
PATH_ADDED=0
PATH_RC=""
case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;  # already on PATH — nothing to do
  *)
    case "$(basename "${SHELL:-/bin/zsh}")" in
      bash) PATH_RC="$HOME/.bashrc" ;;
      *)    PATH_RC="$HOME/.zshrc" ;;
    esac
    if [ -f "$PATH_RC" ] && grep -q '# >>> retrace PATH >>>' "$PATH_RC" 2>/dev/null; then
      : # our block is already there
    else
      printf '\n# >>> retrace PATH >>>\nexport PATH="%s:$PATH"\n# <<< retrace PATH <<<\n' "$BIN_DIR" >> "$PATH_RC"
    fi
    PATH_ADDED=1
    export PATH="$BIN_DIR:$PATH"  # make retrace usable for the rest of this run
    ;;
esac

if [ "$PLATFORM" = "macos" ]; then
  UNINSTALL="launchctl bootout gui/$(id -u)/${PLIST_LABEL}; rm -rf \"$RETRACE_HOME\" \"$BIN_DIR/retrace\" \"$BIN_DIR/retrace-admin\" \"$PLIST_DEST\""
else
  UNINSTALL="systemctl --user disable --now ${SYSTEMD_UNIT}; rm -rf \"$RETRACE_HOME\" \"$BIN_DIR/retrace\" \"$BIN_DIR/retrace-admin\" \"$SYSTEMD_DIR/${SYSTEMD_UNIT}.service\""
fi

cat <<DONE

  Retrace installed.

  Start it:      retrace   (works immediately — free GPT-OSS 20B, no key)
  Your model:    a free shared model is preconfigured. For speed, privacy, and
                 stronger models, run  /model  ->  "Add custom model".

  Manage models: retrace-admin models list
  Browser:       set up by default (Retrace browser MCP + Chrome; 1000x1000
                 normalized vision, Chrome auto-installed if missing). Skip
                 next time with  --no-browser.
  Uninstall:     ${UNINSTALL}

DONE

# Highlighted, can't-miss PATH notice — only when we just added BIN_DIR to PATH.
if [ "$PATH_ADDED" = "1" ]; then
  Y=$'\033[1;33m'; B=$'\033[1;36m'; R=$'\033[0m'; BG=$'\033[43;30m'
  printf '%s╔════════════════════════════════════════════════════════════════════╗%s\n' "$Y" "$R"
  printf '%s║%s  %sACTION NEEDED — one step so the `retrace` command works%s            %s║%s\n' "$Y" "$R" "$BG" "$R" "$Y" "$R"
  printf '%s╠════════════════════════════════════════════════════════════════════╣%s\n' "$Y" "$R"
  printf '%s║%s  Added %s to your PATH in %s.\n' "$Y" "$R" "$BIN_DIR" "$PATH_RC"
  printf '%s║%s  Your CURRENT terminal does not have it yet. Do ONE of these:\n' "$Y" "$R"
  printf '%s║%s\n' "$Y" "$R"
  printf '%s║%s    1) Reload your shell:   %ssource %s%s\n' "$Y" "$R" "$B" "$PATH_RC" "$R"
  printf '%s║%s       (or just open a new terminal window), then run:  %sretrace%s\n' "$Y" "$R" "$B" "$R"
  printf '%s║%s\n' "$Y" "$R"
  printf '%s║%s    2) Or skip that and run the full path directly:\n' "$Y" "$R"
  printf '%s║%s          %s%s/retrace%s\n' "$Y" "$R" "$B" "$BIN_DIR" "$R"
  printf '%s╚════════════════════════════════════════════════════════════════════╝%s\n\n' "$Y" "$R"
fi
