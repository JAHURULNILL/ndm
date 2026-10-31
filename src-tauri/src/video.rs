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
use std::time::{Instant, SystemTime};

/// Build a Command for a bundled console tool (yt-dlp/ffmpeg).
/// On Windows the app is a GUI-subsystem process with no console
/// of its own, so a console child would allocate a flashing
/// console window unless created with CREATE_NO_WINDOW;
/// elsewhere this is a plain Command::new.
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
    let out = tool_cmd(ffmpeg)
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

/// Completion size reconciliation for MERGE/REMUX jobs (F-EXT-04,
/// R3-EXT-01). The question at the finish line is whether the merged
/// output holds the media this job selected — so the baseline is
/// `expected_media`, the expected size of the SELECTED media streams
/// (the chosen video + audio payloads), and never the raw downloaded
/// byte count: a container may legitimately carry a large non-media
/// payload (padding, embedded data, extra renditions) that a remux
/// drops, so a correct merge can be a small fraction of what was
/// downloaded. The earlier form of this guard compared against the
/// download total and declared byte-perfect merges broken (the R3
/// fixture: 100 MB container, 2.3 MB of media, errored 3×). A merged
/// file under half of the expected media is still the drastic case
/// it exists for — a stub where the real movie should be.
///
/// `expected_media == None` (or 0) means the selected streams'
/// sizes are genuinely unknown — the source reported no bitrates to
/// estimate from — and the guard then SKIPS the job rather than
/// fabricating a verdict. `downloaded` is accepted so callers and
/// tests can state the full scenario; it is deliberately never
/// compared against.
///
/// SCOPE (R2-EXT-02): callers apply this ONLY to merge/remux jobs.
/// Audio extraction/transcode jobs (`self.audio`) are never judged
/// by it: their correct output is routinely a small fraction of the
/// source container, and the guard used to fail every one of them
/// after a perfect download.
fn drastic_shrink(
    expected_media: Option<u64>,
    downloaded: u64,
    final_len: u64,
) -> Option<String> {
    let _ = downloaded; // context only — see the note above
    let expected = expected_media?;
    if expected > 0 && final_len.saturating_mul(2) < expected {
        Some(format!(
            "the merged file is much smaller than what was downloaded ({final_len} of {expected} bytes of media in the selected streams) — the source streams may be incomplete"
        ))
    } else {
        None
    }
}

/// The container duration in `ffmpeg -i` text ("Duration:
/// HH:MM:SS.xx, ..."), in seconds. None when unreported.
fn duration_in_ffmpeg_info(text: &str) -> Option<f64> {
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("Duration: ") {
            let token = rest.split(',').next()?.trim();
            if token == "N/A" {
                return None;
            }
            let mut parts = token.split(':');
            let h: f64 = parts.next()?.parse().ok()?;
            let m: f64 = parts.next()?.parse().ok()?;
            let s: f64 = parts.next()?.parse().ok()?;
            return Some(h * 3600.0 + m * 60.0 + s);
        }
    }
    None
}

/// Run `ffmpeg -i` over a file and read its container duration.
fn probe_duration_secs(path: &Path, components_dir: &Path) -> Option<f64> {
    let ffmpeg = find_tool("ffmpeg", components_dir)?;
    let out = tool_cmd(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    duration_in_ffmpeg_info(&String::from_utf8_lossy(&out.stderr))
}

/// W-2 finish-line contradiction check. `drastic_shrink` judges a
/// merge against the RESOLVED streams; this judges the produced
/// file against what the user was SHOWN and picked at add time —
/// the chosen option's probed size and the page video's probed
/// duration (0 = unknown, that half of the check then sits out).
/// A plain contradiction — the delivered file runs half the
/// expected length or less (or twice it), or lands under a quarter
/// of the expected size — means a different stream than the one
/// probed was delivered (the walkthrough's silent-substitution
/// case), and the entry must say so instead of presenting a clean
/// success. Deliberately NOT judged: the raw downloaded byte count
/// (a padded container legitimately remuxes down to its media —
/// R3-EXT-01), and the size half never applies to audio jobs,
/// whose transcodes legitimately change size (only their duration
/// is checked).
fn contradiction_verdict(
    expect_size: u64,
    expect_duration: f64,
    produced_size: u64,
    produced_duration: Option<f64>,
    audio: bool,
) -> Option<String> {
    if expect_duration >= 1.0 {
        if let Some(d) = produced_duration {
            if d >= 0.5 && (d < expect_duration * 0.5 || d > expect_duration * 2.0) {
                return Some(format!(
                    "the downloaded file runs about {}s but the page's video is about {}s — the wrong stream may have been delivered; please retry",
                    d.round() as u64,
                    expect_duration.round() as u64
                ));
            }
        }
    }
    if !audio && expect_size > 0 && produced_size > 0 && produced_size.saturating_mul(4) < expect_size {
        return Some(format!(
            "the downloaded file is {produced_size} bytes but the quality you picked was about {expect_size} bytes — the wrong stream may have been delivered; please retry"
        ));
    }
    None
}

/// Estimate a media file's true media payload in bytes from its
/// `ffmpeg -i` text: the container duration × the sum of its
/// streams' OWN bitrates. Per-stream bitrates describe the media
/// itself, so container padding and other non-media payload do not
/// inflate the estimate the way the file's byte count would (the
/// Duration line's bitrate is derived from that byte count and is
/// useless for this — 134217 kb/s for the padded R3 fixture, whose
/// streams report their true 3010 + 69 kb/s). Returns None when
/// the duration or any stream's bitrate is unreported: an estimate
/// is never fabricated from partial data.
pub fn media_payload_estimate(text: &str) -> Option<u64> {
    let mut duration_secs: Option<f64> = None;
    let mut total_kbps: u64 = 0;
    let mut streams = 0usize;
    for line in text.lines() {
        let t = line.trim_start();
        if duration_secs.is_none() {
            if let Some(rest) = t.strip_prefix("Duration: ") {
                let token = rest.split(',').next()?.trim();
                if token != "N/A" {
                    let mut parts = token.split(':');
                    let h: f64 = parts.next()?.parse().ok()?;
                    let m: f64 = parts.next()?.parse().ok()?;
                    let s: f64 = parts.next()?.parse().ok()?;
                    duration_secs = Some(h * 3600.0 + m * 60.0 + s);
                }
            }
        }
        if t.starts_with("Stream #") {
            streams += 1;
            // the stream's own bitrate is the first "kb/s" figure
            // on its line; a stream that reports none leaves the
            // file's payload unknowable, so the estimate fails
            let (before, _) = t.split_once(" kb/s")?;
            let kbps: u64 = before
                .rsplit(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()?;
            if kbps == 0 {
                return None;
            }
            total_kbps = total_kbps.saturating_add(kbps);
        }
    }
    let duration = duration_secs?;
    if streams == 0 || duration <= 0.0 || total_kbps == 0 {
        return None;
    }
    let bytes = (duration * total_kbps as f64 * 1000.0 / 8.0) as u64;
    (bytes > 0).then_some(bytes)
}

/// Run `ffmpeg -i` over a downloaded file and estimate its media
/// payload (see `media_payload_estimate`).
fn estimate_media_payload(path: &Path, components_dir: &Path) -> Option<u64> {
    let ffmpeg = find_tool("ffmpeg", components_dir)?;
    let out = tool_cmd(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    media_payload_estimate(&stderr)
}

/// Estimate the selected formats' combined media payload from
/// yt-dlp's resolution: each format's bitrate (tbr, else vbr+abr)
/// × the media duration. yt-dlp's `filesize` fields are
/// deliberately NOT used — for direct containers they report the
/// container's byte count, which can far exceed the media inside
/// (R3-EXT-01; for the padded fixture they are absent entirely).
/// None when any selected format reports no bitrate or the
/// duration is unknown.
fn formats_media_bytes(duration_secs: f64, formats: &[&serde_json::Value]) -> Option<u64> {
    if duration_secs <= 0.0 || formats.is_empty() {
        return None;
    }
    let mut total_kbps = 0.0f64;
    for f in formats {
        let br = f["tbr"].as_f64().filter(|b| *b > 0.0).or_else(|| {
            let v = f["vbr"].as_f64().unwrap_or(0.0);
            let a = f["abr"].as_f64().unwrap_or(0.0);
            (v + a > 0.0).then_some(v + a)
        })?;
        total_kbps += br;
    }
    let bytes = (total_kbps * 1000.0 / 8.0 * duration_secs) as u64;
    (bytes > 0).then_some(bytes)
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

/// Remove THIS job's intermediate files from its save folder —
/// BOTH engines' working files for `stem`: the parallel engine's
/// hidden parts and their resume-state journals (".{stem}.
/// ndm_stream*" — the journal rides the same prefix), its mux /
/// audio-pairing temps, and yt-dlp's "{stem}.f<digits>…" split
/// parts and "{stem}.<ext>…" single-format parts (.part / .ytdl).
/// Called when a run publishes "finished": an earlier attempt of
/// the SAME job may have run on the OTHER engine (pause kills the
/// yt-dlp child; the resume tries the parallel engine first), and
/// each engine's own finish cleanup only ever knew its own temps
/// — the abandoned attempt's partial survived next to the
/// finished file (R4 side observation: an orphaned 13.9 MB
/// big2.mp4.part after a pause/resume → finish cycle). The
/// finished file itself never matches: it carries no .part/.ytdl
/// suffix and no leading dot. Strictly this stem — the same rule
/// the discard sweep in main.rs applies.
fn sweep_job_temps(dir: &Path, stem: &str) {
    if stem.is_empty() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for f in rd.flatten() {
        let n = f.file_name().to_string_lossy().to_string();
        let engine_temp = n.starts_with(&format!(".{stem}.ndm_stream"))
            || n.starts_with(&format!(".{stem}.ndm_mux"))
            || n.starts_with(&format!(".{stem}.ndm_audio"));
        let ytdlp_part = (n.ends_with(".part") || n.ends_with(".ytdl")) && {
            let split = n
                .strip_prefix(&format!("{stem}.f"))
                .map(|rest| {
                    rest.chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let single = n
                .strip_prefix(&format!("{stem}."))
                .map(|rest| {
                    ["mp4", "m4a", "mp3", "webm", "mkv", "ts"]
                        .iter()
                        .any(|e| rest == *e || rest.starts_with(&format!("{e}.")))
                })
                .unwrap_or(false);
            split || single
        };
        if engine_temp || ytdlp_part {
            let _ = std::fs::remove_file(f.path());
        }
    }
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
    /// when `done` last changed: a "downloading" job whose bytes
    /// have not moved for a few seconds is stalled, whatever its
    /// last sampled speed says — payload_of decays the reported
    /// rate to 0 from this (F-15 state honesty)
    pub last_progress_at: Instant,
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
            last_progress_at: Instant::now(),
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

/// The filename extensions a job of one kind can actually land on
/// disk with: video jobs mux to MP4 (MKV/WebM are the other video
/// containers a same-title file may already occupy); audio jobs save
/// MP3 (yt-dlp extraction) or M4A (the chosen stream's own
/// container). A stem only collides within its OWN kind — a finished
/// `big2.mp4` must not push the audio half of a download-all pair to
/// "big2 (1).m4a" when no `big2.m4a` exists (R3-EXT-04).
pub fn own_media_exts(audio: bool) -> &'static [&'static str] {
    if audio {
        &["mp3", "m4a"]
    } else {
        &["mp4", "mkv", "webm"]
    }
}

/// True when `cand` is already used inside `dir` by a finished file
/// (or an in-flight `.part`) whose extension belongs to this kind —
/// i.e. the new job's own final filename would collide.
pub fn stem_taken_on_disk(dir: &Path, cand: &str, audio: bool) -> bool {
    own_media_exts(audio).iter().any(|ext| {
        dir.join(format!("{cand}.{ext}")).exists()
            || dir
                .join(format!(".{cand}.ndm_stream0.{ext}.part"))
                .exists()
    })
}

/// Choose a non-colliding filename stem inside `dir` so two downloads
/// of the same kind with the same title (or two qualities of the same
/// video) never overwrite each other or collide on `.part` files.
pub fn unique_stem_on_disk(dir: &Path, base_stem: &str, audio: bool) -> String {
    let stem = safe_filename_stem(base_stem);
    if stem.is_empty() {
        return String::new();
    }
    if !stem_taken_on_disk(dir, &stem, audio) {
        return stem;
    }
    for n in 1..10_000 {
        let cand = format!("{stem} ({n})");
        if !stem_taken_on_disk(dir, &cand, audio) {
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
    /// the selected formats' combined media payload in bytes,
    /// learned from yt-dlp's resolution (bitrates × duration) —
    /// the finish-line size guard's baseline for the yt-dlp
    /// finalize path. None when the resolution reported no usable
    /// bitrates: the guard then skips the job instead of guessing
    /// (R3-EXT-01)
    expected_media: Mutex<Option<u64>>,
    /// separate HLS audio rendition (master-playlist EXT-X-MEDIA) that the
    /// app must fetch and mux itself; yt-dlp's generic extractor cannot
    /// pair it with the chosen variant
    pair_audio_url: Mutex<Option<String>>,
    /// this entry's own speed limit in bytes/sec (0 = none) — applied
    /// to every engine stream and to the yt-dlp fallback
    limit_bps: std::sync::atomic::AtomicU64,
    /// saved Site Login for this page: passed to yt-dlp as
    /// --username/--password whenever non-empty, the way the HTTP
    /// engine applies the same login as Basic credentials
    login: String,
    password: String,
    /// set when a pause landed DURING the ffmpeg merge: the merged
    /// output file, kept on disk and recorded here, which the next
    /// resume finalizes instead of re-downloading everything
    merged_final: Mutex<Option<PathBuf>>,
    /// what the user was shown and chose at add time: the picked
    /// option's probed size in bytes and the page video's probed
    /// duration in seconds (0 = unknown). The finish line
    /// cross-checks the produced file against them (W-2): a
    /// delivery that plainly contradicts the probed expectation is
    /// surfaced as an error, never presented as a clean success.
    expect_size: u64,
    expect_duration: f64,
}

#[derive(Clone)]
struct DirectStream {
    url: String,
    ext: String,
    has_audio: bool,
    cookie: Option<String>,
    referer: Option<String>,
}

/// A completed stream resolution: everything a job start needs to
/// skip the yt-dlp resolve run. Stream URLs are signed and perishable,
/// so resolutions are cached only briefly (see RESOLUTION_TTL) and the
/// cache key covers every input that can change the result — the full
/// page URL, the effective selector, the audio flag, the site login,
/// a hash of its password (m-6: the login NAME alone used to key it,
/// so a corrected password kept being served the resolution the wrong
/// one produced), the proxy, and a hash of the cookie file's BYTES
/// (a re-login or a refreshed session changes the bytes, so it can
/// never be served a resolution made under the old cookies). A cached
/// resolution can therefore only be wrong if the STREAM URL itself
/// died inside the TTL window — and that case is already handled: the
/// engine's start or stream error path falls back to the yt-dlp
/// download, which re-resolves independently. A stale hit can cost
/// one fast failed engine attempt, never a wrong file.
#[derive(Clone)]
struct Resolution {
    title: String,
    streams: Vec<DirectStream>,
    expect_audio: bool,
    expected_media: Option<u64>,
    pair_audio_url: Option<String>,
}

#[derive(PartialEq, Eq)]
struct ResolutionKey {
    url: String,
    sel: String,
    audio: bool,
    login: String,
    password_sig: u64,
    cookies_sig: u64,
    proxy: String,
}

/// FNV-1a over the password's bytes — the cache key must change when
/// the password does, without ever holding the password itself. The
/// empty password hashes to the same sentinel family as a missing
/// cookie file: 0, which no real password's hash is remapped to.
fn password_signature(password: &str) -> u64 {
    if password.is_empty() {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in password.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// How long a resolution stays reusable. Signed CDN URLs live for
/// hours; ten minutes is deliberately far inside that, while still
/// covering the flows that re-resolve today: pause-killed yt-dlp
/// jobs, error retries, restored entries after an app restart, a
/// repeat add of the same quality, and a click that follows a
/// flyout-open warm resolve (see warm_resolution).
const RESOLUTION_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const RESOLUTION_CACHE_CAP: usize = 64;

static RESOLUTION_CACHE: Mutex<Vec<(ResolutionKey, Instant, Resolution)>> =
    Mutex::new(Vec::new());

/// Content signature of the cookie file a resolution was made with:
/// FNV-1a over its bytes. No file and an unreadable file get distinct
/// sentinels so neither can alias a real (or empty) file's hash.
fn cookies_signature(cookies: Option<&Path>) -> u64 {
    let Some(p) = cookies else { return 0 };
    let Ok(bytes) = std::fs::read(p) else {
        return u64::MAX;
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn resolution_cache_get(key: &ResolutionKey, now: Instant) -> Option<Resolution> {
    let mut cache = RESOLUTION_CACHE.lock().unwrap();
    cache.retain(|(_, at, _)| now.duration_since(*at) < RESOLUTION_TTL);
    cache
        .iter()
        .find(|(k, _, _)| k == key)
        .map(|(_, _, r)| r.clone())
}

fn resolution_cache_store(key: ResolutionKey, res: Resolution, now: Instant) {
    let mut cache = RESOLUTION_CACHE.lock().unwrap();
    cache.retain(|(k, at, _)| k != &key && now.duration_since(*at) < RESOLUTION_TTL);
    cache.push((key, now, res));
    while cache.len() > RESOLUTION_CACHE_CAP {
        cache.remove(0);
    }
}

/// The full outcome of one yt-dlp resolve run. `resolved` is None
/// when the selection is not eligible for the parallel engine (the
/// caller then takes the yt-dlp download path) — the learned fields
/// are still meaningful in that case and the job wrapper applies
/// them exactly as the inline code did before this split.
struct ResolutionProbe {
    expect_audio: bool,
    pair_audio_url: Option<String>,
    expected_media: Option<u64>,
    resolved: Option<(String, Vec<DirectStream>)>,
}

/// True when a yt-dlp format is a playlist (HLS/DASH) rather than
/// a directly downloadable media file.
fn format_is_playlist(f: &serde_json::Value) -> bool {
    let proto = f["protocol"].as_str().unwrap_or("");
    if proto.contains("m3u8") || proto.contains("dash") {
        return true;
    }
    let u = f["url"].as_str().unwrap_or("").to_ascii_lowercase();
    u.contains(".m3u8") || u.contains(".mpd")
}

/// The best DIRECT media format in a probe's full format list —
/// the page's own `<video>` file, as opposed to a playlist variant
/// a script or a CDN ladder offered alongside it. None when the
/// list holds no direct video file. Ranking follows the app's
/// usual preference: avc-in-mp4 first, then height, then bitrate.
fn best_direct_format<'a>(v: &'a serde_json::Value) -> Option<&'a serde_json::Value> {
    let formats = v["formats"].as_array()?;
    let mut best: Option<(&serde_json::Value, (u8, u64, u64))> = None;
    for f in formats {
        if format_is_playlist(f) {
            continue;
        }
        let proto = f["protocol"].as_str().unwrap_or("");
        if proto != "http" && proto != "https" {
            continue;
        }
        let u = f["url"].as_str().unwrap_or("");
        if !u.starts_with("http://") && !u.starts_with("https://") {
            continue;
        }
        let ext = f["ext"].as_str().unwrap_or("");
        if !matches!(ext, "mp4" | "webm" | "mkv" | "mov") {
            continue;
        }
        // an entry that DECLARES itself audio-only is not the
        // page's video; undeclared codecs (generic pages) pass
        let vc = f["vcodec"].as_str().unwrap_or("");
        if vc == "none" {
            continue;
        }
        if f["format_id"].as_str().unwrap_or("").contains("storyboard") {
            continue;
        }
        let avc_mp4 = u8::from(ext == "mp4" && vc.starts_with("avc"));
        let height = f["height"].as_u64().unwrap_or(0);
        let tbr = f["tbr"].as_f64().unwrap_or(0.0) as u64;
        let rank = (avc_mp4, height, tbr);
        if best.map(|(_, r)| rank > r).unwrap_or(true) {
            best = Some((f, rank));
        }
    }
    best.map(|(f, _)| f)
}

/// One yt-dlp `-j -f <sel>` run against `url`, parsed into a
/// ResolutionProbe. Free-standing (no job) so the flyout's warm
/// resolve can populate the cache before any job exists.
#[allow(clippy::too_many_arguments)]
fn resolve_streams_core(
    url: &str,
    sel: &str,
    audio: bool,
    cookies: Option<&Path>,
    login: &str,
    password: &str,
    ytdlp: &Path,
    components_dir: &Path,
) -> Option<ResolutionProbe> {
    let mut args = base_args_for_url(url, components_dir, cookies);
    if !login.is_empty() {
        args.push("--username".into());
        args.push(login.to_string());
        args.push("--password".into());
        args.push(password.to_string());
    }
    args.push("-j".into());
    args.push("-f".into());
    args.push(sel.to_string());
    args.push("--".into());
    args.push(url.to_string());

    let out = tool_cmd(ytdlp)
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
    let mut formats: Vec<&serde_json::Value> = match v["requested_formats"].as_array() {
        Some(arr) if !arr.is_empty() => arr.iter().collect(),
        _ => vec![&v],
    };
    // W-2: a page's own direct media source outranks an indirect
    // variant. When the app's best-quality selection resolved to
    // playlist streams only — the shape a page gets when a script
    // planted an HLS/DASH variant next to a plain <video> element —
    // while the probe's full format list holds a direct media file
    // for the same content, the direct file is what the user means
    // by "this video": resolve THAT for the engine instead of
    // letting the yt-dlp fallback fetch the variant. Explicit
    // per-quality selectors keep yt-dlp's own choice — the user
    // picked that variant deliberately.
    if !audio && sel == ndm::video_probe::best_selector() {
        let selection_is_playlist =
            !formats.is_empty() && formats.iter().all(|f| format_is_playlist(f));
        if selection_is_playlist {
            if let Some(direct) = best_direct_format(&v) {
                formats = vec![direct];
            }
        }
    }
    // Learn what this download SHOULD contain before any early
    // return below can lose the information: whether the chosen
    // formats carry sound, and — for HLS sources whose sound is a
    // separate rendition in the master playlist — where it lives.
    let any_audio = formats.iter().any(|f| {
        let ac = f["acodec"].as_str().unwrap_or("none");
        !ac.is_empty() && ac != "none"
    });
    let mut pair_audio_url: Option<String> = None;
    if !any_audio && !audio {
        let clean_url = url
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
            if let Some(u) = ndm::video_probe::fetch_hls_default_audio(url, &referer, &cookie) {
                pair_audio_url = Some(u);
            }
        }
    }
    if formats.is_empty() || formats.len() > 2 {
        return Some(ResolutionProbe {
            expect_audio: any_audio,
            pair_audio_url,
            expected_media: None,
            resolved: None,
        });
    }
    // Learn the selected streams' combined media payload for
    // the finish-line guard (see the field's note): recorded
    // only when every selected format reported a bitrate.
    // (Recorded before the stream checks below on purpose: if
    // this resolution is rejected, the yt-dlp fallback still
    // downloads this same selection, so the estimate stands.)
    let expected_media =
        formats_media_bytes(v["duration"].as_f64().unwrap_or(0.0), &formats);
    let mut streams = Vec::new();
    let mut saw_vcodec = false;
    let mut any_video = false;
    for f in formats {
        let proto = f["protocol"].as_str().unwrap_or("");
        if proto != "https" && proto != "http" {
            return Some(ResolutionProbe {
                expect_audio: any_audio,
                pair_audio_url,
                expected_media,
                resolved: None,
            });
        }
        let u = f["url"].as_str().unwrap_or("");
        if !u.starts_with("http://") && !u.starts_with("https://") {
            return Some(ResolutionProbe {
                expect_audio: any_audio,
                pair_audio_url,
                expected_media,
                resolved: None,
            });
        }
        let lower = u.to_ascii_lowercase();
        if lower.contains(".m3u8") || lower.contains(".mpd") {
            return Some(ResolutionProbe {
                expect_audio: any_audio,
                pair_audio_url,
                expected_media,
                resolved: None,
            });
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
        {
            let vc = f["vcodec"].as_str().unwrap_or("");
            if !vc.is_empty() {
                saw_vcodec = true;
                if vc != "none" {
                    any_video = true;
                }
            }
        }
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
    if !audio && saw_vcodec && !any_video {
        // Defensive guard: the selector resolved to an
        // audio-only set for a VIDEO job (every format declares
        // vcodec "none"). The parallel engine would download
        // the whole set and only fail at the finish line
        // ("no playable video stream") — refuse here instead,
        // so the caller falls back to yt-dlp, which re-resolves
        // independently. Formats that declare no codec info at
        // all (saw_vcodec == false) keep the old behaviour.
        return Some(ResolutionProbe {
            expect_audio: any_audio,
            pair_audio_url,
            expected_media,
            resolved: None,
        });
    }
    Some(ResolutionProbe {
        expect_audio: any_audio,
        pair_audio_url,
        expected_media,
        resolved: Some((resolved_title, streams)),
    })
}

/// Resolve a page URL + selector in the background and cache the
/// result, so the download click that follows (seconds later, once
/// the user picks a quality in the flyout) starts without paying a
/// second yt-dlp run. Called on a dedicated thread by the incoming
/// queue drain for {"kind":"warm"} lines; a no-op when a fresh
/// resolution for the same key already exists.
pub fn warm_resolution(
    url: &str,
    sel: &str,
    audio: bool,
    cookies: Option<PathBuf>,
    login: &str,
    password: &str,
    components_dir: &Path,
) {
    let key = ResolutionKey {
        url: url.to_string(),
        sel: sel.to_string(),
        audio,
        login: login.to_string(),
        password_sig: password_signature(password),
        cookies_sig: cookies_signature(cookies.as_deref()),
        proxy: ndm::global_proxy(),
    };
    if resolution_cache_get(&key, Instant::now()).is_some() {
        return;
    }
    let Some(ytdlp) = find_tool("yt-dlp", components_dir) else {
        return;
    };
    let Some(probe) = resolve_streams_core(
        url,
        sel,
        audio,
        cookies.as_deref(),
        login,
        password,
        &ytdlp,
        components_dir,
    ) else {
        return;
    };
    if let Some((title, streams)) = probe.resolved {
        resolution_cache_store(
            key,
            Resolution {
                title,
                streams,
                expect_audio: probe.expect_audio,
                expected_media: probe.expected_media,
                pair_audio_url: probe.pair_audio_url,
            },
            Instant::now(),
        );
    }
}

impl VideoJob {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        url: &str,
        dir: &Path,
        selector: &str,
        audio: bool,
        title: &str,
        cookies: Option<PathBuf>,
        login: &str,
        password: &str,
        components_dir: &Path,
        expect_size: u64,
        expect_duration: f64,
    ) -> Result<Arc<VideoJob>, String> {
        let initial_stem = {
            let s = safe_filename_stem(title);
            if s.is_empty() {
                None
            } else {
                let final_exists = own_media_exts(audio)
                    .iter()
                    .any(|ext| dir.join(format!("{s}.{ext}")).exists());
                if final_exists {
                    Some(unique_stem_on_disk(dir, &s, audio))
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
            expected_media: Mutex::new(None),
            pair_audio_url: Mutex::new(None),
            limit_bps: std::sync::atomic::AtomicU64::new(0),
            login: login.to_string(),
            password: password.to_string(),
            merged_final: Mutex::new(None),
            expect_size,
            expect_duration,
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
        let unique = unique_stem_on_disk(&self.dir, raw, self.audio);
        if !unique.is_empty() {
            *guard = Some(unique.clone());
        }
        unique
    }

    /// The cache key for this job's resolution under `sel`: every
    /// input that can change which streams yt-dlp would pick.
    fn resolution_key(&self, sel: &str) -> ResolutionKey {
        ResolutionKey {
            url: self.url.clone(),
            sel: sel.to_string(),
            audio: self.audio,
            login: self.login.clone(),
            password_sig: password_signature(&self.password),
            cookies_sig: cookies_signature(self.cookies.as_deref()),
            proxy: ndm::global_proxy(),
        }
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
        let probe = resolve_streams_core(
            &self.url,
            sel,
            self.audio,
            self.cookies.as_deref(),
            &self.login,
            &self.password,
            ytdlp,
            components_dir,
        )?;
        // The learned fields are applied even when the selection
        // turns out ineligible for the engine: the yt-dlp fallback
        // downloads this same selection, so its guards still need
        // them. They are applied UNCONDITIONALLY (m-7): this probe
        // describes THIS selection, so a field it does not report
        // must CLEAR the value a previous selection left behind —
        // the old conditional writes let a fresh probe inherit the
        // earlier selector's pair_audio_url / expected_media, and
        // the cache store in run_parallel_engine then persisted
        // those stale fields under the new selection's key.
        *self.expect_audio.lock().unwrap() = probe.expect_audio;
        *self.pair_audio_url.lock().unwrap() = probe.pair_audio_url.clone();
        *self.expected_media.lock().unwrap() = probe.expected_media;
        probe.resolved
    }

    fn run_parallel_engine(
        self: &Arc<Self>,
        ytdlp: &Path,
        ffmpeg: &Path,
        components_dir: &Path,
        sel: &str,
        my_gen: u64,
    ) -> bool {
        // A fresh cached resolution for this exact URL + selector +
        // cookie set (left by an earlier start of the same selection
        // or by the flyout's warm resolve) skips the yt-dlp run
        // entirely — that run is the whole click-to-first-byte wait.
        // A miss resolves live and leaves its result behind.
        let res_key = self.resolution_key(sel);
        let (resolved_title, streams) = match resolution_cache_get(&res_key, Instant::now()) {
            Some(res) => {
                // a cache hit defines this selection's learned
                // state completely (m-7): assign, never merge with
                // whatever a previous selection left on the job
                *self.expect_audio.lock().unwrap() = res.expect_audio;
                *self.expected_media.lock().unwrap() = res.expected_media;
                *self.pair_audio_url.lock().unwrap() = res.pair_audio_url.clone();
                (res.title, res.streams)
            }
            None => {
                let Some(fresh) = self.resolve_direct_streams(ytdlp, components_dir, sel)
                else {
                    return false;
                };
                resolution_cache_store(
                    res_key,
                    Resolution {
                        title: fresh.0.clone(),
                        streams: fresh.1.clone(),
                        expect_audio: *self.expect_audio.lock().unwrap(),
                        expected_media: *self.expected_media.lock().unwrap(),
                        pair_audio_url: self.pair_audio_url.lock().unwrap().clone(),
                    },
                    Instant::now(),
                );
                fresh
            }
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
            stem = unique_stem_on_disk(&self.dir, "video", self.audio);
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
                    // the part files and their resume state are
                    // KEPT: plain list removal preserves partials
                    // on BOTH engines (parity with the yt-dlp path),
                    // and deleting them is exclusive to the discard
                    // flow in main.rs, which sweeps .ndm_stream*
                    // when the user explicitly discards
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
                if done_sum != p.done {
                    p.last_progress_at = Instant::now();
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

        // The finish-line guard's baseline for this merge: the
        // media payload of the parts about to be merged, estimated
        // from the parts themselves (duration × their streams' own
        // bitrates). The parts' byte counts are NOT the baseline —
        // a part's container can carry arbitrary non-media payload
        // that the remux legitimately drops (R3-EXT-01). Unknown
        // when any part reports no bitrate/duration; the guard then
        // skips this job rather than guessing.
        let expected_media = if self.audio {
            None
        } else {
            let mut sum = 0u64;
            let mut known = !part_paths.is_empty();
            for part in &part_paths {
                match estimate_media_payload(part, components_dir) {
                    Some(n) => sum = sum.saturating_add(n),
                    None => {
                        known = false;
                        break;
                    }
                }
            }
            if known && sum > 0 {
                Some(sum)
            } else {
                None
            }
        };

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

        let merge_ok = tool_cmd(ffmpeg)
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
            if flag == "paused" {
                // the user paused while ffmpeg was merging: the
                // entry must end PAUSED — publishing "finished"
                // would fire the completion actions (sound, power)
                // over a download the user deliberately stopped.
                // A good merge is KEPT and recorded, so the resume
                // finalizes it (see the spawn thread) instead of
                // re-downloading; a failed merge leaves nothing
                // usable, but the pause still wins over a fallback.
                if merge_ok {
                    *self.merged_final.lock().unwrap() = Some(final_path.clone());
                    let mut p = self.progress.lock().unwrap();
                    if let Ok(m) = final_path.metadata() {
                        p.total = m.len();
                        p.done = m.len();
                    }
                    p.status = "paused".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    p.final_path = Some(final_path);
                } else {
                    let mut p = self.progress.lock().unwrap();
                    p.status = "paused".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                }
                return true;
            }
        }

        if merge_ok {
            // the bytes this job actually downloaded, captured BEFORE
            // the finalize below can overwrite the total with the
            // merged file's size (F-EXT-04)
            let downloaded = self.progress.lock().unwrap().total;
            let audio_result = self.ensure_audio(&final_path, components_dir);
            // A stop that landed DURING the audio check (which can
            // take seconds when it fetches a separate rendition)
            // wins over the merge result — the stop check above ran
            // BEFORE the check and cannot see it. A cancelled job
            // must never publish "finished" (R2-EXT-05).
            if let Some(flag) = self.stop_flag.lock().unwrap().clone() {
                if flag == "cancelled" {
                    if let Ok(verified) = &audio_result {
                        let _ = std::fs::remove_file(verified);
                    }
                    let _ = std::fs::remove_file(&final_path);
                    let mut p = self.progress.lock().unwrap();
                    p.status = "cancelled".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    return true;
                }
                if flag == "paused" {
                    // same contract as a pause during the merge
                    // itself: the merge is KEPT and recorded, so the
                    // resume finalizes it instead of re-downloading
                    let kept = match &audio_result {
                        Ok(verified) => verified.clone(),
                        Err(_) => final_path.clone(),
                    };
                    *self.merged_final.lock().unwrap() = Some(kept.clone());
                    let mut p = self.progress.lock().unwrap();
                    if let Ok(m) = kept.metadata() {
                        p.total = m.len();
                        p.done = m.len();
                    }
                    p.status = "paused".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    p.final_path = Some(kept);
                    return true;
                }
            }
            match audio_result {
                Ok(verified) => {
                    let flen = verified.metadata().map(|m| m.len()).unwrap_or(0);
                    // judged against the selected streams' media
                    // payload (expected_media), never the downloaded
                    // total (R3-EXT-01); audio jobs are exempt: an
                    // extracted audio track is legitimately a small
                    // fraction of the downloaded container (R2-EXT-02)
                    let shrink = if flen > 0 && !self.audio {
                        drastic_shrink(expected_media, downloaded, flen)
                    } else {
                        None
                    };
                    // W-2: a merge that passes the media-payload
                    // guard can still contradict what the user was
                    // shown at add time (a different stream than
                    // the one probed) — that is an error too, never
                    // a clean success
                    let verdict = match shrink {
                        Some(msg) => Some(msg),
                        None => self.finish_contradiction(&verified, flen, components_dir),
                    };
                    let finished_now = verdict.is_none();
                    let mut p = self.progress.lock().unwrap();
                    if let Some(msg) = verdict {
                        // keep the true downloaded total; a
                        // drastically smaller deliverable is a
                        // visible error, not a silent success
                        p.status = "error".to_string();
                        p.error = Some(msg);
                        p.speed_bps = 0.0;
                        p.conns = 0;
                        p.final_path = Some(verified);
                    } else {
                        if flen > 0 {
                            p.total = flen;
                            p.done = flen;
                        }
                        p.status = "finished".to_string();
                        p.speed_bps = 0.0;
                        p.conns = 0;
                        p.final_path = Some(verified);
                    }
                    drop(p);
                    if finished_now {
                        // a finished job leaves no working files
                        // behind — including a previous attempt's
                        // yt-dlp partials (R4 side observation)
                        sweep_job_temps(&self.dir, &stem);
                    }
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

    /// The W-2 contradiction check for one produced file: compares
    /// it against this job's add-time expectations (see
    /// `contradiction_verdict`). Skips itself entirely when nothing
    /// was probed at add time.
    fn finish_contradiction(
        &self,
        produced: &Path,
        produced_len: u64,
        components_dir: &Path,
    ) -> Option<String> {
        if self.expect_size == 0 && self.expect_duration <= 0.0 {
            return None;
        }
        let produced_duration = if self.expect_duration > 0.0 {
            probe_duration_secs(produced, components_dir)
        } else {
            None
        };
        contradiction_verdict(
            self.expect_size,
            self.expect_duration,
            produced_len,
            produced_duration,
            self.audio,
        )
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
        if !self.login.is_empty() {
            args.push("--username".into());
            args.push(self.login.clone());
            args.push("--password".into());
            args.push(self.password.clone());
        }
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
        let fetched = tool_cmd(&ytdlp)
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
        let mux_ok = tool_cmd(&ffmpeg)
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
            // a (re)start supersedes the previous attempt's
            // failure: the stored error clears NOW, so progress
            // payloads never carry a stale error alongside a live
            // "downloading" state (the browser panel showed the old
            // error text through whole retry cycles)
            p.error = None;
            p.conns = ndm::global_connections().clamp(1, 32);
            p.last_progress_at = Instant::now();
        }

        let job = self.clone();
        let comp_dir = components_dir.to_path_buf();
        std::thread::spawn(move || {
            // 0. A previous run merged the file but was paused before
            // it could be finalized: verify and publish that merge
            // instead of downloading everything again.
            let kept_merge = job.merged_final.lock().unwrap().take();
            if let Some(merged) = kept_merge {
                if merged.is_file() {
                    // the recorded total here IS the kept merge's
                    // own size (set when the pause landed), so it is
                    // no baseline at all; the guard judges the file
                    // against the selected streams' media payload
                    // learned when the producing run resolved them —
                    // unknown (e.g. the merge came from a run whose
                    // resolution reported no bitrates) means the
                    // guard skips this finalize (R3-EXT-01)
                    let downloaded = job.progress.lock().unwrap().total;
                    let expected_media = *job.expected_media.lock().unwrap();
                    let audio_result = job.ensure_audio(&merged, &comp_dir);
                    // a stop that landed DURING the audio check wins
                    // over the kept merge's verdict, exactly as in
                    // the fresh-merge finalize (R2-EXT-05)
                    if let Some(flag) = job.stop_flag.lock().unwrap().clone() {
                        if flag == "cancelled" {
                            // the kept file stays on disk (it
                            // predates this run); only the status is
                            // the job's to publish
                            let mut p = job.progress.lock().unwrap();
                            p.status = "cancelled".to_string();
                            p.speed_bps = 0.0;
                            p.conns = 0;
                            return;
                        }
                        if flag == "paused" {
                            *job.merged_final.lock().unwrap() = Some(merged.clone());
                            let mut p = job.progress.lock().unwrap();
                            p.status = "paused".to_string();
                            p.speed_bps = 0.0;
                            p.conns = 0;
                            p.final_path = Some(merged);
                            return;
                        }
                    }
                    let mut published_finish = false;
                    match audio_result {
                        Ok(verified) => {
                            let flen = verified.metadata().map(|m| m.len()).unwrap_or(0);
                            // audio jobs exempt (R2-EXT-02), as in
                            // the fresh-merge finalize
                            let shrink = if flen > 0 && !job.audio {
                                drastic_shrink(expected_media, downloaded, flen)
                            } else {
                                None
                            };
                            // W-2: same add-time contradiction
                            // check as the fresh-merge finalize
                            let verdict = match shrink {
                                Some(msg) => Some(msg),
                                None => job.finish_contradiction(&verified, flen, &comp_dir),
                            };
                            let mut p = job.progress.lock().unwrap();
                            if let Some(msg) = verdict {
                                p.status = "error".to_string();
                                p.error = Some(msg);
                                p.speed_bps = 0.0;
                                p.conns = 0;
                                p.final_path = Some(verified);
                            } else {
                                if flen > 0 {
                                    p.total = flen;
                                    p.done = flen;
                                }
                                p.status = "finished".to_string();
                                p.speed_bps = 0.0;
                                p.conns = 0;
                                p.final_path = Some(verified);
                                published_finish = true;
                            }
                        }
                        Err(msg) => {
                            let mut p = job.progress.lock().unwrap();
                            p.status = "error".to_string();
                            p.error = Some(msg);
                            p.speed_bps = 0.0;
                            p.conns = 0;
                            p.final_path = Some(merged);
                        }
                    }
                    if published_finish {
                        // a finished job leaves no working files
                        // behind, whichever engine produced them
                        // (R4 side observation)
                        if let Some(stem) = job.chosen_stem.lock().unwrap().clone() {
                            sweep_job_temps(&job.dir, &stem);
                        }
                    }
                    return;
                }
                // the kept file vanished meanwhile: fall through and
                // download normally
            }
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
            if !job.login.is_empty() {
                args.push("--username".into());
                args.push(job.login.clone());
                args.push("--password".into());
                args.push(job.password.clone());
            }
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
            if lim == 0 && limit > 0 {
                // the per-download limit above takes precedence:
                // yt-dlp honours the LAST --limit-rate it is given,
                // so pushing the global one too would silently
                // override this download's own limiter
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

            // snapshot BEFORE the child starts: taken after the
            // spawn, a fast site could finish and write its final
            // file inside the snapshot window, and find_final_file
            // would then skip that file as "pre-existing" and report
            // a finished download as an error
            let before = dir_snapshot(&job.dir);
            let Ok(mut child) = tool_cmd(&ytdlp)
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
                        let new_done = base + cur_done;
                        if new_done != p.done {
                            p.last_progress_at = Instant::now();
                        }
                        p.done = new_done;
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
                        Ok(verified) => {
                            // W-2: the produced file must also
                            // match what was probed at add time —
                            // a plain contradiction is reported
                            // through the guard-error outcome, not
                            // presented as a clean finish
                            let flen = verified.metadata().map(|m| m.len()).unwrap_or(0);
                            match job.finish_contradiction(&verified, flen, &comp_dir) {
                                Some(msg) => Outcome::GuardError(verified, msg),
                                None => Outcome::Finished(verified),
                            }
                        }
                        Err(msg) => Outcome::GuardError(fp, msg),
                    },
                    None => Outcome::MergeMissing,
                }
            } else {
                Outcome::Failed
            };
            // A stop that landed while the outcome was being
            // resolved (the audio check above can take seconds)
            // wins over a "finished" verdict: the stop flag sampled
            // before the resolution is stale by now, and a
            // cancelled job must never publish "finished"
            // (R2-EXT-05). A pause keeps the produced file as a
            // kept merge, so the resume finalizes it instead of
            // re-downloading.
            let outcome = match (job.stop_flag.lock().unwrap().clone(), outcome) {
                (Some(flag), Outcome::Finished(fp)) if flag == "cancelled" => {
                    let _ = std::fs::remove_file(&fp);
                    Outcome::Stopped(flag)
                }
                (Some(flag), Outcome::Finished(fp)) if flag == "paused" => {
                    *job.merged_final.lock().unwrap() = Some(fp);
                    Outcome::Stopped(flag)
                }
                (Some(flag), Outcome::GuardError(_, _)) if flag == "cancelled" => {
                    Outcome::Stopped(flag)
                }
                (_, o) => o,
            };
            let expected_media = *job.expected_media.lock().unwrap();
            let mut p = job.progress.lock().unwrap();
            p.conns = 0;
            let mut published_finish = false;
            match outcome {
                Outcome::Finished(fp) => {
                    let flen = fp.metadata().map(|m| m.len()).unwrap_or(0);
                    // F-EXT-04 / R3-EXT-01: the merged file is
                    // judged against the selected streams' media
                    // payload (learned at resolution time), NOT
                    // against p.total — what yt-dlp downloaded can
                    // far exceed the media inside a padded
                    // container, and a good merge must not be
                    // declared broken. Unknown payload → the guard
                    // skips. On a verdict the true total stays; it
                    // is not rewritten to the output size. Audio
                    // jobs are exempt: an extracted audio track is
                    // legitimately a small fraction of the
                    // downloaded source (R2-EXT-02).
                    let shrink = if flen > 0 && !job.audio {
                        drastic_shrink(expected_media, p.total, flen)
                    } else {
                        None
                    };
                    if let Some(msg) = shrink {
                        p.status = "error".to_string();
                        p.error = Some(msg);
                    } else {
                        p.status = "finished".to_string();
                        if flen > 0 {
                            p.total = flen;
                            p.done = flen;
                        }
                        published_finish = true;
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
            drop(p);
            if published_finish {
                // a finished job leaves no working files behind —
                // including a previous attempt's parallel-engine
                // parts (R4 side observation)
                if let Some(stem) = job.chosen_stem.lock().unwrap().clone() {
                    sweep_job_temps(&job.dir, &stem);
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
        // A finished job has nothing to cancel — and overwriting
        // its status here would let a stop that lost the finish
        // race flip a completed download to "cancelled", after
        // which a discard could sweep it as if it had never
        // finished (R2-EXT-05, the F-EXT-12 rule).
        if self.progress.lock().unwrap().status == "finished" {
            return;
        }
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

    /// True once nothing of this job is still running: the yt-dlp
    /// child has exited and every engine stream is SETTLED —
    /// terminal with its final resume-state save on disk (a merely
    /// terminal stream can still be between the supervisor's
    /// status settle and its last save, and sweeping in that
    /// window is how the stream journal survived deletes, R4
    /// item 2). The discard flow waits for this before sweeping
    /// the job's files, the way the HTTP path waits for its
    /// engine (F-EXT-08).
    pub fn is_settled(&self) -> bool {
        let child_done = self
            .child
            .lock()
            .unwrap()
            .as_mut()
            .map(|c| matches!(c.try_wait(), Ok(Some(_)) | Err(_)))
            .unwrap_or(true);
        if !child_done {
            return false;
        }
        self.active_downloads
            .lock()
            .unwrap()
            .iter()
            .all(|d| d.is_settled())
    }

    pub fn resume(self: &Arc<Self>, components_dir: &Path) -> Result<(), String> {
        {
            let active = self.active_downloads.lock().unwrap().clone();
            if !active.is_empty() {
                *self.stop_flag.lock().unwrap() = None;
                for d in &active {
                    d.control.resume();
                }
                // same stale-error rule as spawn(): a resumed job
                // is a new attempt, its old error does not ride
                // along in the progress payloads
                let mut p = self.progress.lock().unwrap();
                p.status = "downloading".to_string();
                p.error = None;
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

    fn test_key(url: &str, sel: &str) -> ResolutionKey {
        ResolutionKey {
            url: url.to_string(),
            sel: sel.to_string(),
            audio: false,
            login: String::new(),
            password_sig: 0,
            cookies_sig: 0,
            proxy: String::new(),
        }
    }

    fn test_resolution() -> Resolution {
        Resolution {
            title: "t".to_string(),
            streams: vec![DirectStream {
                url: "https://cdn.example/v.mp4".to_string(),
                ext: "mp4".to_string(),
                has_audio: true,
                cookie: None,
                referer: None,
            }],
            expect_audio: true,
            expected_media: Some(1234),
            pair_audio_url: None,
        }
    }

    #[test]
    fn resolution_cache_hit_miss_and_key_strictness() {
        // unique URLs per test: the cache is process-global and
        // cargo runs tests on parallel threads
        let now = Instant::now();
        let key = test_key("https://example.com/watch?v=cache1", "bv*+ba");
        assert!(resolution_cache_get(&key, now).is_none());
        resolution_cache_store(key, test_resolution(), now);
        let hit = resolution_cache_get(
            &test_key("https://example.com/watch?v=cache1", "bv*+ba"),
            now,
        )
        .expect("same key must hit");
        assert_eq!(hit.streams.len(), 1);
        assert_eq!(hit.expected_media, Some(1234));
        // any key component differing must miss: another video, another
        // selector, the audio variant, another login, other cookies
        assert!(
            resolution_cache_get(&test_key("https://example.com/watch?v=other", "bv*+ba"), now)
                .is_none()
        );
        assert!(
            resolution_cache_get(&test_key("https://example.com/watch?v=cache1", "bv*[height<=720]+ba"), now)
                .is_none()
        );
        let mut audio_key = test_key("https://example.com/watch?v=cache1", "bv*+ba");
        audio_key.audio = true;
        assert!(resolution_cache_get(&audio_key, now).is_none());
        let mut login_key = test_key("https://example.com/watch?v=cache1", "bv*+ba");
        login_key.login = "someone".to_string();
        assert!(resolution_cache_get(&login_key, now).is_none());
        let mut cookie_key = test_key("https://example.com/watch?v=cache1", "bv*+ba");
        cookie_key.cookies_sig = 42;
        assert!(resolution_cache_get(&cookie_key, now).is_none());
    }

    #[test]
    fn resolution_cache_expires_after_ttl() {
        let key = test_key("https://example.com/watch?v=cache2", "bv*+ba");
        let stored_at = Instant::now();
        resolution_cache_store(key, test_resolution(), stored_at);
        assert!(
            resolution_cache_get(
                &test_key("https://example.com/watch?v=cache2", "bv*+ba"),
                stored_at
            )
            .is_some(),
            "a fresh entry must be served"
        );
        assert!(
            resolution_cache_get(
                &test_key("https://example.com/watch?v=cache2", "bv*+ba"),
                stored_at + RESOLUTION_TTL + std::time::Duration::from_secs(1)
            )
            .is_none(),
            "an entry older than the TTL must never be served"
        );
    }

    #[test]
    fn cookies_signature_distinguishes_content() {
        let dir = std::env::temp_dir().join(format!("ndm-cksig-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"session=one").unwrap();
        std::fs::write(&b, b"session=two").unwrap();
        assert_eq!(cookies_signature(Some(&a)), cookies_signature(Some(&a)));
        assert_ne!(cookies_signature(Some(&a)), cookies_signature(Some(&b)));
        assert_ne!(cookies_signature(None), cookies_signature(Some(&a)));
        assert_ne!(
            cookies_signature(Some(&dir.join("missing.txt"))),
            cookies_signature(Some(&a))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_sweep_removes_both_engines_temps_only() {
        // R4 side observation: a job paused on yt-dlp and finished
        // on the parallel engine left its yt-dlp partial behind —
        // the finish sweep must take BOTH engines' temps for the
        // job's stem, plus the stream journals, and nothing else.
        let dir =
            std::env::temp_dir().join(format!("ndm-sweep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gone = [
            ".big2.ndm_stream0.mp4.part",
            ".big2.ndm_stream0.mp4.part.ndm.json",
            ".big2.ndm_mux.mp4",
            ".big2.ndm_audio.m4a",
            "big2.mp4.part",
            "big2.mp4.ytdl",
            "big2.f140.m4a.part",
        ];
        let kept = [
            "big2.mp4",
            "big2.txt.part",
            "big22.mp4.part",
            "other.mp4.part",
            ".big2.ndm_backup",
        ];
        for n in gone.iter().chain(kept.iter()) {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        sweep_job_temps(&dir, "big2");
        for n in gone {
            assert!(!dir.join(n).exists(), "{n} must be swept");
        }
        for n in kept {
            assert!(dir.join(n).exists(), "{n} must survive");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn merge_guard_judges_output_against_selected_media_not_download_total() {
        // (a) R3-EXT-01, the padded container: 100,663,296 bytes
        // were downloaded, but the selected streams hold only
        // ≈2.31 MB of media and the merge produced the byte-perfect
        // 2,342,828-byte movie — PASS. (The old baseline, the
        // download total, errored exactly this case three times.)
        assert!(drastic_shrink(Some(2_309_250), 100_663_296, 2_342_828).is_none());
        // (b) the F-EXT-04 class the guard exists for: the selected
        // media really is ~96 MB and the merge produced a 2.34 MB
        // stub instead of the movie — ERROR, honestly reported
        let err = drastic_shrink(Some(96_000_000), 100_663_296, 2_342_828)
            .expect("a stub merge must still error");
        assert!(err.contains("much smaller than what was downloaded"), "{err}");
        // (c) the expected media size is genuinely unknown — the
        // guard skips the job rather than fabricating a verdict,
        // however lopsided the download total looks
        assert!(drastic_shrink(None, 100_663_296, 2_342_828).is_none());
        assert!(drastic_shrink(None, 0, 0).is_none());
        // a zero estimate is no estimate
        assert!(drastic_shrink(Some(0), 100_663_296, 10).is_none());
        // exactly half of the expected media is within tolerance
        assert!(drastic_shrink(Some(1000), 5000, 500).is_none());
        // a normal remux (audio re-encode) shrinks a few percent
        assert!(drastic_shrink(Some(1000), 1000, 970).is_none());
    }

    #[test]
    fn media_payload_estimate_uses_stream_bitrates_not_file_size() {
        // the R3 fixture's real `ffmpeg -i` header: the Duration
        // line's 134217 kb/s is derived from the padded file size,
        // while the streams report their true bitrates — 3010 + 69
        // kb/s over 6.00 s is the ≈2.31 MB payload of the
        // 2,342,828-byte movie inside the 100 MB container
        let info = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'big.mp4':
  Duration: 00:00:06.00, start: 0.000000, bitrate: 134217 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 1280x720 [SAR 1:1 DAR 16:9], 3010 kb/s, 30 fps, 30 tbr, 15360 tbn (default)
  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 44100 Hz, mono, fltp, 69 kb/s (default)
At least one output file must be specified";
        assert_eq!(media_payload_estimate(info), Some(2_309_250));
    }

    #[test]
    fn media_payload_estimate_refuses_partial_data() {
        // a stream with no reported bitrate → unknown, not a guess
        let no_bitrate = "\
  Duration: 00:00:06.00, start: 0.000000, bitrate: 134217 kb/s
  Stream #0:0: Video: h264, yuv420p, 1280x720, 30 fps";
        assert_eq!(media_payload_estimate(no_bitrate), None);
        // unknown duration → unknown
        let no_duration = "\
  Duration: N/A, bitrate: N/A
  Stream #0:0: Audio: aac, 44100 Hz, mono, 69 kb/s";
        assert_eq!(media_payload_estimate(no_duration), None);
        // no streams at all → unknown
        assert_eq!(
            media_payload_estimate("  Duration: 00:00:06.00, bitrate: 100 kb/s"),
            None
        );
    }

    #[test]
    fn formats_media_bytes_estimates_from_bitrates_only() {
        let v = serde_json::json!({"formats": [
            {"tbr": 800.0},
            {"vbr": null, "abr": 128.0}
        ]});
        let formats: Vec<&serde_json::Value> =
            v["formats"].as_array().unwrap().iter().collect();
        // (800 + 128) kb/s × 100 s = 11,600,000 bytes of media
        assert_eq!(formats_media_bytes(100.0, &formats), Some(11_600_000));
        // a selected format with no bitrate at all → unknown
        let v2 = serde_json::json!({"formats": [{"tbr": 800.0}, {"ext": "mp4"}]});
        let formats2: Vec<&serde_json::Value> =
            v2["formats"].as_array().unwrap().iter().collect();
        assert_eq!(formats_media_bytes(100.0, &formats2), None);
        // unknown duration → unknown
        assert_eq!(formats_media_bytes(0.0, &formats), None);
    }

    #[test]
    fn unique_stem_on_disk_disambiguates_duplicate_titles() {
        let dir = std::env::temp_dir().join(format!("ndm-uniq-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s0 = unique_stem_on_disk(&dir, "Fixture Player — Test Video", false);
        assert_eq!(s0, "Fixture Player — Test Video");
        std::fs::write(dir.join(format!("{s0}.mp4")), b"1").unwrap();
        let s1 = unique_stem_on_disk(&dir, "Fixture Player — Test Video", false);
        assert_eq!(s1, "Fixture Player — Test Video (1)");
        std::fs::write(dir.join(format!("{s1}.mp4")), b"2").unwrap();
        let s2 = unique_stem_on_disk(&dir, "Fixture Player — Test Video", false);
        assert_eq!(s2, "Fixture Player — Test Video (2)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_stem_on_disk_ignores_the_other_kinds_files() {
        // R3-EXT-04: the video half of a download-all pair landing
        // first (big2.mp4 — and likewise any other video container)
        // must not rename the audio half: no big2.m4a / big2.mp3
        // exists, so the audio stem stays "big2".
        let dir = std::env::temp_dir().join(format!("ndm-uniq-cross-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big2.mp4"), b"v").unwrap();
        std::fs::write(dir.join("big2.mkv"), b"v").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "big2", true), "big2");
        // Symmetrically, audio files never rename a video stem.
        std::fs::write(dir.join("song.m4a"), b"a").unwrap();
        std::fs::write(dir.join("song.mp3"), b"a").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "song", false), "song");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_stem_on_disk_renames_on_same_kind_collision() {
        // Genuine collision: the entry's OWN final filename already
        // exists, so "(1)" is correct — for either audio container.
        let dir = std::env::temp_dir().join(format!("ndm-uniq-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big2.m4a"), b"a").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "big2", true), "big2 (1)");
        std::fs::write(dir.join("song.mp3"), b"a").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "song", true), "song (1)");
        // …and the video kind against its own container.
        std::fs::write(dir.join("clip.mkv"), b"v").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "clip", false), "clip (1)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn contradiction_verdict_flags_plain_mismatches_only() {
        // the walkthrough's shape: the page's video runs ~40 min,
        // the delivered file runs 6 s — a plain contradiction
        assert!(contradiction_verdict(0, 2400.0, 2_342_828, Some(6.0), false).is_some());
        // same story told by size alone: picked ~100 MB, got 2.3 MB
        assert!(contradiction_verdict(100_663_296, 0.0, 2_342_828, None, false).is_some());
        // a delivery that matches the probe passes clean
        assert!(contradiction_verdict(100_000_000, 2400.0, 98_000_000, Some(2395.0), false).is_none());
        // nothing probed → nothing to contradict (the padded-
        // container fixture: option size unknown, duration unknown)
        assert!(contradiction_verdict(0, 0.0, 2_342_828, Some(6.0), false).is_none());
        // an unprobeable produced duration never fabricates a verdict
        assert!(contradiction_verdict(0, 2400.0, 2_342_828, None, false).is_none());
        // remux/transcode slack is not a contradiction: 60% of the
        // expected size and 95% of the duration are both fine
        assert!(contradiction_verdict(100_000_000, 2400.0, 60_000_000, Some(2280.0), false).is_none());
        // audio jobs: size is never judged (transcodes shrink
        // legitimately), duration still is
        assert!(contradiction_verdict(100_000_000, 2400.0, 3_000_000, Some(2390.0), true).is_none());
        assert!(contradiction_verdict(0, 2400.0, 3_000_000, Some(6.0), true).is_some());
    }

    #[test]
    fn direct_format_wins_over_playlist_variants() {
        // the W-2 fixture's shape: a direct <video> file plus a
        // script-planted HLS ladder in one probe
        let v = serde_json::json!({
            "formats": [
                {"format_id": "hls-720", "ext": "mp4", "protocol": "m3u8_native",
                 "url": "http://cdn.example/hls/v720/pl.m3u8", "height": 720, "vcodec": "avc1"},
                {"format_id": "0", "ext": "mp4", "protocol": "http",
                 "url": "http://cdn.example/media/big.mp4", "vcodec": null, "acodec": null},
                {"format_id": "hls-360", "ext": "mp4", "protocol": "m3u8_native",
                 "url": "http://cdn.example/hls/v360/pl.m3u8", "height": 360, "vcodec": "avc1"}
            ]
        });
        let direct = best_direct_format(&v).expect("a direct format exists");
        assert_eq!(direct["url"].as_str(), Some("http://cdn.example/media/big.mp4"));
        assert!(format_is_playlist(&v["formats"][0]));
        assert!(!format_is_playlist(&v["formats"][1]));
        // a list with only playlists yields no direct candidate
        let v2 = serde_json::json!({
            "formats": [
                {"format_id": "hls-720", "ext": "mp4", "protocol": "m3u8_native",
                 "url": "http://cdn.example/hls/v720/pl.m3u8", "height": 720, "vcodec": "avc1"}
            ]
        });
        assert!(best_direct_format(&v2).is_none());
        // an audio-only direct entry is not the page's video
        let v3 = serde_json::json!({
            "formats": [
                {"format_id": "a1", "ext": "mp4", "protocol": "https",
                 "url": "https://cdn.example/audio.m4a", "vcodec": "none", "acodec": "aac"}
            ]
        });
        assert!(best_direct_format(&v3).is_none());
    }

    #[test]
    fn password_signature_separates_passwords() {
        // m-6: the resolution cache key must change when the
        // password does, and stay stable when it doesn't
        assert_eq!(password_signature(""), 0);
        assert_eq!(password_signature("hunter2"), password_signature("hunter2"));
        assert_ne!(password_signature("hunter2"), password_signature("hunter3"));
        assert_ne!(password_signature("hunter2"), password_signature(""));
    }

    #[test]
    fn duration_parses_from_ffmpeg_info() {
        let text = "  Duration: 00:00:06.01, start: 0.000000, bitrate: 134217 kb/s\n";
        assert_eq!(duration_in_ffmpeg_info(text), Some(6.01));
        let text2 = "Duration: 01:02:03.50, start: 0.000000";
        assert_eq!(duration_in_ffmpeg_info(text2), Some(3723.5));
        assert_eq!(duration_in_ffmpeg_info("Duration: N/A"), None);
        assert_eq!(duration_in_ffmpeg_info("no duration here"), None);
    }
}

