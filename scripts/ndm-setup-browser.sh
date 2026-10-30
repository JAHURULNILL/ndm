#!/usr/bin/env bash
# NDM browser setup — run ONCE per user after installing the .deb.
#
# Connects your browser to the NDM app — fully automatic, nothing to
# copy: the extension's ID is fixed by the public key in its manifest
# (njjmngpfmblaghapeenfdfkahhdocbic), so this script writes the native
# messaging manifest for every browser it finds, Firefox included.
# Just load the extension once (chrome://extensions -> Developer mode
# -> "Load unpacked" -> /usr/share/ndm/extension) and run this script
# with no arguments. (An ID may still be passed to override, e.g. for
# a fork with a different key.)
# Safe to re-run any time (e.g. after moving the extension folder).
set -euo pipefail

HOST="/usr/lib/ndm/ndm-host"
EXT_DIR="/usr/share/ndm/extension"
[ -x "$HOST" ] || { echo "ndm-host not found at $HOST — is the NDM package installed?" >&2; exit 1; }

write_manifest() { # $1 = dir, $2 = json body
  mkdir -p "$1"
  printf '%s\n' "$2" > "$1/com.ndm.host.json"
  echo "  wrote $1/com.ndm.host.json"
}

echo "NDM native host: $HOST"

# validate the ID argument before ANY manifest is written: an invalid
# run must leave the system untouched, not half-configured
EXT_ID="${1:-njjmngpfmblaghapeenfdfkahhdocbic}"
if ! [[ "$EXT_ID" =~ ^[a-p]{32}$ ]]; then
  echo "error: '$EXT_ID' does not look like a Chrome extension ID (32 letters, a-p)" >&2
  exit 1
fi

# --- Firefox (extension ID is fixed: ndm@nill.download) ---
write_manifest "$HOME/.mozilla/native-messaging-hosts" \
  "{\"name\":\"com.ndm.host\",\"description\":\"Nill Download Manager native messaging host\",\"path\":\"$HOST\",\"type\":\"stdio\",\"allowed_extensions\":[\"ndm@nill.download\"]}"
echo "Firefox: done. In Firefox load the extension via about:debugging ->"
echo "  This Firefox -> Load Temporary Add-on -> $EXT_DIR/manifest.json"
echo "  (temporary add-ons last until Firefox restarts; a signed build is"
echo "   on the roadmap for a permanent install.)"

# --- Chrome family: the ID is fixed by the manifest key ---
BODY="{\"name\":\"com.ndm.host\",\"description\":\"Nill Download Manager native messaging host\",\"path\":\"$HOST\",\"type\":\"stdio\",\"allowed_origins\":[\"chrome-extension://$EXT_ID/\"]}"
FOUND=0
for cfg in "google-chrome" "google-chrome-beta" "chromium" "chromium-browser" "BraveSoftware/Brave-Browser" "microsoft-edge" "vivaldi" "opera"; do
  if [ -d "$HOME/.config/$cfg" ]; then
    write_manifest "$HOME/.config/$cfg/NativeMessagingHosts" "$BODY"
    FOUND=1
  fi
done
[ "$FOUND" -eq 1 ] || echo "  (no Chrome-family config dir found under ~/.config — manifest not written)"
echo "Chrome family: done for ID $EXT_ID"
echo "  If the extension is not loaded yet: chrome://extensions ->"
echo "  Developer mode -> Load unpacked -> $EXT_DIR (then restart the"
echo "  browser if it was already running)."

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "Tip: install ffmpeg (sudo apt install ffmpeg) so NDM can merge"
  echo "  best-quality video+audio into a single file."
fi
echo "Setup finished."
