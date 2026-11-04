//! Shared video probing and sidecar tool discovery (used by both the
//! `ndm-host` native messaging bridge for the in-browser IDM popup and
//! the `ndm-app` Tauri desktop backend).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QualityOption {
    pub selector: String,
    pub label: String,
    pub ext: String,
    pub note: String,
    pub size_bytes: u64,
    /// True when `size_bytes` is an estimate (yt-dlp's
    /// filesize_approx or a bitrate×duration guess) rather than an
    /// exact filesize — the picker shows such sizes as approximate
    /// instead of stating them as fact. Defaults to false when an
    /// older saved payload has no such field.
    #[serde(default)]
    pub size_approx: bool,
    pub size_text: String,
    pub audio: bool,
    pub height: u64,
    /// Which door produced this option: "" (or "ytdlp") for the
    /// yt-dlp probe, "browser" for a stream captured from the
    /// user's own browser tab (the browser-capture door, 2.3.0).
    /// Older payloads have no field → the yt-dlp door.
    #[serde(default)]
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoInfo {
    pub title: String,
    pub duration: u64,
    pub thumbnail: String,
    pub uploader: String,
    pub options: Vec<QualityOption>,
}

/// Format a byte count into a clean human-readable string ("77.2 MB", "1.24 GB").
pub fn fmt_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        let m = b / MB;
        if m >= 100.0 {
            format!("{:.0} MB", m)
        } else {
            format!("{:.1} MB", m)
        }
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Locate a helper tool: app-data components dir -> folder next to the
/// running executable -> standard user data dir -> $NDM_TOOLS_DIR (dev) -> PATH.
pub fn find_tool(name: &str, components_dir: &Path) -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if !components_dir.as_os_str().is_empty() {
        candidates.push(components_dir.join(&exe_name));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(&exe_name));
            candidates.push(dir.join("components").join(&exe_name));
            if let Some(prefix) = dir.parent() {
                candidates.push(prefix.join("lib/ndm/components").join(&exe_name));
                candidates.push(prefix.join("bin").join(&exe_name));
            }
        }
    }
    if let Some(default_comp) = default_components_dir() {
        candidates.push(default_comp.join(&exe_name));
    }
    if cfg!(debug_assertions) {
        if let Ok(dir) = std::env::var("NDM_TOOLS_DIR") {
            candidates.push(PathBuf::from(dir).join(&exe_name));
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            candidates.push(dir.join(&exe_name));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

pub fn default_components_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|p| PathBuf::from(p).join("com.ndm.downloadmanager").join("components"))
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
            return Some(
                PathBuf::from(xdg)
                    .join("com.ndm.downloadmanager")
                    .join("components"),
            );
        }
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".local/share/com.ndm.downloadmanager/components")
        })
    }
}

pub fn is_youtube_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    let after_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    let host = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
        || host.ends_with(".youtu.be")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com")
}

pub fn cookie_file_usable_for_url(url: &str, file: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else {
        return false;
    };
    let mut has_youtube_auth = false;
    let mut has_non_youtube = false;
    for line in text.lines() {
        let l = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if l.starts_with('#') || l.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = l.split('\t').collect();
        if parts.len() < 7 {
            continue;
        }
        let domain = parts[0].to_ascii_lowercase();
        let name = parts[5];
        if domain.contains("youtube.com")
            || domain.contains("google.com")
            || domain.contains("youtu.be")
        {
            if matches!(
                name,
                "LOGIN_INFO"
                    | "SID"
                    | "HSID"
                    | "SSID"
                    | "SAPISID"
                    | "__Secure-1PSID"
                    | "__Secure-3PSID"
            ) {
                has_youtube_auth = true;
            }
        } else {
            has_non_youtube = true;
        }
    }
    if !url.trim().is_empty() && is_youtube_url(url) {
        has_youtube_auth
    } else {
        has_non_youtube || has_youtube_auth
    }
}

pub fn cookie_file_has_real_session(file: &Path) -> bool {
    cookie_file_usable_for_url("", file)
}

/// True when yt-dlp must run with NO JavaScript runtime on this
/// machine: the fetched component set carries no deno. That is
/// a pre-Windows-10 Windows — Deno's own installation docs
/// require Windows 10 version 1709 or later (it needs
/// IsWow64Process2), and the newest Node yt-dlp accepts (22+)
/// has the same Windows 10 floor, so no supported runtime can
/// run there at all — and it is ANY 32-bit Windows, for which
/// Deno publishes no build at any version. The decision follows
/// the fetch set itself (see
/// `ndm::components::no_js_runtime_here`), so it cannot drift
/// from what was actually installed. Everywhere else this is
/// false and the runtime found on disk is passed exactly as
/// before.
pub fn no_js_runtime_mode() -> bool {
    crate::components::no_js_runtime_here()
}

/// The YouTube player clients a no-JS-runtime run may use: the
/// clients from the normal list that answer from the embedded/TV
/// player responses, WITHOUT `default` — `default` expands to the
/// web clients whose formats are gated behind the JavaScript
/// challenge (n/sig) that only a JS runtime can solve (yt-dlp's
/// EJS wiki). Embedded/TV/mweb responses expose their formats
/// without that challenge, which is what maximises what still
/// works on a machine that cannot run a runtime.
fn youtube_clients_no_js() -> &'static str {
    "youtube:player_client=tv_embedded,web_embedded,mweb"
}

fn youtube_clients_default() -> &'static str {
    "youtube:player_client=default,tv_embedded,web_embedded,mweb"
}

/// The plain-language form of a yt-dlp failure that happened
/// because the site needs JavaScript challenge solving and this
/// machine cannot run a JS runtime (see `no_js_runtime_mode`).
/// Callers apply it ONLY in no-JS mode; anywhere else the raw
/// error stands. Returns None when the error is not of that kind.
/// Recognised shapes are yt-dlp's own: its "No supported
/// JavaScript runtime could be found" / EJS wording, and the
/// downstream symptom on challenge-gated sites — the requested
/// format (or every format) never materialising.
pub fn js_required_error(err: &str) -> Option<String> {
    let l = err.to_lowercase();
    let js_named = l.contains("javascript")
        || l.contains("js runtime")
        || l.contains("yt-dlp-ejs")
        || l.contains("no supported js");
    let formats_starved = l.contains("requested format is not available")
        || l.contains("only images are available")
        || l.contains("no video formats found");
    if js_named || formats_starved {
        Some(
            "This site needs a JavaScript tool to unlock its videos, and that tool \
             can't run on this PC — it needs 64-bit Windows 10 or newer. \
             Videos from most other sites, and direct video links, still work on this PC."
                .to_string(),
        )
    } else {
        None
    }
}

pub fn base_args_for_url(url: &str, components_dir: &Path, cookies: Option<&Path>) -> Vec<String> {
    base_args_for_url_mode(url, components_dir, cookies, no_js_runtime_mode())
}

fn base_args_for_url_mode(
    url: &str,
    components_dir: &Path,
    cookies: Option<&Path>,
    no_js: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--newline".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--socket-timeout".into(),
        "15".into(),
        "--extractor-args".into(),
        if no_js {
            youtube_clients_no_js()
        } else {
            youtube_clients_default()
        }
        .into(),
    ];
    if cfg!(debug_assertions) && std::env::var("NDM_INSECURE").is_ok() {
        args.push("--no-check-certificates".into());
    }
    let proxy = crate::global_proxy();
    if !proxy.is_empty() {
        args.push("--proxy".into());
        args.push(proxy);
    }
    if let Some(file) = cookies {
        if file.is_file() && cookie_file_usable_for_url(url, file) {
            args.push("--cookies".into());
            args.push(file.to_string_lossy().into_owned());
        }
    }
    if no_js {
        // --no-js-runtimes clears every runtime, including the
        // deno yt-dlp enables by default when it merely FINDS one
        // on PATH — a stray broken runtime must never be tried on
        // a machine whose OS cannot run it.
        args.push("--no-js-runtimes".into());
    } else if let Some(deno) = find_tool("deno", components_dir) {
        args.push("--js-runtimes".into());
        args.push(format!("deno:{}", deno.display()));
    } else if let Some(node) = find_tool("node", components_dir) {
        args.push("--js-runtimes".into());
        args.push(format!("node:{}", node.display()));
    }
    args
}

pub fn base_args(components_dir: &Path, cookies: Option<&Path>) -> Vec<String> {
    base_args_for_url("", components_dir, cookies)
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Raw-byte cap for a fetched thumbnail (finding #11): the probe
/// reply travels over Chrome native messaging, whose hard cap is
/// 1 MiB per message for the WHOLE JSON — and base64 inflates the
/// image by 4/3 on top of the format list and metadata sharing the
/// same reply. The old 768 KiB raw cap already encoded to ~1 MiB
/// before any JSON overhead. 384 KiB raw encodes to ~512 KiB,
/// leaving half the channel for everything else; the host's
/// write_reply size guard is the final backstop.
pub const THUMBNAIL_MAX_RAW_BYTES: usize = 384 * 1024;

/// Fetch a thumbnail URL into a self-contained `data:image/...;base64,...` URI
/// so that strict page CSPs, Facebook/Instagram CORP headers, or expiring signed
/// URLs never break the preview image inside the browser overlay or desktop app.
/// W3-14: is this thumbnail URL's host a public destination?
/// The URL is page-controlled metadata: if its host is a literal
/// IP in a loopback / link-local / private / otherwise
/// non-public range, the fetch is refused (blind-SSRF guard).
/// HostNAMES pass here — resolving a name and checking every
/// answer (DNS-rebinding grade) is out of scope for this guard
/// and is noted as such.
fn thumbnail_host_is_public(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((_, r)) => r,
        None => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(h) = authority.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return true; // a hostname, not a literal IP
    };
    match ip {
        std::net::IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
        }
        std::net::IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00  // unique-local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80) // link-local fe80::/10
        }
    }
}

fn fetch_thumbnail_data_uri(url: &str) -> Option<String> {
    if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    if !thumbnail_host_is_public(url) {
        return None;
    }
    let agent = crate::apply_proxy_for(ureq::AgentBuilder::new(), url)
        .timeout_connect(Duration::from_secs(4))
        .timeout_read(Duration::from_secs(4))
        .user_agent(crate::BROWSER_UA)
        .build();
    let resp = agent
        .get(url)
        .set("Accept-Encoding", "identity")
        .call()
        .ok()?;
    if crate::response_encoding(&resp).is_some() {
        return None;
    }
    // W3-14: only a real image/* body may become a data URI —
    // a non-image response is dropped, never relabelled jpeg
    let mime = resp
        .header("Content-Type")
        .and_then(|ct| ct.split(';').next())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s.starts_with("image/"))?;
    use std::io::Read as _;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(THUMBNAIL_MAX_RAW_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.is_empty() || buf.len() > THUMBNAIL_MAX_RAW_BYTES {
        return None;
    }
    Some(format!("data:{mime};base64,{}", base64_encode(&buf)))
}

/// Extract or estimate the byte size of one yt-dlp format entry.
/// Returns (bytes, is_approx): the size is exact only when yt-dlp
/// reported a real `filesize`; `filesize_approx` and the
/// bitrate×duration fallback are estimates, and an unknown size
/// (0) is never presented as exact either.
fn format_size_bytes(f: &serde_json::Value, duration_secs: f64) -> (u64, bool) {
    if let Some(s) = f["filesize"].as_u64().filter(|&s| s > 0) {
        return (s, false);
    }
    if let Some(s) = f["filesize_approx"].as_u64().filter(|&s| s > 0) {
        return (s, true);
    }
    let kbps = f["tbr"]
        .as_f64()
        .filter(|&b| b > 0.0)
        .or_else(|| {
            let v = f["vbr"].as_f64().unwrap_or(0.0);
            let a = f["abr"].as_f64().unwrap_or(0.0);
            (v + a > 0.0).then_some(v + a)
        })
        .unwrap_or(0.0);
    if kbps > 0.0 && duration_secs > 0.0 {
        (((kbps * 1000.0 / 8.0) * duration_secs) as u64, true)
    } else {
        (0, true)
    }
}

fn pick_thumbnail_url(v: &serde_json::Value) -> String {
    if let Some(arr) = v["thumbnails"].as_array() {
        // Prefer a medium-high resolution thumbnail (width 320..=960) so the
        // preview is crisp without downloading a multi-megabyte 4K still.
        let mut best: Option<&str> = None;
        for t in arr.iter().rev() {
            if let Some(u) = t["url"].as_str() {
                if u.starts_with("http://") || u.starts_with("https://") {
                    let w = t["width"].as_u64().unwrap_or(0);
                    if best.is_none() {
                        best = Some(u);
                    }
                    if (240..=960).contains(&w) {
                        best = Some(u);
                        break;
                    }
                }
            }
        }
        if let Some(u) = best {
            return u.to_string();
        }
    }
    v["thumbnail"].as_str().unwrap_or("").to_string()
}

fn quality_tag(h: u64) -> &'static str {
    match h {
        2160.. => "4K Ultra HD",
        1440..=2159 => "2K QHD",
        1080..=1439 => "Full HD",
        720..=1079 => "HD",
        480..=719 => "SD",
        _ => "",
    }
}

/// Build the robust yt-dlp format selector for a specific height `h`.
/// Always pairs video-only DASH streams with AAC (`m4a` / `mp4a`) audio first
/// before falling back to generic `ba`, and targets exact height `h` before
/// `<= h` so selecting 2160p/1440p never silently downgrades to 1080p.
pub fn selector_for_height(h: u64) -> String {
    format!(
        "bv*[height={h}][vcodec^=avc][ext=mp4]+ba[ext=m4a]/\
         bv*[height={h}][vcodec^=avc]+ba[ext=m4a]/\
         bv*[height={h}][ext=mp4]+ba[ext=m4a]/\
         bv*[height={h}]+ba[ext=m4a]/\
         bv*[height={h}]+ba/\
         b[height={h}][acodec!=none]/\
         bv*[height<={h}][vcodec^=avc][ext=mp4]+ba[ext=m4a]/\
         bv*[height<={h}][ext=mp4]+ba[ext=m4a]/\
         bv*[height<={h}]+ba[ext=m4a]/\
         bv*[height<={h}]+ba/\
         b[height<={h}][acodec!=none]/b[acodec!=none]/b"
    )
}

pub fn best_selector() -> String {
    "bv*[vcodec^=avc][ext=mp4]+ba[ext=m4a]/\
     bv*[ext=mp4]+ba[ext=m4a]/\
     bv*+ba[ext=m4a]/\
     bv*+ba/\
     b[acodec!=none]/b"
        .into()
}

/// Resolve a possibly-relative playlist URI against the URL of the
/// manifest that referenced it.
pub fn resolve_playlist_uri(base: &str, uri: &str) -> String {
    let uri = uri.trim();
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return uri.to_string();
    }
    if let Some(stripped) = uri.strip_prefix("//") {
        let scheme = base.split(':').next().unwrap_or("https");
        return format!("{scheme}://{stripped}");
    }
    if uri.starts_with('/') {
        // scheme + authority of the base
        let after = base.split("://").nth(1).unwrap_or("");
        let host = after.split('/').next().unwrap_or("");
        let scheme = base.split(':').next().unwrap_or("https");
        return format!("{scheme}://{host}{uri}");
    }
    match base.rfind('/') {
        Some(i) => format!("{}{}", &base[..=i], uri),
        None => uri.to_string(),
    }
}

/// Parse an HLS *master* playlist and return the absolute URL of its
/// default audio rendition (`#EXT-X-MEDIA:TYPE=AUDIO,...,DEFAULT=YES`),
/// falling back to the first audio rendition when none is marked
/// default. Returns `None` for media playlists and masters without a
/// separate audio rendition. This is the piece yt-dlp's generic
/// extractor does not surface: a variant can be downloaded perfectly
/// and still be silent, because its sound lives at this URI.
pub fn hls_default_audio_rendition(manifest: &str, base_url: &str) -> Option<String> {
    if !manifest.contains("#EXT-X-STREAM-INF") {
        return None; // not a master playlist
    }
    let mut first_audio: Option<String> = None;
    for line in manifest.lines() {
        let line = line.trim();
        if !line.starts_with("#EXT-X-MEDIA:") || !line.contains("TYPE=AUDIO") {
            continue;
        }
        let uri = line
            .split("URI=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or("");
        if uri.is_empty() {
            continue;
        }
        let abs = resolve_playlist_uri(base_url, uri);
        if line.contains("DEFAULT=YES") {
            return Some(abs);
        }
        if first_audio.is_none() {
            first_audio = Some(abs);
        }
    }
    first_audio
}

/// Fetch a URL's body as text (small files only — HLS manifests).
/// Follows one level of master-playlist indirection for the audio
/// rendition search: when `url` itself is a master, its renditions
/// are resolved against it.
pub fn fetch_hls_default_audio(url: &str, referer: &str, cookie: &str) -> Option<String> {
    // through the configured proxy like every other fetch (and
    // protocol-gated): on a proxy-only network a bare request here
    // silently found "no separate audio" while downloads worked
    let agent = crate::apply_proxy_for(ureq::AgentBuilder::new(), url)
        .timeout(Duration::from_secs(15))
        .build();
    let mut req = agent
        .get(url)
        .set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        .set("Accept-Encoding", "identity");
    if !referer.is_empty() {
        req = req.set("Referer", referer);
    }
    if !cookie.is_empty() {
        req = req.set("Cookie", cookie);
    }
    let resp = req.call().ok()?;
    // W1 F11: without this check a gzipped manifest silently
    // degrades to "no separate audio found" — compressed bytes
    // contain no #EXTM3U marker — and the download that follows
    // comes out audio-less instead of failing honestly
    if crate::response_encoding(&resp).is_some() {
        return None;
    }
    let text = resp.into_string().ok()?;
    if text.len() > 4 * 1024 * 1024 || !text.contains("#EXTM3U") {
        return None;
    }
    hls_default_audio_rendition(&text, url)
}

/// The vcodec yt-dlp actually reported, if any. `null` (the
/// generic extractor does this for direct media files) and a
/// missing key both mean "unknown" — NOT "no video stream".
/// Only the explicit string `"none"` is an audio-only signal
/// (R2-EXT-01: collapsing null into "none" made every direct
/// .mp4 probe as audio-only and the video row vanished).
fn reported_vcodec(f: &serde_json::Value) -> Option<&str> {
    f["vcodec"].as_str().filter(|s| !s.is_empty())
}

/// The acodec yt-dlp actually reported, if any (same null rule
/// as `reported_vcodec`).
fn reported_acodec(f: &serde_json::Value) -> Option<&str> {
    f["acodec"].as_str().filter(|s| !s.is_empty())
}

/// Container extensions that are audio-only by nature. Used as
/// the positive audio evidence when yt-dlp left the codecs
/// unknown: a direct .mp3 with null codecs is audio, while a
/// direct .mp4 with null codecs is a video file.
fn is_audio_container(ext: &str) -> bool {
    matches!(
        ext,
        "mp3" | "m4a" | "aac" | "opus" | "ogg" | "oga" | "wav" | "flac" | "mka" | "wma" | "aif"
            | "aiff"
    )
}

/// Does this format carry a video stream? A reported vcodec
/// other than "none" does. Unknown codecs count as video
/// unless the container itself is an audio-only format.
fn format_has_video(f: &serde_json::Value) -> bool {
    match reported_vcodec(f) {
        Some(vc) => vc != "none",
        None => !is_audio_container(f["ext"].as_str().unwrap_or("")),
    }
}

/// Is this format an audio-only stream? Explicit signal:
/// vcodec "none" together with a real acodec. vcodec "none"
/// with the acodec unreported: audio when the container is an
/// audio-only format — yt-dlp's generic extractor describes a
/// direct .m4a exactly that way ({vcodec: "none", acodec: null}),
/// and a stream that declares no video inside an audio container
/// has no other honest reading. (An explicit acodec "none" still
/// rejects: that format declares it carries no audio either.)
/// Unknown vcodec: only when the container is an audio-only
/// format (a direct .mp3/.m4a file IS its audio stream).
fn format_is_audio_only(f: &serde_json::Value) -> bool {
    match reported_vcodec(f) {
        Some(vc) => {
            if vc != "none" {
                return false;
            }
            match reported_acodec(f) {
                Some(ac) => ac != "none",
                None => is_audio_container(f["ext"].as_str().unwrap_or("")),
            }
        }
        None => is_audio_container(f["ext"].as_str().unwrap_or("")),
    }
}

/// The format list a probe result builds its options from.
/// yt-dlp's generic extractor describes a DIRECT media file (a
/// bare .mp4/.mp3 link) with the media fields on the result
/// object itself and NO `formats` array at all — reading only
/// the array yielded an empty list, so a direct .mp4 offered
/// nothing but an audio option and the "video" came down as an
/// audio-only .m4a (M3). A result that carries its own media URL
/// and is not a playlist IS its own single format — the same
/// fallback core's resolver applies to `requested_formats`.
pub fn probe_formats(v: &serde_json::Value) -> Vec<serde_json::Value> {
    let formats = v["formats"].as_array().cloned().unwrap_or_default();
    if !formats.is_empty() {
        return formats;
    }
    let direct_media = v["url"].as_str().map(|u| !u.is_empty()).unwrap_or(false)
        && v["entries"].is_null()
        && v["_type"].as_str() != Some("playlist");
    if direct_media {
        return vec![v.clone()];
    }
    formats
}

/// Build the quality options from yt-dlp's format list. Pure
/// (no I/O) so the classification rules — above all "no video
/// streams means no video options" — are unit-testable.
fn build_options(formats: &[serde_json::Value], duration_f64: f64) -> Vec<QualityOption> {
    // Best audio-only stream size (preferring m4a/aac to match our selector).
    // Also track HOW the audio is delivered: an audio option built
    // from HLS renditions must not be labelled "MP3" (F-EXT-02).
    let mut best_m4a_size: u64 = 0;
    let mut best_m4a_approx = true;
    let mut best_m4a_ext = String::new();
    let mut best_any_audio_size: u64 = 0;
    let mut best_any_audio_approx = true;
    let mut best_any_audio_ext = String::new();
    // Preference-ordered candidates for the size-less case below:
    // the ext of an m4a/aac-preferred audio format, and of any
    // audio format, in the selector's own preference order.
    let mut pref_m4a_ext = String::new();
    let mut pref_any_audio_ext = String::new();
    let mut has_direct_audio = false;
    let mut has_hls_audio = false;
    for f in formats {
        if format_is_audio_only(f) {
            let acodec = reported_acodec(f).unwrap_or("");
            let proto = f["protocol"].as_str().unwrap_or("");
            if proto.contains("m3u8") {
                has_hls_audio = true;
            } else {
                has_direct_audio = true;
            }
            let (sz, sz_approx) = format_size_bytes(f, duration_f64);
            let ext = f["ext"].as_str().unwrap_or("");
            if ext == "m4a" || acodec.starts_with("mp4a") || acodec.starts_with("aac") {
                // yt-dlp lists formats worst→best, so the LAST
                // match recorded here is the one the selector's
                // first branch would pick
                pref_m4a_ext = ext.to_string();
                if sz > best_m4a_size {
                    best_m4a_size = sz;
                    best_m4a_approx = sz_approx;
                    best_m4a_ext = ext.to_string();
                }
            }
            pref_any_audio_ext = ext.to_string();
            if sz > best_any_audio_size {
                best_any_audio_size = sz;
                best_any_audio_approx = sz_approx;
                best_any_audio_ext = ext.to_string();
            }
        }
    }
    // The ext the audio selector (ba[ext=m4a]/ba/b) will actually
    // fetch, in the selector's own preference order. The audio
    // option's label must name THIS format — a hardcoded "MP3"
    // labelled a download that landed on disk as .m4a (N-09).
    // When sizes are known the size winner is the fetch (it tracks
    // the selector's "best"); when NO format reports a usable size
    // — a direct media file reports filesize: null — the size race
    // never starts, so fall back to the preference candidates:
    // an m4a/aac format is what ba[ext=m4a] fetches, and without
    // one, ba takes the best remaining audio format (R4 item 12).
    let audio_ext = if best_m4a_size > 0 {
        best_m4a_ext
    } else if best_any_audio_size > 0 {
        best_any_audio_ext
    } else if !pref_m4a_ext.is_empty() {
        pref_m4a_ext
    } else {
        pref_any_audio_ext
    };
    let audio_bytes = if best_m4a_size > 0 {
        best_m4a_size
    } else if best_any_audio_size > 0 {
        best_any_audio_size
    } else if duration_f64 > 0.0 {
        ((128_000.0 / 8.0) * duration_f64) as u64
    } else {
        0
    };
    // The audio size is approximate when the winning format's size
    // was estimated — and the duration-based fallback above is an
    // estimate by construction, as is an unknown (0) size.
    let audio_approx = if best_m4a_size > 0 {
        best_m4a_approx
    } else if best_any_audio_size > 0 {
        best_any_audio_approx
    } else {
        true
    };

    // Any real video stream at all? Storyboards (mhtml) don't count.
    let has_video = formats.iter().any(|f| {
        let ext = f["ext"].as_str().unwrap_or("");
        format_has_video(f) && ext != "mhtml"
    });

    // Collect distinct video heights (excluding storyboards/mhtml).
    let mut heights: Vec<u64> = formats
        .iter()
        .filter(|f| {
            let ext = f["ext"].as_str().unwrap_or("");
            format_has_video(f) && ext != "mhtml"
        })
        .filter_map(|f| f["height"].as_u64())
        .filter(|&h| h >= 120)
        .collect();
    heights.sort_unstable();
    heights.dedup();

    // Estimate size for a given height `h` by finding the format our selector prefers.
    // Returns (bytes, is_approx): a combined video+audio size is
    // approximate when ANY of its parts is.
    //
    // 209 picker honesty (§5 C-F1): the candidates are walked in
    // selector_for_height's own branch order — the first class
    // with any candidate is the class the download comes from —
    // because the old single-pool ranking let a big MUXED format
    // outrank the DASH pair on raw size within its codec tier:
    // the 360p row displayed the muxed file's honest size while
    // the selector's first branch downloaded a different,
    // smaller pair of streams. Within one class, rank the way
    // yt-dlp's best-pick does at a fixed height: a direct
    // protocol before an HLS rendition, higher fps before
    // lower, and only then the larger size. The selector's
    // `<= h` tail branches never apply here: `h` itself comes
    // from the format list, so exact-height candidates exist
    // by construction.
    let size_for_height = |h: u64| -> (u64, bool) {
        // The candidate's selector class (branch number in
        // selector_for_height): 1-4 are the video-only branches
        // (branch 5 differs only in the audio leg, which
        // audio_bytes already models), 6 is the muxed `b[...]`
        // fallback. None: not a video format at this height.
        let class_of = |f: &serde_json::Value| -> Option<u8> {
            if !format_has_video(f) || f["height"].as_u64() != Some(h) {
                return None;
            }
            let vc = reported_vcodec(f).unwrap_or("");
            let ext = f["ext"].as_str().unwrap_or("");
            let avc = vc.starts_with("avc");
            if reported_acodec(f) == Some("none") {
                Some(match (avc, ext == "mp4") {
                    (true, true) => 1,
                    (true, false) => 2,
                    (false, true) => 3,
                    (false, false) => 4,
                })
            } else {
                // a real acodec — or an unknown one (a direct
                // file is muxed) — makes this a combined
                // format: only the `b[...]` branch can take it
                Some(6)
            }
        };
        let mut best_class = u8::MAX;
        let mut best_rank = (0u8, 0u64, 0u64);
        let mut chosen: Option<&serde_json::Value> = None;
        for f in formats {
            let Some(class) = class_of(f) else { continue };
            let proto = f["protocol"].as_str().unwrap_or("");
            let is_direct: u8 = if proto.contains("m3u8") { 0 } else { 1 };
            let fps = f["fps"].as_u64().unwrap_or(0);
            let (sz, _) = format_size_bytes(f, duration_f64);
            let rank = (is_direct, fps, sz);
            if class < best_class || (class == best_class && rank > best_rank) {
                best_class = class;
                best_rank = rank;
                chosen = Some(f);
            }
        }
        if let Some(f) = chosen {
            let (v_sz, v_approx) = format_size_bytes(f, duration_f64);
            if v_sz == 0 {
                return (0, true);
            }
            // Only an EXPLICIT acodec "none" marks a video-only
            // stream whose size needs the audio added. A real
            // acodec — or an unknown one (null: a direct file is
            // muxed, so its size already covers the audio) — does not.
            if reported_acodec(f) == Some("none") {
                (v_sz + audio_bytes, v_approx || audio_approx)
            } else {
                (v_sz, v_approx)
            }
        } else {
            (0, true)
        }
    };

    let mut options: Vec<QualityOption> = Vec::new();

    if !has_video {
        // Pure audio — a direct .mp3, an audio-only extractor
        // result: NO video option may be offered. The old code fell
        // into the heights-empty fallback below and invented a
        // "Best Quality" MP4 row for it; the panel then created a
        // VIDEO job for audio content, which downloaded to 100%
        // and failed the playable-stream check (F-EXT-10). Only
        // the audio option (pushed below) is returned.
    } else if heights.is_empty() {
        // Fallback when extractor reports no pixel heights (e.g. direct stream)
        let mut top_size: u64 = 0;
        let mut top_approx = true;
        for f in formats.iter() {
            let (sz, ap) = format_size_bytes(f, duration_f64);
            if sz > top_size {
                top_size = sz;
                top_approx = ap;
            }
        }
        let st = fmt_size(top_size);
        let note = if st.is_empty() {
            "MP4 • Video + Audio".to_string()
        } else {
            format!("MP4 • Video + Audio • {st}")
        };
        options.push(QualityOption {
            selector: best_selector(),
            label: "Best Quality".into(),
            ext: "MP4".into(),
            note,
            size_bytes: top_size,
            size_approx: top_approx,
            size_text: st,
            audio: false,
            height: 0,
            source: String::new(),
        });
    } else {
        for &h in heights.iter().rev() {
            let (sz, sz_approx) = size_for_height(h);
            let st = fmt_size(sz);
            let tag = quality_tag(h);
            let label = if tag.is_empty() {
                format!("{h}p")
            } else {
                format!("{h}p ({tag})")
            };
            let note = if st.is_empty() {
                "MP4 • Video + Audio".to_string()
            } else {
                format!("MP4 • Video + Audio • {st}")
            };
            options.push(QualityOption {
                selector: selector_for_height(h),
                label,
                ext: "MP4".into(),
                note,
                size_bytes: sz,
                size_approx: sz_approx,
                size_text: st,
                audio: false,
                height: h,
                source: String::new(),
            });
        }
    }

    // The audio option's label must describe what will actually be
    // fetched: when the only audio streams are HLS renditions, the
    // result is an HLS audio stream, not an MP3 file (F-EXT-02).
    let hls_only_audio = !has_direct_audio && has_hls_audio;
    let (a_ext, a_label, a_kind): (String, String, String) = if hls_only_audio {
        ("Audio (HLS)".into(), "Audio Only (HLS)".into(), "HLS".into())
    } else if !audio_ext.is_empty() {
        let up = audio_ext.to_uppercase();
        (up.clone(), format!("Audio Only ({up})"), up)
    } else {
        // No audio format was reported at all (size was estimated):
        // stay neutral rather than inventing a format name.
        ("Audio".into(), "Audio Only".into(), "Audio".into())
    };
    let audio_st = fmt_size(audio_bytes);
    let audio_note = if audio_st.is_empty() {
        format!("{a_kind} • High Quality Audio")
    } else {
        format!("{a_kind} • Audio Only • {audio_st}")
    };
    options.push(QualityOption {
        selector: "ba[ext=m4a]/ba/b".into(),
        label: a_label,
        ext: a_ext,
        note: audio_note,
        size_bytes: audio_bytes,
        size_approx: audio_approx,
        size_text: audio_st,
        audio: true,
        height: 0,
        source: String::new(),
    });

    options
}

/// W-8: yt-dlp's html5 extractor enumerates a page's media
/// elements and titles them after the page title plus the element
/// number — "Walk Test Movie 2026 (1)" for the FIRST video on the
/// page (id "video-1") — and the app used to show and save that
/// enumeration verbatim, so every first-time Add Video name
/// carried a spurious "(1)" that looks like a duplicate marker.
/// Strip exactly that suffix: a trailing " (N)" whose N equals
/// the number in an html5-style id ("video-N"). Any other title —
/// including a genuine " (1)" the uploader actually wrote, whose
/// entry id will not match — passes through untouched.
pub fn clean_probe_title(title: &str, id: &str) -> String {
    let Some(n) = id
        .strip_prefix("video-")
        .and_then(|rest| rest.parse::<u64>().ok())
    else {
        return title.to_string();
    };
    let suffix = format!(" ({n})");
    match title.strip_suffix(&suffix) {
        Some(base) if !base.trim().is_empty() => base.to_string(),
        _ => title.to_string(),
    }
}

/// Build a Command for a bundled console tool. On Windows the
/// desktop app is a GUI-subsystem process with no console of its
/// own, so a console child would allocate a flashing console
/// window unless created with CREATE_NO_WINDOW; elsewhere this
/// is a plain Command::new.
fn tool_cmd<S: AsRef<std::ffi::OsStr>>(prog: S) -> Command {
    #[allow(unused_mut)] // mut is only exercised by the Windows branch
    let mut c = Command::new(prog);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// How long a probe result stays reusable. A probe is one full
/// yt-dlp process spawn plus the page fetch — the single most
/// expensive step in the whole add flow — and the picker's data
/// (title, formats, sizes) does not change second to second, so a
/// repeated Fetch Info on the same page, and the resolve the
/// Download click runs a moment later, must not pay it again.
/// Two minutes is deliberately short: signed stream URLs inside
/// the cached raw dump stay far inside their real lifetimes, and
/// the resolution cache in core applies its own longer TTL to the
/// streams it derives from this dump.
pub const PROBE_TTL: Duration = Duration::from_secs(120);
const PROBE_CACHE_CAP: usize = 32;

/// Everything that can change a probe's result, as a cache key:
/// the full page URL, a signature of the effective yt-dlp base
/// arguments (which already encode the proxy, the JS-runtime
/// mode and the cookie file's path), and a signature of the
/// cookie file's BYTES — a re-login changes the bytes under the
/// same path, and a resolution made under the old session must
/// never be served to the new one.
#[derive(PartialEq, Eq, Clone)]
struct ProbeKey {
    url: String,
    args_sig: u64,
    cookies_sig: u64,
}

struct ProbeEntry {
    key: ProbeKey,
    at: Instant,
    info: VideoInfo,
    raw: serde_json::Value,
}

static PROBE_CACHE: Mutex<Vec<ProbeEntry>> = Mutex::new(Vec::new());

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Content signature of a cookie file (FNV-1a over its bytes);
/// no file and an unreadable file get distinct sentinels so
/// neither can alias a real file's hash. Mirrors the resolution
/// cache's signature in core's video module.
fn probe_cookies_signature(cookies: Option<&Path>) -> u64 {
    let Some(p) = cookies else { return 0 };
    let Ok(bytes) = std::fs::read(p) else {
        return u64::MAX;
    };
    cookie_text_signature(&String::from_utf8_lossy(&bytes))
}

/// Canonical signature of a Netscape cookie file's TEXT: the
/// (domain, path, name, value) tuples, sorted, then hashed.
///
/// The raw bytes are NOT a stable identity for "the same browser
/// session": the extension captures the cookie jar twice for one
/// download — once for the picker's warm resolve, once for the
/// Download click — and `chrome.cookies.getAll` does not promise
/// a stable ordering between the two captures (a cookie the page
/// re-set in between comes back in a different position). The
/// byte hash therefore differed between the warm's cache key and
/// the job's, the warmed resolution was invisible to the job,
/// and every picker click paid a second full yt-dlp resolve
/// (2.3.1 start-latency bug: ~17 s of "Probing connection…"
/// after the picker had JUST probed the same page). Sorting the
/// tuples makes the signature a function of the cookie SET:
/// same session, any capture order — same signature; a changed
/// value (re-login, refreshed session) still changes it.
pub fn cookie_text_signature(text: &str) -> u64 {
    let mut tuples: Vec<(&str, &str, &str, &str)> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        // "#HttpOnly_" lines ARE cookies (yt-dlp's own parser
        // strips the prefix); other '#' lines are comments.
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() >= 7 {
            tuples.push((f[0], f[2], f[5], f[6]));
        } else {
            // not a Netscape line: keep the raw content in the
            // set so ANY content difference still changes the
            // signature (ordering is normalized by the sort)
            tuples.push((line, "", "", ""));
        }
    }
    tuples.sort_unstable();
    let mut buf = String::new();
    for (d, p, n, v) in tuples {
        buf.push_str(d);
        buf.push('\u{1}');
        buf.push_str(p);
        buf.push('\u{1}');
        buf.push_str(n);
        buf.push('\u{1}');
        buf.push_str(v);
        buf.push('\n');
    }
    fnv1a(buf.as_bytes())
}

fn probe_key(url: &str, components_dir: &Path, cookies: Option<&Path>) -> ProbeKey {
    let args = base_args_for_url(url, components_dir, cookies);
    ProbeKey {
        url: url.to_string(),
        args_sig: fnv1a(args.join("\u{1}").as_bytes()),
        cookies_sig: probe_cookies_signature(cookies),
    }
}

fn probe_cache_lock() -> std::sync::MutexGuard<'static, Vec<ProbeEntry>> {
    PROBE_CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

fn probe_cache_get(key: &ProbeKey, now: Instant) -> Option<(VideoInfo, serde_json::Value)> {
    let mut cache = probe_cache_lock();
    cache.retain(|e| now.duration_since(e.at) < PROBE_TTL);
    cache
        .iter()
        .find(|e| &e.key == key)
        .map(|e| (e.info.clone(), e.raw.clone()))
}

fn probe_cache_store(key: ProbeKey, info: VideoInfo, raw: serde_json::Value, now: Instant) {
    let mut cache = probe_cache_lock();
    cache.retain(|e| e.key != key && now.duration_since(e.at) < PROBE_TTL);
    cache.push(ProbeEntry {
        key,
        at: now,
        info,
        raw,
    });
    while cache.len() > PROBE_CACHE_CAP {
        cache.remove(0);
    }
}

/// The raw yt-dlp `-J` dump behind a fresh cached probe of this
/// exact URL + options, if one exists. Core's stream resolver
/// derives a single-format page's streams from this dump instead
/// of spawning yt-dlp a second time (see core::video).
pub fn cached_probe_json(
    url: &str,
    components_dir: &Path,
    cookies: Option<&Path>,
) -> Option<serde_json::Value> {
    let key = probe_key(url, components_dir, cookies);
    probe_cache_get(&key, Instant::now()).map(|(_, raw)| raw)
}

pub fn probe(
    url: &str,
    components_dir: &Path,
    cookies: Option<&Path>,
) -> Result<VideoInfo, String> {
    let key = probe_key(url, components_dir, cookies);
    if let Some((info, _)) = probe_cache_get(&key, Instant::now()) {
        return Ok(info);
    }
    let (info, raw) = probe_uncached(url, components_dir, cookies)?;
    probe_cache_store(key, info.clone(), raw, Instant::now());
    Ok(info)
}

fn probe_uncached(
    url: &str,
    components_dir: &Path,
    cookies: Option<&Path>,
) -> Result<(VideoInfo, serde_json::Value), String> {
    let ytdlp = find_tool("yt-dlp", components_dir)
        .ok_or("yt-dlp not found — the video component is not installed yet")?;
    let mut cmd = tool_cmd(&ytdlp);
    cmd.args(base_args_for_url(url, components_dir, cookies))
        .args(["-J", "--", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start yt-dlp: {e}"))?;
    let out_pipe = child.stdout.take().ok_or("no stdout from yt-dlp")?;
    let mut err_pipe = child.stderr.take().ok_or("no stderr from yt-dlp")?;
    let out_thread = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut v = Vec::new();
        let _ = out_pipe.take(64 * 1024 * 1024 + 1).read_to_end(&mut v);
        v
    });
    let err_thread = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut tail: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match err_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > 65536 {
                        let cut = tail.len() - 65536;
                        tail.drain(..cut);
                    }
                }
            }
        }
        tail
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("yt-dlp took too long to read this page (45s limit)".to_string());
                }
                std::thread::sleep(Duration::from_millis(80));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("yt-dlp failed: {e}"));
            }
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    if stdout.len() > 64 * 1024 * 1024 {
        return Err("this page's metadata is too large to read".to_string());
    }
    let stderr = err_thread.join().unwrap_or_default();
    if !status.success() {
        let err = String::from_utf8_lossy(&stderr);
        // On a machine that cannot run a JavaScript runtime, a
        // challenge-gated site must fail in plain language, not
        // as a raw yt-dlp line (see js_required_error). Checked
        // against the whole stderr: the JS wording often rides
        // in a WARNING above the final ERROR line.
        if no_js_runtime_mode() {
            if let Some(m) = js_required_error(&err) {
                return Err(m);
            }
        }
        let last = err
            .lines()
            .rev()
            .find(|l| l.contains("ERROR"))
            .unwrap_or("yt-dlp could not read this page")
            .trim()
            .to_string();
        return Err(last);
    }
    let v: serde_json::Value =
        serde_json::from_slice(&stdout).map_err(|e| format!("bad yt-dlp output: {e}"))?;
    let title = clean_probe_title(
        v["title"].as_str().unwrap_or("Video"),
        v["id"].as_str().unwrap_or(""),
    );
    let duration_f64 = v["duration"]
        .as_f64()
        .or_else(|| v["duration"].as_u64().map(|u| u as f64))
        .unwrap_or(0.0);
    let duration = duration_f64.round() as u64;
    let uploader = v["uploader"]
        .as_str()
        .or_else(|| v["channel"].as_str())
        .or_else(|| v["extractor_key"].as_str())
        .unwrap_or("")
        .to_string();

    let raw_thumb = pick_thumbnail_url(&v);
    let thumbnail = fetch_thumbnail_data_uri(&raw_thumb).unwrap_or(raw_thumb);

    let formats = probe_formats(&v);

    let options = build_options(&formats, duration_f64);

    let info = VideoInfo {
        title,
        duration,
        thumbnail,
        uploader,
        options,
    };
    Ok((info, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp_cookies(name: &str, content: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ndm-test-cookies-{name}-{}.txt", std::process::id()));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn thumbnail_host_guard_refuses_non_public_ip_literals() {
        // W3-14: literal non-public IPs are refused; public IPs
        // and hostnames (resolution check out of scope) pass
        for u in [
            "http://127.0.0.1:8080/x.jpg",
            "https://10.0.0.5/x.jpg",
            "https://192.168.1.1/x.jpg",
            "https://172.16.0.9/x.jpg",
            "https://169.254.1.1/x.jpg",
            "https://100.64.0.1/x.jpg",
            "https://[::1]/x.jpg",
            "https://[fe80::1]/x.jpg",
            "https://[fd00::1]/x.jpg",
            "https://user@127.0.0.1/x.jpg",
        ] {
            assert!(!thumbnail_host_is_public(u), "must refuse {u}");
            assert_eq!(fetch_thumbnail_data_uri(u), None, "no fetch for {u}");
        }
        for u in [
            "https://8.8.8.8/x.jpg",
            "https://i.ytimg.com/vi/x/hq.jpg",
            "http://thumbs.example.com:8080/x.jpg",
        ] {
            assert!(thumbnail_host_is_public(u), "must allow {u}");
        }
    }

    #[test]
    fn picker_sizes_are_true_totals_of_what_each_row_downloads() {
        // 2.3.1 (bug 7b audit): the field recording showed 360p
        // at 10.0 MB ABOVE 480p at 7.4 MB. Two shapes must both
        // display the TRUE total of the streams the row's own
        // selector will fetch:
        let audio = serde_json::json!({
            "format_id": "140", "ext": "m4a", "vcodec": "none",
            "acodec": "mp4a.40.2", "filesize": 2_000_000, "protocol": "https"
        });
        let size_of = |opts: &[QualityOption], h: u64| {
            opts.iter()
                .find(|o| o.height == h)
                .map(|o| o.size_bytes)
                .unwrap_or(0)
        };
        // Shape A — the field's: 360p exists ONLY as a muxed
        // format (10 MB complete), 480p only as a video-only
        // stream (5.4 MB) that pairs with the 2 MB audio. Then
        // 10.0 > 7.4 on screen is CORRECT: each number is the
        // full size of the file that row produces.
        let formats_a = vec![
            serde_json::json!({
                "format_id": "18", "ext": "mp4", "height": 360,
                "vcodec": "avc1.42001E", "acodec": "mp4a.40.2",
                "filesize": 10_000_000, "protocol": "https"
            }),
            serde_json::json!({
                "format_id": "135", "ext": "mp4", "height": 480,
                "vcodec": "avc1.4d401e", "acodec": "none",
                "filesize": 5_400_000, "protocol": "https"
            }),
            audio.clone(),
        ];
        let opts = build_options(&formats_a, 100.0);
        assert_eq!(size_of(&opts, 360), 10_000_000, "muxed row = its own full size");
        assert_eq!(size_of(&opts, 480), 7_400_000, "pair row = video + audio");
        // Shape B: BOTH a muxed (10 MB) and a video-only (6 MB)
        // 360p exist. The selector's first branches take the
        // video-only PAIR, so the row must show 6 + 2 = 8 MB —
        // never the muxed file's 10 MB.
        let formats_b = vec![
            serde_json::json!({
                "format_id": "18", "ext": "mp4", "height": 360,
                "vcodec": "avc1.42001E", "acodec": "mp4a.40.2",
                "filesize": 10_000_000, "protocol": "https"
            }),
            serde_json::json!({
                "format_id": "134", "ext": "mp4", "height": 360,
                "vcodec": "avc1.4d401e", "acodec": "none",
                "filesize": 6_000_000, "protocol": "https"
            }),
            audio.clone(),
        ];
        let opts = build_options(&formats_b, 100.0);
        assert_eq!(
            size_of(&opts, 360),
            8_000_000,
            "the row must price the pair its selector fetches, not the muxed alternative"
        );
    }

    #[test]
    fn direct_media_result_without_formats_offers_its_video() {
        // M3: yt-dlp's generic extractor on a bare .mp4 link —
        // the media fields sit on the result object itself and
        // there is NO formats array.
        let v: serde_json::Value = serde_json::json!({
            "id": "clip",
            "title": "clip",
            "url": "http://127.0.0.1:8077/clip.mp4",
            "ext": "mp4",
            "duration": 12,
        });
        let formats = probe_formats(&v);
        assert_eq!(formats.len(), 1);
        let options = build_options(&formats, 12.0);
        assert!(
            options.iter().any(|o| !o.audio),
            "a direct .mp4 must offer a video option, got: {:?}",
            options.iter().map(|o| &o.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn direct_audio_result_without_formats_offers_no_video() {
        // the same shape for a bare .mp3: audio only, honestly
        let v: serde_json::Value = serde_json::json!({
            "id": "song",
            "title": "song",
            "url": "http://127.0.0.1:8077/song.mp3",
            "ext": "mp3",
            "duration": 200,
        });
        let formats = probe_formats(&v);
        assert_eq!(formats.len(), 1);
        let options = build_options(&formats, 200.0);
        assert!(options.iter().all(|o| o.audio));
        assert!(!options.is_empty());
    }

    #[test]
    fn formats_array_and_playlists_pass_through_unchanged() {
        let v: serde_json::Value = serde_json::json!({
            "id": "x",
            "title": "x",
            "url": "http://h/watch",
            "formats": [
                { "format_id": "18", "ext": "mp4", "vcodec": "avc1", "acodec": "mp4a" }
            ],
        });
        let formats = probe_formats(&v);
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0]["format_id"], "18");

        // a playlist result is not its own format, even with a url
        let p: serde_json::Value = serde_json::json!({
            "id": "pl",
            "_type": "playlist",
            "url": "http://h/list",
            "entries": [{ "id": "a" }],
        });
        assert!(probe_formats(&p).is_empty());
    }

    #[test]
    fn mixed_youtube_and_other_site_cookies_preserved_for_other_site() {
        let body = "# Netscape HTTP Cookie File\n\
            .youtube.com\tTRUE\t/\tTRUE\t1999999999\tVISITOR_INFO1_LIVE\tabcdef\n\
            .facebook.com\tTRUE\t/\tTRUE\t1999999999\tc_user\t10000123456\n";
        let f = write_temp_cookies("mixed", body);
        assert!(cookie_file_has_real_session(&f));
        assert!(cookie_file_usable_for_url("https://www.facebook.com/watch/?v=123", &f));
        assert!(!cookie_file_usable_for_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ", &f));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn logged_in_youtube_cookies_accepted_for_youtube() {
        let body = "# Netscape HTTP Cookie File\n\
            .youtube.com\tTRUE\t/\tTRUE\t1999999999\tVISITOR_INFO1_LIVE\tabcdef\n\
            .youtube.com\tTRUE\t/\tTRUE\t1999999999\tSAPISID\tsigned_token\n";
        let f = write_temp_cookies("yt-auth", body);
        assert!(cookie_file_has_real_session(&f));
        assert!(cookie_file_usable_for_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ", &f));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn hls_master_yields_default_audio_rendition() {
        let master = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"main\",DEFAULT=YES,URI=\"aud/pl.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,AUDIO=\"aud\"\n\
            v360/prog.m3u8\n";
        assert_eq!(
            hls_default_audio_rendition(master, "http://x.test/hls/master.m3u8"),
            Some("http://x.test/hls/aud/pl.m3u8".to_string())
        );
    }

    #[test]
    fn hls_media_playlist_has_no_rendition() {
        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\nseg1.ts\n";
        assert_eq!(
            hls_default_audio_rendition(media, "http://x.test/v360/prog.m3u8"),
            None
        );
    }

    #[test]
    fn hls_master_without_default_picks_first_audio() {
        let master = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"alt\",DEFAULT=NO,URI=\"/abs/a.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\n\
            v/prog.m3u8\n";
        assert_eq!(
            hls_default_audio_rendition(master, "https://cdn.test:8443/h/master.m3u8"),
            Some("https://cdn.test:8443/abs/a.m3u8".to_string())
        );
    }

    /// Serve one HTTP response with the given extra headers and
    /// body on a loopback port; return the manifest URL.
    fn serve_manifest(extra_headers: &str, body: &str) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
            body.len()
        );
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body.as_bytes());
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = sock.write_all(&raw);
            let _ = sock.flush();
        });
        format!("http://127.0.0.1:{port}/master.m3u8")
    }

    #[test]
    fn hls_fetch_rejects_an_encoded_manifest() {
        // W1 F11: the same master playlist, served plain vs with a
        // gzip Content-Encoding label — plain resolves the default
        // audio rendition, encoded must be rejected outright (None
        // for the honest reason) rather than parsed as compressed
        // bytes that happen to contain no #EXTM3U and silently
        // degrade to "no audio"
        let master = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"main\",DEFAULT=YES,URI=\"aud/pl.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,AUDIO=\"aud\"\n\
            v360/prog.m3u8\n";
        let plain = serve_manifest("", master);
        let expected = format!(
            "{}/aud/pl.m3u8",
            plain.rsplit_once('/').map(|(base, _)| base).unwrap()
        );
        assert_eq!(fetch_hls_default_audio(&plain, "", ""), Some(expected));
        let encoded = serve_manifest("Content-Encoding: gzip\r\n", master);
        assert_eq!(fetch_hls_default_audio(&encoded, "", ""), None);
    }

    #[test]
    fn pure_audio_formats_yield_audio_only_options() {
        // a direct .mp3 as yt-dlp reports it: one format, no video
        // stream at all — the F-EXT-10 case. No video/"Best Quality"
        // option may be invented for it.
        let formats = vec![serde_json::json!({
            "vcodec": "none", "acodec": "mp3", "ext": "mp3",
            "filesize": 100_000, "protocol": "https"
        })];
        let options = build_options(&formats, 30.0);
        assert_eq!(
            options.len(),
            1,
            "labels: {:?}",
            options.iter().map(|o| &o.label).collect::<Vec<_>>()
        );
        assert!(options[0].audio);
    }

    #[test]
    fn null_vcodec_direct_mp4_offers_video() {
        // R2-EXT-01: yt-dlp's generic extractor reports a direct
        // media file with vcodec/acodec as JSON null (verified
        // against the bundled yt-dlp `-j`), NOT the string "none".
        // Null means "unknown", so the video row must survive.
        let formats = vec![serde_json::json!({
            "format_id": "0", "url": "http://cdn.test/big.mp4",
            "ext": "mp4", "vcodec": null, "acodec": null,
            "filesize": 100_663_296, "protocol": "https"
        })];
        let options = build_options(&formats, 30.0);
        assert!(
            options.iter().any(|o| !o.audio),
            "a direct .mp4 must offer a video option, got: {:?}",
            options.iter().map(|o| &o.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn missing_vcodec_key_direct_mp4_offers_video() {
        // Same unknown-codec rule when the key is absent entirely.
        let formats = vec![serde_json::json!({
            "format_id": "0", "url": "http://cdn.test/clip.mp4",
            "ext": "mp4", "acodec": null,
            "filesize": 5_000_000, "protocol": "https"
        })];
        let options = build_options(&formats, 30.0);
        assert!(options.iter().any(|o| !o.audio));
    }

    #[test]
    fn null_vcodec_direct_mp3_stays_audio_only() {
        // The other side of R2-EXT-01: unknown codecs must not
        // resurrect the F-EXT-10 bug — a direct .mp3 is an audio
        // container, so no video row may be invented for it.
        let formats = vec![serde_json::json!({
            "format_id": "0", "url": "http://cdn.test/song.mp3",
            "ext": "mp3", "vcodec": null, "acodec": null,
            "filesize": 100_000, "protocol": "https"
        })];
        let options = build_options(&formats, 30.0);
        assert_eq!(
            options.len(),
            1,
            "labels: {:?}",
            options.iter().map(|o| &o.label).collect::<Vec<_>>()
        );
        assert!(options[0].audio);
    }

    #[test]
    fn null_vcodec_with_height_yields_that_height_row() {
        // Unknown codec but a reported height: the per-height row
        // (and its size estimate) must be built from the format,
        // not dropped by the size ranking.
        let formats = vec![serde_json::json!({
            "format_id": "0", "url": "http://cdn.test/movie.mp4",
            "ext": "mp4", "vcodec": null, "acodec": null, "height": 720,
            "filesize": 5_000_000, "protocol": "https"
        })];
        let options = build_options(&formats, 60.0);
        let video = options
            .iter()
            .find(|o| !o.audio)
            .expect("a video option must exist");
        assert_eq!(video.height, 720);
        assert_eq!(video.size_bytes, 5_000_000);
    }

    #[test]
    fn hls_only_audio_is_not_labelled_mp3() {
        // F-EXT-02: the only audio streams are HLS renditions.
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 5_000_000, "protocol": "m3u8_native"
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp4a.40.2", "ext": "m4a",
                "filesize": 900_000, "protocol": "m3u8_native"
            }),
        ];
        let options = build_options(&formats, 60.0);
        assert!(options.iter().any(|o| !o.audio), "video rows must exist");
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert!(!audio.ext.contains("MP3"), "ext: {}", audio.ext);
        assert!(audio.ext.contains("HLS"), "ext: {}", audio.ext);
        assert!(!audio.label.contains("MP3"), "label: {}", audio.label);
    }

    #[test]
    fn direct_audio_label_follows_the_real_format() {
        // N-09: the best audio here is an .m4a — the option must say
        // M4A, because that is the file the download produces. (The
        // old hardcoded "MP3" label is what this test used to assert.)
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 5_000_000, "protocol": "https"
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp4a.40.2", "ext": "m4a",
                "filesize": 900_000, "protocol": "https"
            }),
        ];
        let options = build_options(&formats, 60.0);
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert_eq!(audio.label, "Audio Only (M4A)");
        assert_eq!(audio.ext, "M4A");
        assert!(options.iter().any(|o| !o.audio && o.height == 720));
    }

    #[test]
    fn mp3_audio_keeps_its_mp3_label() {
        // When the best (only) audio really is an MP3, MP3 it is.
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 5_000_000, "protocol": "https"
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp3", "ext": "mp3",
                "filesize": 900_000, "protocol": "https"
            }),
        ];
        let options = build_options(&formats, 60.0);
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert_eq!(audio.label, "Audio Only (MP3)");
        assert_eq!(audio.ext, "MP3");
    }

    #[test]
    fn sizeless_direct_m4a_keeps_its_format_label() {
        // R4 item 12: a direct media file's format reports
        // filesize: null and no bitrate (real yt-dlp JSON for a
        // direct .m4a link), so no size ever exceeds 0 — the label
        // must still name the container, from the selector's
        // preference order, not fall back to neutral "Audio Only".
        let formats = vec![serde_json::json!({
            "format_id": "direct",
            "url": "https://example.invalid/tune.m4a",
            "ext": "m4a",
            "vcodec": null,
            "acodec": null,
            "filesize": null,
            "protocol": "https"
        })];
        let options = build_options(&formats, 0.0);
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert_eq!(audio.label, "Audio Only (M4A)");
        assert_eq!(audio.ext, "M4A");
        assert!(
            options.iter().all(|o| o.audio),
            "an audio file gets no video row"
        );
    }

    #[test]
    fn sizeless_direct_mp3_keeps_its_format_label() {
        // Same size-less direct-file shape, MP3 container: no
        // m4a/aac candidate exists, so ba falls through to the
        // best (only) audio format — an MP3.
        let formats = vec![serde_json::json!({
            "format_id": "direct",
            "url": "https://example.invalid/song.mp3",
            "ext": "mp3",
            "vcodec": null,
            "acodec": null,
            "filesize": null,
            "protocol": "https"
        })];
        let options = build_options(&formats, 0.0);
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert_eq!(audio.label, "Audio Only (MP3)");
        assert_eq!(audio.ext, "MP3");
        assert!(
            options.iter().all(|o| o.audio),
            "an audio file gets no video row"
        );
    }

    #[test]
    fn none_vcodec_null_acodec_m4a_is_audio_only() {
        // R5 item 2: the EXACT shape yt-dlp's generic extractor
        // returns for a direct .m4a — vcodec is the string "none"
        // (not null, so the unknown-vcodec branch never runs) and
        // acodec is null. The old rule demanded a present acodec,
        // rejected the format, and the label fell back to neutral
        // "Audio Only". A no-video stream in an audio container
        // is audio.
        let formats = vec![serde_json::json!({
            "format_id": "fmp4",
            "ext": "m4a",
            "vcodec": "none",
            "acodec": null,
            "filesize": null
        })];
        let options = build_options(&formats, 0.0);
        assert_eq!(options.len(), 1, "exactly one option: {options:?}");
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert_eq!(audio.label, "Audio Only (M4A)");
        assert_eq!(audio.ext, "M4A");
        assert!(
            options.iter().all(|o| o.audio),
            "no video option may be invented for an audio file"
        );
    }

    #[test]
    fn none_vcodec_null_acodec_mp3_is_audio_only() {
        // Same generic-extractor shape in an MP3 container whose
        // acodec is null (the MP3 case used to pass only when the
        // format happened to carry acodec: "mp3").
        let formats = vec![serde_json::json!({
            "format_id": "fmp3",
            "ext": "mp3",
            "vcodec": "none",
            "acodec": null,
            "filesize": null
        })];
        let options = build_options(&formats, 0.0);
        assert_eq!(options.len(), 1, "exactly one option: {options:?}");
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert_eq!(audio.label, "Audio Only (MP3)");
        assert_eq!(audio.ext, "MP3");
        assert!(
            options.iter().all(|o| o.audio),
            "no video option may be invented for an audio file"
        );
    }

    #[test]
    fn sizeless_m4a_beside_a_sized_video_still_labels_m4a() {
        // The audio format reports no size while the video format
        // does: the size race for AUDIO still never starts, so the
        // preference fallback names the m4a the selector fetches.
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 5_000_000, "protocol": "https"
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp4a.40.2", "ext": "m4a",
                "filesize": null, "protocol": "https"
            }),
        ];
        let options = build_options(&formats, 60.0);
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert_eq!(audio.label, "Audio Only (M4A)");
        assert_eq!(audio.ext, "M4A");
        assert!(options.iter().any(|o| !o.audio && o.height == 720));
    }

    #[test]
    fn anonymous_youtube_cookies_skipped() {
        let body = "# Netscape HTTP Cookie File\n\
            .youtube.com\tTRUE\t/\tTRUE\t1999999999\tVISITOR_INFO1_LIVE\tabcdef\n\
            .youtube.com\tTRUE\t/\tTRUE\t1999999999\tYSC\txyz123\n";
        let f = write_temp_cookies("yt-anon", body);
        assert!(!cookie_file_has_real_session(&f));
        assert!(!cookie_file_usable_for_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ", &f));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn find_tool_prefers_the_given_components_dir() {
        // the fetched-components dir is the FIRST place find_tool
        // looks: a component installed there wins over every later
        // candidate (the exe-adjacent legacy bundle included) — this
        // order is what lets a slim install's fetched engine, and an
        // upgrader's already-present one, both be found without a
        // second system
        let exe_name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
        let dir = std::env::temp_dir().join(format!(
            "ndm-test-findtool-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join(exe_name);
        std::fs::write(&fake, b"fake").unwrap();
        assert_eq!(find_tool("yt-dlp", &dir), Some(fake));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_tool_unknown_tool_is_none() {
        let dir = std::env::temp_dir().join(format!(
            "ndm-test-findtool-empty-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(find_tool("ndm-no-such-tool-xyz", &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_title_strips_only_the_html5_enumeration_suffix() {
        // W-8: the html5 extractor's enumeration for the page's
        // first media element is not part of the title
        assert_eq!(
            clean_probe_title("Walk Test Movie 2026 (1)", "video-1"),
            "Walk Test Movie 2026"
        );
        assert_eq!(clean_probe_title("Second Clip (2)", "video-2"), "Second Clip");
        // a real title's own parenthetical survives: the id is not
        // the html5 enumeration, or the numbers do not agree
        assert_eq!(
            clean_probe_title("Movie (1)", "dQw4w9WgXcQ"),
            "Movie (1)"
        );
        assert_eq!(clean_probe_title("Movie (2)", "video-1"), "Movie (2)");
        assert_eq!(clean_probe_title("Plain Title", "video-1"), "Plain Title");
        // the suffix alone is never a title
        assert_eq!(clean_probe_title(" (1)", "video-1"), " (1)");
    }

    #[test]
    fn size_approx_flags_estimates_and_combines() {
        // A6: an exact `filesize` is the only non-approx source.
        // Here the VIDEO size is exact but the AUDIO size comes
        // from filesize_approx, so the combined video option (and
        // the audio option) must both report approx.
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 5_000_000, "protocol": "https"
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp4a.40.2", "ext": "m4a",
                "filesize_approx": 900_000, "protocol": "https"
            }),
        ];
        let options = build_options(&formats, 60.0);
        let video = options
            .iter()
            .find(|o| !o.audio)
            .expect("a video option must exist");
        assert_eq!(video.size_bytes, 5_900_000);
        assert!(video.size_approx, "one estimated part taints the sum");
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert!(audio.size_approx);

        // Fully exact sources stay exact.
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 5_000_000, "protocol": "https"
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp4a.40.2", "ext": "m4a",
                "filesize": 900_000, "protocol": "https"
            }),
        ];
        let options = build_options(&formats, 60.0);
        let video = options
            .iter()
            .find(|o| !o.audio)
            .expect("a video option must exist");
        assert!(!video.size_approx);
        let audio = options.iter().find(|o| o.audio).expect("audio option");
        assert!(!audio.size_approx);
    }

    // 209 picker honesty (§5 C-F1): the row's size must be the
    // size of the format the row's selector will fetch FIRST.
    // The expectations below are hand-computed from
    // selector_for_height's branch order over each synthetic
    // table — the estimator's pick must equal the selector's
    // first match at every height.
    fn row_at(options: &[QualityOption], h: u64) -> &QualityOption {
        options
            .iter()
            .find(|o| !o.audio && o.height == h)
            .unwrap_or_else(|| panic!("a video row at {h}p must exist"))
    }

    #[test]
    fn picker_size_measures_the_dash_pair_not_the_big_muxed_file() {
        // The incident shape: at 360p a large MUXED file (the
        // legacy combined format) coexists with the DASH pair.
        // selector_for_height(360) matches branch 1 first —
        // video-only avc/mp4 + m4a — so the row must measure
        // 4.0 MB + 1.5 MB, never the muxed file's 12.5 MB (the
        // old pooled ranking displayed 12.5 MB, more than the
        // 720p row's pair, for a download of 5.5 MB).
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "mp4a.40.2", "ext": "mp4",
                "height": 360, "filesize": 12_500_000, "protocol": "https", "fps": 30
            }),
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "none", "ext": "mp4",
                "height": 360, "filesize": 4_000_000, "protocol": "https", "fps": 30
            }),
            serde_json::json!({
                "vcodec": "avc1.640028", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 8_000_000, "protocol": "https", "fps": 30
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp4a.40.2", "ext": "m4a",
                "filesize": 1_500_000, "protocol": "https"
            }),
        ];
        let options = build_options(&formats, 100.0);
        assert_eq!(row_at(&options, 360).size_bytes, 5_500_000);
        assert_eq!(row_at(&options, 720).size_bytes, 9_500_000);
        assert!(
            row_at(&options, 360).size_bytes < row_at(&options, 720).size_bytes,
            "the 360p > 720p inversion is gone"
        );
    }

    #[test]
    fn picker_size_uses_the_muxed_format_when_no_dash_stream_exists() {
        // No video-only stream at 480p: the selector falls
        // through to its muxed `b[...]` branch, so the row
        // measures the muxed file itself — with NO audio adder
        // (its size already covers the audio).
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "mp4a.40.2", "ext": "mp4",
                "height": 480, "filesize": 7_800_000, "protocol": "https", "fps": 30
            }),
            serde_json::json!({
                "vcodec": "avc1.640028", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 8_000_000, "protocol": "https", "fps": 30
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp4a.40.2", "ext": "m4a",
                "filesize": 1_500_000, "protocol": "https"
            }),
        ];
        let options = build_options(&formats, 100.0);
        assert_eq!(row_at(&options, 480).size_bytes, 7_800_000);
        assert_eq!(row_at(&options, 720).size_bytes, 9_500_000);
    }

    #[test]
    fn picker_size_within_a_class_prefers_the_higher_fps_stream() {
        // Two video-only avc/mp4 streams at 720p: yt-dlp's
        // best-pick takes the 60 fps one, so the row measures
        // it (plus the audio leg), not the 30 fps stream.
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.640028", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 8_000_000, "protocol": "https", "fps": 30
            }),
            serde_json::json!({
                "vcodec": "avc1.640028", "acodec": "none", "ext": "mp4",
                "height": 720, "filesize": 11_000_000, "protocol": "https", "fps": 60
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp4a.40.2", "ext": "m4a",
                "filesize": 1_500_000, "protocol": "https"
            }),
        ];
        let options = build_options(&formats, 100.0);
        assert_eq!(row_at(&options, 720).size_bytes, 12_500_000);
    }

    #[test]
    fn picker_size_dash_class_wins_even_when_dash_is_hls_only() {
        // The class order dominates the protocol rank: an
        // HLS-only DASH stream is still what the selector's
        // first branches fetch, so it — not the direct muxed
        // file — is what the row measures.
        let formats = vec![
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "none", "ext": "mp4",
                "height": 360, "filesize": 4_200_000, "protocol": "m3u8_native", "fps": 30
            }),
            serde_json::json!({
                "vcodec": "avc1.64001f", "acodec": "mp4a.40.2", "ext": "mp4",
                "height": 360, "filesize": 12_500_000, "protocol": "https", "fps": 30
            }),
            serde_json::json!({
                "vcodec": "none", "acodec": "mp4a.40.2", "ext": "m4a",
                "filesize": 1_500_000, "protocol": "https"
            }),
        ];
        let options = build_options(&formats, 100.0);
        assert_eq!(row_at(&options, 360).size_bytes, 5_700_000);
    }

    // ---------- WS-B: no-JS-runtime mode (pre-Windows-10) ----------

    #[test]
    fn no_js_args_disable_every_runtime_and_skip_default_client() {
        // On a machine whose OS cannot run any supported JS
        // runtime, yt-dlp must be told so explicitly: every
        // runtime cleared (a stray deno on PATH must not be
        // tried), and the YouTube clients restricted to the ones
        // that answer without the JS challenge — `default` (the
        // web clients) must not appear.
        let dir = std::env::temp_dir();
        let args = base_args_for_url_mode("https://www.youtube.com/watch?v=x", &dir, None, true);
        assert!(args.iter().any(|a| a == "--no-js-runtimes"), "args: {args:?}");
        assert!(!args.iter().any(|a| a == "--js-runtimes"), "args: {args:?}");
        let clients = args
            .iter()
            .find(|a| a.starts_with("youtube:player_client="))
            .expect("extractor args must carry the client list");
        assert!(!clients.contains("default"), "clients: {clients}");
        assert!(clients.contains("tv_embedded"), "clients: {clients}");
        assert!(clients.contains("web_embedded"), "clients: {clients}");
        assert!(clients.contains("mweb"), "clients: {clients}");
    }

    #[test]
    fn js_mode_args_keep_the_runtime_and_default_client() {
        // The control case: with a runtime available the args are
        // exactly what they were before the no-JS mode existed.
        let dir = std::env::temp_dir();
        let args = base_args_for_url_mode("https://www.youtube.com/watch?v=x", &dir, None, false);
        assert!(!args.iter().any(|a| a == "--no-js-runtimes"), "args: {args:?}");
        let clients = args
            .iter()
            .find(|a| a.starts_with("youtube:player_client="))
            .expect("extractor args must carry the client list");
        assert!(clients.contains("default"), "clients: {clients}");
    }

    #[test]
    fn js_required_error_maps_yt_dlp_js_failures_to_plain_language() {
        // yt-dlp's own phrasings when challenge solving is
        // impossible without a runtime.
        for raw in [
            "WARNING: [youtube] No supported JavaScript runtime could be found. \
             YouTube extraction without a JS runtime has been deprecated, and some formats may be missing.",
            "ERROR: [youtube] abc123: Requested format is not available. Use --list-formats for a list of available formats",
            "ERROR: [youtube] abc123: Only images are available for download. use --list-formats to see them",
        ] {
            let m = js_required_error(raw).unwrap_or_else(|| panic!("must map: {raw}"));
            assert!(m.contains("JavaScript tool"), "message: {m}");
            assert!(m.contains("Windows 10"), "message: {m}");
            assert!(!m.contains("yt-dlp"), "no raw tool wording: {m}");
        }
        // unrelated failures keep their raw text (None = no mapping)
        assert!(js_required_error("ERROR: [generic] could not resolve host").is_none());
        assert!(js_required_error("ERROR: HTTP Error 404: Not Found").is_none());
    }

    // ---------- WS-B: probe cache ----------

    fn cache_test_info(title: &str) -> VideoInfo {
        VideoInfo {
            title: title.to_string(),
            duration: 12,
            thumbnail: String::new(),
            uploader: String::new(),
            options: Vec::new(),
        }
    }

    #[test]
    fn probe_cache_hit_miss_and_expiry() {
        // unique URLs per test: the cache is process-global and
        // cargo runs tests on parallel threads
        let dir = std::env::temp_dir();
        let url = "https://probe-cache.example/watch?v=pc1";
        let key = probe_key(url, &dir, None);
        let now = Instant::now();
        assert!(probe_cache_get(&key, now).is_none());
        probe_cache_store(
            key.clone(),
            cache_test_info("Cached Clip"),
            serde_json::json!({ "id": "pc1" }),
            now,
        );
        let (info, raw) = probe_cache_get(&key, now).expect("fresh entry must hit");
        assert_eq!(info.title, "Cached Clip");
        assert_eq!(raw["id"], "pc1");
        // a different URL is a different probe
        let other = probe_key("https://probe-cache.example/watch?v=pc2", &dir, None);
        assert!(probe_cache_get(&other, now).is_none());
        // an entry older than the TTL is never served
        assert!(
            probe_cache_get(&key, now + PROBE_TTL + Duration::from_secs(1)).is_none(),
            "an entry older than the TTL must miss"
        );
    }

    #[test]
    fn probe_cache_key_changes_with_cookie_bytes() {
        // same URL, same cookie PATH, different session bytes:
        // the second session must never be served the first
        // session's probe.
        let dir = std::env::temp_dir();
        let url = "https://probe-cache.example/watch?v=pc3";
        let f1 = write_temp_cookies("pc-a", ".example.com\tTRUE\t/\tTRUE\t1999999999\tSID\tone\n");
        let k1 = probe_key(url, &dir, Some(&f1));
        probe_cache_store(k1, cache_test_info("One"), serde_json::json!({}), Instant::now());
        std::fs::write(&f1, ".example.com\tTRUE\t/\tTRUE\t1999999999\tSID\ttwo\n").unwrap();
        let k2 = probe_key(url, &dir, Some(&f1));
        assert!(probe_cache_get(&k2, Instant::now()).is_none());
        let _ = std::fs::remove_file(&f1);
    }

    /// With a counting stub yt-dlp, two probes of the same page
    /// run the process exactly ONCE — the second Fetch Info is
    /// served from the probe cache. This is the add→picker
    /// latency fix measured end to end at unit level.
    #[cfg(unix)]
    #[test]
    fn repeated_probe_spawns_ytdlp_once() {
        let dir = std::env::temp_dir().join(format!("ndm-probe-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let count_file = dir.join("count.txt");
        let json = "{\"id\":\"clip\",\"title\":\"Clip\",\"url\":\"http://127.0.0.1:9/clip.mp4\",\"ext\":\"mp4\",\"duration\":12}";
        let script = format!(
            "#!/bin/sh\necho x >> \"{}\"\nprintf '%s\\n' '{}'\n",
            count_file.display(),
            json
        );
        let stub = dir.join("yt-dlp");
        std::fs::write(&stub, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let url = "https://probe-cache.example/direct/clip.mp4";
        let first = probe(url, &dir, None).expect("first probe");
        let second = probe(url, &dir, None).expect("second probe");
        assert_eq!(first.title, "Clip");
        assert_eq!(second.title, "Clip");
        let runs = std::fs::read_to_string(&count_file)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        assert_eq!(runs, 1, "a repeated probe must not spawn yt-dlp again");
        // the raw dump the click's resolver derives from is cached too
        let raw = cached_probe_json(url, &dir, None).expect("raw dump must be cached");
        assert_eq!(raw["id"], "clip");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

