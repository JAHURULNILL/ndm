//! Shared video probing and sidecar tool discovery (used by both the
//! `ndm-host` native messaging bridge for the in-browser IDM popup and
//! the `ndm-app` Tauri desktop backend).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QualityOption {
    pub selector: String,
    pub label: String,
    pub ext: String,
    pub note: String,
    pub size_bytes: u64,
    pub size_text: String,
    pub audio: bool,
    pub height: u64,
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

pub fn base_args_for_url(url: &str, components_dir: &Path, cookies: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--newline".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--socket-timeout".into(),
        "15".into(),
        "--extractor-args".into(),
        "youtube:player_client=default,tv_embedded,web_embedded,mweb".into(),
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
    if let Some(deno) = find_tool("deno", components_dir) {
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

/// Fetch a thumbnail URL into a self-contained `data:image/...;base64,...` URI
/// so that strict page CSPs, Facebook/Instagram CORP headers, or expiring signed
/// URLs never break the preview image inside the browser overlay or desktop app.
fn fetch_thumbnail_data_uri(url: &str) -> Option<String> {
    if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let agent = crate::apply_proxy_for(ureq::AgentBuilder::new(), url)
        .timeout_connect(Duration::from_secs(4))
        .timeout_read(Duration::from_secs(4))
        .user_agent(crate::BROWSER_UA)
        .build();
    let resp = agent.get(url).call().ok()?;
    let mime = resp
        .header("Content-Type")
        .and_then(|ct| ct.split(';').next())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s.starts_with("image/"))
        .unwrap_or_else(|| "image/jpeg".to_string());
    use std::io::Read as _;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(768 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.is_empty() || buf.len() >= 768 * 1024 {
        return None;
    }
    Some(format!("data:{mime};base64,{}", base64_encode(&buf)))
}

/// Extract or estimate the byte size of one yt-dlp format entry.
fn format_size_bytes(f: &serde_json::Value, duration_secs: f64) -> u64 {
    if let Some(s) = f["filesize"].as_u64().filter(|&s| s > 0) {
        return s;
    }
    if let Some(s) = f["filesize_approx"].as_u64().filter(|&s| s > 0) {
        return s;
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
        ((kbps * 1000.0 / 8.0) * duration_secs) as u64
    } else {
        0
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
        .set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36");
    if !referer.is_empty() {
        req = req.set("Referer", referer);
    }
    if !cookie.is_empty() {
        req = req.set("Cookie", cookie);
    }
    let text = req.call().ok()?.into_string().ok()?;
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

/// Build the quality options from yt-dlp's format list. Pure
/// (no I/O) so the classification rules — above all "no video
/// streams means no video options" — are unit-testable.
fn build_options(formats: &[serde_json::Value], duration_f64: f64) -> Vec<QualityOption> {
    // Best audio-only stream size (preferring m4a/aac to match our selector).
    // Also track HOW the audio is delivered: an audio option built
    // from HLS renditions must not be labelled "MP3" (F-EXT-02).
    let mut best_m4a_size: u64 = 0;
    let mut best_m4a_ext = String::new();
    let mut best_any_audio_size: u64 = 0;
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
            let sz = format_size_bytes(f, duration_f64);
            let ext = f["ext"].as_str().unwrap_or("");
            if ext == "m4a" || acodec.starts_with("mp4a") || acodec.starts_with("aac") {
                // yt-dlp lists formats worst→best, so the LAST
                // match recorded here is the one the selector's
                // first branch would pick
                pref_m4a_ext = ext.to_string();
                if sz > best_m4a_size {
                    best_m4a_size = sz;
                    best_m4a_ext = ext.to_string();
                }
            }
            pref_any_audio_ext = ext.to_string();
            if sz > best_any_audio_size {
                best_any_audio_size = sz;
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
    let size_for_height = |h: u64| -> u64 {
        let mut best_rank = (0u8, 0u8, 0u64);
        let mut chosen: Option<&serde_json::Value> = None;
        for f in formats {
            let vc = reported_vcodec(f).unwrap_or("");
            if !format_has_video(f) || f["height"].as_u64() != Some(h) {
                continue;
            }
            let ext = f["ext"].as_str().unwrap_or("");
            let proto = f["protocol"].as_str().unwrap_or("");
            let is_direct = !proto.contains("m3u8");
            let codec_tier: u8 = if vc.starts_with("avc") && ext == "mp4" && is_direct {
                5
            } else if vc.starts_with("avc") && ext == "mp4" {
                4
            } else if ext == "mp4" && is_direct {
                3
            } else if ext == "mp4" {
                2
            } else {
                1
            };
            let sz = format_size_bytes(f, duration_f64);
            let has_size: u8 = if sz > 0 { 1 } else { 0 };
            let rank = (codec_tier, has_size, sz);
            if chosen.is_none() || rank > best_rank {
                best_rank = rank;
                chosen = Some(f);
            }
        }
        if let Some(f) = chosen {
            let v_sz = format_size_bytes(f, duration_f64);
            if v_sz == 0 {
                return 0;
            }
            // Only an EXPLICIT acodec "none" marks a video-only
            // stream whose size needs the audio added. A real
            // acodec — or an unknown one (null: a direct file is
            // muxed, so its size already covers the audio) — does not.
            if reported_acodec(f) == Some("none") {
                v_sz + audio_bytes
            } else {
                v_sz
            }
        } else {
            0
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
        let top_size = formats
            .iter()
            .map(|f| format_size_bytes(f, duration_f64))
            .max()
            .unwrap_or(0);
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
            size_text: st,
            audio: false,
            height: 0,
        });
    } else {
        for &h in heights.iter().rev() {
            let sz = size_for_height(h);
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
                size_text: st,
                audio: false,
                height: h,
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
        label: a_label.into(),
        ext: a_ext.into(),
        note: audio_note,
        size_bytes: audio_bytes,
        size_text: audio_st,
        audio: true,
        height: 0,
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

pub fn probe(
    url: &str,
    components_dir: &Path,
    cookies: Option<&Path>,
) -> Result<VideoInfo, String> {
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

    let formats = v["formats"].as_array().cloned().unwrap_or_default();

    let options = build_options(&formats, duration_f64);

    Ok(VideoInfo {
        title,
        duration,
        thumbnail,
        uploader,
        options,
    })
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
}

