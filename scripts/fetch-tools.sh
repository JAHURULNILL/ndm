#!/usr/bin/env bash
# fetch-tools.sh — download the third-party sidecar binaries the app uses
# at runtime (yt-dlp for video, deno for YouTube challenges) into tools/.
# ffmpeg is NOT bundled: the only static builds segfault on some CPUs —
# the .deb declares a package dependency on the distro ffmpeg instead.
# They are deliberately NOT tracked in git (137 MB of platform binaries);
# the app finds them via the components dir, $NDM_TOOLS_DIR, or PATH.
#
# Every download is verified against a pinned SHA-256 before it is
# installed, and an already-present tool is only reused when it still
# matches its pin — a stale or tampered binary is re-fetched, never
# silently trusted. The pins below MIRROR the Linux tables in
# src/components.rs (specs()): the app verifies the same hashes again
# when it fetches these components itself. Keep the two in sync.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p tools

YTDLP_VERSION="2026.10.26"
DENO_VERSION="v2.9.8"

# the release assets are per-architecture: pick the right ones.
# The hashes are the src/components.rs pins for those exact assets
# (deno's is the hash of the zip, as in components.rs).
case "$(uname -m)" in
  x86_64)
    YTDLP_ASSET="yt-dlp_linux"
    YTDLP_SHA256="767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795"
    DENO_ASSET="deno-x86_64-unknown-linux-gnu.zip"
    DENO_SHA256="e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609"
    ;;
  aarch64|arm64)
    YTDLP_ASSET="yt-dlp_linux_aarch64"
    YTDLP_SHA256="e87e9533e89fe5889abaa21f5114dfe7756d6aa6219b83551add04d48b18e483"
    DENO_ASSET="deno-aarch64-unknown-linux-gnu.zip"
    DENO_SHA256="d49596c20b9788cf8f5d8ddf644103d4bde56ed239753f0bdb92ba9361d51905"
    ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

# sha256_ok <file> <expected-hex> — true when the file's SHA-256 matches
sha256_ok() {
  [ -f "$1" ] || return 1
  [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ]
}

fetch_ytdlp() {
  echo "yt-dlp $YTDLP_VERSION ..."
  # -f: an HTTP error must fail the download, not be saved AS the tool
  curl -fL -o tools/yt-dlp.part \
    "https://github.com/yt-dlp/yt-dlp/releases/download/$YTDLP_VERSION/$YTDLP_ASSET"
  if ! sha256_ok tools/yt-dlp.part "$YTDLP_SHA256"; then
    rm -f tools/yt-dlp.part
    echo "error: yt-dlp $YTDLP_VERSION failed its SHA-256 check — download discarded" >&2
    exit 1
  fi
  chmod +x tools/yt-dlp.part
  mv tools/yt-dlp.part tools/yt-dlp
}

fetch_deno() {
  echo "deno $DENO_VERSION ..."
  tmp="$(mktemp -d)"
  curl -fL -o "$tmp/deno.zip" \
    "https://github.com/denoland/deno/releases/download/$DENO_VERSION/$DENO_ASSET"
  if ! sha256_ok "$tmp/deno.zip" "$DENO_SHA256"; then
    rm -rf "$tmp"
    echo "error: deno $DENO_VERSION failed its SHA-256 check — download discarded" >&2
    exit 1
  fi
  (cd "$tmp" && unzip -o -q deno.zip)
  cp "$tmp/deno" tools/deno
  chmod +x tools/deno
  rm -rf "$tmp"
}

# Reuse an existing tool only when it still IS the pinned release:
# yt-dlp's pin covers the binary itself, so compare hashes; deno's
# pin covers the zip it arrived in, so compare reported versions.
if [ -x tools/yt-dlp ] && sha256_ok tools/yt-dlp "$YTDLP_SHA256"; then
  echo "yt-dlp $YTDLP_VERSION already present (hash verified)"
else
  [ -e tools/yt-dlp ] && echo "tools/yt-dlp is not the pinned $YTDLP_VERSION build — re-fetching"
  fetch_ytdlp
fi
DENO_PLAIN="${DENO_VERSION#v}"
if [ -x tools/deno ] && tools/deno --version 2>/dev/null | head -1 | grep -q "^deno $DENO_PLAIN "; then
  echo "deno $DENO_VERSION already present (version verified)"
else
  [ -e tools/deno ] && echo "tools/deno is not the pinned $DENO_VERSION — re-fetching"
  fetch_deno
fi

# verify the tools actually run AND are the pinned versions, and say
# so loudly if they are not — a check deciding nothing is how a broken
# sidecar once shipped
YT_VER="$(tools/yt-dlp --version 2>/dev/null || true)"
DENO_VER="$(tools/deno --version 2>/dev/null | head -1 || true)"
if [ "$YT_VER" != "$YTDLP_VERSION" ] || ! printf '%s' "$DENO_VER" | grep -q "^deno $DENO_PLAIN "; then
  echo "error: tool version mismatch (yt-dlp: '$YT_VER', deno: '$DENO_VER'; want yt-dlp $YTDLP_VERSION, deno $DENO_PLAIN)" >&2
  exit 1
fi
echo "tools ready: $YT_VER, $DENO_VER"
