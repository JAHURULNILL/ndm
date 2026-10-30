<div align="center">

# Nill Download Manager

**A fast, resumable download manager for Linux and Windows — multi-connection downloads, browser integration, a video downloader and a site grabber in one small native app.**

[![Latest release](https://img.shields.io/github/v/release/JAHURULNILL/ndm)](https://github.com/JAHURULNILL/ndm/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows-lightgrey)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust%20%2B%20Tauri%202-orange)

[Download](#download) · [Features](#features) · [Browser extension](#browser-extension) · [Building from source](#building-from-source) · [Website](https://ndm.zahurul.tech)

</div>

---

NDM is a desktop download manager written in Rust on top of Tauri 2. It splits
files into segments and downloads them over multiple connections, resumes
interrupted downloads exactly where they stopped, takes over downloads from
your browser, grabs videos and whole sites, and stays out of your way while
doing it — the app is a single ~5 MB installer with no bundled runtime.

The current release is **1.9.0**. See the
[release notes](https://github.com/JAHURULNILL/ndm/releases/tag/v1.9.0) for
what changed.

## Features

**Download engine**

- Multi-connection downloads with dynamic segmentation (8 connections by
  default, adjustable per download and per server)
- True pause / resume — progress is kept in a per-file block map, so a resumed
  download continues from the exact byte, even after an app restart
- Automatic retries (up to 10 attempts, 30 s apart); permanent errors such as
  404/410 are never retried
- Per-server connection limits with an exceptions list
- Global and per-download speed limits, plus a download quota per period
  (e.g. 200 MB per 5 hours)
- Proxy support: system proxy, PAC URL, or manual proxy with authentication,
  with separate on/off switches for HTTP, HTTPS and FTP

**Browser integration**

- Companion extension for Chrome, Chromium, Brave, Edge and Firefox
- File downloads are intercepted and handed to NDM — the browser's copy is
  only removed after NDM confirms it took the download
- A panel on the video player lists the available qualities; one click starts
  the download in NDM
- The browser session (cookies, referer) travels with login-gated downloads,
  so files and videos behind a sign-in download correctly

**Video**

- Downloads from 1,000+ sites through yt-dlp, merged with ffmpeg
- Quality picker with per-format sizes; HLS/DASH streams are sniffed directly
  in the page
- Split audio/video streams are paired and merged before a download may call
  itself finished — a video that was supposed to have sound never silently
  finishes without it

**Organising**

- Categories (Documents, Programs, Video, Music, Archives, …) that route each
  download to its own folder automatically
- Scheduler with named queues — each queue has its own time window,
  simultaneous-download cap and completion behaviour
- Site Grabber: a four-step wizard that crawls a site (depth limits for the
  same site and for other sites, extension and size filters, subfolder
  routing) and downloads everything it finds; projects can be saved and
  reused
- Site Logins: credentials saved per site are applied automatically to
  matching downloads
- Batch add (one URL per line), clipboard link monitoring, import/export of
  the download list, and a floating drop basket
- Customisable toolbar and list columns; the list refreshes in place every
  200 ms without rebuilding

**When downloads finish**

- Per-download, per-queue and global completion actions: open the file, open
  the folder, run a program — or turn off / restart / sleep / hibernate /
  log off the PC
- One-click in-app updates; a release can mark a minimum supported version
  when an update is mandatory

## Download

### Linux (.deb — Debian, Ubuntu and derivatives)

```bash
wget https://github.com/JAHURULNILL/ndm/releases/download/v1.9.0/NDM_1.9.0_amd64.deb
sudo apt install ./NDM_1.9.0_amd64.deb
```

The package depends on `libwebkit2gtk-4.1-0`, `libgtk-3-0` and `ffmpeg`,
which apt resolves automatically. On first launch the app quietly downloads
its video components (yt-dlp and deno, ~83 MB, verified against pinned
SHA-256 hashes before use) — nothing else to set up.

A portable tarball for other distributions can be built from source with
`scripts/package-portable.sh`.

### Windows

A portable Windows build (app + host + extension in one ZIP) is defined in
the [GitHub Actions workflow](.github/workflows/build.yml), which runs on
release tags. It targets Windows 10/11 with the WebView2 runtime
(preinstalled on Windows 11 and on updated Windows 10); a signed .msi
installer is a later step.

### Browser extension

The extension ships inside the .deb at `/usr/share/ndm/extension` and is
also attached to every release as `ndm-extension-<version>.zip`.

1. Unzip it (not needed for the .deb copy).
2. Open `chrome://extensions`, enable **Developer mode**, choose
   **Load unpacked** and select the extension folder.
3. Launch NDM once — it registers the native-messaging host with your
   browsers automatically.

Firefox can load it temporarily via `about:debugging`
(add-on ID `ndm@nill.download`); a signed Firefox build is on the roadmap.
Full details: [extension/README.md](extension/README.md).

## How it works

| Part | What it does |
|---|---|
| `src/lib.rs` | The download engine — segmentation, resume, retries, grabber crawl |
| `src/bin/ndm-host.rs` | Native-messaging host that bridges the browser extension and the app |
| `src/components.rs` | First-run component manager (yt-dlp / deno, SHA-256-verified) |
| `src/updater.rs` | Update checker driven by [`release.json`](release.json) in this repo |
| `src/main.rs` | Command-line frontend for the engine |
| `src-tauri/` | The Tauri application: state, scheduler, settings, IPC commands |
| `ui/` | The app window (plain HTML/CSS/JS, no framework) |
| `extension/` | The MV3 browser extension |

The extension never talks to the app over the network. It sends one JSON
message per action to the `com.ndm.host` native-messaging host, which queues
it for the app — requests are not lost if the app is still starting, and the
host starts the app when a download arrives while it is closed.

DRM-protected services (Netflix, Prime Video, Disney+, …) are excluded by
design: their streams are encrypted and cannot be downloaded by any download
manager.

## Building from source

Requirements: a stable Rust toolchain, and on Linux the WebKitGTK 4.1 /
GTK 3 development packages plus ffmpeg:

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev ffmpeg

cargo build --release                                     # engine, CLI, host
cargo build --release --manifest-path src-tauri/Cargo.toml  # desktop app
bash scripts/package-deb.sh                               # -> NDM_<version>_amd64.deb
```

## License

Dual-licensed under MIT or Apache-2.0, as declared in
[`Cargo.toml`](Cargo.toml).
