// Video downloads via yt-dlp (sidecar process) — YouTube, Facebook and the
// 1,700+ other sites yt-dlp supports. NDM never parses those sites itself;
// yt-dlp + ffmpeg do the extraction and merging, NDM owns the list, the
// progress, pause/resume and persistence — exactly like the HTTP engine.
//
// Pause = stop the yt-dlp process (its .part files stay on disk); resume =
// start it again with the same arguments, yt-dlp continues the partial files.

pub use ndm::video_probe::{base_args, find_tool, probe, VideoInfo};

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

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
    cands
        .iter()
        .rev()
        .find(|(_, p)| {
            p.extension().map(|x| x.to_string_lossy().to_lowercase())
                == Some(want.to_string())
        })
        .or_else(|| cands.last())
        .map(|(_, p)| p.clone())
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

pub struct VideoJob {
    pub url: String,
    pub dir: PathBuf,
    pub selector: String,
    pub audio: bool,
    pub title: String,
    pub cookies: Option<PathBuf>,
    pub progress: Arc<Mutex<VideoProgress>>,
    child: Mutex<Option<Child>>,
    stop_flag: Mutex<Option<String>>, // "paused" | "cancelled" when WE stopped it
    spawn_lock: Mutex<()>,
    gen: std::sync::atomic::AtomicU64,
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
        let job = Arc::new(VideoJob {
            url: url.to_string(),
            dir: dir.to_path_buf(),
            selector: selector.to_string(),
            audio,
            title: title.to_string(),
            cookies,
            progress: Arc::new(Mutex::new(VideoProgress::default())),
            child: Mutex::new(None),
            stop_flag: Mutex::new(None),
            spawn_lock: Mutex::new(()),
            gen: std::sync::atomic::AtomicU64::new(0),
        });
        job.spawn(components_dir)?;
        Ok(job)
    }

    fn spawn(self: &Arc<Self>, components_dir: &Path) -> Result<(), String> {
        let ytdlp = find_tool("yt-dlp", components_dir)
            .ok_or("yt-dlp not found — the video component is not installed yet")?;
        if find_tool("ffmpeg", components_dir).is_none() {
            return Err("ffmpeg is not available on this system — it joins the video and audio into one playable file. Install ffmpeg with your system package manager (Debian/Ubuntu: sudo apt install ffmpeg), or use an NDM build that bundles it, then try again".to_string());
        }
        let mut args = base_args(components_dir, self.cookies.as_deref());
        let sel = if self.selector.trim().is_empty() {
            ndm::video_probe::best_selector()
        } else {
            self.selector.clone()
        };
        args.push("-f".into());
        args.push(sel);
        if self.audio {
            args.push("-x".into());
            args.push("--audio-format".into());
            args.push("mp3".into());
            args.push("--audio-quality".into());
            args.push("0".into());
        } else {
            args.push("--merge-output-format".into());
            args.push("mp4".into());
            // Guarantee universally playable audio inside MP4 containers:
            // stream-copy video (0 CPU cost, instant) and convert any
            // non-AAC audio (Opus/WebM, Vorbis, HE-AAC) to standard LC-AAC.
            args.push("--postprocessor-args".into());
            args.push("Merger+ffmpeg_o1:-c:v copy -c:a aac -b:a 192k".into());
        }
        // High-speed parallel fragment & ranged chunk downloading:
        // - 16 concurrent fragments for HLS/DASH streams
        // - 10M HTTP chunk size bypasses YouTube single-stream bandwidth throttling
        let frags = ndm::global_connections().clamp(1, 16);
        args.push("--concurrent-fragments".into());
        args.push(frags.to_string());
        args.push("--buffer-size".into());
        args.push("64K".into());
        args.push("--http-chunk-size".into());
        args.push("10M".into());
        args.push("--hls-prefer-native".into());
        args.push("--trim-filenames".into());
        args.push("60".into());

        let limit = ndm::global_limit();
        if limit > 0 {
            args.push("--limit-rate".into());
            args.push(format!("{}K", (limit / 1024).max(1)));
        }
        if let Some(ffmpeg) = find_tool("ffmpeg", components_dir) {
            if let Some(parent) = ffmpeg.parent() {
                args.push("--ffmpeg-location".into());
                args.push(parent.display().to_string());
            }
        }
        args.push("--progress-template".into());
        args.push(
            "NDMPROG:%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s"
                .into(),
        );
        // Honor the user's custom/edited title when valid, otherwise fall back
        // to yt-dlp's byte-sliced title (150 UTF-8 bytes max) so multi-byte
        // Bengali/Unicode titles plus `.f<format_id>.mp4.part` never exceed
        // the 255-byte Linux/Windows filename limit.
        let stem = safe_filename_stem(&self.title);
        let out_tmpl = if stem.is_empty() {
            self.dir.join("%(title).150B.%(ext)s")
        } else {
            self.dir.join(format!("{stem}.%(ext)s"))
        };
        args.push("-o".into());
        args.push(out_tmpl.display().to_string());
        args.push("--".into());
        args.push(self.url.clone());

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
        let mut child = Command::new(&ytdlp)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start yt-dlp: {e}"))?;
        let stdout = child.stdout.take().ok_or("no stdout from yt-dlp")?;
        let stderr = child.stderr.take();
        let my_pid = child.id();
        {
            let mut slot = self.child.lock().unwrap();
            if let Some(mut old) = slot.take() {
                let _ = old.wait();
            }
            *slot = Some(child);
        }
        self.progress.lock().unwrap().status = "downloading".to_string();

        let job = self.clone();
        std::thread::spawn(move || {
            let err_buf = Arc::new(Mutex::new(String::new()));
            if let Some(mut se) = stderr {
                let eb = err_buf.clone();
                std::thread::spawn(move || {
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
                });
            }
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
            let mut p = job.progress.lock().unwrap();
            if status.map(|s| s.success()).unwrap_or(false) {
                let found = detected_final
                    .filter(|fp| fp.is_file())
                    .or_else(|| p.final_path.clone().filter(|fp| fp.is_file()))
                    .or_else(|| find_final_file(&job.dir, &before, job.audio));
                match found {
                    Some(fp) => {
                        p.status = "finished".to_string();
                        if let Ok(m) = fp.metadata() {
                            p.total = m.len();
                            p.done = m.len();
                        }
                        p.final_path = Some(fp);
                    }
                    None => {
                        p.status = "error".to_string();
                        p.error = Some(
                            "the video and audio downloaded but could not be merged into one playable file — merging needs ffmpeg. Reinstall NDM with apt (ffmpeg is a package dependency), then try again"
                                .to_string(),
                        );
                    }
                }
                p.speed_bps = 0.0;
            } else if let Some(flag) = job.stop_flag.lock().unwrap().clone() {
                p.status = flag;
                p.speed_bps = 0.0;
            } else {
                p.status = "error".to_string();
                p.speed_bps = 0.0;
                let eb = err_buf.lock().unwrap();
                p.error = eb
                    .lines()
                    .rev()
                    .find(|l| l.contains("ERROR"))
                    .map(|l| l.trim().to_string())
                    .or_else(|| Some("yt-dlp failed".to_string()));
            }
        });
        Ok(())
    }

    pub fn pause(&self) {
        let _spawn_guard = self.spawn_lock.lock().unwrap();
        *self.stop_flag.lock().unwrap() = Some("paused".to_string());
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
        let mut p = self.progress.lock().unwrap();
        if p.status == "downloading" {
            p.status = "paused".to_string();
            p.speed_bps = 0.0;
        }
    }

    pub fn cancel(&self) {
        let _spawn_guard = self.spawn_lock.lock().unwrap();
        *self.stop_flag.lock().unwrap() = Some("cancelled".to_string());
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
        self.progress.lock().unwrap().status = "cancelled".to_string();
    }

    pub fn resume(self: &Arc<Self>, components_dir: &Path) -> Result<(), String> {
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
