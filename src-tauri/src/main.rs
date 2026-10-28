// NDM — Nill Download Manager (app backend)
// A thin Tauri shell over the proven `ndm` engine. The download list is
// persisted to <app data>/downloads.json and restored on launch; unfinished
// entries come back as Paused with their exact on-disk progress (read from
// the engine's own state files), so closing the app — or losing power —
// never loses a download. Resume continues from the same byte.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

mod video;

#[derive(Clone, Serialize)]
struct ProgressPayload {
    id: String,
    name: String,
    url: String,
    total: u64,
    done_bytes: u64,
    speed_bps: f64,
    conns: usize,
    status: String,
    blocks: String,
    category: String,
    added: String,
    resume_capability: String,
    completion: String,
    error: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedEntry {
    id: String,
    name: String,
    url: String,
    out_path: String,
    #[serde(default = "default_category")]
    category: String,
    #[serde(default)]
    added: String,
    #[serde(default)]
    total: u64,
    #[serde(default)]
    done_bytes: u64,
    #[serde(default = "default_status")]
    status: String,
    /// "http" (engine) or "video" (yt-dlp); older saves have no kind → http
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default)]
    selector: String,
    #[serde(default)]
    audio: bool,
    #[serde(default)]
    expect_audio: bool,
    /// token naming this entry's captured browser-cookies file
    /// (app-data cookies/<token>.txt); "" = the browser sent none
    #[serde(default)]
    cookies: String,
    /// page the browser was on when it handed this download over;
    /// repeated as the Referer header (some hosts refuse without it)
    #[serde(default)]
    referer: String,
    /// "", "open_file" or "open_folder" — runs when the download finishes
    #[serde(default)]
    completion: String,
}

fn default_kind() -> String {
    "http".to_string()
}

fn default_category() -> String {
    "All Downloads".to_string()
}

fn default_status() -> String {
    "paused".to_string()
}

/// User preferences, persisted to <app data>/settings.json. Field names are
/// the exact contract the frontend Options/Scheduler dialogs use.
#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    #[serde(default = "def_dir")]
    default_dir: String,
    #[serde(default = "def_conns")]
    max_connections: usize,
    #[serde(default)]
    speed_limit_bps: u64,
    #[serde(default)]
    max_simultaneous: usize,
    #[serde(default)]
    auto_resume: bool,
    #[serde(default)]
    scheduler_enabled: bool,
    #[serde(default = "def_sched_start")]
    sched_start: String,
    #[serde(default = "def_sched_stop")]
    sched_stop: String,
    #[serde(default = "def_vq")]
    video_quality: String,
    /// check for a new version shortly after launch (and count this
    /// installation once, anonymously — an empty GET, no data sent)
    #[serde(default = "def_true")]
    auto_update: bool,
    #[serde(default)]
    install_counted: bool,
    /// optional outbound proxy, e.g. "http://127.0.0.1:8080" or
    /// "socks5://host:1080"; empty = direct connection
    #[serde(default)]
    proxy: String,
}

fn def_true() -> bool {
    true
}

fn def_dir() -> String {
    "~/Downloads".to_string()
}
fn def_conns() -> usize {
    16
}
fn def_sched_start() -> String {
    "22:00".to_string()
}
fn def_sched_stop() -> String {
    "06:00".to_string()
}
fn def_vq() -> String {
    "best".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            default_dir: def_dir(),
            max_connections: def_conns(),
            speed_limit_bps: 0,
            max_simultaneous: 0,
            auto_resume: false,
            scheduler_enabled: false,
            sched_start: def_sched_start(),
            sched_stop: def_sched_stop(),
            video_quality: def_vq(),
            auto_update: true,
            install_counted: false,
            proxy: String::new(),
        }
    }
}

impl Settings {
    fn sanitize(&mut self) {
        self.max_connections = self.max_connections.clamp(1, 32);
        self.max_simultaneous = self.max_simultaneous.min(10);
        if self.default_dir.trim().is_empty() {
            self.default_dir = def_dir();
        }
        self.sched_start = normalize_hhmm(&self.sched_start, def_sched_start());
        self.sched_stop = normalize_hhmm(&self.sched_stop, def_sched_stop());
        const QUALITIES: [&str; 9] = [
            "best", "2160", "1440", "1080", "720", "480", "360", "240", "audio",
        ];
        if !QUALITIES.contains(&self.video_quality.as_str()) {
            self.video_quality = def_vq();
        }
        self.proxy = self.proxy.trim().to_string();
    }
}

/// Accept "7:00" or "07:00" and always store zero-padded "07:00" — the
/// scheduler compares times as strings, so padding is load-bearing.
fn normalize_hhmm(value: &str, fallback: String) -> String {
    let parts: Vec<&str> = value.trim().split(':').collect();
    if parts.len() == 2 {
        if let (Ok(h), Ok(m)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
            if h < 24 && m < 60 {
                return format!("{h:02}:{m:02}");
            }
        }
    }
    fallback
}

/// Is `now` ("HH:MM") inside the configured scheduler window?
fn in_window(s: &Settings, now: &str) -> bool {
    if s.sched_start == s.sched_stop {
        // equal times mean "all day", not "never": an always-empty
        // window would silently disable the scheduler
        return true;
    }
    if s.sched_start <= s.sched_stop {
        now >= s.sched_start.as_str() && now < s.sched_stop.as_str()
    } else {
        // overnight window, e.g. 22:00 -> 06:00
        now >= s.sched_start.as_str() || now < s.sched_stop.as_str()
    }
}

fn current_settings(state: &AppState) -> Settings {
    state.settings.lock().unwrap().clone()
}

fn load_settings(state: &AppState) {
    let path = state.settings_file.lock().unwrap().clone();
    if path.as_os_str().is_empty() {
        return;
    }
    if let Ok(raw) = std::fs::read(&path) {
        if let Ok(mut s) = serde_json::from_slice::<Settings>(&raw) {
            s.sanitize();
            ndm::set_global_limit(s.speed_limit_bps);
            ndm::set_global_connections(s.max_connections);
            ndm::set_global_proxy(&s.proxy);
            *state.settings.lock().unwrap() = s;
        }
    }
}

fn save_settings(state: &AppState) {
    let path = state.settings_file.lock().unwrap().clone();
    if path.as_os_str().is_empty() {
        return;
    }
    let settings = current_settings(state);
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = std::fs::File::create(&tmp) {
        use std::io::Write;
        if serde_json::to_writer(&mut f, &settings).is_ok() && f.flush().is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Progress restored from disk for an entry that has no live engine handle.
#[derive(Clone)]
struct Restored {
    total: u64,
    done_bytes: u64,
    blocks: Vec<u8>,
    resumable: bool,
    status: String,
}

struct Entry {
    id: String,
    name: String,
    url: String,
    out_path: PathBuf,
    category: String,
    added: String,
    download: Option<Arc<ndm::Download>>,
    video: Option<Arc<video::VideoJob>>,
    kind: String,
    selector: String,
    audio: bool,
    expect_audio: bool,
    cookies: String,
    referer: String,
    restored: Mutex<Option<Restored>>,
    /// waiting for a scheduler/queue slot; started automatically by the pump
    queued: AtomicBool,
    // (last sample time, last done_bytes, smoothed rate) for a live rate readout
    sample: Mutex<(Instant, u64, f64)>,
    /// an emit loop is currently running for this entry (spawn guard)
    emitting: AtomicBool,
    /// the engine is starting (probe in flight) — counts against the cap
    starting: AtomicBool,
    /// the entry was removed; its emit loop must stop at the next tick
    removed: AtomicBool,
    /// pause was pressed while the start probe was still in flight:
    /// honoured the moment the download goes live
    pause_requested: AtomicBool,
    /// "", "open_file" or "open_folder" — runs once when it finishes
    completion: Mutex<String>,
}

struct AppState {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    order: Mutex<Vec<String>>,
    seq: AtomicU64,
    data_file: Mutex<PathBuf>,
    components_dir: Mutex<PathBuf>,
    cookies_dir: Mutex<PathBuf>,
    save_lock: Mutex<()>,
    settings: Mutex<Settings>,
    settings_file: Mutex<PathBuf>,
    /// "Start Queue Now" override: lets the pump run outside the window
    /// until the window starts/ends or "Stop Queue Now" clears it
    sched_override: AtomicBool,
    /// video links from the extension that arrived before the webview
    /// was listening; the frontend drains this once on load
    pending_videos: Mutex<Vec<serde_json::Value>>,
    /// recent probe results by page URL (10 min): a yt-dlp extraction
    /// takes seconds and a page's formats do not change minute to
    /// minute — the browser panel and the dialog share this cache
    probe_cache: Mutex<HashMap<String, (Instant, video::VideoInfo)>>,
    pending_files: Mutex<Vec<serde_json::Value>>,
    /// a first-run video-component setup is currently downloading
    components_busy: AtomicBool,
    /// the newest release known from the update manifest, if newer
    pending_update: Mutex<Option<ndm::updater::ReleaseInfo>>,
    /// the running version fell below the manifest's min_supported:
    /// downloads stay blocked until the update is installed
    update_required: AtomicBool,
    update_busy: AtomicBool,
}

impl AppState {
    fn components(&self) -> PathBuf {
        self.components_dir.lock().unwrap().clone()
    }

    fn cookies_dir(&self) -> PathBuf {
        self.cookies_dir.lock().unwrap().clone()
    }

    /// Resolve a cookies token (minted by handle_incoming_line, carried
    /// by the frontend, or restored with an entry) to its captured file.
    /// Tokens are app-generated ("ck" + digits); anything else resolves
    /// to nothing, so a token can never point outside the cookies folder.
    fn cookies_path(&self, token: &str) -> Option<PathBuf> {
        if token.is_empty()
            || token.len() > 24
            || !token.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return None;
        }
        let p = self.cookies_dir().join(format!("{token}.txt"));
        p.is_file().then_some(p)
    }
}

impl AppState {
    fn get(&self, id: &str) -> Option<Arc<Entry>> {
        self.entries.lock().unwrap().get(id).cloned()
    }
}

fn category_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" => "Compressed",
        "pdf" | "doc" | "docx" | "txt" | "md" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" => {
            "Documents"
        }
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => "Music",
        "mp4" | "mkv" | "avi" | "webm" | "mov" | "flv" => "Video",
        "exe" | "msi" | "deb" | "rpm" | "appimage" | "dmg" | "iso" | "apk" => "Programs",
        _ => "All Downloads",
    }
}

fn file_name_for(url: &str) -> String {
    // the engine's version percent-decodes and can never return a dot
    // segment or a name containing path separators
    ndm::file_name_of(url)
}

fn downsample_blocks(blocks: &[u8]) -> String {
    // UI canvas caps at 1024 cells; merge groups keeping the most "active" state
    if blocks.len() <= 1024 {
        return blocks.iter().map(|b| (b'0' + (*b).min(2)) as char).collect();
    }
    let group = blocks.len().div_ceil(1024);
    let mut out = String::with_capacity(1024);
    for chunk in blocks.chunks(group) {
        let c = if chunk.iter().all(|&b| b == 1) {
            '1'
        } else if chunk.contains(&2) {
            '2'
        } else {
            '0'
        };
        out.push(c);
    }
    out
}

fn synth_blocks(total: u64, done: u64, downloading: bool) -> String {
    // video jobs have no per-block map; synthesize a 128-cell map from the ratio
    const N: u64 = 128;
    if total == 0 {
        return String::new();
    }
    let done_cells = (done.saturating_mul(N) / total).min(N);
    let mut s = String::with_capacity(N as usize);
    for i in 0..N {
        if i < done_cells {
            s.push('1');
        } else if downloading && i == done_cells {
            s.push('2');
        } else {
            s.push('0');
        }
    }
    s
}

fn payload_of(entry: &Entry) -> ProgressPayload {
    if let Some(job) = &entry.video {
        let p = job.progress.lock().unwrap().clone();
        return ProgressPayload {
            id: entry.id.clone(),
            name: entry.name.clone(),
            url: entry.url.clone(),
            total: p.total,
            done_bytes: p.done,
            speed_bps: p.speed_bps,
            conns: if p.status == "downloading" { 1 } else { 0 },
            blocks: synth_blocks(p.total, p.done, p.status == "downloading"),
            status: p.status.clone(),
            category: entry.category.clone(),
            added: entry.added.clone(),
            resume_capability: "Yes (yt-dlp resume)".to_string(),
            completion: entry.completion.lock().unwrap().clone(),
            error: p.error.clone().unwrap_or_default(),
        };
    }
    let (total, done_bytes, rate, conns, status, blocks, resumable, error_text) = match &entry.download {
        Some(dl) => {
            let p = dl.progress();
            let rate = if p.status == ndm::Status::Downloading {
                entry.sample.lock().unwrap().2
            } else {
                0.0
            };
            (
                p.total,
                p.done_bytes,
                rate,
                p.conns,
                p.status.as_str().to_string(),
                downsample_blocks(&p.blocks),
                p.resumable,
                p.error.unwrap_or_default(),
            )
        }
        None => match &*entry.restored.lock().unwrap() {
            Some(r) => (
                r.total,
                r.done_bytes,
                0.0,
                0,
                r.status.clone(),
                downsample_blocks(&r.blocks),
                r.resumable,
                String::new(),
            ),
            None => (
                0,
                0,
                0.0,
                0,
                "queued".to_string(),
                String::new(),
                false,
                String::new(),
            ),
        },
    };
    let status = if entry.download.is_none()
        && entry.video.is_none()
        && entry.queued.load(Ordering::Relaxed)
    {
        "queued".to_string()
    } else {
        status
    };
    ProgressPayload {
        id: entry.id.clone(),
        name: entry.name.clone(),
        url: entry.url.clone(),
        total,
        done_bytes,
        speed_bps: rate,
        conns,
        status,
        blocks,
        category: entry.category.clone(),
        added: entry.added.clone(),
        resume_capability: if resumable {
            "Yes (Range supported)".to_string()
        } else {
            "No".to_string()
        },
        completion: entry.completion.lock().unwrap().clone(),
        error: error_text,
    }
}

/// Snapshot the whole list to disk (write-then-rename, crash-safe).
fn persist(state: &AppState) {
    let _guard = state.save_lock.lock().unwrap();
    let path = state.data_file.lock().unwrap().clone();
    if path.as_os_str().is_empty() {
        return;
    }
    let order = state.order.lock().unwrap().clone();
    let saved: Vec<SavedEntry> = order
        .iter()
        .filter_map(|id| state.get(id))
        .map(|e| {
            let p = payload_of(&e);
            SavedEntry {
                id: e.id.clone(),
                name: e.name.clone(),
                url: e.url.clone(),
                out_path: e.out_path.to_string_lossy().to_string(),
                category: e.category.clone(),
                added: e.added.clone(),
                total: p.total,
                done_bytes: p.done_bytes,
                // a live download is, from the next launch's perspective, paused
                status: if e.queued.load(Ordering::Relaxed) {
                    "queued".to_string()
                } else if p.status == "downloading" {
                    "paused".to_string()
                } else {
                    p.status
                },
                kind: e.kind.clone(),
                selector: e.selector.clone(),
                audio: e.audio,
                expect_audio: e.expect_audio,
                cookies: e.cookies.clone(),
                referer: e.referer.clone(),
                completion: e.completion.lock().unwrap().clone(),
            }
        })
        .collect();
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = std::fs::File::create(&tmp) {
        use std::io::Write;
        if serde_json::to_writer(&mut f, &saved).is_ok() && f.flush().is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Advance the smoothed rate sample. Called ONLY from the emit loop, so
/// the displayed rate depends on one fixed cadence — payload_of is a pure
/// getter and never mutates this.
fn update_sample(entry: &Entry) {
    let Some(dl) = &entry.download else { return };
    let p = dl.progress();
    let mut s = entry.sample.lock().unwrap();
    let now = Instant::now();
    let dt = now.duration_since(s.0).as_secs_f64();
    if dt >= 0.2 {
        let inst = (p.done_bytes.saturating_sub(s.1)) as f64 / dt;
        s.2 = if s.2 == 0.0 { inst } else { 0.5 * s.2 + 0.5 * inst };
        if p.done_bytes != s.1 || p.status != ndm::Status::Downloading {
            s.0 = now;
        }
        s.1 = p.done_bytes;
    }
}

/// Open a file or folder with the OS default handler (best effort).
fn open_target(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c.arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot open: {e}"))
}

/// The per-download "Options on completion" action, run once at finish.
/// For video entries out_path is the folder (the final file name comes
/// from yt-dlp), so both actions open the folder there.
fn run_completion_action(entry: &Entry) {
    let action = entry.completion.lock().unwrap().clone();
    let target = match action.as_str() {
        "open_file" => entry.out_path.clone(),
        "open_folder" => {
            if entry.kind == "video" {
                entry.out_path.clone()
            } else {
                entry
                    .out_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| entry.out_path.clone())
            }
        }
        _ => return,
    };
    let _ = open_target(&target);
}

/// Emit progress for one entry until it pauses, queues, or reaches a
/// terminal state. Guarded per entry: no matter how many code paths ask,
/// at most one loop per entry ever runs (pause/resume cycles used to
/// stack up duplicate loops).
fn spawn_emit_loop(app: AppHandle, entry: Arc<Entry>) {
    if entry.emitting.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        loop {
            if entry.removed.load(Ordering::Relaxed) {
                break;
            }
            update_sample(&entry);
            let payload = payload_of(&entry);
            let status = payload.status.clone();
            let _ = app.emit("progress", payload);
            match status.as_str() {
                "finished" => {
                    run_completion_action(&entry);
                    let state = app.state::<AppState>();
                    persist(&state);
                    pump_queue(&app);
                    break;
                }
                "error" | "cancelled" => {
                    let state = app.state::<AppState>();
                    persist(&state);
                    pump_queue(&app);
                    break;
                }
                "paused" | "queued" => {
                    let state = app.state::<AppState>();
                    persist(&state);
                    break;
                }
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        entry.emitting.store(false, Ordering::SeqCst);
    });
}

/// Replace a stub entry with a live one once its engine handle exists.
fn swap_live(
    state: &AppState,
    stub: &Entry,
    dl: Arc<ndm::Download>,
) -> Result<Arc<Entry>, Arc<Entry>> {
    // seed the rate sampler with the engine's real position: seeding 0
    // made the first readout divide the entry's ENTIRE history by a
    // few seconds — a resumed 500 MB download "ran" at ~2 GB/s
    let done0 = dl.progress().done_bytes;
    let live = Arc::new(Entry {
        id: stub.id.clone(),
        name: stub.name.clone(),
        url: stub.url.clone(),
        out_path: stub.out_path.clone(),
        category: stub.category.clone(),
        added: stub.added.clone(),
        download: Some(dl),
        video: None,
        kind: "http".to_string(),
        selector: String::new(),
        cookies: stub.cookies.clone(),
        referer: stub.referer.clone(),
        audio: false,
        expect_audio: false,
        restored: Mutex::new(None),
        queued: AtomicBool::new(false),
        sample: Mutex::new((Instant::now(), done0, 0.0)),
        emitting: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        removed: AtomicBool::new(stub.removed.load(Ordering::Relaxed)),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(stub.completion.lock().unwrap().clone()),
    });
    // re-verify under the SAME lock that inserts: a remove landing in
    // the gap after start_engine's own check must not leave a live
    // entry sitting in the map but outside the list order — invisible
    // and unstoppable. On rejection the uninserted live entry goes back
    // to the caller so its engine can be cancelled.
    let mut entries = state.entries.lock().unwrap();
    let still_current = entries
        .get(&stub.id)
        .map(|e| std::ptr::eq(e.as_ref(), stub))
        .unwrap_or(false);
    if stub.removed.load(Ordering::SeqCst) || !still_current {
        return Err(live);
    }
    entries.insert(live.id.clone(), live.clone());
    Ok(live)
}

async fn start_engine(app: AppHandle, stub: &Arc<Entry>) -> Result<Arc<Entry>, String> {
    let url = stub.url.clone();
    let out_path = stub.out_path.clone();
    let max_conn = {
        let state = app.state::<AppState>();
        current_settings(&state).max_connections
    };
    // a caught browser download carries the browser's own session for
    // this link (cookies token + referer): the engine repeats them on
    // the probe and every block request, so login-gated files download
    // exactly as they would have in the browser
    let auth = {
        let state = app.state::<AppState>();
        let cookie = state
            .cookies_path(&stub.cookies)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| cookie_header_from_netscape(&text));
        let referer = if stub.referer.is_empty() {
            None
        } else {
            Some(stub.referer.clone())
        };
        ndm::Auth { cookie, referer }
    };
    // the probe inside ndm::start can take seconds; the entry must count
    // against the simultaneous cap from this moment, not from first bytes.
    // The claim is atomic: two starts for one stub (double resume, pump
    // racing a manual resume) must not probe and spawn in parallel
    if stub.starting.swap(true, Ordering::SeqCst) {
        return Err("the download is already starting".to_string());
    }
    let dl = match tauri::async_runtime::spawn_blocking(move || {
        ndm::start_with_auth(&url, &out_path, max_conn, ndm::DEFAULT_BLOCK, auth)
    })
    .await
    {
        Ok(Ok(dl)) => dl,
        Ok(Err(e)) => {
            stub.starting.store(false, Ordering::SeqCst);
            return Err(e);
        }
        Err(e) => {
            stub.starting.store(false, Ordering::SeqCst);
            return Err(format!("internal join error: {e}"));
        }
    };
    // the entry may have been deleted or refreshed while the probe was
    // running: never resurrect a stub the map no longer holds, or the
    // download would run invisibly with no row and no way to stop it
    {
        let state = app.state::<AppState>();
        let still_current = state
            .get(&stub.id)
            .map(|e| Arc::ptr_eq(&e, stub))
            .unwrap_or(false);
        if stub.removed.load(Ordering::SeqCst) || !still_current {
            dl.control.cancel();
            // the fresh engine may already have written its state file;
            // remove it so it cannot claim the path as a phantom later
            let _ = std::fs::remove_file(ndm::state_path_for(&stub.out_path));
            stub.starting.store(false, Ordering::SeqCst);
            return Err("the download was removed before it could start".to_string());
        }
    }
    let live = {
        let state = app.state::<AppState>();
        match swap_live(&state, stub, dl) {
            Ok(live) => {
                persist(&state);
                live
            }
            Err(live) => {
                // removed (or replaced) in the final gap after the
                // check above: stop the engine we just made
                if let Some(d) = &live.download {
                    d.control.cancel();
                }
                let _ = std::fs::remove_file(ndm::state_path_for(&stub.out_path));
                stub.starting.store(false, Ordering::SeqCst);
                return Err("the download was removed before it could start".to_string());
            }
        }
    };
    // a pause pressed while the probe was in flight is honoured the
    // moment the download goes live (the press used to be discarded)
    if stub.pause_requested.swap(false, Ordering::SeqCst) {
        if let Some(d) = &live.download {
            d.control.pause();
        }
    }
    spawn_emit_loop(app, live.clone());
    Ok(live)
}

/// The user's home folder. Windows GUI processes have no HOME variable,
/// so resolving "~/Downloads" from HOME alone would silently land in
/// "<current directory>/Downloads" there — fall back to the Windows
/// variables before giving up.
fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    if let Ok(p) = std::env::var("USERPROFILE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    if let (Ok(d), Ok(p)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        return PathBuf::from(format!("{d}{p}"));
    }
    PathBuf::from(".")
}

fn resolve_dir(dir: &str) -> PathBuf {
    let home = home_dir();
    if dir.trim().is_empty() {
        home.join("Downloads")
    } else if let Some(rest) = dir.trim().strip_prefix("~/") {
        // the UI's default "~/Downloads" must land in the real home folder
        home.join(rest)
    } else {
        PathBuf::from(dir.trim())
    }
}

/// Pick a file name in `dir` that no live entry and no engine state file
/// claims — two downloads must never share one output/state path.
fn unique_out_path(state: &AppState, dir: &Path, name: &str) -> (String, PathBuf) {
    let taken = |p: &Path| -> bool {
        p.exists()
            || ndm::state_path_for(p).exists()
            || state
                .entries
                .lock()
                .unwrap()
                .values()
                .any(|e| e.out_path == p)
    };
    let first = dir.join(name);
    if !taken(&first) {
        return (name.to_string(), first);
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1..1000 {
        let candidate = format!("{stem} ({n}){ext}");
        let p = dir.join(&candidate);
        if !taken(&p) {
            return (candidate, p);
        }
    }
    (name.to_string(), first)
}

/// Register a not-yet-started HTTP download in the list; returns its stub.
/// `category` overrides the extension-based category (the Site Grabber
/// passes "Grabber" so its downloads land in Grabber projects).
/// Build the `Cookie:` header value from a captured Netscape cookie
/// file (the form the extension hands over). The capture was scoped to
/// this download's URL, so every pair in the file is one the browser
/// itself would have sent.
fn cookie_header_from_netscape(text: &str) -> Option<String> {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() >= 7 {
            pairs.push(format!("{}={}", f[5], f[6]));
        }
    }
    if pairs.is_empty() {
        None
    } else {
        Some(pairs.join("; "))
    }
}

fn register_http_stub(
    state: &AppState,
    url: &str,
    dir: &str,
    category: Option<&str>,
    name_hint: Option<&str>,
    cookies_token: &str,
    referer: &str,
) -> Result<Arc<Entry>, String> {
    let dir = resolve_dir(dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create folder: {e}"))?;
    // a name the browser suggested (Content-Disposition) wins over the
    // URL tail, stripped of any path parts a hostile page could smuggle
    let base_name = name_hint
        .map(|n| n.rsplit(['/', '\\']).next().unwrap_or(n).trim().to_string())
        .filter(|n| !n.is_empty() && n != "." && n != "..")
        .unwrap_or_else(|| file_name_for(url));
    let (name, out_path) = unique_out_path(state, &dir, &base_name);
    let id = format!("dl-{}", state.seq.fetch_add(1, Ordering::Relaxed) + 1);
    let stub = Arc::new(Entry {
        id: id.clone(),
        name: name.clone(),
        url: url.to_string(),
        out_path,
        category: category
            .map(|c| c.to_string())
            .unwrap_or_else(|| category_for(&name).to_string()),
        added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        download: None,
        video: None,
        kind: "http".to_string(),
        selector: String::new(),
        cookies: cookies_token.to_string(),
        referer: referer.to_string(),
        audio: false,
        expect_audio: false,
        restored: Mutex::new(None),
        queued: AtomicBool::new(false),
        sample: Mutex::new((Instant::now(), 0, 0.0)),
        emitting: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        removed: AtomicBool::new(false),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(String::new()),
    });
    state
        .entries
        .lock()
        .unwrap()
        .insert(id.clone(), stub.clone());
    state.order.lock().unwrap().push(id);
    Ok(stub)
}

/// How many entries are actively downloading right now.
fn count_active(state: &AppState) -> usize {
    let order = state.order.lock().unwrap().clone();
    order
        .iter()
        .filter_map(|id| state.get(id))
        .filter(|e| {
            e.starting.load(Ordering::Relaxed) || payload_of(e).status == "downloading"
        })
        .count()
}

/// Start one entry that is not currently running: a queued/restored HTTP
/// entry gets the engine; a restored video entry gets a fresh yt-dlp job
/// (which continues its .part files); a live-but-paused one just resumes.
fn start_entry(app: &AppHandle, entry: Arc<Entry>) {
    if let Some(dl) = &entry.download {
        dl.control.resume();
        return;
    }
    if let Some(job) = &entry.video {
        let state = app.state::<AppState>();
        match job.resume(&state.components()) {
            Ok(()) => spawn_emit_loop(app.clone(), entry),
            Err(e) => {
                // the last silent start path: a missing yt-dlp here
                // used to leave the entry sitting on "paused" forever
                {
                    let mut p = job.progress.lock().unwrap();
                    p.status = "error".to_string();
                    p.error = Some(e);
                }
                let _ = app.emit("progress", payload_of(&entry));
                persist(&state);
            }
        }
        return;
    }
    entry.queued.store(false, Ordering::Relaxed);
    if entry.kind == "video" {
        // claim the start atomically (the HTTP path does this inside
        // start_engine): two starts for one video entry would run two
        // yt-dlp processes into the same .part files
        if entry.starting.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = app.state::<AppState>();
        match video::VideoJob::start(
            &entry.url,
            &entry.out_path,
            &entry.selector,
            entry.audio,
            entry.expect_audio,
            state.cookies_path(&entry.cookies),
            &state.components(),
        ) {
            Ok(job) => {
            // the entry may have been removed or replaced while the job
            // was being created: never install a live entry the list no
            // longer holds (the video edition of the probe-delete race).
            // Check and insert happen under the SAME lock — a delete
            // landing between them would otherwise strand a live job
            // in the map with no list row able to reach it.
            let live = Arc::new(Entry {
                id: entry.id.clone(),
                name: entry.name.clone(),
                url: entry.url.clone(),
                out_path: entry.out_path.clone(),
                category: entry.category.clone(),
                added: entry.added.clone(),
                download: None,
                video: Some(job),
                kind: "video".to_string(),
                selector: entry.selector.clone(),
                cookies: entry.cookies.clone(),
                referer: entry.referer.clone(),
                audio: entry.audio,
                expect_audio: entry.expect_audio,
                restored: Mutex::new(None),
                queued: AtomicBool::new(false),
                sample: Mutex::new((Instant::now(), 0, 0.0)),
                emitting: AtomicBool::new(false),
                starting: AtomicBool::new(false),
                removed: AtomicBool::new(entry.removed.load(Ordering::Relaxed)),
                pause_requested: AtomicBool::new(false),
                completion: Mutex::new(entry.completion.lock().unwrap().clone()),
            });
            {
                let mut map = state.entries.lock().unwrap();
                let still_current = map
                    .get(&entry.id)
                    .map(|e| Arc::ptr_eq(e, &entry))
                    .unwrap_or(false);
                if entry.removed.load(Ordering::SeqCst) || !still_current {
                    drop(map);
                    if let Some(j) = &live.video {
                        j.cancel();
                    }
                    entry.starting.store(false, Ordering::SeqCst);
                    return;
                }
                map.insert(live.id.clone(), live.clone());
            }
            if entry.pause_requested.swap(false, Ordering::SeqCst) {
                if let Some(j) = &live.video {
                    j.pause();
                }
            }
            entry.starting.store(false, Ordering::SeqCst);
            spawn_emit_loop(app.clone(), live);
            persist(&state);
            }
            Err(_) => {
                entry.starting.store(false, Ordering::SeqCst);
                // mirror the HTTP path: a failed start must surface as a
                // visible error, not sit silently on a stale status
                *entry.restored.lock().unwrap() = Some(Restored {
                    total: 0,
                    done_bytes: 0,
                    blocks: Vec::new(),
                    resumable: false,
                    status: "error".to_string(),
                });
                let _ = app.emit("progress", payload_of(&entry));
                persist(&state);
            }
        }
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if start_engine(app.clone(), &entry).await.is_err() {
            // a failed start must not sit on "Queued" forever: surface it
            // as an error the user can see and retry (the engine's state
            // file survives, so a retry still resumes). Only ever touch
            // OUR entry: a refresh may have replaced it under this id
            // while we were starting, and the fresh entry is not ours
            // to deface.
            let state = app.state::<AppState>();
            if let Some(e) = state.get(&entry.id) {
                if Arc::ptr_eq(&e, &entry) {
                    *e.restored.lock().unwrap() = Some(Restored {
                        total: 0,
                        done_bytes: 0,
                        blocks: Vec::new(),
                        resumable: false,
                        status: "error".to_string(),
                    });
                    let _ = app.emit("progress", payload_of(&e));
                }
            }
            persist(&state);
        }
    });
}

/// The pump must not start downloads outside the scheduler window —
/// unless the user pressed "Start Queue Now" (the override lasts until
/// the window next starts or ends, or "Stop Queue Now" clears it).
fn scheduler_blocks(state: &AppState) -> bool {
    let s = current_settings(state);
    if !s.scheduler_enabled || state.sched_override.load(Ordering::Relaxed) {
        return false;
    }
    let now = chrono::Local::now().format("%H:%M").to_string();
    !in_window(&s, &now)
}

/// Fill free simultaneous-download slots from the queue (Settings cap).
fn pump_queue(app: &AppHandle) {
    let state = app.state::<AppState>();
    let max = current_settings(&state).max_simultaneous;
    if max == 0 || scheduler_blocks(&state) {
        return;
    }
    let mut active = count_active(&state);
    if active >= max {
        return;
    }
    let order = state.order.lock().unwrap().clone();
    for id in order {
        if active >= max {
            break;
        }
        let Some(e) = state.get(&id) else { continue };
        // atomic claim: two pumps running at once must never start the
        // same entry twice (two engines, one output file)
        if e.queued.swap(false, Ordering::SeqCst) {
            start_entry(app, e);
            active += 1;
        }
    }
}

/// Start every queued entry regardless of the simultaneous cap.
fn start_all_queued(app: &AppHandle) {
    let state = app.state::<AppState>();
    let order = state.order.lock().unwrap().clone();
    for id in order {
        if let Some(e) = state.get(&id) {
            if e.queued.swap(false, Ordering::SeqCst) {
                start_entry(app, e);
            }
        }
    }
}

/// Pause everything that is currently downloading (engine + video jobs).
/// With `requeue` (the scheduler's own window-end pause), entries that
/// were actually downloading are handed back to the queue so the next
/// window start claims them again — without it they sat paused forever
/// while merely-queued entries around them started. A manual Stop
/// Queue Now passes false: the user's stop is not the window's.
fn pause_all(app: &AppHandle, requeue: bool) {
    let state = app.state::<AppState>();
    let order = state.order.lock().unwrap().clone();
    for id in order {
        if let Some(e) = state.get(&id) {
            let was_downloading = payload_of(&e).status == "downloading";
            if let Some(dl) = &e.download {
                dl.control.pause();
            }
            if let Some(job) = &e.video {
                job.pause();
            }
            if requeue && was_downloading {
                e.queued.store(true, Ordering::Relaxed);
            }
            let _ = app.emit("progress", payload_of(&e));
        }
    }
    persist(&state);
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    current_settings(&state)
}

#[tauri::command]
fn set_settings(app: AppHandle, state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    let mut s = settings;
    s.sanitize();
    ndm::set_global_limit(s.speed_limit_bps);
    ndm::set_global_connections(s.max_connections);
    ndm::set_global_proxy(&s.proxy);
    let unlimited = s.max_simultaneous == 0;
    *state.settings.lock().unwrap() = s;
    save_settings(&state);
    if unlimited {
        // pump_queue treats max == 0 as "no cap to enforce" and starts
        // nobody; lifting the cap in Options must still drain the
        // queue (Start Queue Now already branches this way)
        if !scheduler_blocks(&state) {
            start_all_queued(&app);
        }
    } else {
        pump_queue(&app);
    }
    Ok(())
}

#[tauri::command]
fn scheduler_start_now(app: AppHandle, state: State<'_, AppState>) {
    state.sched_override.store(true, Ordering::SeqCst);
    if current_settings(&state).max_simultaneous == 0 {
        start_all_queued(&app);
    } else {
        pump_queue(&app);
    }
}

#[tauri::command]
fn scheduler_stop_now(app: AppHandle, state: State<'_, AppState>) {
    state.sched_override.store(false, Ordering::SeqCst);
    pause_all(&app, false);
}

#[tauri::command]
async fn add_download(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    dir: String,
    later: Option<bool>,
    category: Option<String>,
    name: Option<String>,
    cookies: Option<String>,
    referer: Option<String>,
) -> Result<String, String> {
    if state.update_required.load(Ordering::SeqCst) {
        return Err("NDM needs its latest update before it can download. Please install the update first.".to_string());
    }
    let stub = register_http_stub(
        &state,
        &url,
        &dir,
        category.as_deref(),
        name.as_deref(),
        cookies.as_deref().unwrap_or(""),
        referer.as_deref().unwrap_or(""),
    )?;
    let id = stub.id.clone();
    let max = current_settings(&state).max_simultaneous;
    let must_queue =
        later.unwrap_or(false) || (max > 0 && count_active(&state) >= max);
    if must_queue {
        // "Download Later" or no free slot: register as queued; the pump
        // (or the scheduler) starts it when a slot frees up
        stub.queued.store(true, Ordering::Relaxed);
        let _ = app.emit("progress", payload_of(&stub));
        persist(&state);
        return Ok(id);
    }
    match start_engine(app, &stub).await {
        Ok(_) => Ok(id),
        Err(e) => {
            // remove only OUR stub: a refresh during the probe replaces
            // the entry under this id, and that fresh entry must survive
            let ours = state
                .get(&id)
                .map(|cur| Arc::ptr_eq(&cur, &stub))
                .unwrap_or(false);
            if ours {
                state.entries.lock().unwrap().remove(&id);
                state.order.lock().unwrap().retain(|x| x != &id);
                // the entry is gone: its captured session must not
                // outlive it (same rule as remove_download)
                if let Some(path) = state.cookies_path(&stub.cookies) {
                    let _ = std::fs::remove_file(path);
                }
            }
            persist(&state);
            Err(e)
        }
    }
}

#[tauri::command]
fn pause_download(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    let entry = state.get(&id).ok_or("unknown download id")?;
    if payload_of(&entry).status == "finished" {
        // pausing a finished entry would flip it back to "paused" and a
        // later resume would re-download over the completed file
        return Ok(());
    }
    if entry.download.is_none() && entry.video.is_none() {
        // Not running: leave the queue and mark paused, keeping any restored
        // progress record intact. (An earlier version replaced the whole
        // entry here and threw the restored progress away.)
        if entry.starting.load(Ordering::SeqCst) {
            // the start probe is still in flight: record the press —
            // the start path honours it the moment the download goes
            // live, instead of discarding it and starting anyway
            entry.pause_requested.store(true, Ordering::SeqCst);
        }
        entry.queued.store(false, Ordering::Relaxed);
        {
            let mut r = entry.restored.lock().unwrap();
            match r.as_mut() {
                Some(r) => r.status = "paused".to_string(),
                None => {
                    *r = Some(Restored {
                        total: 0,
                        done_bytes: 0,
                        blocks: Vec::new(),
                        resumable: false,
                        status: "paused".to_string(),
                    })
                }
            }
        }
        let _ = app.emit("progress", payload_of(&entry));
        persist(&state);
        return Ok(());
    }
    if let Some(dl) = &entry.download {
        dl.control.pause();
    }
    if let Some(job) = &entry.video {
        job.pause();
    }
    let _ = app.emit("progress", payload_of(&entry));
    persist(&state);
    Ok(())
}

#[tauri::command]
async fn resume_download(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let entry = state.get(&id).ok_or("unknown download id")?;
    if payload_of(&entry).status == "finished" {
        return Ok(()); // nothing to resume; guards a silent re-download
    }
    // a manual Resume always takes the entry out of the queue
    entry.queued.store(false, Ordering::Relaxed);
    entry.pause_requested.store(false, Ordering::SeqCst);
    if let Some(dl) = &entry.download {
        dl.control.resume();
        let _ = app.emit("progress", payload_of(&entry));
        Ok(())
    } else if entry.kind == "video" {
        // live job resumes in place; a restored entry starts a fresh job
        // with the same arguments and yt-dlp continues its .part files
        if let Some(job) = &entry.video {
            job.resume(&state.components())?;
            spawn_emit_loop(app, entry);
        } else {
            // same arbitration as the HTTP path: an atomic claim, and a
            // staleness re-check before the live entry is installed —
            // a double resume must not run two yt-dlp processes into
            // the same .part files, and a remove during the start must
            // not be resurrected by it
            if entry.starting.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            let job = match video::VideoJob::start(
                &entry.url,
                &entry.out_path,
                &entry.selector,
                entry.audio,
                entry.expect_audio,
                state.cookies_path(&entry.cookies),
                &state.components(),
            ) {
                Ok(j) => j,
                Err(e) => {
                    entry.starting.store(false, Ordering::SeqCst);
                    return Err(e);
                }
            };
            let live = Arc::new(Entry {
                id: entry.id.clone(),
                name: entry.name.clone(),
                url: entry.url.clone(),
                out_path: entry.out_path.clone(),
                category: entry.category.clone(),
                added: entry.added.clone(),
                download: None,
                video: Some(job),
                kind: "video".to_string(),
                selector: entry.selector.clone(),
                cookies: entry.cookies.clone(),
                referer: entry.referer.clone(),
                audio: entry.audio,
                expect_audio: entry.expect_audio,
                restored: Mutex::new(None),
                queued: AtomicBool::new(false),
                sample: Mutex::new((Instant::now(), 0, 0.0)),
                emitting: AtomicBool::new(false),
                starting: AtomicBool::new(false),
                removed: AtomicBool::new(entry.removed.load(Ordering::Relaxed)),
                pause_requested: AtomicBool::new(false),
                completion: Mutex::new(entry.completion.lock().unwrap().clone()),
            });
            {
                let mut map = state.entries.lock().unwrap();
                let still_current = map
                    .get(&entry.id)
                    .map(|e| Arc::ptr_eq(e, &entry))
                    .unwrap_or(false);
                if entry.removed.load(Ordering::SeqCst) || !still_current {
                    drop(map);
                    if let Some(j) = &live.video {
                        j.cancel();
                    }
                    entry.starting.store(false, Ordering::SeqCst);
                    return Ok(());
                }
                map.insert(live.id.clone(), live.clone());
            }
            entry.starting.store(false, Ordering::SeqCst);
            spawn_emit_loop(app, live);
            persist(&state);
        }
        Ok(())
    } else {
        // restored entry: start the engine — it resumes from its state file
        start_engine(app, &entry).await?;
        Ok(())
    }
}

#[tauri::command]
fn remove_download(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    if let Some(entry) = state.get(&id) {
        entry.removed.store(true, Ordering::SeqCst);
        if let Some(dl) = &entry.download {
            dl.control.cancel();
        }
        if let Some(job) = &entry.video {
            job.cancel();
        }
        // the engine's state file is our own bookkeeping: leaving it
        // behind would make unique_out_path treat the path as taken
        // forever (phantom "name (1)" on every future re-add). The
        // partial data file itself is the user's — it stays.
        if entry.kind != "video" {
            let _ = std::fs::remove_file(ndm::state_path_for(&entry.out_path));
        }
        // a captured-cookies file belongs to this entry alone and holds
        // the user's browser session: it must not outlive the entry
        if let Some(path) = state.cookies_path(&entry.cookies) {
            let _ = std::fs::remove_file(path);
        }
    }
    state.entries.lock().unwrap().remove(&id);
    state.order.lock().unwrap().retain(|x| x != &id);
    persist(&state);
    pump_queue(&app);
    Ok(())
}

#[tauri::command]
fn open_path(state: State<'_, AppState>, id: String, folder: bool) -> Result<(), String> {
    let entry = state.get(&id).ok_or("unknown download id")?;
    let target = if entry.kind == "video" {
        entry.out_path.clone() // a video entry's out_path is its folder
    } else if folder {
        entry
            .out_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| entry.out_path.clone())
    } else {
        entry.out_path.clone()
    };
    open_target(&target)
}

#[tauri::command]
fn set_completion(state: State<'_, AppState>, id: String, action: String) -> Result<(), String> {
    if !matches!(action.as_str(), "" | "open_file" | "open_folder") {
        return Err("unknown completion action".to_string());
    }
    let entry = state.get(&id).ok_or("unknown download id")?;
    *entry.completion.lock().unwrap() = action;
    persist(&state);
    Ok(())
}

/// Which video components are not installed anywhere the app looks.
/// The slim installer ships without them (that is what keeps it
/// IDM-sized); they are fetched on first use into the app-data
/// components folder, verified against pinned hashes.
fn missing_components(state: &AppState) -> Vec<&'static str> {
    let dir = state.components();
    ndm::components::specs()
        .iter()
        .filter(|s| video::find_tool(s.name, &dir).is_none())
        .map(|s| s.name)
        .collect()
}

#[tauri::command]
fn components_status(state: State<'_, AppState>) -> serde_json::Value {
    let missing = missing_components(&state);
    serde_json::json!({
        "missing": missing,
        "ffmpeg": video::find_tool("ffmpeg", &state.components()).is_some(),
        "ready": missing.is_empty(),
    })
}

/// Run the one-time component setup for whatever is missing. A call
/// while a run is already in flight is a no-op — the running worker's
/// progress events settle every listener.
fn run_component_setup(app: AppHandle) {
    let state = app.state::<AppState>();
    if state.components_busy.swap(true, Ordering::SeqCst) {
        return;
    }
    let dir = state.components();
    std::thread::spawn(move || {
        // fully automatic: nobody is asked anything. If the machine
        // is offline or a download fails, wait and try again in the
        // background until the setup is complete — the user should
        // never have to think about the video engine at all.
        let mut attempt = 0u32;
        let result = loop {
            let mut pass: Result<(), String> = Ok(());
            for spec in ndm::components::specs() {
                if video::find_tool(spec.name, &dir).is_some() {
                    continue;
                }
                let app2 = app.clone();
                let cb = move |p: ndm::components::Progress| {
                    let _ = app2.emit(
                        "components_progress",
                        serde_json::json!({
                            "name": p.name,
                            "downloaded": p.downloaded,
                            "total": p.total,
                            "phase": p.phase,
                        }),
                    );
                };
                if let Err(e) = ndm::components::download_component(&spec, &dir, &cb) {
                    pass = Err(e);
                    break;
                }
            }
            match pass {
                Ok(()) => break Ok(()),
                Err(e) => {
                    attempt += 1;
                    if attempt >= 20 {
                        break Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_secs(90));
                }
            }
        };
        let state = app.state::<AppState>();
        state.components_busy.store(false, Ordering::SeqCst);
        let payload = match result {
            Ok(()) => serde_json::json!({ "phase": "done" }),
            Err(e) => serde_json::json!({ "phase": "error", "message": e }),
        };
        let _ = app.emit("components_progress", payload);
    });
}

#[tauri::command]
fn ensure_components(app: AppHandle) {
    run_component_setup(app);
}

#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

/// Save the current list as JSON in the user's Downloads folder —
/// the counterpart of the Import menu item.
#[tauri::command]
fn export_downloads(state: State<'_, AppState>) -> Result<String, String> {
    let items = list_downloads(state.clone());
    let home = std::env::var("HOME").map_err(|_| "no home folder".to_string())?;
    let dir = PathBuf::from(home).join("Downloads");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("ndm-downloads-export.json");
    let body = serde_json::to_string_pretty(&items).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

/// Fetch the release manifest and compare it with the running
/// version. An available update is remembered for install_update;
/// falling below min_supported blocks new downloads until the
/// update is installed.
/// Minimal POSIX single-quote escaping for the one place a path is
/// handed to a shell (the pkexec update wrapper).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn do_check_update(app: &AppHandle) -> serde_json::Value {
    let state = app.state::<AppState>();
    let info = match ndm::updater::fetch_release() {
        Ok(info) => info,
        Err(e) => return serde_json::json!({ "status": "error", "message": e }),
    };
    let current = env!("CARGO_PKG_VERSION");
    if ndm::updater::version_cmp(&info.version, current) != std::cmp::Ordering::Greater {
        return serde_json::json!({ "status": "current", "version": current });
    }
    let mandatory = !info.min_supported.is_empty()
        && ndm::updater::version_cmp(current, &info.min_supported)
            == std::cmp::Ordering::Less;
    if mandatory {
        state.update_required.store(true, Ordering::SeqCst);
    }
    let payload = serde_json::json!({
        "status": "available",
        "version": info.version,
        "notes": info.notes,
        "mandatory": mandatory,
    });
    *state.pending_update.lock().unwrap() = Some(info);
    let _ = app.emit("update_available", payload.clone());
    payload
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<serde_json::Value, String> {
    let joined =
        tauri::async_runtime::spawn_blocking(move || do_check_update(&app)).await;
    joined.map_err(|e| e.to_string())
}

/// One-click update: download the new package, verify its hash,
/// hand it to the system installer with admin rights, and let the
/// UI offer a restart. Falls back to opening the package in the
/// desktop's software installer when pkexec/apt is unavailable.
#[tauri::command]
fn install_update(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let info = state
        .pending_update
        .lock()
        .unwrap()
        .clone()
        .ok_or("no update is waiting to install")?;
    if info.deb_url.is_empty() {
        return Err("this update has no installable package".to_string());
    }
    if state.update_busy.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("updates");
    std::thread::spawn(move || {
        let dest = dir.join("ndm-update.deb");
        let app2 = app.clone();
        let cb = move |done: u64, total: u64| {
            let _ = app2.emit(
                "update_progress",
                serde_json::json!({ "phase": "downloading", "downloaded": done, "total": total }),
            );
        };
        let outcome = ndm::updater::download_file(&info.deb_url, &dest, &info.deb_sha256, &cb)
            .and_then(|_| {
                let _ = app.emit(
                    "update_progress",
                    serde_json::json!({ "phase": "installing" }),
                );
                let script = format!(
                    "DEBIAN_FRONTEND=noninteractive apt-get install -y {}",
                    shell_quote(&dest.to_string_lossy())
                );
                let status = std::process::Command::new("pkexec")
                    .args(["sh", "-c", &script])
                    .status();
                match status {
                    Ok(st) if st.success() => Ok("installed"),
                    _ => {
                        // PackageKit route (GNOME Software's backend)
                        let pk = std::process::Command::new("pkcon")
                            .args(["install-local", "-y"])
                            .arg(&dest)
                            .status();
                        if let Ok(st) = pk {
                            if st.success() {
                                return Ok("installed");
                            }
                        }
                        // last resort: the desktop's own installer UI
                        let opened = std::process::Command::new("xdg-open")
                            .arg(&dest)
                            .status();
                        match opened {
                            Ok(st) if st.success() => Ok("opened"),
                            _ => Err("the system installer could not be started".to_string()),
                        }
                    }
                }
            });
        {
            let state = app.state::<AppState>();
            state.update_busy.store(false, Ordering::SeqCst);
        }
        let payload = match outcome {
            Ok(phase) => serde_json::json!({ "phase": phase }),
            Err(e) => serde_json::json!({ "phase": "error", "message": e }),
        };
        let _ = app.emit("update_progress", payload);
    });
    Ok(())
}

#[tauri::command]
fn take_pending_files(state: State<'_, AppState>) -> Vec<serde_json::Value> {
    std::mem::take(&mut state.pending_files.lock().unwrap())
}

#[tauri::command]
fn take_pending_videos(state: State<'_, AppState>) -> Vec<serde_json::Value> {
    std::mem::take(&mut state.pending_videos.lock().unwrap())
}

/// The user closed the video dialog without starting the download:
/// the captured browser session for that link must not linger on disk.
#[tauri::command]
fn discard_cookies(state: State<'_, AppState>, token: String) {
    if let Some(p) = state.cookies_path(&token) {
        let _ = std::fs::remove_file(p);
    }
}

#[tauri::command]
/// "Refresh download address" for an expired link: point the entry at a
/// fresh URL for the same file. The engine state file is keyed to the old
/// URL, so it is removed and the download restarts cleanly rather than
/// trusting stale state; the entry keeps its name, folder and place in
/// the list.
fn refresh_url(app: AppHandle, state: State<'_, AppState>, id: String, new_url: String) -> Result<(), String> {
    let new_url = new_url.trim().to_string();
    if !(new_url.starts_with("http://") || new_url.starts_with("https://")) {
        return Err("the new address must be an http(s) URL".to_string());
    }
    let entry = state.get(&id).ok_or("unknown download id")?;
    let status = payload_of(&entry).status;
    if status == "downloading" {
        return Err("pause the download before refreshing its address".to_string());
    }
    if status == "finished" {
        return Err("this download has already finished".to_string());
    }
    if entry.kind == "video" {
        // a video entry stores the page address, and yt-dlp resolves a
        // fresh media address from it on every start — there is no
        // expiring link to refresh (and the HTTP state-file cleanup
        // below would be meaningless for it)
        return Err("video addresses are re-resolved on every start; nothing to refresh".to_string());
    }
    if entry.starting.load(Ordering::SeqCst) {
        // replacing the entry under an in-flight start would leave the
        // start's own completion/error handling pointing at this fresh
        // entry — refuse until the start settles
        return Err("the download is starting — try again in a moment".to_string());
    }
    if let Some(dl) = &entry.download {
        dl.control.cancel();
    }
    if let Some(job) = &entry.video {
        job.cancel();
    }
    entry.removed.store(true, Ordering::SeqCst); // stop a stray emit loop
    let total = payload_of(&entry).total;
    let _ = std::fs::remove_file(ndm::state_path_for(&entry.out_path));
    let fresh = Arc::new(Entry {
        id: entry.id.clone(),
        name: entry.name.clone(),
        url: new_url,
        out_path: entry.out_path.clone(),
        category: entry.category.clone(),
        added: entry.added.clone(),
        download: None,
        video: None,
        kind: entry.kind.clone(),
        selector: entry.selector.clone(),
        cookies: entry.cookies.clone(),
        referer: entry.referer.clone(),
        audio: entry.audio,
        expect_audio: entry.expect_audio,
        restored: Mutex::new(Some(Restored {
            total,
            done_bytes: 0,
            blocks: Vec::new(),
            resumable: true,
            status: "paused".to_string(),
        })),
        queued: AtomicBool::new(false),
        sample: Mutex::new((Instant::now(), 0, 0.0)),
        emitting: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        removed: AtomicBool::new(false),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(entry.completion.lock().unwrap().clone()),
    });
    state
        .entries
        .lock()
        .unwrap()
        .insert(fresh.id.clone(), fresh.clone());
    let _ = app.emit("progress", payload_of(&fresh));
    persist(&state);
    Ok(())
}

#[tauri::command]
fn list_downloads(state: State<'_, AppState>) -> Vec<ProgressPayload> {
    let order = state.order.lock().unwrap().clone();
    order
        .iter()
        .filter_map(|id| state.get(id))
        .map(|e| payload_of(&e))
        .collect()
}

/// Ten-minute probe cache: re-clicking a video (the pill, the
/// dialog's Fetch button) must not pay the multi-second yt-dlp
/// extraction again for an answer nothing could have changed.
fn probe_cache_get(state: &AppState, url: &str) -> Option<video::VideoInfo> {
    let cache = state.probe_cache.lock().unwrap();
    let (at, info) = cache.get(url)?;
    (at.elapsed() < std::time::Duration::from_secs(600)).then(|| info.clone())
}

fn probe_cache_put(state: &AppState, url: &str, info: &video::VideoInfo) {
    state
        .probe_cache
        .lock()
        .unwrap()
        .insert(url.to_string(), (Instant::now(), info.clone()));
}

#[tauri::command]
async fn probe_video(
    state: State<'_, AppState>,
    url: String,
    cookies: Option<String>,
) -> Result<video::VideoInfo, String> {
    if let Some(info) = probe_cache_get(&state, &url) {
        return Ok(info);
    }
    let components = state.components();
    let cookies = state.cookies_path(cookies.as_deref().unwrap_or(""));
    let url2 = url.clone();
    let info = tauri::async_runtime::spawn_blocking(move || {
        video::probe(&url2, &components, cookies.as_deref())
    })
    .await
    .map_err(|e| format!("internal join error: {e}"))??;
    probe_cache_put(&state, &url, &info);
    Ok(info)
}

#[tauri::command]
// a Tauri command's parameters ARE its IPC surface (url, folder,
// format choice, captured-session token) — eight named inputs is the
// honest shape here, not a code smell
#[allow(clippy::too_many_arguments)]
async fn add_video(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    dir: String,
    selector: String,
    audio: bool,
    has_audio: bool,
    title: String,
    cookies: Option<String>,
) -> Result<String, String> {
    let dir = resolve_dir(&dir);
    // the finish path's promise: a video from a page WITH sound must
    // not "finish" as a silent file (audio-only jobs are exempt)
    let expect_audio = has_audio && !audio;
    // a cookies token is honoured only while its captured file exists;
    // anything else (typed, stale, forged) means "no cookies"
    let cookies_token = cookies
        .filter(|t| state.cookies_path(t).is_some())
        .unwrap_or_default();
    start_video_download(
        app,
        state,
        url,
        dir,
        selector,
        audio,
        expect_audio,
        title,
        cookies_token,
    )
}

/// Create the list entry and (slot permitting) the live job for one
/// video download. Shared by the add_video command and the browser
/// panel's background start (queue "start" entries): the two paths
/// differ only in where their inputs come from.
#[allow(clippy::too_many_arguments)]
fn start_video_download(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    dir: PathBuf,
    selector: String,
    audio: bool,
    expect_audio: bool,
    title: String,
    cookies_token: String,
) -> Result<String, String> {
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create folder: {e}"))?;
    // two entries for the same video in the same folder would run two
    // yt-dlp processes writing the same .part files (and a finished
    // dupe would fake-complete off the existing file) — refuse it
    {
        let entries = state.entries.lock().unwrap();
        let dupe = entries.values().any(|e| {
            e.kind == "video"
                && e.url == url
                && e.out_path == dir
                && payload_of(e).status != "error"
        });
        if dupe {
            return Err("this video is already in the download list".to_string());
        }
    }
    let name = if title.trim().is_empty() {
        file_name_for(&url)
    } else {
        title.trim().to_string()
    };
    let id = format!("dl-{}", state.seq.fetch_add(1, Ordering::Relaxed) + 1);
    // videos honour the same simultaneous cap as files: with no free slot
    // the entry waits in the queue and the pump starts its job later
    let max = current_settings(&state).max_simultaneous;
    if max > 0 && count_active(&state) >= max {
        let entry = Arc::new(Entry {
            id: id.clone(),
            name,
            url,
            out_path: dir,
            category: "Video".to_string(),
            added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            download: None,
            video: None,
            kind: "video".to_string(),
            selector,
            cookies: cookies_token.clone(),
            referer: String::new(),
            audio,
            expect_audio,
            restored: Mutex::new(Some(Restored {
                total: 0,
                done_bytes: 0,
                blocks: Vec::new(),
                resumable: true,
                status: "queued".to_string(),
            })),
            queued: AtomicBool::new(true),
            sample: Mutex::new((Instant::now(), 0, 0.0)),
            emitting: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            completion: Mutex::new(String::new()),
        });
        state
            .entries
            .lock()
            .unwrap()
            .insert(id.clone(), entry.clone());
        state.order.lock().unwrap().push(id.clone());
        let _ = app.emit("progress", payload_of(&entry));
        persist(&state);
        return Ok(id);
    }
    let job = video::VideoJob::start(
        &url,
        &dir,
        &selector,
        audio,
        expect_audio,
        state.cookies_path(&cookies_token),
        &state.components(),
    )?;
    let entry = Arc::new(Entry {
        id: id.clone(),
        name,
        url,
        out_path: dir,
        category: "Video".to_string(),
        added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        download: None,
        video: Some(job),
        kind: "video".to_string(),
        selector,
        cookies: cookies_token.clone(),
        referer: String::new(),
        audio,
        expect_audio,
        restored: Mutex::new(None),
        queued: AtomicBool::new(false),
        sample: Mutex::new((Instant::now(), 0, 0.0)),
        emitting: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        removed: AtomicBool::new(false),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(String::new()),
    });
    state
        .entries
        .lock()
        .unwrap()
        .insert(id.clone(), entry.clone());
    state.order.lock().unwrap().push(id.clone());
    spawn_emit_loop(app, entry);
    persist(&state);
    Ok(id)
}

#[derive(Clone, Serialize)]
struct GrabItemDto {
    url: String,
    name: String,
    kind: String,
    size: Option<u64>,
    label: String,
}

#[derive(Clone, Serialize)]
struct GrabPageDto {
    title: String,
    items: Vec<GrabItemDto>,
}

#[tauri::command]
async fn grab_page(url: String) -> Result<GrabPageDto, String> {
    let page = tauri::async_runtime::spawn_blocking(move || ndm::grab_page(&url))
        .await
        .map_err(|e| format!("internal join error: {e}"))??;
    Ok(GrabPageDto {
        title: page.title,
        items: page
            .items
            .into_iter()
            .map(|it| GrabItemDto {
                kind: category_for(&it.name).to_string(),
                url: it.url,
                name: it.name,
                size: it.size,
                label: it.label,
            })
            .collect(),
    })
}

/// Rebuild the in-memory list from downloads.json + engine state files.
fn restore_saved(state: &AppState) {
    let path = state.data_file.lock().unwrap().clone();
    let raw = match std::fs::read(&path) {
        Ok(r) => r,
        Err(_) => return,
    };
    // parse entry by entry: one malformed record (a legacy field, a
    // hand-edited value) must not silently drop the ENTIRE list — the
    // periodic snapshot would then overwrite the file with the empty
    // list and the user's history would be gone for good
    let raw_entries: Vec<serde_json::Value> = match serde_json::from_slice(&raw) {
        Ok(s) => s,
        Err(_) => return,
    };
    let saved: Vec<SavedEntry> = raw_entries
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .filter(|s: &SavedEntry| !s.id.is_empty())
        .collect();
    let mut max_seq = 0u64;
    for s in saved {
        if let Some(n) = s.id.strip_prefix("dl-").and_then(|x| x.parse::<u64>().ok()) {
            max_seq = max_seq.max(n);
        }
        if state.entries.lock().unwrap().contains_key(&s.id) {
            // a duplicated id in the file must not render the row twice
            continue;
        }
        let out_path = PathBuf::from(&s.out_path);
        if s.kind == "video" {
            let finished = s.status == "finished";
            let done = if finished { s.total } else { s.done_bytes.min(s.total) };
            let n = 128usize;
            let done_cells = if s.total > 0 {
                ((done as u128 * n as u128) / s.total as u128) as usize
            } else {
                0
            };
            let mut blocks = vec![0u8; n];
            for b in blocks.iter_mut().take(done_cells.min(n)) {
                *b = 1;
            }
            let entry = Arc::new(Entry {
                id: s.id.clone(),
                name: s.name.clone(),
                url: s.url.clone(),
                out_path,
                category: s.category.clone(),
                added: s.added.clone(),
                download: None,
                video: None,
                kind: "video".to_string(),
                selector: s.selector.clone(),
                cookies: s.cookies.clone(),
                referer: s.referer.clone(),
                audio: s.audio,
                expect_audio: s.expect_audio,
                restored: Mutex::new(Some(Restored {
                    total: s.total,
                    done_bytes: done,
                    blocks,
                    resumable: true,
                    status: if finished {
                        "finished".to_string()
                    } else if s.status == "error" {
                        "error".to_string()
                    } else if s.status == "queued" {
                        "queued".to_string()
                    } else {
                        "paused".to_string()
                    },
                })),
                queued: AtomicBool::new(s.status == "queued" && !finished),
                sample: Mutex::new((Instant::now(), 0, 0.0)),
                emitting: AtomicBool::new(false),
                starting: AtomicBool::new(false),
                removed: AtomicBool::new(false),
                pause_requested: AtomicBool::new(false),
                completion: Mutex::new(s.completion.clone()),
            });
            state.entries.lock().unwrap().insert(s.id.clone(), entry);
            state.order.lock().unwrap().push(s.id.clone());
            continue;
        }
        let s_queued = s.status == "queued";
        // A download that completed just before a hard kill may have been
        // snapshotted as "paused"; if we recorded all bytes and the file on
        // disk is full-size, it really is finished.
        let file_complete = s.total > 0
            && s.done_bytes >= s.total
            && out_path.metadata().map(|m| m.len()).unwrap_or(0) == s.total;
        let restored = if s.status == "finished" || file_complete {
            // capped: the vector is display data (downsampled to <=1024
            // cells for the UI), and an unchecked total from a corrupt
            // file must never turn into a giant allocation at startup
            let nblocks = (s.total as usize)
                .div_ceil(ndm::DEFAULT_BLOCK as usize)
                .clamp(1, 4096);
            Restored {
                total: s.total,
                done_bytes: s.total,
                blocks: vec![1u8; nblocks],
                resumable: true,
                status: "finished".to_string(),
            }
        } else {
            match ndm::peek_state(&s.url, &out_path) {
                Some(p) => Restored {
                    total: p.total,
                    done_bytes: p.done_bytes,
                    blocks: p.blocks,
                    resumable: true,
                    status: if s.status == "error" {
                        "error".to_string()
                    } else if s_queued {
                        "queued".to_string()
                    } else {
                        "paused".to_string()
                    },
                },
                None => Restored {
                    total: s.total,
                    done_bytes: s.done_bytes.min(s.total),
                    blocks: Vec::new(),
                    resumable: false,
                    status: if s.status == "error" {
                        "error".to_string()
                    } else if s_queued {
                        "queued".to_string()
                    } else {
                        "paused".to_string()
                    },
                },
            }
        };
        let entry = Arc::new(Entry {
            id: s.id.clone(),
            name: s.name.clone(),
            url: s.url.clone(),
            out_path,
            category: s.category.clone(),
            added: s.added.clone(),
            download: None,
            video: None,
            kind: "http".to_string(),
            selector: String::new(),
            // the session belongs to the download, not the process:
            // a login-gated file must still resume after a restart
            cookies: s.cookies.clone(),
            referer: s.referer.clone(),
            audio: false,
            expect_audio: false,
            restored: Mutex::new(Some(restored)),
            queued: AtomicBool::new(s_queued),
            sample: Mutex::new((Instant::now(), 0, 0.0)),
            emitting: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            completion: Mutex::new(s.completion.clone()),
        });
        state.entries.lock().unwrap().insert(s.id.clone(), entry);
        state.order.lock().unwrap().push(s.id.clone());
    }
    state.seq.store(max_seq + 1, Ordering::Relaxed);
}

/// Ack ids arrive from the host through the queue file: keep only
/// filename-safe characters before one becomes a file name.
fn sanitize_ack_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect()
}

/// Tell the waiting host "the app has this one": an `acks/<id>.ack`
/// marker beside the queue file; the host deletes it as it reads it.
/// Without the marker the browser must not claim the hand-off
/// actually reached the app.
fn write_ack(queue_dir: &Path, id: &str) {
    if id.is_empty() {
        return;
    }
    let dir = queue_dir.join("acks");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(format!("{id}.ack")), b"");
    }
}

/// The host relays this file's content back to the browser panel as
/// its reply (probe results, start verdicts): written via a temp
/// name + rename so the host can never read a half-written result.
fn write_result(queue_dir: &Path, id: &str, body: &serde_json::Value) {
    if id.is_empty() {
        return;
    }
    let dir = queue_dir.join("results");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join(format!("{id}.tmp"));
    if std::fs::write(&tmp, body.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &dir.join(format!("{id}.json")));
    }
}

/// Markers and results whose host timed out (the app was down) would
/// otherwise pile up forever; drop any older than an hour.
fn prune_acks(queue_dir: &Path) {
    let now = std::time::SystemTime::now();
    for sub in ["acks", "results"] {
        let Ok(rd) = std::fs::read_dir(queue_dir.join(sub)) else {
            continue;
        };
        for e in rd.flatten() {
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| {
                    now.duration_since(t)
                        .map(|d| d.as_secs() > 3600)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Process the browser extension's incoming queue file. A leftover
/// `incoming.processing` from a crashed run is drained first, so queued
/// links are never silently lost.
fn drain_incoming(handle: &AppHandle, queue_dir: &Path) {
    prune_acks(queue_dir);
    let queue = queue_dir.join("incoming.jsonl");
    let processing = queue_dir.join("incoming.processing");
    if !processing.exists() {
        if !queue.exists() {
            return;
        }
        if std::fs::rename(&queue, &processing).is_err() {
            return;
        }
    }
    let Ok(text) = std::fs::read_to_string(&processing) else {
        return;
    };
    let lines: Vec<&str> = text.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        handle_incoming_line(handle, queue_dir, line);
        // drop the processed line from the file before moving on: a
        // crash mid-drain then re-delivers only unprocessed lines
        // (at-least-once) instead of losing the rest of the queue
        let remaining = &lines[idx + 1..];
        if remaining.is_empty() {
            let _ = std::fs::remove_file(&processing);
        } else {
            let _ = std::fs::write(&processing, remaining.join("\n") + "\n");
        }
    }
}

/// One queued extension link: a file goes straight into the list (with
/// the user's settings applied); a video is handed to the frontend.
fn handle_incoming_line(handle: &AppHandle, queue_dir: &Path, line: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    // the host holds its reply until this entry's ack appears: write
    // it only once the entry is truly surfaced below, never for a
    // line that gets dropped here
    let ack_id = v["id"].as_str().map(sanitize_ack_id).unwrap_or_default();
    let kind = v["kind"].as_str().unwrap_or("file");
    let url = v["url"].as_str().unwrap_or("");
    if url.is_empty() {
        return;
    }
    // The extension may attach the browser's own cookies for this
    // download (Netscape format), captured at click time — that is what
    // lets login-gated files and videos (private Facebook posts,
    // members-only file hosts, …) download with no setup at all.
    // Persist them under an app-generated token; the frontend hands the
    // token back when the user starts the download, never the values.
    let mut token = String::new();
    if let Some(text) = v["cookies"].as_str().filter(|t| !t.is_empty() && t.len() <= 600_000) {
        let state = handle.state::<AppState>();
        let dir = state.cookies_dir();
        let candidate = format!("ck{}", state.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let file = dir.join(format!("{candidate}.txt"));
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&file, text).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
            }
            token = candidate;
        }
    }
    if kind == "probe" || kind == "start" {
        // the in-page panel's requests: answered through a result
        // file the host relays back into the page — the window is
        // never raised and the dialog never opens for these
        handle_panel_line(handle, queue_dir, &v, &ack_id, &token);
        return;
    }
    if kind == "video" {
        // IDM raises its download dialog over the browser; our dialog
        // lives in the main window, so bring that window forward first
        // — a dialog opening inside a background window is invisible.
        if let Some(w) = handle.get_webview_window("main") {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
        }
        let payload = serde_json::json!({
            "url": url,
            "title": v["title"].as_str().unwrap_or(""),
            "cookies": token,
        });
        // the webview may not be listening yet at startup: also keep the
        // link in a pending list the frontend pulls once it is ready
        let state = handle.state::<AppState>();
        state.pending_videos.lock().unwrap().push(payload.clone());
        let _ = handle.emit("incoming_video", payload);
        write_ack(queue_dir, &ack_id);
        return;
    }
    // a caught file download is presented exactly like IDM's: the
    // app comes forward with its download dialog prefilled (address,
    // the browser's suggested file name, the captured session), and
    // the user presses Start — nothing downloads behind their back
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    let payload = serde_json::json!({
        "url": url,
        "title": v["title"].as_str().unwrap_or(""),
        "pageUrl": v["pageUrl"].as_str().unwrap_or(""),
        "cookies": token,
    });
    let state = handle.state::<AppState>();
    state.pending_files.lock().unwrap().push(payload.clone());
    let _ = handle.emit("incoming_file", payload);
    write_ack(queue_dir, &ack_id);
}

/// One browser-panel request. "probe" answers with the page's
/// quality list; "start" begins the chosen download in the
/// background (default folder, the user's simultaneous cap and
/// dedupe rules all apply exactly as for the dialog). Both write
/// their answer to results/<id>.json BEFORE the ack, so when the
/// host sees the ack the result is already complete on disk.
fn handle_panel_line(
    handle: &AppHandle,
    queue_dir: &Path,
    v: &serde_json::Value,
    ack_id: &str,
    token: &str,
) {
    let state = handle.state::<AppState>();
    let kind = v["kind"].as_str().unwrap_or("");
    let url = v["url"].as_str().unwrap_or("");
    if kind == "probe" {
        let cookies = state.cookies_path(token);
        let info = match probe_cache_get(&state, url) {
            Some(info) => Ok(info),
            None => {
                let r = video::probe(url, &state.components(), cookies.as_deref());
                if let Ok(info) = &r {
                    probe_cache_put(&state, url, info);
                }
                r
            }
        };
        let body = match info {
            Ok(info) => {
                // a live entry for this page is reported, not
                // re-probed into a dead end: the panel shows
                // "already in NDM" instead of options the app
                // would refuse at start time (errored entries do
                // not block a fresh attempt, so they don't count)
                let entries = state.entries.lock().unwrap();
                let already = entries
                    .values()
                    .find(|e| {
                        e.kind == "video" && e.url == url && payload_of(e).status != "error"
                    })
                    .map(|e| payload_of(e).status);
                serde_json::json!({
                    "title": info.title,
                    "duration": info.duration,
                    "has_audio": info.has_audio,
                    "already": already,
                    "options": serde_json::to_value(&info.options).unwrap_or_default(),
                })
            }
            Err(e) => serde_json::json!({ "error": e }),
        };
        write_result(queue_dir, ack_id, &body);
        write_ack(queue_dir, ack_id);
        return;
    }
    // kind == "start"
    let selector = v["selector"].as_str().unwrap_or("").to_string();
    let body = if selector.is_empty() {
        serde_json::json!({ "started": false, "reason": "no quality was selected" })
    } else {
        let audio = v["audio"].as_bool().unwrap_or(false);
        let expect_audio = v["hasAudio"].as_bool().unwrap_or(false) && !audio;
        let title = v["title"].as_str().unwrap_or("").to_string();
        let dir = resolve_dir(&current_settings(&state).default_dir);
        match start_video_download(
            handle.clone(),
            state,
            url.to_string(),
            dir,
            selector,
            audio,
            expect_audio,
            title,
            token.to_string(),
        ) {
            Ok(_) => serde_json::json!({ "started": true }),
            Err(e) => serde_json::json!({ "started": false, "reason": e }),
        }
    };
    write_result(queue_dir, ack_id, &body);
    write_ack(queue_dir, ack_id);
}

/// The installed ndm-host binary, resolved from this app's own path:
/// next to the app in a dev build, ../lib/ndm in the .deb layout.
fn native_host_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for cand in [dir.join("ndm-host"), dir.join("../lib/ndm/ndm-host")] {
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn write_if_changed(path: &Path, body: &str) {
    if std::fs::read_to_string(path).map(|cur| cur == body).unwrap_or(false) {
        return;
    }
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(path, body);
}

/// Connect the browser extension with zero terminal steps: on every
/// start, (re)write the native-messaging host manifests for each
/// browser config present, using the extension's fixed ID (pinned by
/// the public key in its manifest). Idempotent; all errors ignored —
/// the ndm-setup-browser command remains as a manual fallback.
fn register_native_hosts() {
    let Some(host) = native_host_path() else { return };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".config"));
    let host = host.to_string_lossy();
    // built with serde_json, not string interpolation: a path
    // containing a quote or backslash must not corrupt the manifest
    let chrome = serde_json::json!({
        "name": "com.ndm.host",
        "description": "NDM — Nill Download Manager native messaging host",
        "path": host,
        "type": "stdio",
        "allowed_origins": ["chrome-extension://njjmngpfmblaghapeenfdfkahhdocbic/"],
    })
    .to_string();
    for cfg in [
        "google-chrome", "google-chrome-beta", "chromium", "chromium-browser",
        "BraveSoftware/Brave-Browser", "microsoft-edge", "vivaldi", "opera",
    ] {
        let base = config.join(cfg);
        if base.is_dir() {
            write_if_changed(
                &base.join("NativeMessagingHosts").join("com.ndm.host.json"),
                &chrome,
            );
        }
    }
    let moz = home.join(".mozilla");
    if moz.is_dir() {
        let firefox = serde_json::json!({
            "name": "com.ndm.host",
            "description": "NDM — Nill Download Manager native messaging host",
            "path": host,
            "type": "stdio",
            "allowed_extensions": ["ndm@nill.download"],
        })
        .to_string();
        write_if_changed(
            &moz.join("native-messaging-hosts").join("com.ndm.host.json"),
            &firefox,
        );
    }
}

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            entries: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            seq: AtomicU64::new(1),
            data_file: Mutex::new(PathBuf::new()),
            components_dir: Mutex::new(PathBuf::new()),
            cookies_dir: Mutex::new(PathBuf::new()),
            save_lock: Mutex::new(()),
            settings: Mutex::new(Settings::default()),
            settings_file: Mutex::new(PathBuf::new()),
            sched_override: AtomicBool::new(false),
            pending_videos: Mutex::new(Vec::new()),
            probe_cache: Mutex::new(HashMap::new()),
            pending_files: Mutex::new(Vec::new()),
            components_busy: AtomicBool::new(false),
            pending_update: Mutex::new(None),
            update_required: AtomicBool::new(false),
            update_busy: AtomicBool::new(false),
        })
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let components = dir.join("components");
            std::fs::create_dir_all(&components)?;
            register_native_hosts();
            let file = dir.join("downloads.json");
            let settings_path = dir.join("settings.json");
            {
                let state = app.state::<AppState>();
                *state.data_file.lock().unwrap() = file;
                *state.components_dir.lock().unwrap() = components;
                let cookies_dir = dir.join("cookies");
                let _ = std::fs::create_dir_all(&cookies_dir);
                *state.cookies_dir.lock().unwrap() = cookies_dir;
                *state.settings_file.lock().unwrap() = settings_path;
                load_settings(&state);
                restore_saved(&state);
            }
            // silent update check + the one-time anonymous install
            // count. Nothing is shown unless an update exists.
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(12));
                    let st = handle.state::<AppState>();
                    if current_settings(&st).auto_update {
                        let _ = do_check_update(&handle);
                    }
                    if !current_settings(&st).install_counted {
                        if ndm::updater::count_hit("installs-15") {
                            let mut settings = st.settings.lock().unwrap();
                            settings.install_counted = true;
                            drop(settings);
                            save_settings(&st);
                        }
                    }
                });
            }
            // slim-install warm-up: if the video components were
            // never fetched, run the one-time setup quietly in the
            // background so the first video click feels instant
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    let st = handle.state::<AppState>();
                    if !missing_components(&st).is_empty() {
                        run_component_setup(handle.clone());
                    }
                });
            }
            // optional auto-resume: unfinished entries start by themselves
            {
                let state = app.state::<AppState>();
                let s = current_settings(&state);
                if s.auto_resume {
                    let order = state.order.lock().unwrap().clone();
                    let pending: Vec<Arc<Entry>> = order
                        .iter()
                        .filter_map(|id| state.get(id))
                        .filter(|e| {
                            e.download.is_none()
                                && matches!(
                                    payload_of(e).status.as_str(),
                                    "paused" | "queued"
                                )
                        })
                        .collect();
                    if scheduler_blocks(&state) {
                        // the scheduler owns the queue right now: leave the
                        // entries queued so the window start picks them up
                        for e in &pending {
                            e.queued.store(true, Ordering::Relaxed);
                        }
                        persist(&state);
                    } else if s.max_simultaneous > 0 {
                        for e in &pending {
                            e.queued.store(true, Ordering::Relaxed);
                        }
                        pump_queue(app.handle());
                    } else {
                        let handle = app.handle().clone();
                        for e in pending {
                            start_entry(&handle, e);
                        }
                    }
                }
            }
            // scheduler: every 30 s, start the queue inside the configured
            // window and pause everything when the window ends
            let handle4 = app.handle().clone();
            std::thread::spawn(move || {
                let mut was_in_window = false;
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    let state = handle4.state::<AppState>();
                    let s = current_settings(&state);
                    if !s.scheduler_enabled {
                        was_in_window = false;
                        continue;
                    }
                    let now = chrono::Local::now().format("%H:%M").to_string();
                    let in_win = in_window(&s, &now);
                    if in_win && !was_in_window {
                        // the window governs again; any manual override ends
                        state.sched_override.store(false, Ordering::SeqCst);
                        if s.max_simultaneous == 0 {
                            start_all_queued(&handle4);
                        } else {
                            pump_queue(&handle4);
                        }
                    } else if !in_win && was_in_window {
                        state.sched_override.store(false, Ordering::SeqCst);
                        pause_all(&handle4, true);
                    }
                    was_in_window = in_win;
                }
            });
            // periodic snapshot so even a hard power cut keeps a fresh list
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(5));
                let state = handle.state::<AppState>();
                persist(&state);
            });
            // incoming queue from the browser extension (via ndm-host):
            // video links open the video dialog in the UI; file links are
            // added straight to the list
            let handle2 = app.handle().clone();
            let queue_dir = dir.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(2));
                drain_incoming(&handle2, &queue_dir);
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add_download,
            pause_download,
            resume_download,
            remove_download,
            list_downloads,
            probe_video,
            add_video,
            grab_page,
            get_settings,
            set_settings,
            scheduler_start_now,
            scheduler_stop_now,
            open_path,
            set_completion,
            refresh_url,
            take_pending_videos,
            take_pending_files,
            components_status,
            ensure_components,
            app_version,
            quit_app,
            restart_app,
            export_downloads,
            check_update,
            install_update,
            discard_cookies
        ])
        .run(tauri::generate_context!())
        .expect("error while running NDM");
}
