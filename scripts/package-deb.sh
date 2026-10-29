#!/usr/bin/env bash
# Assemble the NDM .deb from the release binary (no cargo-tauri CLI
# needed). Mirrors the layout Tauri's bundler produces:
#   usr/bin/ndm-app, usr/share/applications/NDM.desktop,
#   usr/share/icons/hicolor/<size>/apps/ndm-app.png
# Usage: scripts/package-deb.sh [version]   (default: from tauri.conf.json)
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
  VERSION=$(python3 -c "import json; print(json.load(open('src-tauri/tauri.conf.json'))['version'])")
fi
BIN="src-tauri/target/release/ndm-app"
[ -x "$BIN" ] || { echo "release binary missing — run: cargo build --release --manifest-path src-tauri/Cargo.toml" >&2; exit 1; }
HOST_BIN="target/release/ndm-host"
[ -x "$HOST_BIN" ] || { echo "ndm-host missing — run: cargo build --release" >&2; exit 1; }

ARCH=$(dpkg --print-architecture)
OUT="NDM_${VERSION}_${ARCH}.deb"
# stage on the real disk, not /tmp: this VM's /tmp is a 512 MB tmpfs and
# the staged tree (app + ~140 MB of sidecar components) has overflowed it
# mid-compression more than once, truncating the .deb
mkdir -p "$PWD/.pkg-tmp"
STAGE=$(mktemp -d -p "$PWD/.pkg-tmp")
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/usr/bin" "$STAGE/usr/share/applications" "$STAGE/DEBIAN"
chmod 755 "$STAGE" "$STAGE/DEBIAN"   # mktemp -d is 700; dpkg-deb wants 755-775
install -m 755 "$BIN" "$STAGE/usr/bin/ndm-app"

# Browser bridge + the extension itself. The video engine (yt-dlp +
# deno) deliberately does NOT ship in the package: that is what keeps
# this installer IDM-sized (~3.6 MB). The app fetches both on first
# video use into its app-data components folder, verifying pinned
# SHA-256 hashes (see src/components.rs). ffmpeg does not ship
# either: the package Depends on the distro ffmpeg (DEBIAN/control),
# which find_tool locates on PATH.
mkdir -p "$STAGE/usr/lib/ndm" "$STAGE/usr/share/ndm/extension"
install -m 755 "$HOST_BIN" "$STAGE/usr/lib/ndm/ndm-host"
for f in manifest.json background.js content.js page-hook.js popup.html popup.js icon16.png icon48.png icon128.png; do
  install -m 644 "extension/$f" "$STAGE/usr/share/ndm/extension/$f"
done
install -m 755 scripts/ndm-setup-browser.sh "$STAGE/usr/bin/ndm-setup-browser"

for spec in "32x32 32x32.png" "64x64 64x64.png" "128x128 128x128.png" "256x256 128x128@2x.png" "512x512 icon.png"; do
  set -- $spec
  mkdir -p "$STAGE/usr/share/icons/hicolor/$1/apps"
  install -m 644 "src-tauri/icons/$2" "$STAGE/usr/share/icons/hicolor/$1/apps/ndm-app.png"
done

cat > "$STAGE/usr/share/applications/NDM.desktop" << 'EOF'
[Desktop Entry]
Categories=Network;FileTransfer;
Comment=NDM — fast, resumable downloads for Linux and Windows
Exec=ndm-app
StartupWMClass=ndm-app
Icon=ndm-app
Name=NDM
Terminal=false
Type=Application
EOF
# the build shell's umask can leave heredoc-created files at 660
# (root-only): a normal user's menu then cannot read the .desktop
# entry and the app silently never appears in the app list
chmod 644 "$STAGE/usr/share/applications/NDM.desktop"
find "$STAGE/usr/share" -type f -exec chmod a+r {} +

INSTALLED_KB=$(du -sk "$STAGE/usr" | cut -f1)
cat > "$STAGE/DEBIAN/control" << EOF
Package: ndm
Version: $VERSION
Architecture: $ARCH
Installed-Size: $INSTALLED_KB
Maintainer: Nill
Priority: optional
Depends: libwebkit2gtk-4.1-0, libgtk-3-0, ffmpeg
Description: NDM — fast, resumable downloads for Linux and Windows
 Nill Download Manager: multi-connection downloads with resume,
 browser integration, a video downloader and a site grabber.
 The browser extension lives in /usr/share/ndm/extension; the app
 registers the browser connection automatically on first launch.
EOF

# Zero-terminal browser setup: register the native messaging host for
# the installing user right away (the app also re-registers on every
# launch, so this is only a head start — best effort, never fatal).
cat > "$STAGE/DEBIAN/postinst" << 'POSTINST'
#!/bin/sh
U="${SUDO_USER:-}"
[ -n "$U" ] && [ "$U" != "root" ] || exit 0
H=$(getent passwd "$U" | cut -d: -f6)
[ -n "$H" ] && [ -d "$H" ] || exit 0
HOST=/usr/lib/ndm/ndm-host
ID=njjmngpfmblaghapeenfdfkahhdocbic
CBODY="{\"name\":\"com.ndm.host\",\"description\":\"NDM - Nill Download Manager native messaging host\",\"path\":\"$HOST\",\"type\":\"stdio\",\"allowed_origins\":[\"chrome-extension://$ID/\"]}"
for cfg in google-chrome google-chrome-beta chromium chromium-browser BraveSoftware/Brave-Browser microsoft-edge vivaldi opera; do
  [ -d "$H/.config/$cfg" ] || continue
  d="$H/.config/$cfg/NativeMessagingHosts"
  mkdir -p "$d" || continue
  printf '%s' "$CBODY" > "$d/com.ndm.host.json"
  chown -R "$U" "$d" 2>/dev/null || true
done
if [ -d "$H/.mozilla" ]; then
  d="$H/.mozilla/native-messaging-hosts"
  mkdir -p "$d" || exit 0
  printf '%s' "{\"name\":\"com.ndm.host\",\"description\":\"NDM - Nill Download Manager native messaging host\",\"path\":\"$HOST\",\"type\":\"stdio\",\"allowed_extensions\":[\"ndm@nill.download\"]}" > "$d/com.ndm.host.json"
  chown -R "$U" "$d" 2>/dev/null || true
fi
exit 0
POSTINST
chmod 755 "$STAGE/DEBIAN/postinst"

find "$STAGE" -type d -exec chmod 755 {} +
dpkg-deb --build --root-owner-group "$STAGE" "$OUT"
# a truncated archive must never pass as "built" (bit us once when /tmp
# filled up mid-build and the failure was silent)
dpkg-deb --info "$OUT" > /dev/null
[ "$(stat -c %s "$OUT")" -gt 1000000 ] || { echo "ERROR: $OUT is suspiciously small" >&2; exit 1; }
echo "built $OUT"
