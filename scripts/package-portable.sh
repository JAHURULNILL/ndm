#!/bin/bash
# NDM portable Linux build: one folder that runs on any modern Linux
# distro — no installation, no root. Bundles the app, the browser
# bridge and every component (yt-dlp, deno, ffmpeg), so video merging
# works out of the box even where no package manager package exists.
# The one system requirement is WebKitGTK 4.1 (the GUI library every
# Tauri app needs); install.sh checks for it and prints the exact
# package command for the distro it finds.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP=src-tauri/target/release/ndm-app
HOST=target/release/ndm-host
for f in "$APP" "$HOST" tools/yt-dlp tools/deno tools/ffmpeg; do
  [ -f "$f" ] || { echo "missing $f — build release binaries and fetch tools first" >&2; exit 1; }
done

PKG=.pkg-portable
rm -rf "$PKG"
mkdir -p "$PKG/ndm/components" "$PKG/ndm/extension"
cp "$APP" "$PKG/ndm/ndm-app"
cp "$HOST" "$PKG/ndm/ndm-host"
cp tools/yt-dlp tools/deno tools/ffmpeg "$PKG/ndm/components/"
cp extension/manifest.json extension/background.js extension/content.js \
   extension/page-hook.js extension/popup.html extension/popup.js \
   extension/icon16.png extension/icon48.png extension/icon128.png \
   "$PKG/ndm/extension/"
cp extension/icon128.png "$PKG/ndm/ndm.png"
strip "$PKG/ndm/components/ffmpeg" 2>/dev/null || true
chmod +x "$PKG/ndm/ndm-app" "$PKG/ndm/ndm-host" "$PKG/ndm/components/"*

cat > "$PKG/ndm/NDM.desktop" << 'EOF'
[Desktop Entry]
Type=Application
Name=NDM — Nill Download Manager
Comment=Fast download manager with browser integration
Exec=NDM_DIR/ndm-app
Icon=NDM_DIR/ndm.png
Categories=Network;FileTransfer;
Terminal=false
EOF

cat > "$PKG/ndm/install.sh" << 'EOF'
#!/bin/bash
# User-level setup for the portable build — no root needed:
#  * registers the browser bridge (native messaging host) for the
#    Chrome family and Firefox, pointing at THIS folder
#  * adds an NDM entry to the applications menu
# Run it again after moving the folder. The app also re-registers the
# bridge itself on every launch, so this is mostly about the menu.
set -eu
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# JSON-escape the path: a quote or backslash in the folder name must
# not corrupt the native-messaging manifest
DIR=${DIR//\\/\\\\}
DIR=${DIR//\"/\\"}
ID=njjmngpfmblaghapeenfdfkahhdocbic
MANIFEST="{\"name\": \"com.ndm.host\", \"description\": \"NDM native messaging host\", \"path\": \"$DIR/ndm-host\", \"type\": \"stdio\", \"allowed_origins\": [\"chrome-extension://$ID/\"]}"
for cfg in "$HOME/.config/google-chrome" "$HOME/.config/google-chrome-beta" \
           "$HOME/.config/chromium" "$HOME/.config/BraveSoftware/Brave-Browser" \
           "$HOME/.config/microsoft-edge" "$HOME/.config/vivaldi" "$HOME/.config/opera"; do
  if [ -d "$cfg" ]; then
    mkdir -p "$cfg/NativeMessagingHosts"
    printf '%s' "$MANIFEST" > "$cfg/NativeMessagingHosts/com.ndm.host.json"
    echo "browser bridge registered for: $cfg"
  fi
done
if [ -d "$HOME/.mozilla" ]; then
  mkdir -p "$HOME/.mozilla/native-messaging-hosts"
  printf '{"name": "com.ndm.host", "description": "NDM native messaging host", "path": "%s/ndm-host", "type": "stdio", "allowed_extensions": ["ndm@nill.download"]}' "$DIR" \
    > "$HOME/.mozilla/native-messaging-hosts/com.ndm.host.json"
  echo "browser bridge registered for: Firefox"
fi
mkdir -p "$HOME/.local/share/applications"
sed "s|NDM_DIR|$DIR|g" "$DIR/NDM.desktop" > "$HOME/.local/share/applications/ndm.desktop"
chmod +x "$HOME/.local/share/applications/ndm.desktop"
echo "menu entry added: NDM — Nill Download Manager"
if ! ldconfig -p 2>/dev/null | grep -q 'libwebkit2gtk-4.1\.so'; then
  echo ""
  echo "NOTE: the GUI library WebKitGTK 4.1 was not found. Install it once:"
  echo "  Debian/Ubuntu: sudo apt install libwebkit2gtk-4.1-0"
  echo "  Fedora:        sudo dnf install webkit2gtk4.1"
  echo "  Arch:          sudo pacman -S webkit2gtk-4.1"
  echo "  openSUSE:      sudo zypper install libwebkit2gtk-4_1-0"
fi
echo "Done. Start NDM with: $DIR/ndm-app"
EOF
chmod +x "$PKG/ndm/install.sh"

cat > "$PKG/ndm/README.txt" << 'EOF'
NDM — Nill Download Manager (portable Linux build)

Run ./install.sh once (no root needed), then start ./ndm-app.
Everything is inside this folder: the app, the browser bridge and
the video components (yt-dlp, deno, ffmpeg). To remove NDM, delete
the folder and ~/.local/share/applications/ndm.desktop.

Browser downloads: load the extension from the extension/ folder
(chrome://extensions → Developer mode → Load unpacked) and every
browser download — files, apps, documents, archives — will be
offered to NDM with its download dialog, like IDM. Videos get a
download pill on the player.
EOF

OUT="NDM_${VERSION}_Linux_Portable.tar.xz"
tar -cJf "$OUT" -C "$PKG" ndm
rm -rf "$PKG"
echo "built $OUT ($(stat -c%s "$OUT") bytes)"
