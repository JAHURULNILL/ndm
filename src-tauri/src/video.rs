// Video downloads via yt-dlp (sidecar process) — YouTube, Facebook and the
// 1,700+ other sites yt-dlp supports. NDM never parses those sites itself;
// yt-dlp + ffmpeg do the extraction and merging, NDM owns the list, the
// progress, pause/resume and persistence — exactly like the HTTP engine.
//
// Pause = stop the yt-dlp process (its .part files stay on disk); resume =
// start it again with the same arguments, yt-dlp continues the partial files.

use serde::Serialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::SystemTime;
use std::sync::{Arc, Mutex};

#[derive(Clone, Serialize)]
pub struct QualityOption {
    pub selector: String,
    pub label: String,
    pub note: String,
    pub audio: bool,
}

#[derive(Clone, Serialize)]
pub struct VideoInfo {
    pub title: String,
    pub duration: u64,
    pub options: Vec<QualityOption>,
    /// the page's own format list carries sound at all — a finished
    /// video job whose source had audio must produce a file WITH an
    /// audio stream; the finish path enforces this (see spawn())
    pub has_audio: bool,
}

/// Locate a helper tool: app-data components dir → folder next to the NDM
/// executable → $NDM_TOOLS_DIR (dev) → PATH.
pub fn find_tool(name: &str, components_dir: &Path) -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut candidates: Vec<PathBuf> = vec![components_dir.join(&exe_name)];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(&exe_name));
            // portable layout: <folder>/components next to the app
            candidates.push(dir.join("components").join(&exe_name));
            // packaged layout: /usr/bin/<app> -> /usr/lib/ndm/components
            if let Some(prefix) = dir.parent() {
                candidates.push(prefix.join("lib/ndm/components").join(&exe_name));
            }
        }
    }
    // dev-only override: a release build resolves tools from the
    // fixed locations above (plus PATH), never from the environment
    if cfg!(debug_assertions) {
        if let Ok(dir) = std::env::var("NDM_TOOLS_DIR") {
            candidates.push(PathBuf::from(dir).join(&exe_name));
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        // split_paths handles ';' + drive-letter colons on Windows
        for dir in std::env::split_paths(&path_var) {
            candidates.push(dir.join(&exe_name));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn base_args(components_dir: &Path, cookies: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--newline".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--socket-timeout".into(),
        "15".into(),
    ];
    // dev-only escape hatch (this VM's egress proxy MITMs TLS): a
    // release build never weakens certificate checks
    if cfg!(debug_assertions) && std::env::var("NDM_INSECURE").is_ok() {
        args.push("--no-check-certificates".into());
    }
    let proxy = ndm::global_proxy();
    if !proxy.is_empty() {
        args.push("--proxy".into());
        args.push(proxy);
    }
    // login-gated videos (private Facebook posts, age-gated pages, …)
    // need the user's own browser session: the extension captured the
    // browser's cookies for this page at click time into a Netscape
    // file, and the app hands that file to yt-dlp for this download
    if let Some(file) = cookies {
        args.push("--cookies".into());
        args.push(file.to_string_lossy().into_owned());
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
    if name.ends_with(".part") || name.ends_with(".ytdl") {
        return true;
    }
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    if let Some((_, tail)) = stem.rsplit_once('.') {
        let t = tail.as_bytes();
        if tail.len() <= 12
            && t.first() == Some(&b'f')
            && tail[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
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

pub fn probe(url: &str, components_dir: &Path, cookies: Option<&Path>) -> Result<VideoInfo, String> {
    let ytdlp = find_tool("yt-dlp", components_dir)
        .ok_or("yt-dlp not found — the video component is not installed yet")?;
    let mut cmd = Command::new(&ytdlp);
    cmd.args(base_args(components_dir, cookies))
        .args(["-J", "--", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // a hung extraction must neither hang the probe nor outlive it:
    // spawn the child explicitly, drain its pipes on threads, poll for
    // exit against a hard deadline, and kill the process on timeout
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start yt-dlp: {e}"))?;
    let out_pipe = child.stdout.take().ok_or("no stdout from yt-dlp")?;
    let mut err_pipe = child.stderr.take().ok_or("no stderr from yt-dlp")?;
    let out_thread = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut v = Vec::new();
        // capped: a pathological page must not stream metadata forever
        let _ = out_pipe.take(64 * 1024 * 1024 + 1).read_to_end(&mut v);
        v
    });
    let err_thread = std::thread::spawn(move || {
        // rolling tail (same as spawn): errors live at the end, and a
        // warning-spewing extractor must not grow this without bound
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        "yt-dlp took too long to read this page (90s limit)".to_string()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                // do not abandon the child: a dropped Child keeps
                // running orphaned — kill and reap it first
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
    let duration = v["duration"].as_u64().unwrap_or(0);
    let has_audio = v["formats"]
        .as_array()
        .map(|fs| {
            fs.iter().any(|f| {
                f["acodec"].as_str().map(|c| c != "none").unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let mut heights: Vec<u64> = v["formats"]
        .as_array()
        .map(|fs| {
            fs.iter()
                // storyboard entries carry a height but no video codec —
                // they are preview thumbnails, not downloadable quality
                .filter(|f| {
                    f["vcodec"].as_str().map(|c| c != "none").unwrap_or(false)
                })
                .filter_map(|f| f["height"].as_u64())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    heights.sort_unstable();
    heights.dedup();

    // Selector discipline: a single-file fallback must itself carry
    // audio ([acodec!=none]) — an unrestricted /b fallback can land on
    // a video-only DASH file and "succeed" with a silent video. MP4/M4A
    // leads each chain so merged files play everywhere.
    let mut options = vec![QualityOption {
        selector: "bv*[vcodec^=avc][ext=mp4]+ba[ext=m4a]/bv*[ext=mp4]+ba[ext=m4a]/bv*+ba/b[acodec!=none]".into(),
        label: "Best quality".into(),
        note: "highest available, merged with ffmpeg".into(),
        audio: false,
    }];
    for h in heights.iter().rev() {
        options.push(QualityOption {
            selector: format!(
                "bv*[height<={h}][vcodec^=avc][ext=mp4]+ba[ext=m4a]/bv*[height<={h}][ext=mp4]+ba[ext=m4a]/bv*[height<={h}]+ba/b[height<={h}][acodec!=none]"
            ),
            label: format!("{h}p"),
            note: format!("up to {h}p + audio"),
            audio: false,
        });
    }
    options.push(QualityOption {
        selector: "ba".into(),
        label: "Audio only (MP3)".into(),
        note: "extracts audio as .mp3".into(),
        audio: true,
    });
    Ok(VideoInfo {
        title,
        duration,
        options,
        has_audio,
    })
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

/// (has_video, has_audio) from `ffmpeg -i` output: streams are listed
/// on stderr as "Stream #0:0: Video: h264 …" / "Stream #0:1: Audio:
/// aac …". Pure text check so it can be unit-tested on samples.
fn streams_in_ffmpeg_info(text: &str) -> (bool, bool) {
    (text.contains(" Video: "), text.contains(" Audio: "))
}

/// Stream presence in a finished file, probed with ffmpeg itself
/// (it exits non-zero on a bare -i; the stream listing is the answer,
/// not the exit code). None = the check could not run at all —
/// callers must read that as "unknown", never as a failed download.
fn probe_streams(path: &Path, components_dir: &Path) -> Option<(bool, bool)> {
    let ffmpeg = find_tool("ffmpeg", components_dir)?;
    let out = Command::new(ffmpeg).arg("-i").arg(path).output().ok()?;
    Some(streams_in_ffmpeg_info(&String::from_utf8_lossy(&out.stderr)))
}

pub struct VideoJob {
    pub url: String,
    pub dir: PathBuf,
    pub selector: String,
    pub audio: bool,
    /// the source page had sound and this is a video job: the
    /// finished file MUST carry an audio stream (checked at finish)
    pub expect_audio: bool,
    /// remembered so the finish path can find ffmpeg for the
    /// stream check without the caller threading it through
    pub components_dir: PathBuf,
    pub cookies: Option<PathBuf>,
    pub progress: Arc<Mutex<VideoProgress>>,
    child: Mutex<Option<Child>>,
    stop_flag: Mutex<Option<String>>, // "paused" | "cancelled" when WE stopped it
    /// held across the check + spawn + store sequence in spawn(), and by
    /// pause()/cancel(), so a job can never run two yt-dlp processes or
    /// lose one beyond their reach
    spawn_lock: Mutex<()>,
    /// bumped on every spawn: a progress-reader thread only ever
    /// collects and judges the child IT was spawned for — after a
    /// pause+quick-resume it must not steal or grade the new child
    gen: std::sync::atomic::AtomicU64,
}

impl VideoJob {
    pub fn start(
        url: &str,
        dir: &Path,
        selector: &str,
        audio: bool,
        expect_audio: bool,
        cookies: Option<PathBuf>,
        components_dir: &Path,
    ) -> Result<Arc<VideoJob>, String> {
        let job = Arc::new(VideoJob {
            url: url.to_string(),
            dir: dir.to_path_buf(),
            selector: selector.to_string(),
            audio,
            expect_audio,
            components_dir: components_dir.to_path_buf(),
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
        // joining the video and audio into one playable file (and MP3
        // extraction) needs ffmpeg; without it yt-dlp downloads both
        // halves and silently skips the merge. Refuse up front in
        // plain words instead of stranding the user with half-files.
        if find_tool("ffmpeg", components_dir).is_none() {
            return Err("ffmpeg is not available on this system — it joins the video and audio into one playable file. Install ffmpeg with your system package manager (Debian/Ubuntu: sudo apt install ffmpeg), or use an NDM build that bundles it, then try again".to_string());
        }
        let mut args = base_args(components_dir, self.cookies.as_deref());
        args.push("-f".into());
        args.push(self.selector.clone());
        if self.audio {
            args.push("-x".into());
            args.push("--audio-format".into());
            args.push("mp3".into());
        } else {
            args.push("--merge-output-format".into());
            args.push("mp4".into());
        }
        // download DASH/HLS fragments in parallel — measured on a 106-segment
        // HLS stream: sequential 1.26 MiB/s vs 8-way parallel 4.06 MiB/s
        // (16-way was slower again, so the cap is 8)
        let frags = ndm::global_connections().clamp(1, 8);
        args.push("--concurrent-fragments".into());
        args.push(frags.to_string());
        args.push("--hls-prefer-native".into());
        // honour the global speed limit (Settings); applies per (re)start
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
        // NOTE: no --print here — with --print active, this yt-dlp
        // suppresses ALL progress output (measured: zero progress
        // lines for the whole download), so the UI sat at 0 B until
        // "finished". The reader finds the final file by diffing the
        // download directory instead (find_final_file).
        args.push("-o".into());
        args.push(
            self.dir
                .join("%(title)s.%(ext)s")
                .display()
                .to_string(),
        );
        args.push("--".into());
        args.push(self.url.clone());

        // held across check + spawn + store: two racing spawn() calls
        // (double-clicked Resume, queue pump vs. manual resume) must
        // never both launch yt-dlp for one job — they would write the
        // same .part files concurrently, and the second store would
        // orphan the first process beyond pause/cancel's reach
        let _spawn_guard = self.spawn_lock.lock().unwrap();
        {
            let mut slot = self.child.lock().unwrap();
            if let Some(c) = slot.as_mut() {
                if matches!(c.try_wait(), Ok(None)) {
                    return Err("this video is already downloading".to_string());
                }
            }
        }
        // cleared BEFORE the process exists: a pause landing during the
        // spawn sets the flag after this point and is honoured (pause
        // itself waits on the spawn lock, then kills the stored child)
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
            // reap the child being replaced (the running-check above
            // proved it already exited) instead of dropping its handle
            if let Some(mut old) = slot.take() {
                let _ = old.wait();
            }
            *slot = Some(child);
        }
        self.progress.lock().unwrap().status = "downloading".to_string();

        // progress reader
        let job = self.clone();
        std::thread::spawn(move || {
            let err_buf = Arc::new(Mutex::new(String::new()));
            if let Some(mut se) = stderr {
                let eb = err_buf.clone();
                std::thread::spawn(move || {
                    // rolling tail only: a looping extractor must not
                    // grow this buffer for the whole life of the job
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
            // yt-dlp reports progress PER FILE: a video+audio download
            // restarts downloaded_bytes at zero for the second stream,
            // which used to slam the UI bar back to ~0% mid-download.
            // Accumulate: when a new stream starts, fold the finished
            // stream's size into a base the displayed numbers build on.
            let mut base: u64 = 0;
            let mut cur_done: u64 = 0;
            let mut cur_total: u64 = 0;
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
                } else if line.contains("[Merger]") || line.contains("[ExtractAudio]") {
                    // streams are done; ffmpeg is producing the final
                    // file — show a full bar at rest, not a stale speed
                    let mut p = job.progress.lock().unwrap();
                    if p.total > 0 {
                        p.done = p.total;
                    }
                    p.speed_bps = 0.0;
                }
            }
            // stdout closed: the process is done or dying — collect the
            // exit status. The child is TAKEN out of the slot first: the
            // old code held child.lock() across wait(), so a resume()
            // swapping in a new child in that window wedged pause/cancel
            // behind the lock for the new process's whole lifetime.
            if job.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                // a newer spawn owns this job now (pause + quick
                // resume): this reader must neither steal its child
                // from the slot nor publish a verdict over its status
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
            // a clean exit is the ground truth and beats the stop flag:
            // a pause landing microseconds after yt-dlp finished on
            // its own must not relabel a completed download "paused"
            if status.map(|s| s.success()).unwrap_or(false) {
                // yt-dlp can exit 0 WITHOUT producing the final file —
                // when ffmpeg is missing it downloads both streams,
                // silently skips the merge and still claims success,
                // leaving two unplayable half-files behind. Never call
                // that "finished": the file itself is the ground truth.
                let found = p
                    .final_path
                    .clone()
                    .filter(|fp| fp.is_file())
                    .or_else(|| find_final_file(&job.dir, &before, job.audio));
                match found {
                    Some(fp) => {
                        // a file with pictures but no sound, from a
                        // page that HAS sound, is a broken merge —
                        // not a finished download. Say so plainly
                        // instead of letting the user discover a
                        // silent video in their player.
                        let silent = job.expect_audio
                            && matches!(
                                probe_streams(&fp, &job.components_dir),
                                Some((true, false))
                            );
                        if silent {
                            p.status = "error".to_string();
                            p.error = Some(
                                "the downloaded file has no sound — the video and audio could not be joined into one playable file. The silent file was kept in the download folder; please try the download again"
                                    .to_string(),
                            );
                            p.final_path = Some(fp);
                        } else {
                            p.status = "finished".to_string();
                            if let Ok(m) = fp.metadata() {
                                p.total = m.len();
                                p.done = m.len();
                            }
                            p.final_path = Some(fp);
                        }
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

#[cfg(test)]
mod tests {
    use super::streams_in_ffmpeg_info;

    #[test]
    fn merged_mp4_reports_both_streams() {
        let info = "  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p, 1920x1080, 2500 kb/s, 30 fps\n  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 44100 Hz, stereo, fltp, 128 kb/s";
        assert_eq!(streams_in_ffmpeg_info(info), (true, true));
    }

    #[test]
    fn video_only_file_reports_missing_audio() {
        let info = "  Stream #0:0: Video: vp9, yuv420p, 1280x720, 1100 kb/s, 30 fps";
        assert_eq!(streams_in_ffmpeg_info(info), (true, false));
    }

    #[test]
    fn audio_only_file_reports_no_video() {
        let info = "  Stream #0:0: Audio: mp3, 44100 Hz, stereo, fltp, 192 kb/s";
        assert_eq!(streams_in_ffmpeg_info(info), (false, true));
    }
}
