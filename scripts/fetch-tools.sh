#!/usr/bin/env bash
# fetch-tools.sh — download the third-party sidecar binaries the app uses
# at runtime (yt-dlp for video, deno for YouTube challenges) into tools/.
# ffmpeg is NOT bundled: the only static builds segfault on some CPUs —
# the .deb declares a package dependency on the distro ffmpeg instead.
# They are deliberately NOT tracked in git (137 MB of platform binaries);
# the app finds them via the components dir, $NDM_TOOLS_DIR, or PATH.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p tools

YTDLP_VERSION="2026.10.26"
DENO_VERSION="v2.9.8"

# the release assets are per-architecture: pick the right ones
case "$(uname -m)" in
  x86_64)        YTDLP_ASSET="yt-dlp_linux";        DENO_ASSET="deno-x86_64-unknown-linux-gnu.zip" ;;
  aarch64|arm64) YTDLP_ASSET="yt-dlp_linux_aarch64"; DENO_ASSET="deno-aarch64-unknown-linux-gnu.zip" ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

# -f: an HTTP error must fail the download, not be saved AS the tool
if [ ! -x tools/yt-dlp ]; then
  echo "yt-dlp $YTDLP_VERSION ..."
  curl -fL -o tools/yt-dlp \
    "https://github.com/yt-dlp/yt-dlp/releases/download/$YTDLP_VERSION/$YTDLP_ASSET"
  chmod +x tools/yt-dlp
fi
if [ ! -x tools/deno ]; then
  echo "deno $DENO_VERSION ..."
  tmp="$(mktemp -d)"
  curl -fL -o "$tmp/deno.zip" \
    "https://github.com/denoland/deno/releases/download/$DENO_VERSION/$DENO_ASSET"
  (cd "$tmp" && unzip -o -q deno.zip)
  cp "$tmp/deno" tools/deno
  chmod +x tools/deno
  rm -rf "$tmp"
fi
# verify the tools actually run, and say so loudly if they do not —
# this line deciding nothing is how a broken sidecar once shipped
YT_VER="$(tools/yt-dlp --version 2>/dev/null || true)"
DENO_VER="$(tools/deno --version 2>/dev/null | head -1 || true)"
if [ -z "$YT_VER" ] || [ -z "$DENO_VER" ]; then
  echo "error: a downloaded tool does not run (yt-dlp: '$YT_VER', deno: '$DENO_VER')" >&2
  exit 1
fi
echo "tools ready: $YT_VER, $DENO_VER"
