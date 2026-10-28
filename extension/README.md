# NDM Browser Extension

Companion extension for **NDM — Nill Download Manager**. It hands downloads
from the browser to the NDM desktop app, the way a classic download-manager
extension does:

- **Video panel** — on YouTube, Facebook and other video pages a small
  floating card appears at the bottom-right: **⬇ Download with NDM**.
  One click sends the page to NDM, which downloads the video with its
  built-in video engine (quality picker inside the app).
- **Right-click menus** —
  - *Download with NDM* on any link (video-page links are sent as videos),
  - *Download video with NDM* on a `<video>` element,
  - *Download this page's video with NDM* on supported video sites.
- **Download interception** (on by default, toggle in the popup) — when the
  browser starts downloading a common file type (zip/rar/7z, exe/msi/deb,
  iso, apk, pdf, Office documents, mp3/mp4/mkv, torrent…), the browser
  download is cancelled and the file is sent to NDM instead.
- **Popup** — shows whether the NDM bridge (native messaging host) is
  reachable, the interception toggle, and a one-click
  "Download current page video" button on video pages.

## How it talks to the app

The extension never talks to the app directly over the network. It uses
**Native Messaging**: every action is one JSON object sent to the host
program registered as **`com.ndm.host`**:

```json
{ "type": "download", "kind": "video", "url": "https://…", "pageUrl": "https://…", "title": "…" }
```

`kind` is `"video"` (handled by NDM's video engine) or `"file"` (handled by
the multi-connection file engine). The host appends each message to a queue
file that the NDM app watches, so requests are not lost even if the app is
still starting.

## Install (developer / unpacked)

The extension must be loaded once, and the native messaging host manifest
must name this extension's ID (see below).

### Chrome / Chromium / Edge / Brave

1. Open `chrome://extensions`.
2. Turn on **Developer mode** (top-right).
3. Click **Load unpacked** and select this `extension/` folder.
4. Note the **ID** shown on the extension's card — you need it for the
   host manifest.

### Firefox

1. Open `about:debugging#/runtime/this-firefox`.
2. Click **Load Temporary Add-on…** and select `manifest.json` in this
   folder. (Temporary add-ons are removed when Firefox restarts; a signed
   build is needed for a permanent install.)
3. The Firefox add-on ID is fixed: `ndm@nill.download`
   (from `browser_specific_settings.gecko.id` in the manifest).

## Finding the extension ID (needed for the host manifest)

The native messaging host manifest installed on your system must list this
extension as an allowed origin, otherwise the browser refuses the
connection and the popup shows "NDM bridge not found".

- **Chrome/Chromium:** the ID is the 32-character string on the
  extension's card at `chrome://extensions` (Developer mode). The host
  manifest's `allowed_origins` must contain
  `chrome-extension://<THAT-ID>/`.
- **Firefox:** the ID is `ndm@nill.download`; the host manifest's
  `allowed_extensions` must contain it.

## Limitations

- **blob: video sources** — many players stream through `blob:` URLs that
  only exist inside the page. For those, the extension sends the **page
  URL** instead and NDM's video engine resolves the real streams.
- **DRM sites are excluded by design** — no panel and no video capture on
  Netflix, Prime Video, Disney+ or Hotstar; their streams are encrypted
  and cannot be downloaded by any download manager.
- **Interception is all-or-nothing per download** — the browser API does
  not reveal a file's size before the download starts, so even small files
  of a listed type are handed to NDM. Turn the toggle off in the popup if
  you prefer the browser for small files.
- **Facebook private videos** — videos that need a login are resolved by
  the app side; the extension only sends the page URL.
- **Firefox installs are temporary** in developer mode (see above).

## Files

| File | Purpose |
|---|---|
| `manifest.json` | MV3 manifest (Chrome + Firefox) |
| `background.js` | Service worker: menus, native messaging, interception |
| `content.js` | Video detection + floating "Download with NDM" panel |
| `popup.html` / `popup.js` | Toolbar popup: status, toggle, send button |
| `icon16/48/128.png` | Extension icons (regenerate with `gen_icons.py`) |
