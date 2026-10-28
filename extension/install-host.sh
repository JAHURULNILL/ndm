#!/usr/bin/env bash
# install-host.sh — installs the NDM native messaging host (ndm-host) for
# Chrome/Chromium, and optionally Firefox, on Linux.
#
# Usage:
#   ./install-host.sh <chrome-extension-id> [--firefox]
#
# The extension id is shown on the browser's extensions page once the
# unpacked NDM extension is loaded. The ndm-host binary is copied to a
# stable path under the app data dir so repo rebuilds never break the
# manifests, and host manifests are generated from the templates next to
# this script.

set -euo pipefail

EXT_ID="${1:-}"
if [ -z "$EXT_ID" ]; then
    echo "usage: $0 <chrome-extension-id> [--firefox]" >&2
    exit 1
fi
# Chrome extension ids are 32 letters in a-p; anything else (a typo, a
# flag in the wrong position) would write manifests pointing at an
# extension that does not exist, failing silently much later
if ! [[ "$EXT_ID" =~ ^[a-p]{32}$ ]]; then
    echo "error: '$EXT_ID' does not look like a Chrome extension id (32 letters, a-p)" >&2
    echo "usage: $0 <chrome-extension-id> [--firefox]" >&2
    exit 1
fi

WANT_FIREFOX=0
for arg in "$@"; do
    [ "$arg" = "--firefox" ] && WANT_FIREFOX=1
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# 1. Build the host binary. Always: cargo's incremental build is cheap
# when nothing changed, and a stale binary copied "because it exists"
# once shipped an old bridge against a fixed extension.
HOST_BIN="$REPO_ROOT/target/release/ndm-host"
(cd "$REPO_ROOT" && cargo build --release --bin ndm-host)

# 2. Copy it to its stable location.
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
STABLE_DIR="$DATA_HOME/com.ndm.downloadmanager"
STABLE_BIN="$STABLE_DIR/ndm-host"
mkdir -p "$STABLE_DIR"
cp "$HOST_BIN" "$STABLE_BIN"
chmod +x "$STABLE_BIN"
echo "host binary: $STABLE_BIN"

# 3. Write one manifest from a template, substituting the placeholders.
#    $1 = template, $2 = target directory
write_manifest() {
    local template="$1" dir="$2"
    mkdir -p "$dir"
    sed -e "s|__HOST_PATH__|$STABLE_BIN|g" -e "s|__EXT_ID__|$EXT_ID|g" \
        "$template" > "$dir/com.ndm.host.json"
    echo "manifest:    $dir/com.ndm.host.json"
}

write_manifest "$SCRIPT_DIR/com.ndm.host.chrome.json.tmpl" \
    "$HOME/.config/google-chrome/NativeMessagingHosts"
write_manifest "$SCRIPT_DIR/com.ndm.host.chrome.json.tmpl" \
    "$HOME/.config/chromium/NativeMessagingHosts"
write_manifest "$SCRIPT_DIR/com.ndm.host.chrome.json.tmpl" \
    "$HOME/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts"
write_manifest "$SCRIPT_DIR/com.ndm.host.chrome.json.tmpl" \
    "$HOME/.config/microsoft-edge/NativeMessagingHosts"

if [ "$WANT_FIREFOX" -eq 1 ]; then
    write_manifest "$SCRIPT_DIR/com.ndm.host.firefox.json.tmpl" \
        "$HOME/.mozilla/native-messaging-hosts"
fi

echo "done — restart the browser so it picks up the host."
