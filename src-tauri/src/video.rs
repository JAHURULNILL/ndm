// Video downloads via yt-dlp (sidecar process) — YouTube, Facebook and the
// 1,700+ other sites yt-dlp supports. NDM never parses those sites itself;
// yt-dlp + ffmpeg do the extraction and merging, NDM owns the list, the
// progress, pause/resume and persistence — exactly like the HTTP engine.
//
// Pause = stop the yt-dlp process (its .part files stay on disk); resume =
// start it again with the same arguments, yt-dlp continues the partial files.

pub use ndm::video_probe::{base_args_for_url, find_tool, probe, VideoInfo};

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Parse `ffmpeg -hide_banner -i <file>` stderr to check `(has_video, has_audio)`.
pub fn streams_in_ffmpeg_info(text: &str) -> (bool, bool) {
    let mut video = false;
    let mut audio = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("Stream #") {
            if t.contains(": Video:") {
                video = true;
            } else if t.contains(": Audio:") {
                audio = true;
            }
        }
    }
    (video, audio)
}

/// Inspect a finished media file with `ffmpeg -hide_banner -i <path>` to
/// verify which stream types (`(has_video, has_audio)`) are actually present.
pub fn probe_streams(path: &Path, components_dir: &Path) -> Option<(bool, bool)> {
    let ffmpeg = find_tool("ffmpeg", components_dir)?;
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    Some(streams_in_ffmpeg_info(&stderr))
}

/// (name -> (len, mtime)) for every file in `dir`, taken just before a
/// download starts so the finished file can be recognised afterwards.
fn dir_snapshot(dir: &Path) -> HashMap<String, (u64, SystemTime)> {
    let mut out = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    out.insert(
                        e.file_name().to_string_lossy().into_owned(),
                        (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
                    );
                }
            }
        }
    }
    out
}

/// "<title>.f<format-id>.<ext>" stream files wait for the merge step and
/// are never the deliverable; .part/.ytdl are yt-dlp's working files.
fn is_intermediate(name: &str) -> bool {
    if name.ends_with(".part") || name.ends_with(".ytdl") || name.ends_with(".temp.mp4") {
        return true;
    }
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    if let Some((_, tail)) = stem.rsplit_once('.') {
        let t = tail.as_bytes();
        if tail.len() <= 24
            && t.first() == Some(&b'f')
            && tail[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return true;
        }
    }
    false
}

/// The file a successful run must have produced: newly appeared or
/// changed since `before`, not a stream intermediate. The merged/
/// extracted extension wins; otherwise the newest candidate.
fn find_final_file(
    dir: &Path,
    before: &HashMap<String, (u64, SystemTime)>,
    audio: bool,
    stem_hint: &str,
) -> Option<PathBuf> {
    let mut cands: Vec<(SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_intermediate(&name) {
                continue;
            }
            if let Ok(m) = e.metadata() {
                if !m.is_file() {
                    continue;
                }
                let key = (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH));
                if before.get(&name) == Some(&key) {
                    continue; // untouched by this run
                }
                cands.push((key.1, e.path()));
            }
        }
    }
    cands.sort_by_key(|(t, _)| *t);
    let want = if audio { "mp3" } else { "mp4" };
    let pick = |pool: &[&(SystemTime, PathBuf)]| -> Option<PathBuf> {
        pool.iter()
            .rev()
            .find(|(_, p)| {
                p.extension().map(|x| x.to_string_lossy().to_lowercase())
                    == Some(want.to_string())
            })
            .or_else(|| pool.last())
            .map(|(_, p)| p.clone())
    };
    // This job's own stem wins first: with two video jobs sharing one
    // download folder (the normal case), the directory-wide newest
    // file can be the OTHER job's result, and certifying it would
    // report this download finished with someone else's video.
    if !stem_hint.is_empty() {
        let prefix = format!("{stem_hint}.");
        let own: Vec<&(SystemTime, PathBuf)> = cands
            .iter()
            .filter(|(_, p)| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
            })
            .collect();
        if let Some(p) = pick(&own) {
            return Some(p);
        }
    }
    let all: Vec<&(SystemTime, PathBuf)> = cands.iter().collect();
    pick(&all)
}

/// Parse the output file path from yt-dlp status lines when present.
fn parse_output_path_line(line: &str) -> Option<PathBuf> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("[Merger] Merging formats into \"") {
        if let Some(path_str) = rest.strip_suffix('"') {
            return Some(PathBuf::from(path_str));
        }
    }
    if let Some(rest) = line.strip_prefix("[ExtractAudio] Destination: ") {
        return Some(PathBuf::from(rest.trim()));
    }
    if let Some(rest) = line.strip_prefix("[ExtractAudio] Not converting audio ") {
        if let Some((path_str, _)) = rest.split_once("; ") {
            return Some(PathBuf::from(path_str.trim()));
        }
    }
    if let Some(rest) = line.strip_prefix("[download] ") {
        if let Some(path_str) = rest.strip_suffix(" has already been downloaded") {
            return Some(PathBuf::from(path_str.trim()));
        }
        if let Some(dest) = rest.strip_prefix("Destination: ") {
            let p = PathBuf::from(dest.trim());
            let fname = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !fname.is_empty() && !is_intermediate(&fname) {
                return Some(p);
            }
        }
    }
    None
}

#[derive(Clone)]
pub struct VideoProgress {
    pub status: String, // downloading | paused | finished | error | cancelled
    pub total: u64,
    pub done: u64,
    pub speed_bps: f64,
    pub conns: usize,
    pub final_path: Option<PathBuf>,
    pub error: Option<String>,
}

impl Default for VideoProgress {
    fn default() -> Self {
        VideoProgress {
            status: "downloading".into(),
            total: 0,
            done: 0,
            speed_bps: 0.0,
            conns: 0,
            final_path: None,
            error: None,
        }
    }
}

/// Sanitize a user-supplied or page-supplied video title into a safe
/// cross-platform filename stem capped at 150 UTF-8 bytes so yt-dlp's
/// `.f<id>.mp4.part` suffix never hits the 255-byte filesystem limit.
pub fn safe_filename_stem(title: &str) -> String {
    let t = title.trim();
    let t = t
        .strip_suffix(".mp4")
        .or_else(|| t.strip_suffix(".MP4"))
        .or_else(|| t.strip_suffix(".mp3"))
        .or_else(|| t.strip_suffix(".MP3"))
        .or_else(|| t.strip_suffix(".mkv"))
        .or_else(|| t.strip_suffix(".webm"))
        .or_else(|| t.strip_suffix(".m4a"))
        .unwrap_or(t);
    let cleaned: String = t
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '%') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() || trimmed == "index" || trimmed == "watch" {
        return String::new();
    }
    let mut end = trimmed.len().min(150);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].trim().trim_matches('.').to_string()
}

/// Choose a non-colliding filename stem inside `dir` so two videos with the
/// same title (or two qualities of the same video) never overwrite each other
/// or collide on `.part` files.
pub fn unique_stem_on_disk(dir: &Path, base_stem: &str) -> String {
    let stem = safe_filename_stem(base_stem);
    if stem.is_empty() {
        return String::new();
    }
    let stem_taken = |cand: &str| -> bool {
        for ext in ["mp4", "mp3", "m4a", "mkv", "webm"] {
            if dir.join(format!("{cand}.{ext}")).exists() {
                return true;
            }
            if dir
                .join(format!(".{cand}.ndm_stream0.{ext}.part"))
                .exists()
            {
                return true;
            }
        }
        false
    };
    if !stem_taken(&stem) {
        return stem;
    }
    for n in 1..10_000 {
        let cand = format!("{stem} ({n})");
        if !stem_taken(&cand) {
            return cand;
        }
    }
    stem
}

pub struct VideoJob {
    pub url: String,
    pub dir: PathBuf,
    pub selector: String,
    pub audio: bool,
    pub title: String,
    pub cookies: Option<PathBuf>,
    pub progress: Arc<Mutex<VideoProgress>>,
    chosen_stem: Mutex<Option<String>>,
    child: Mutex<Option<Child>>,
    active_downloads: Mutex<Vec<Arc<ndm::Download>>>,
    stop_flag: Mutex<Option<String>>, // "paused" | "cancelled" when WE stopped it
    spawn_lock: Mutex<()>,
    gen: std::sync::atomic::AtomicU64,
    /// the chosen formats carry a sound stream (learned from yt-dlp's own
    /// resolution) — a "finished" video without sound is then a failure,
    /// never a success
    expect_audio: Mutex<bool>,
    /// separate HLS audio rendition (master-playlist EXT-X-MEDIA) that the
    /// app must fetch and mux itself; yt-dlp's generic extractor cannot
    /// pair it with the chosen variant
    pair_audio_url: Mutex<Option<String>>,
    /// this entry's own speed limit in bytes/sec (0 = none) — applied
    /// to every engine stream and to the yt-dlp fallback
    limit_bps: std::sync::atomic::AtomicU64,
}

#[derive(Clone)]
struct DirectStream {
    url: String,
    ext: String,
    has_audio: bool,
    cookie: Option<String>,
    referer: Option<String>,
}

impl VideoJob {
    pub fn start(
        url: &str,
        dir: &Path,
        selector: &str,
        audio: bool,
        title: &str,
        cookies: Option<PathBuf>,
        components_dir: &Path,
    ) -> Result<Arc<VideoJob>, String> {
        let initial_stem = {
            let s = safe_filename_stem(title);
            if s.is_empty() {
                None
            } else {
                let final_exists = ["mp4", "mp3", "m4a", "mkv", "webm"]
                    .iter()
                    .any(|ext| dir.join(format!("{s}.{ext}")).exists());
                if final_exists {
                    Some(unique_stem_on_disk(dir, &s))
                } else {
                    Some(s)
                }
            }
        };
        let effective_title = initial_stem.clone().unwrap_or_else(|| title.to_string());
        let job = Arc::new(VideoJob {
            url: url.to_string(),
            dir: dir.to_path_buf(),
            selector: selector.to_string(),
            audio,
            title: effective_title,
            cookies,
            progress: Arc::new(Mutex::new(VideoProgress::default())),
            chosen_stem: Mutex::new(initial_stem),
            child: Mutex::new(None),
            active_downloads: Mutex::new(Vec::new()),
            stop_flag: Mutex::new(None),
            spawn_lock: Mutex::new(()),
            gen: std::sync::atomic::AtomicU64::new(0),
            expect_audio: Mutex::new(false),
            pair_audio_url: Mutex::new(None),
            limit_bps: std::sync::atomic::AtomicU64::new(0),
        });
        job.spawn(components_dir)?;
        Ok(job)
    }

    fn resolve_or_lock_stem(&self, fallback_title: &str) -> String {
        let mut guard = self.chosen_stem.lock().unwrap();
        if let Some(existing) = guard.as_ref() {
            if !existing.is_empty() {
                return existing.clone();
            }
        }
        let raw = if !self.title.trim().is_empty() {
            &self.title
        } else {
            fallback_title
        };
        let unique = unique_stem_on_disk(&self.dir, raw);
        if !unique.is_empty() {
            *guard = Some(unique.clone());
        }
        unique
    }

    /// Resolve direct HTTP(S) stream URLs via yt-dlp JSON so we can download
    /// them over NDM's 32-connection parallel Rust engine instead of yt-dlp's
    /// single-threaded Python HTTP loop.
    fn resolve_direct_streams(
        &self,
        ytdlp: &Path,
        components_dir: &Path,
        sel: &str,
    ) -> Option<(String, Vec<DirectStream>)> {
        let mut args = base_args_for_url(&self.url, components_dir, self.cookies.as_deref());
        args.push("-j".into());
        args.push("-f".into());
        args.push(sel.to_string());
        args.push("--".into());
        args.push(self.url.clone());

        let out = Command::new(ytdlp)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| l.trim_start().starts_with('{'))?
            .to_string();
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        let resolved_title = v["title"].as_str().unwrap_or("").to_string();
        let formats: Vec<&serde_json::Value> = match v["requested_formats"].as_array() {
            Some(arr) if !arr.is_empty() => arr.iter().collect(),
            _ => vec![&v],
        };
        // Learn what this download SHOULD contain before any early
        // return below can lose the information: whether the chosen
        // formats carry sound, and — for HLS sources whose sound is a
        // separate rendition in the master playlist — where it lives.
        {
            let any_audio = formats.iter().any(|f| {
                let ac = f["acodec"].as_str().unwrap_or("none");
                !ac.is_empty() && ac != "none"
            });
            *self.expect_audio.lock().unwrap() = any_audio;
            if !any_audio && !self.audio {
                let clean_url = self
                    .url
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let is_hls = clean_url.ends_with(".m3u8")
                    || formats
                        .iter()
                        .any(|f| f["protocol"].as_str().unwrap_or("").contains("m3u8"));
                if is_hls {
                    let hdr = &formats[0]["http_headers"];
                    let cookie = hdr["Cookie"]
                        .as_str()
                        .or_else(|| hdr["cookie"].as_str())
                        .unwrap_or("")
                        .to_string();
                    let referer = hdr["Referer"]
                        .as_str()
                        .or_else(|| hdr["referer"].as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some(u) = ndm::video_probe::fetch_hls_default_audio(
                        &self.url,
                        &referer,
                        &cookie,
                    ) {
                        *self.pair_audio_url.lock().unwrap() = Some(u);
                    }
                }
            }
        }
        if formats.is_empty() || formats.len() > 2 {
            return None;
        }
        let mut streams = Vec::new();
        for f in formats {
            let proto = f["protocol"].as_str().unwrap_or("");
            if proto != "https" && proto != "http" {
                return None;
            }
            let u = f["url"].as_str().unwrap_or("");
            if !u.starts_with("http://") && !u.starts_with("https://") {
                return None;
            }
            let lower = u.to_ascii_lowercase();
            if lower.contains(".m3u8") || lower.contains(".mpd") {
                return None;
            }
            // The extension comes from the network: keep it a plain
            // short token before it is ever used inside a file path.
            let raw_ext = f["ext"].as_str().unwrap_or("mp4");
            let cleaned: String = raw_ext
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(5)
                .collect::<String>()
                .to_ascii_lowercase();
            let ext = if cleaned.len() < 2 {
                "mp4".to_string()
            } else {
                cleaned
            };
            let has_audio = {
                let ac = f["acodec"].as_str().unwrap_or("none");
                !ac.is_empty() && ac != "none"
            };
            let headers = &f["http_headers"];
            let cookie = headers["Cookie"]
                .as_str()
                .or_else(|| headers["cookie"].as_str())
                .map(|s| s.to_string());
            let referer = headers["Referer"]
                .as_str()
                .or_else(|| headers["referer"].as_str())
                .map(|s| s.to_string());
            streams.push(DirectStream {
                url: u.to_string(),
                ext,
                has_audio,
                cookie,
                referer,
            });
        }
        Some((resolved_title, streams))
    }

    fn run_parallel_engine(
        self: &Arc<Self>,
        ytdlp: &Path,
        ffmpeg: &Path,
        components_dir: &Path,
        sel: &str,
        my_gen: u64,
    ) -> bool {
        let Some((resolved_title, streams)) =
            self.resolve_direct_streams(ytdlp, components_dir, sel)
        else {
            return false;
        };
        // The resolved streams are the ground truth for what the
        // merged file must contain (the finish-line guard reads this).
        if streams.iter().any(|s| s.has_audio) {
            *self.expect_audio.lock().unwrap() = true;
        }
        if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen
            || self.stop_flag.lock().unwrap().is_some()
        {
            return true;
        }

        let mut stem = self.resolve_or_lock_stem(&resolved_title);
        if stem.is_empty() {
            stem = unique_stem_on_disk(&self.dir, "video");
        }
        let final_ext = if self.audio {
            if sel.contains("m4a") && !sel.contains("mp3") {
                "m4a"
            } else {
                "mp3"
            }
        } else {
            "mp4"
        };
        let final_path = self.dir.join(format!("{stem}.{final_ext}"));

        let max_conn = ndm::global_connections().clamp(1, 32);
        let per_stream_conn = if streams.len() > 1 {
            (max_conn / 2).max(1)
        } else {
            max_conn
        };

        let mut part_paths: Vec<PathBuf> = Vec::new();
        let mut dls: Vec<Arc<ndm::Download>> = Vec::new();
        for (i, st) in streams.iter().enumerate() {
            let part = self
                .dir
                .join(format!(".{stem}.ndm_stream{i}.{}.part", st.ext));
            let auth = ndm::Auth {
                cookie: st.cookie.clone(),
                referer: st.referer.clone(),
                authorization: None,
            };
            match ndm::start_with_auth(&st.url, &part, per_stream_conn, ndm::DEFAULT_BLOCK, auth) {
                Ok(dl) => {
                    let lim = self.limit();
                    if lim > 0 {
                        dl.set_limit(lim);
                    }
                    part_paths.push(part);
                    dls.push(dl);
                }
                Err(_) => {
                    for d in &dls {
                        d.control.cancel();
                    }
                    // KEEP the part files and their resume state: a
                    // stream that failed to START must not destroy the
                    // bytes the other stream already fetched — a retry
                    // resumes them instead of restarting from byte 0
                    // (only a user cancel deletes partials).
                    return false;
                }
            }
        }

        *self.active_downloads.lock().unwrap() = dls.clone();

        // Poll parallel multi-connection progress until all streams finish
        loop {
            if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                for d in &dls {
                    d.control.cancel();
                }
                return true;
            }
            if let Some(flag) = self.stop_flag.lock().unwrap().clone() {
                if flag == "cancelled" {
                    for d in &dls {
                        d.control.cancel();
                    }
                    for p in &part_paths {
                        let _ = std::fs::remove_file(p);
                        let _ = std::fs::remove_file(ndm::state_path_for(p));
                    }
                    let mut p = self.progress.lock().unwrap();
                    p.status = "cancelled".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    return true;
                }
            }

            let mut total_sum = 0u64;
            let mut done_sum = 0u64;
            let mut speed_sum = 0.0f64;
            let mut conns_sum = 0usize;
            let mut all_finished = true;
            let mut any_error = false;
            let mut any_paused = false;

            for d in &dls {
                let pr = d.progress();
                total_sum = total_sum.saturating_add(pr.total);
                done_sum = done_sum.saturating_add(pr.done_bytes);
                if pr.status == ndm::Status::Downloading {
                    speed_sum += pr.speed_bps;
                    conns_sum = conns_sum.saturating_add(pr.conns);
                }
                match pr.status {
                    ndm::Status::Finished => {}
                    ndm::Status::Paused => {
                        all_finished = false;
                        any_paused = true;
                    }
                    ndm::Status::Error | ndm::Status::Cancelled => {
                        all_finished = false;
                        any_error = true;
                    }
                    ndm::Status::Downloading => {
                        all_finished = false;
                    }
                }
            }

            {
                let mut p = self.progress.lock().unwrap();
                if total_sum > 0 {
                    p.total = total_sum;
                }
                p.done = done_sum;
                p.speed_bps = if any_paused { 0.0 } else { speed_sum };
                p.conns = if any_paused || all_finished {
                    0
                } else {
                    conns_sum.max(1)
                };
                p.status = if any_paused {
                    "paused".to_string()
                } else {
                    "downloading".to_string()
                };
            }

            if any_error {
                for d in &dls {
                    d.control.cancel();
                }
                // KEEP part files + resume state (see above): the
                // yt-dlp fallback uses different file names, so the
                // engine partials cost nothing to keep and a later
                // retry resumes instead of re-downloading gigabytes.
                self.active_downloads.lock().unwrap().clear();
                return false; // Fall back to yt-dlp
            }

            if all_finished {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
        }

        self.active_downloads.lock().unwrap().clear();
        {
            let mut p = self.progress.lock().unwrap();
            if p.total > 0 {
                p.done = p.total;
            }
            p.speed_bps = 0.0;
            p.conns = 0;
        }

        // Merge / remux into final output file with ffmpeg
        let mut ff_args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
        for part in &part_paths {
            ff_args.push("-i".into());
            ff_args.push(part.display().to_string());
        }
        if self.audio {
            ff_args.push("-vn".into());
            if final_ext == "m4a" {
                ff_args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "192k".into()]);
            } else {
                ff_args.extend(["-c:a".into(), "libmp3lame".into(), "-q:a".into(), "0".into()]);
            }
        } else if part_paths.len() >= 2 {
            ff_args.extend([
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                "1:a:0".into(),
                "-c:v".into(),
                "copy".into(),
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                "192k".into(),
                "-movflags".into(),
                "+faststart".into(),
            ]);
        } else if final_ext == "mp4" || final_ext == "m4a" || final_ext == "mov" {
            // Normalize the audio to AAC like the two-stream branch:
            // a verbatim copy can leave Opus-in-MP4 that many players
            // refuse to play.
            ff_args.extend([
                "-c:v".into(),
                "copy".into(),
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                "192k".into(),
                "-movflags".into(),
                "+faststart".into(),
            ]);
        } else {
            ff_args.extend([
                "-c".into(),
                "copy".into(),
                "-movflags".into(),
                "+faststart".into(),
            ]);
        }
        ff_args.push(final_path.display().to_string());

        let merge_ok = Command::new(ffmpeg)
            .args(&ff_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && final_path.is_file();

        for p in &part_paths {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_file(ndm::state_path_for(p));
        }

        // A cancel that landed DURING the merge (which can take
        // seconds) must win over the merge result: publishing
        // "finished" here would resurrect a download the user just
        // cancelled and leave the merged file in place.
        if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
            return true; // a newer job owns the progress now
        }
        if let Some(flag) = self.stop_flag.lock().unwrap().clone() {
            if flag == "cancelled" {
                if merge_ok {
                    let _ = std::fs::remove_file(&final_path);
                }
                let mut p = self.progress.lock().unwrap();
                p.status = "cancelled".to_string();
                p.speed_bps = 0.0;
                p.conns = 0;
                return true;
            }
        }

        if merge_ok {
            match self.ensure_audio(&final_path, components_dir) {
                Ok(verified) => {
                    let mut p = self.progress.lock().unwrap();
                    if let Ok(m) = verified.metadata() {
                        p.total = m.len();
                        p.done = m.len();
                    }
                    p.status = "finished".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    p.final_path = Some(verified);
                    true
                }
                Err(msg) => {
                    // The bytes arrived but the file must not be
                    // presented as a success (e.g. it has no sound).
                    // Terminal: do NOT fall back to a second download.
                    let mut p = self.progress.lock().unwrap();
                    p.status = "error".to_string();
                    p.error = Some(msg);
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    p.final_path = Some(final_path);
                    true
                }
            }
        } else {
            false
        }
    }

    /// The silent-video guard. A video download is only "finished" when
    /// the file on disk actually contains the sound the source offered:
    /// verify the merged file's streams, and when the sound lives in a
    /// separate HLS rendition, download it and mux it in. Returns the
    /// final path (possibly the muxed replacement) or a user-readable
    /// error — a silent file must never pass as a success.
    fn ensure_audio(
        &self,
        final_path: &Path,
        components_dir: &Path,
    ) -> Result<PathBuf, String> {
        if self.audio {
            return Ok(final_path.to_path_buf());
        }
        let expect = *self.expect_audio.lock().unwrap();
        let pair = self.pair_audio_url.lock().unwrap().clone();
        let (has_video, has_audio) = match probe_streams(final_path, components_dir) {
            Some(pair) => pair,
            None => {
                if expect || pair.is_some() {
                    return Err("the download completed but its sound could not be verified (the stream check failed) — the file may be silent; please retry".to_string());
                }
                return Ok(final_path.to_path_buf());
            }
        };
        if !has_video {
            return Err("the downloaded file has no playable video stream — the source appears to be broken".to_string());
        }
        if has_audio {
            return Ok(final_path.to_path_buf());
        }
        // Video is here, sound is missing.
        if !expect && pair.is_none() {
            return Ok(final_path.to_path_buf()); // the source itself is silent
        }
        let pair_url = pair.ok_or_else(|| {
            "this video was expected to have sound, but no audio stream could be found for it — the file would be silent".to_string()
        })?;
        let ffmpeg = find_tool("ffmpeg", components_dir)
            .ok_or("ffmpeg is not available, so the separate audio track cannot be joined")?;
        let ytdlp = find_tool("yt-dlp", components_dir)
            .ok_or("yt-dlp is not available, so the separate audio track cannot be fetched")?;

        // 1. Fetch the audio rendition as its own small download.
        let stem = final_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "video".to_string());
        let audio_tmpl = self.dir.join(format!(".{stem}.ndm_audio.%(ext)s"));
        // Clear any stale audio left by an earlier failed attempt:
        // the fetch uses --no-overwrites with this fixed template and
        // the result scan below ignores pre-existing names, so a
        // leftover would strand the retry with "could not be
        // downloaded" even though a usable file is sitting there.
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            let stale_prefix = format!(".{stem}.ndm_audio.");
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(&stale_prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let before = dir_snapshot(&self.dir);
        let mut args = base_args_for_url(&pair_url, components_dir, self.cookies.as_deref());
        args.push("-f".into());
        args.push("bestaudio/best".into());
        args.push("--ffmpeg-location".into());
        args.push(
            ffmpeg
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        args.push("--no-overwrites".into());
        args.push("-o".into());
        args.push(audio_tmpl.display().to_string());
        args.push("--".into());
        args.push(pair_url.clone());
        let fetched = Command::new(&ytdlp)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let audio_file = if fetched {
            // whatever new file the rendition download produced
            let prefix = format!(".{stem}.ndm_audio.");
            let mut found: Option<PathBuf> = None;
            if let Ok(rd) = std::fs::read_dir(&self.dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with(&prefix) && !name.ends_with(".part") {
                        if !before.contains_key(&name) {
                            found = Some(e.path());
                            break;
                        }
                    }
                }
            }
            found
        } else {
            None
        };
        let audio_file = audio_file.ok_or_else(|| {
            "the separate audio track could not be downloaded, so the video would be silent — please retry".to_string()
        })?;

        // 2. Mux video + audio into a temp file and verify it.
        let mux_path = self.dir.join(format!(".{stem}.ndm_mux.mp4"));
        let mux_ok = Command::new(&ffmpeg)
            .args([
                "-y",
                "-loglevel",
                "error",
                "-i",
            ])
            .arg(final_path)
            .arg("-i")
            .arg(&audio_file)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-movflags",
                "+faststart",
            ])
            .arg(&mux_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && mux_path.is_file();
        let _ = std::fs::remove_file(&audio_file);
        if !mux_ok {
            let _ = std::fs::remove_file(&mux_path);
            return Err("joining the separate audio track into the video failed — the file would be silent; please retry".to_string());
        }
        match probe_streams(&mux_path, components_dir) {
            Some((true, true)) => {}
            _ => {
                let _ = std::fs::remove_file(&mux_path);
                return Err("the joined file still has no sound — refusing to call it finished".to_string());
            }
        }

        // 3. Replace the silent file with the muxed one. One rename
        // over the destination is atomic where the platform allows
        // it; deleting first would leave a crash window in which NO
        // file exists at the final path (only the hidden mux temp,
        // which nothing would ever pick up again).
        #[cfg(windows)]
        let _ = std::fs::remove_file(final_path);
        std::fs::rename(&mux_path, final_path).map_err(|e| {
            format!("the sound was joined but the file could not be saved in place: {e}")
        })?;
        Ok(final_path.to_path_buf())
    }

    fn spawn(self: &Arc<Self>, components_dir: &Path) -> Result<(), String> {
        let ytdlp = find_tool("yt-dlp", components_dir)
            .ok_or("yt-dlp not found — the video component is not installed yet")?;
        let ffmpeg = find_tool("ffmpeg", components_dir).ok_or(
            "ffmpeg is not available on this system — it joins the video and audio into one playable file. Install ffmpeg with your system package manager (Debian/Ubuntu: sudo apt install ffmpeg), or use an NDM build that bundles it, then try again".to_string(),
        )?;
        let sel = if self.selector.trim().is_empty() {
            ndm::video_probe::best_selector()
        } else {
            self.selector.clone()
        };

        let _spawn_guard = self.spawn_lock.lock().unwrap();
        {
            let mut slot = self.child.lock().unwrap();
            if let Some(c) = slot.as_mut() {
                if matches!(c.try_wait(), Ok(None)) {
                    return Err("this video is already downloading".to_string());
                }
            }
        }
        *self.stop_flag.lock().unwrap() = None;
        let my_gen = self
            .gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        {
            let mut p = self.progress.lock().unwrap();
            p.status = "downloading".to_string();
            p.conns = ndm::global_connections().clamp(1, 32);
        }

        let job = self.clone();
        let comp_dir = components_dir.to_path_buf();
        std::thread::spawn(move || {
            // 1. Try parallel multi-connection Rust engine download first
            if job.run_parallel_engine(&ytdlp, &ffmpeg, &comp_dir, &sel, my_gen) {
                return;
            }
            if job.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen
                || job.stop_flag.lock().unwrap().is_some()
            {
                return;
            }

            // 2. Fallback to yt-dlp with concurrent fragments for HLS/DASH streams
            let mut args = base_args_for_url(&job.url, &comp_dir, job.cookies.as_deref());
            args.push("-f".into());
            args.push(sel.clone());
            if job.audio {
                args.push("-x".into());
                args.push("--audio-format".into());
                args.push("mp3".into());
                args.push("--audio-quality".into());
                args.push("0".into());
            } else {
                args.push("--merge-output-format".into());
                args.push("mp4".into());
                args.push("--postprocessor-args".into());
                args.push("Merger+ffmpeg_o1:-c:v copy -c:a aac -b:a 192k".into());
            }
            let lim = job.limit();
            if lim > 0 {
                args.push("--limit-rate".into());
                args.push(lim.to_string());
            }
            let frags = ndm::global_connections().clamp(1, 32);
            args.push("--concurrent-fragments".into());
            args.push(frags.to_string());
            args.push("--buffer-size".into());
            args.push("512K".into());
            args.push("--http-chunk-size".into());
            args.push("10M".into());
            args.push("--hls-prefer-native".into());
            args.push("--no-overwrites".into());

            let limit = ndm::global_limit();
            if limit > 0 {
                args.push("--limit-rate".into());
                args.push(format!("{}K", (limit / 1024).max(1)));
            }
            if let Some(parent) = ffmpeg.parent() {
                args.push("--ffmpeg-location".into());
                args.push(parent.display().to_string());
            }
            args.push("--progress-template".into());
            args.push(
                "NDMPROG:%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s"
                    .into(),
            );
            let stem = job.resolve_or_lock_stem("");
            let out_tmpl = if stem.is_empty() {
                job.dir.join("%(title).150B [%(id)s].%(ext)s")
            } else {
                job.dir.join(format!("{stem}.%(ext)s"))
            };
            args.push("-o".into());
            args.push(out_tmpl.display().to_string());
            args.push("--".into());
            args.push(job.url.clone());

            let Ok(mut child) = Command::new(&ytdlp)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            else {
                let mut p = job.progress.lock().unwrap();
                p.status = "error".to_string();
                p.conns = 0;
                p.error = Some("could not start yt-dlp".to_string());
                return;
            };
            let Some(stdout) = child.stdout.take() else {
                return;
            };
            let stderr = child.stderr.take();
            let my_pid = child.id();
            {
                let mut slot = job.child.lock().unwrap();
                if let Some(mut old) = slot.take() {
                    let _ = old.wait();
                }
                *slot = Some(child);
            }

            let err_buf = Arc::new(Mutex::new(String::new()));
            let err_thread = if let Some(mut se) = stderr {
                let eb = err_buf.clone();
                Some(std::thread::spawn(move || {
                    use std::io::Read as _;
                    let mut tail: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 8192];
                    loop {
                        match se.read(&mut chunk) {
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
                    *eb.lock().unwrap() = String::from_utf8_lossy(&tail).to_string();
                }))
            } else {
                None
            };
            let before = dir_snapshot(&job.dir);
            let mut base: u64 = 0;
            let mut cur_done: u64 = 0;
            let mut cur_total: u64 = 0;
            let mut detected_final: Option<PathBuf> = None;

            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(rest) = line.strip_prefix("NDMPROG:") {
                    let parts: Vec<&str> = rest.split('|').collect();
                    let parse = |i: usize| -> Option<f64> {
                        parts.get(i).and_then(|s| s.trim().parse::<f64>().ok())
                    };
                    let mut p = job.progress.lock().unwrap();
                    p.conns = frags;
                    if let Some(d) = parse(0) {
                        let d = d as u64;
                        if cur_done > 262_144 && d + 262_144 < cur_done {
                            base += if cur_total > 0 { cur_total } else { cur_done };
                            cur_total = 0;
                        }
                        cur_done = d;
                        p.done = base + cur_done;
                    }
                    if let Some(t) = parse(1).or_else(|| parse(2)) {
                        if t > 0.0 {
                            cur_total = t as u64;
                            p.total = base + cur_total;
                        }
                    }
                    if let Some(sp) = parse(3) {
                        p.speed_bps = sp;
                    }
                } else {
                    if let Some(fp) = parse_output_path_line(&line) {
                        detected_final = Some(fp);
                    }
                    if line.contains("[Merger]") || line.contains("[ExtractAudio]") {
                        let mut p = job.progress.lock().unwrap();
                        if p.total > 0 {
                            p.done = p.total;
                        }
                        p.speed_bps = 0.0;
                    }
                }
            }
            if job.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                return;
            }
            let taken = {
                let mut slot = job.child.lock().unwrap();
                match slot.as_mut() {
                    Some(c) if c.id() == my_pid => slot.take(),
                    _ => None,
                }
            };
            let status = match taken {
                Some(mut c) => c.wait().ok(),
                None => None,
            };
            // The stderr reader publishes its tail only at EOF; wait
            // for it, or the failure reason races us and is lost.
            if let Some(h) = err_thread {
                let _ = h.join();
            }
            let succeeded = status.map(|s| s.success()).unwrap_or(false);
            // A pause/cancel that landed while yt-dlp was finishing
            // wins over the success report: the user stopped this job,
            // so its files stay partial and resumable.
            let stop = job.stop_flag.lock().unwrap().clone();
            // Resolve the outcome BEFORE taking the progress lock: the
            // audio guard shells out to ffmpeg/yt-dlp and can take a
            // few seconds on the pairing path.
            enum Outcome {
                Finished(PathBuf),
                GuardError(PathBuf, String),
                MergeMissing,
                Stopped(String),
                Failed,
            }
            let outcome = if let Some(flag) = stop {
                Outcome::Stopped(flag)
            } else if succeeded {
                let prev_final = job.progress.lock().unwrap().final_path.clone();
                let stem_hint = job
                    .chosen_stem
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_default();
                let found = detected_final
                    .filter(|fp| fp.is_file())
                    .or_else(|| prev_final.filter(|fp| fp.is_file()))
                    .or_else(|| find_final_file(&job.dir, &before, job.audio, &stem_hint));
                match found {
                    Some(fp) => match job.ensure_audio(&fp, &comp_dir) {
                        Ok(verified) => Outcome::Finished(verified),
                        Err(msg) => Outcome::GuardError(fp, msg),
                    },
                    None => Outcome::MergeMissing,
                }
            } else {
                Outcome::Failed
            };
            let mut p = job.progress.lock().unwrap();
            p.conns = 0;
            match outcome {
                Outcome::Finished(fp) => {
                    p.status = "finished".to_string();
                    if let Ok(m) = fp.metadata() {
                        p.total = m.len();
                        p.done = m.len();
                    }
                    p.final_path = Some(fp);
                    p.speed_bps = 0.0;
                }
                Outcome::GuardError(fp, msg) => {
                    p.status = "error".to_string();
                    p.error = Some(msg);
                    p.final_path = Some(fp);
                    p.speed_bps = 0.0;
                }
                Outcome::MergeMissing => {
                    p.status = "error".to_string();
                    p.error = Some(
                        "the video and audio downloaded but could not be merged into one playable file — merging needs ffmpeg. Reinstall NDM with apt (ffmpeg is a package dependency), then try again"
                            .to_string(),
                    );
                    p.speed_bps = 0.0;
                }
                Outcome::Stopped(flag) => {
                    p.status = flag;
                    p.speed_bps = 0.0;
                }
                Outcome::Failed => {
                    p.status = "error".to_string();
                    p.speed_bps = 0.0;
                    let eb = err_buf.lock().unwrap();
                    p.error = eb
                        .lines()
                        .rev()
                        .find(|l| l.contains("ERROR"))
                        .map(|l| l.trim().to_string())
                        .or_else(|| {
                            eb.lines()
                                .rev()
                                .find(|l| !l.trim().is_empty())
                                .map(|l| l.trim().to_string())
                        })
                        .or_else(|| Some("yt-dlp failed".to_string()));
                }
            }
        });
        Ok(())
    }

    /// Set this job's own speed limit (bytes/sec, 0 = none): live
    /// engine streams adopt it immediately; the yt-dlp fallback reads
    /// it when (re)spawned.
    pub fn set_limit(&self, bytes_per_sec: u64) {
        self.limit_bps
            .store(bytes_per_sec, std::sync::atomic::Ordering::Relaxed);
        for d in self.active_downloads.lock().unwrap().iter() {
            d.set_limit(bytes_per_sec);
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit_bps.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn pause(&self) {
        let _spawn_guard = self.spawn_lock.lock().unwrap();
        let active = self.active_downloads.lock().unwrap().clone();
        if !active.is_empty() {
            for d in &active {
                d.control.pause();
            }
            let mut p = self.progress.lock().unwrap();
            p.status = "paused".to_string();
            p.speed_bps = 0.0;
            p.conns = 0;
            return;
        }
        *self.stop_flag.lock().unwrap() = Some("paused".to_string());
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
        let mut p = self.progress.lock().unwrap();
        if p.status == "downloading" {
            p.status = "paused".to_string();
            p.speed_bps = 0.0;
            p.conns = 0;
        }
    }

    pub fn cancel(&self) {
        let _spawn_guard = self.spawn_lock.lock().unwrap();
        *self.stop_flag.lock().unwrap() = Some("cancelled".to_string());
        for d in self.active_downloads.lock().unwrap().iter() {
            d.control.cancel();
        }
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
        let mut p = self.progress.lock().unwrap();
        p.status = "cancelled".to_string();
        p.conns = 0;
    }

    pub fn resume(self: &Arc<Self>, components_dir: &Path) -> Result<(), String> {
        {
            let active = self.active_downloads.lock().unwrap().clone();
            if !active.is_empty() {
                *self.stop_flag.lock().unwrap() = None;
                for d in &active {
                    d.control.resume();
                }
                self.progress.lock().unwrap().status = "downloading".to_string();
                return Ok(());
            }
        }
        let running = self
            .child
            .lock()
            .unwrap()
            .as_mut()
            .map(|c| c.try_wait().map(|s| s.is_none()).unwrap_or(false))
            .unwrap_or(false);
        if running {
            return Ok(());
        }
        self.spawn(components_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_mp4_reports_both_streams() {
        let info = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'out.mp4':
  Duration: 00:03:12.04, start: 0.000000, bitrate: 2145 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p, 1920x1080, 1980 kb/s, 30 fps
  Stream #0:1[0x2](eng): Audio: aac (LC) (mp4a / 0x6134706D), 48000 Hz, stereo, fltp, 160 kb/s
At least one output file must be specified";
        assert_eq!(streams_in_ffmpeg_info(info), (true, true));
    }

    #[test]
    fn video_only_file_reports_missing_audio() {
        let info = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'silent.mp4':
  Duration: 00:03:12.04, start: 0.000000, bitrate: 1980 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p, 1920x1080, 1980 kb/s, 30 fps
At least one output file must be specified";
        assert_eq!(streams_in_ffmpeg_info(info), (true, false));
    }

    #[test]
    fn audio_only_file_reports_no_video() {
        let info = "\
Input #0, mp3, from 'song.mp3':
  Duration: 00:03:12.04, start: 0.025057, bitrate: 192 kb/s
  Stream #0:0: Audio: mp3, 44100 Hz, stereo, fltp, 192 kb/s";
        assert_eq!(streams_in_ffmpeg_info(info), (false, true));
    }

    #[test]
    fn safe_filename_stem_preserves_full_title_without_truncating_at_60_chars() {
        assert_eq!(
            safe_filename_stem("Fixture Player — Test Video"),
            "Fixture Player — Test Video"
        );
        assert_eq!(
            safe_filename_stem("Fixture Player — Test Video.mp4"),
            "Fixture Player — Test Video"
        );
    }

    #[test]
    fn unique_stem_on_disk_disambiguates_duplicate_titles() {
        let dir = std::env::temp_dir().join(format!("ndm-uniq-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s0 = unique_stem_on_disk(&dir, "Fixture Player — Test Video");
        assert_eq!(s0, "Fixture Player — Test Video");
        std::fs::write(dir.join(format!("{s0}.mp4")), b"1").unwrap();
        let s1 = unique_stem_on_disk(&dir, "Fixture Player — Test Video");
        assert_eq!(s1, "Fixture Player — Test Video (1)");
        std::fs::write(dir.join(format!("{s1}.mp4")), b"2").unwrap();
        let s2 = unique_stem_on_disk(&dir, "Fixture Player — Test Video");
        assert_eq!(s2, "Fixture Player — Test Video (2)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

