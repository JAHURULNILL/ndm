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

pub fn base_args(components_dir: &Path, cookies: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--newline".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--socket-timeout".into(),
        "15".into(),
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
        if file.is_file() {
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
    let agent = crate::apply_proxy(ureq::AgentBuilder::new())
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

pub fn probe(
    url: &str,
    components_dir: &Path,
    cookies: Option<&Path>,
) -> Result<VideoInfo, String> {
    let ytdlp = find_tool("yt-dlp", components_dir)
        .ok_or("yt-dlp not found — the video component is not installed yet")?;
    let mut cmd = Command::new(&ytdlp);
    cmd.args(base_args(components_dir, cookies))
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
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("yt-dlp took too long to read this page (90s limit)".to_string());
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
    let title = v["title"].as_str().unwrap_or("Video").to_string();
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

    // Best audio-only stream size (preferring m4a/aac to match our selector).
    let mut best_m4a_size: u64 = 0;
    let mut best_any_audio_size: u64 = 0;
    for f in &formats {
        let vcodec = f["vcodec"].as_str().unwrap_or("none");
        let acodec = f["acodec"].as_str().unwrap_or("none");
        if vcodec == "none" && acodec != "none" {
            let sz = format_size_bytes(f, duration_f64);
            let ext = f["ext"].as_str().unwrap_or("");
            if ext == "m4a" || acodec.starts_with("mp4a") || acodec.starts_with("aac") {
                if sz > best_m4a_size {
                    best_m4a_size = sz;
                }
            }
            if sz > best_any_audio_size {
                best_any_audio_size = sz;
            }
        }
    }
    let audio_bytes = if best_m4a_size > 0 {
        best_m4a_size
    } else if best_any_audio_size > 0 {
        best_any_audio_size
    } else if duration_f64 > 0.0 {
        ((128_000.0 / 8.0) * duration_f64) as u64
    } else {
        0
    };

    // Collect distinct video heights (excluding storyboards/mhtml).
    let mut heights: Vec<u64> = formats
        .iter()
        .filter(|f| {
            let vc = f["vcodec"].as_str().unwrap_or("none");
            let ext = f["ext"].as_str().unwrap_or("");
            vc != "none" && ext != "mhtml"
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
        for f in &formats {
            let vc = f["vcodec"].as_str().unwrap_or("none");
            if vc == "none" || f["height"].as_u64() != Some(h) {
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
            let ac = f["acodec"].as_str().unwrap_or("none");
            if ac != "none" && !ac.is_empty() {
                v_sz
            } else {
                v_sz + audio_bytes
            }
        } else {
            0
        }
    };

    let mut options: Vec<QualityOption> = Vec::new();

    if heights.is_empty() {
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

    let audio_st = fmt_size(audio_bytes);
    let audio_note = if audio_st.is_empty() {
        "MP3 • High Quality Audio".to_string()
    } else {
        format!("MP3 • Audio Only • {audio_st}")
    };
    options.push(QualityOption {
        selector: "ba[ext=m4a]/ba/b".into(),
        label: "Audio Only (MP3)".into(),
        ext: "MP3".into(),
        note: audio_note,
        size_bytes: audio_bytes,
        size_text: audio_st,
        audio: true,
        height: 0,
    });

    Ok(VideoInfo {
        title,
        duration,
        thumbnail,
        uploader,
        options,
    })
}
