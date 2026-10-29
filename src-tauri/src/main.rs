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
    /// the named queue this entry belongs to
    queue: String,
    /// cumulative bytes per connection this session (IDM's
    /// per-connection progress table); empty for video jobs
    conns_detail: Vec<u64>,
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
    /// why the entry last failed — shown in the list and the status
    /// dialog, and kept across restarts so "Error" is never a dead end
    #[serde(default)]
    error: String,
    /// the named queue this entry belongs to (older saves → Main)
    #[serde(default = "main_queue_name")]
    queue: String,
    /// "Use authorization" login for this download (IDM parity);
    /// the password is stored with the entry exactly like IDM
    /// stores its per-site logins
    #[serde(default)]
    login: String,
    #[serde(default)]
    password: String,
}

fn default_kind() -> String {
    "http".to_string()
}

fn default_category() -> String {
    "General".to_string()
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
    /// IDM-style category routing: category name → its own default
    /// folder. Applies when a download is headed for the ordinary
    /// default folder; an explicitly chosen folder always wins.
    #[serde(default)]
    category_dirs: std::collections::HashMap<String, String>,
    /// IDM-style per-server connection exceptions, one per line as
    /// "host-or-*.domain = max-connections" (see conns_for_url).
    #[serde(default)]
    conn_exceptions: Vec<String>,
    /// exit the app when the queue drains (IDM's per-queue
    /// "Exit when done", as a global switch)
    #[serde(default)]
    exit_when_done: bool,
    /// play the system sound on download complete / failure
    /// (IDM's Sounds options)
    #[serde(default = "def_true")]
    sound_events: bool,
    /// show the "Download complete" dialog (IDM shows it by default)
    #[serde(default = "def_true")]
    show_complete_dialog: bool,
    /// named download queues (IDM's Scheduler); sanitize() guarantees
    /// at least the Main queue exists
    #[serde(default)]
    queues: Vec<QueueCfg>,
    /// IDM's Download limits: stop starting (and pause) downloads
    /// once this many MB were downloaded inside the window;
    /// 0 = no limit
    #[serde(default)]
    quota_mb: u64,
    /// the quota window length in hours (IDM's default is 5)
    #[serde(default = "def_quota_hours")]
    quota_hours: u64,
    /// watch the clipboard for download links and offer them in the
    /// Add URL dialog (IDM's clipboard monitoring)
    #[serde(default = "def_true")]
    clipboard_monitor: bool,
}

fn def_quota_hours() -> u64 {
    5
}

fn def_true() -> bool {
    true
}

fn def_dir() -> String {
    "~/Downloads".to_string()
}
fn def_conns() -> usize {
    // IDM's default maximum connection number
    8
}

fn main_queue_name() -> String {
    // IDM's built-in queue name
    "Main download queue".to_string()
}

/// One named download queue (IDM's Scheduler): entries belong to a
/// queue, the simultaneous cap applies per queue, and a queue can
/// carry its own daily window and exit-when-done action.
#[derive(Clone, Serialize, Deserialize)]
struct QueueCfg {
    name: String,
    #[serde(default)]
    use_schedule: bool,
    #[serde(default = "def_sched_start")]
    start: String,
    #[serde(default = "def_sched_stop")]
    stop: String,
    #[serde(default)]
    exit_when_done: bool,
}

impl Default for QueueCfg {
    fn default() -> Self {
        QueueCfg {
            name: main_queue_name(),
            use_schedule: false,
            start: def_sched_start(),
            stop: def_sched_stop(),
            exit_when_done: false,
        }
    }
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
            category_dirs: std::collections::HashMap::new(),
            conn_exceptions: Vec::new(),
            exit_when_done: false,
            sound_events: true,
            show_complete_dialog: true,
            queues: vec![QueueCfg::default()],
            quota_mb: 0,
            quota_hours: def_quota_hours(),
            clipboard_monitor: true,
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
        self.category_dirs = self
            .category_dirs
            .drain()
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, v)| !k.is_empty() && !v.is_empty())
            .collect();
        self.conn_exceptions = self
            .conn_exceptions
            .iter()
            .filter_map(|line| parse_conn_exception(line).map(|_| line.trim().to_string()))
            .collect();
        // queues: guarantee the Main queue exists, drop blank or
        // duplicate names, normalize every queue's window; a settings
        // file from before named queues existed seeds Main from the
        // legacy global scheduler fields
        if self.queues.is_empty() {
            self.queues.push(QueueCfg {
                name: main_queue_name(),
                use_schedule: self.scheduler_enabled,
                start: self.sched_start.clone(),
                stop: self.sched_stop.clone(),
                exit_when_done: false,
            });
        }
        let mut seen = std::collections::HashSet::new();
        self.queues.retain(|q| {
            let name = q.name.trim();
            !name.is_empty() && seen.insert(name.to_lowercase())
        });
        if !self.queues.iter().any(|q| q.name == main_queue_name()) {
            self.queues.insert(0, QueueCfg::default());
        }
        for q in &mut self.queues {
            q.name = q.name.trim().to_string();
            q.start = normalize_hhmm(&q.start, def_sched_start());
            q.stop = normalize_hhmm(&q.stop, def_sched_stop());
        }
        // keep the legacy global fields mirroring Main, so any reader
        // that still consults them stays consistent
        if let Some(main) = self.queues.iter().find(|q| q.name == main_queue_name()) {
            self.scheduler_enabled = main.use_schedule;
            self.sched_start = main.start.clone();
            self.sched_stop = main.stop.clone();
        }
        if self.quota_hours == 0 {
            self.quota_hours = def_quota_hours();
        }
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
fn window_open(start: &str, stop: &str, now: &str) -> bool {
    if start == stop {
        // equal times mean "all day", not "never": an always-empty
        // window would silently disable the scheduler
        return true;
    }
    if start <= stop {
        now >= start && now < stop
    } else {
        // overnight window, e.g. 22:00 -> 06:00
        now >= start || now < stop
    }
}

fn in_window(s: &Settings, now: &str) -> bool {
    window_open(&s.sched_start, &s.sched_stop, now)
}

/// The config of one queue by name (the Main queue's config when the
/// name is unknown — an entry must never fall out of the system).
fn queue_cfg(s: &Settings, name: &str) -> QueueCfg {
    s.queues
        .iter()
        .find(|q| q.name == name)
        .cloned()
        .unwrap_or_default()
}

/// The queue an entry currently belongs to.
fn entry_queue(e: &Entry) -> String {
    e.queue.lock().unwrap().clone()
}

/// Base64 (standard alphabet) for the Basic authorization header —
/// small enough not to earn a dependency.
fn base64_encode(data: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0] as u32,
            *chunk.get(1).unwrap_or(&0) as u32,
            *chunk.get(2).unwrap_or(&0) as u32,
        ];
        let n = (b[0] << 16) | (b[1] << 8) | b[2];
        out.push(ALPHA[(n >> 18) as usize & 63] as char);
        out.push(ALPHA[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Does this clipboard text look like a download IDM would offer to
/// take? A single http(s) URL whose file type maps to a real
/// category, or a known video-page URL.
fn clipboard_url_qualifies(text: &str) -> bool {
    if text.is_empty() || text.len() > 2048 || text.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    if !(text.starts_with("http://") || text.starts_with("https://")) {
        return false;
    }
    if category_for(&file_name_for(text)) != "General" {
        return true;
    }
    const VIDEO_HINTS: [&str; 8] = [
        "youtube.com/watch",
        "youtu.be/",
        "vimeo.com/",
        "dailymotion.com/video",
        "tiktok.com/",
        "instagram.com/reel",
        "facebook.com/watch",
        "twitch.tv/videos",
    ];
    VIDEO_HINTS.iter().any(|h| text.contains(h))
}

/// The Authorization header for an entry's saved login, if any
/// (IDM's "Use authorization" on the Add URL dialog).
fn basic_auth_header(login: &str, password: &str) -> Option<String> {
    if login.is_empty() {
        return None;
    }
    Some(format!(
        "Basic {}",
        base64_encode(format!("{login}:{password}").as_bytes())
    ))
}

fn current_settings(state: &AppState) -> Settings {
    state.settings.lock().unwrap().clone()
}

/// Parse one per-server exception line: "host-or-*.domain = N".
fn parse_conn_exception(line: &str) -> Option<(String, usize)> {
    let (pat, n) = line.split_once('=')?;
    let pat = pat.trim().trim_start_matches("http://").trim_start_matches("https://");
    let pat = pat.split('/').next().unwrap_or("").trim().to_lowercase();
    let n: usize = n.trim().parse().ok()?;
    if pat.is_empty() || !(1..=64).contains(&n) {
        return None;
    }
    Some((pat, n))
}

fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host == suffix || host.ends_with(&format!(".{suffix}"))
    } else {
        host == pattern || host.ends_with(&format!(".{pattern}"))
    }
}

/// The connection cap for one URL: the global setting, lowered by a
/// matching per-server exception when one exists (IDM's Connection
/// tab Exceptions table — some servers throttle or ban parallel
/// connections, and one number never fit every host).
fn conns_for_url(s: &Settings, url: &str) -> usize {
    let mut cap = s.max_connections;
    if s.conn_exceptions.is_empty() {
        return cap;
    }
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    if host.is_empty() {
        return cap;
    }
    for line in &s.conn_exceptions {
        if let Some((pat, n)) = parse_conn_exception(line) {
            if host_pattern_matches(&pat, &host) {
                cap = cap.min(n);
            }
        }
    }
    cap.max(1)
}

/// IDM categories are routing rules, not just labels: when a download
/// is headed for the ordinary default folder (the global setting, or
/// ~/Downloads as the extension sends it) and its category carries
/// its own folder, the category folder wins. A folder the user picked
/// explicitly for this download is never overridden.
fn route_category_dir(state: &AppState, category: &str, dir: std::path::PathBuf) -> std::path::PathBuf {
    let s = current_settings(state);
    let Some(target) = s
        .category_dirs
        .get(category)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    else {
        return dir;
    };
    if dir == resolve_dir(&s.default_dir) || dir == resolve_dir("~/Downloads") {
        resolve_dir(target)
    } else {
        dir
    }
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
    error: String,
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
    /// the named queue this entry belongs to (movable between
    /// queues from the status dialog, like IDM)
    queue: Mutex<String>,
    /// "Use authorization" credentials for this download ("" = none)
    login: String,
    password: String,
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
    /// "Start Queue Now" overrides, per queue: lets the pump run that
    /// queue outside its window until the window next starts/ends or
    /// "Stop Queue Now" clears it
    sched_override: Mutex<std::collections::HashSet<String>>,
    /// download-quota bookkeeping: (done-bytes baseline at the window
    /// start, window start epoch seconds, warning already shown)
    quota_state: Mutex<(u64, i64, bool)>,
    /// where quota.json lives (next to downloads.json)
    quota_file: Mutex<PathBuf>,
    /// video links from the extension that arrived before the webview
    /// was listening; the frontend drains this once on load
    pending_videos: Mutex<Vec<serde_json::Value>>,
    pending_files: Mutex<Vec<serde_json::Value>>,
    /// a first-run video-component setup is currently downloading
    components_busy: AtomicBool,
    /// IDM-queue-style automatic retries of failed downloads:
    /// entry id → (attempts so far, when the next attempt is due)
    retries: Mutex<HashMap<String, (u32, Instant)>>,
    /// downloads have actually run in this session (gates the
    /// exit-when-done action so an idle launch never self-closes)
    had_active: AtomicBool,
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
    // ONE canonical category table for the whole product — the host
    // (ndm-host.rs) and the extension (content.js categoryForName)
    // carry the identical sets, and the catch-all is "General",
    // exactly like IDM's category of last resort. (Older saved lists
    // may still carry the retired fallback name; restore maps it.)
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "tgz" | "tbz2" | "txz" => {
            "Compressed"
        }
        "exe" | "msi" | "deb" | "rpm" | "dmg" | "pkg" | "appimage" | "apk" | "bin" | "iso"
        | "img" | "flatpakref" => "Programs",
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" | "ts" => "Video",
        "mp3" | "m4a" | "flac" | "wav" | "ogg" | "opus" | "aac" | "wma" => "Music",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "csv"
        | "epub" | "mobi" => "Documents",
        _ => "General",
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
        let (hint_total, hint_done) = entry
            .restored
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| (r.total, r.done_bytes))
            .unwrap_or((0, 0));
        let total = if p.total > 0 { p.total } else { hint_total };
        let done_bytes = if p.done > 0 {
            p.done
        } else if p.status == "downloading" && hint_done > 0 {
            if total > 0 {
                hint_done.min(total)
            } else {
                hint_done
            }
        } else {
            p.done
        };
        let display_name = p
            .final_path
            .as_ref()
            .and_then(|fp| fp.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| entry.name.clone());
        return ProgressPayload {
            id: entry.id.clone(),
            name: display_name,
            url: entry.url.clone(),
            total,
            done_bytes,
            speed_bps: p.speed_bps,
            conns: if p.status == "downloading" {
                if p.conns > 0 {
                    p.conns
                } else {
                    ndm::global_connections().clamp(1, 32)
                }
            } else {
                0
            },
            blocks: if p.status == "finished" {
                "1".repeat(128)
            } else {
                synth_blocks(total, done_bytes, p.status == "downloading")
            },
            status: p.status.clone(),
            category: entry.category.clone(),
            added: entry.added.clone(),
            resume_capability: "Yes (yt-dlp resume)".to_string(),
            completion: entry.completion.lock().unwrap().clone(),
            error: p.error.clone().unwrap_or_default(),
            queue: entry_queue(entry),
            conns_detail: Vec::new(),
        };
    }
    let (total, done_bytes, rate, conns, status, blocks, resumable, error_text, conns_detail) =
        match &entry.download {
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
                p.conn_bytes,
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
                r.error.clone(),
                Vec::new(),
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
                Vec::new(),
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
        queue: entry_queue(entry),
        conns_detail,
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
                status: if e.queued.load(Ordering::Relaxed) {
                    "queued".to_string()
                } else {
                    p.status
                },
                kind: e.kind.clone(),
                selector: e.selector.clone(),
                audio: e.audio,
                cookies: e.cookies.clone(),
                referer: e.referer.clone(),
                completion: e.completion.lock().unwrap().clone(),
                error: p.error.clone(),
                queue: entry_queue(&e),
                login: e.login.clone(),
                password: e.password.clone(),
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

fn resolve_video_file(entry: &Entry) -> PathBuf {
    if let Some(job) = &entry.video {
        if let Some(fp) = job
            .progress
            .lock()
            .unwrap()
            .final_path
            .clone()
            .filter(|p| p.is_file())
        {
            return fp;
        }
    }
    if entry.out_path.is_file() {
        return entry.out_path.clone();
    }
    let want_ext = if entry.audio { "mp3" } else { "mp4" };
    // Check direct filename match first (e.g. after app restart when entry.name holds the file or stem)
    if !entry.name.is_empty() && !entry.name.contains(['/', '\\']) {
        let direct = entry.out_path.join(&entry.name);
        if direct.is_file() {
            return direct;
        }
    }
    let stem = video::safe_filename_stem(&entry.name);
    if !stem.is_empty() {
        for ext in [want_ext, "mkv", "webm", "m4a"] {
            let cand = entry.out_path.join(format!("{stem}.{ext}"));
            if cand.is_file() {
                return cand;
            }
        }
    }
    let total = payload_of(entry).total;
    if let Ok(rd) = std::fs::read_dir(&entry.out_path) {
        let mut by_size: Option<PathBuf> = None;
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let ext = p
                .extension()
                .map(|x| x.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if ext != want_ext && ext != "mkv" && ext != "webm" && ext != "m4a" {
                continue;
            }
            if total > 0 && e.metadata().map(|m| m.len()).unwrap_or(0) == total {
                by_size = Some(p);
                break;
            }
        }
        if let Some(p) = by_size {
            return p;
        }
    }
    entry.out_path.clone()
}

/// IDM's Sounds options, lite: play the desktop sound theme's event
/// sound (via canberra or paplay) when a player exists; where none
/// does, this is a silent no-op — never an error.
fn play_event_sound(finished: bool) {
    let (id, file) = if finished {
        ("complete", "complete.oga")
    } else {
        ("dialog-error", "dialog-error.oga")
    };
    std::thread::spawn(move || {
        let played = std::process::Command::new("canberra-gtk-play")
            .args(["-i", id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !played {
            let path = format!("/usr/share/sounds/freedesktop/stereo/{file}");
            if std::path::Path::new(&path).exists() {
                let _ = std::process::Command::new("paplay")
                    .arg(&path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
    });
}

/// Errors that retrying cannot fix (IDM retries blindly; sparing the
/// user five minutes of doomed retries on a dead link is the one
/// deliberate deviation, and it only skips the hopeless cases).
fn error_is_permanent(error: &str) -> bool {
    error.contains("Unsupported URL")
        || error.contains("HTTP Error 404")
        || error.contains("HTTP Error 410")
        || error.contains("no playable video stream")
        || error.contains("required update")
}

/// Register a failed entry for an automatic retry — IDM's queue
/// semantics: a failed file is retried (its Scheduler default is 10
/// attempts) instead of sitting dead until the user notices.
fn register_retry(state: &AppState, entry: &Entry) {
    if error_is_permanent(&payload_of(entry).error) {
        return;
    }
    let mut map = state.retries.lock().unwrap();
    let (count, _) = map.get(&entry.id).cloned().unwrap_or((0, Instant::now()));
    if count < 10 {
        map.insert(
            entry.id.clone(),
            (count + 1, Instant::now() + Duration::from_secs(30)),
        );
    }
}

/// Total bytes recorded as downloaded across all entries right now.
fn total_done_bytes(state: &AppState) -> u64 {
    state
        .entries
        .lock()
        .unwrap()
        .values()
        .map(|e| payload_of(e).done_bytes)
        .sum()
}

fn save_quota(state: &AppState) {
    let path = state.quota_file.lock().unwrap().clone();
    if path.as_os_str().is_empty() {
        return;
    }
    let q = state.quota_state.lock().unwrap();
    let _ = std::fs::write(
        &path,
        serde_json::json!({ "baseline": q.0, "start": q.1 }).to_string(),
    );
}

fn load_quota(state: &AppState) {
    let path = state.quota_file.lock().unwrap().clone();
    if path.as_os_str().is_empty() {
        return;
    }
    if let Ok(raw) = std::fs::read(&path) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) {
            let baseline = v["baseline"].as_u64().unwrap_or(0);
            let start = v["start"].as_i64().unwrap_or(0);
            if start > 0 {
                *state.quota_state.lock().unwrap() = (baseline, start, false);
            }
        }
    }
}

/// IDM's Download limits: has this window's byte budget run out?
/// The window state (baseline + start) persists across restarts, so
/// the budget is honest; an expired window resets itself.
fn quota_blocks(state: &AppState) -> bool {
    let s = current_settings(state);
    if s.quota_mb == 0 {
        return false;
    }
    let now = chrono::Utc::now().timestamp();
    let expired = {
        let q = state.quota_state.lock().unwrap();
        q.1 == 0 || now - q.1 >= (s.quota_hours as i64) * 3600
    };
    if expired {
        *state.quota_state.lock().unwrap() = (total_done_bytes(state), now, false);
        save_quota(state);
        return false;
    }
    let baseline = state.quota_state.lock().unwrap().0;
    total_done_bytes(state).saturating_sub(baseline) >= s.quota_mb * 1024 * 1024
}

/// IDM's "Exit when done": when it is on (globally, or on the queue
/// the finished entry belonged to), downloads ran this session, and
/// nothing is active or queued anymore, the app closes itself.
fn maybe_exit_when_done(app: &AppHandle, state: &AppState, finished_queue: &str) {
    let s = current_settings(state);
    if !s.exit_when_done && !queue_cfg(&s, finished_queue).exit_when_done {
        return;
    }
    if !state.had_active.load(Ordering::Relaxed) {
        return;
    }
    if count_active(state) > 0 {
        return;
    }
    let any_queued = state
        .entries
        .lock()
        .unwrap()
        .values()
        .any(|e| e.queued.load(Ordering::Relaxed));
    if !any_queued {
        app.exit(0);
    }
}

/// The per-download "Options on completion" action, run once at finish.
fn run_completion_action(entry: &Entry) {
    let action = entry.completion.lock().unwrap().clone();
    let target = match action.as_str() {
        "open_file" => {
            if entry.kind == "video" {
                resolve_video_file(entry)
            } else {
                entry.out_path.clone()
            }
        }
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
        let mut last_persist = Instant::now();
        loop {
            if entry.removed.load(Ordering::Relaxed) {
                break;
            }
            update_sample(&entry);
            let payload = payload_of(&entry);
            let status = payload.status.clone();
            let total_now = payload.total;
            let _ = app.emit("progress", payload);
            if status == "downloading" {
                app.state::<AppState>()
                    .had_active
                    .store(true, Ordering::Relaxed);
            }
            match status.as_str() {
                "finished" => {
                    run_completion_action(&entry);
                    let state = app.state::<AppState>();
                    state.retries.lock().unwrap().remove(&entry.id);
                    if current_settings(&state).sound_events {
                        play_event_sound(true);
                    }
                    if current_settings(&state).show_complete_dialog {
                        let done_path = if entry.kind == "video" {
                            resolve_video_file(&entry)
                        } else {
                            entry.out_path.clone()
                        };
                        let _ = app.emit(
                            "download_complete",
                            serde_json::json!({
                                "id": entry.id,
                                "name": entry.name,
                                "path": done_path.to_string_lossy(),
                                "total": total_now,
                            }),
                        );
                    }
                    persist(&state);
                    pump_queue(&app);
                    maybe_exit_when_done(&app, &state, &entry_queue(&entry));
                    break;
                }
                "error" | "cancelled" => {
                    let state = app.state::<AppState>();
                    if status == "error" {
                        if current_settings(&state).sound_events {
                            play_event_sound(false);
                        }
                        register_retry(&state, &entry);
                    } else {
                        state.retries.lock().unwrap().remove(&entry.id);
                    }
                    persist(&state);
                    pump_queue(&app);
                    maybe_exit_when_done(&app, &state, &entry_queue(&entry));
                    break;
                }
                "paused" | "queued" => {
                    let state = app.state::<AppState>();
                    persist(&state);
                    pump_queue(&app);
                    break;
                }
                _ => {
                    // keep the on-disk snapshot fresh while bytes are
                    // moving, not only at the end: a crash or kill
                    // mid-download should restore close to the truth
                    if last_persist.elapsed() >= Duration::from_secs(2) {
                        let state = app.state::<AppState>();
                        persist(&state);
                        last_persist = Instant::now();
                    }
                }
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
        restored: Mutex::new(None),
        queued: AtomicBool::new(false),
        sample: Mutex::new((Instant::now(), done0, 0.0)),
        emitting: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        removed: AtomicBool::new(stub.removed.load(Ordering::Relaxed)),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(stub.completion.lock().unwrap().clone()),
        queue: Mutex::new(entry_queue(&stub)),
        login: stub.login.clone(),
        password: stub.password.clone(),
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
        conns_for_url(&current_settings(&state), &url)
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
        let authorization = basic_auth_header(&stub.login, &stub.password);
        ndm::Auth {
            cookie,
            referer,
            authorization,
        }
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

/// An entry for the same URL that is still alive (not finished,
/// errored or cancelled). IDM asks what to do when a duplicate link
/// is added ("download again" vs keeping the existing one); callers
/// use this to ask — or, on the silent browser path, to point at the
/// entry that is already there instead of starting a second copy.
fn find_active_duplicate(state: &AppState, kind: &str, url: &str) -> Option<String> {
    let entries = state.entries.lock().unwrap();
    entries
        .values()
        .find(|e| {
            e.kind == kind
                && e.url == url
                && !matches!(
                    payload_of(e).status.as_str(),
                    "error" | "finished" | "cancelled"
                )
        })
        .map(|e| e.id.clone())
}

fn register_http_stub(
    state: &AppState,
    url: &str,
    dir: &str,
    category: Option<&str>,
    name_hint: Option<&str>,
    cookies_token: &str,
    referer: &str,
    queue: &str,
    login: &str,
    password: &str,
) -> Result<Arc<Entry>, String> {
    // a name the browser suggested (Content-Disposition) wins over the
    // URL tail, stripped of any path parts a hostile page could smuggle
    let base_name = name_hint
        .map(|n| n.rsplit(['/', '\\']).next().unwrap_or(n).trim().to_string())
        .filter(|n| !n.is_empty() && n != "." && n != "..")
        .unwrap_or_else(|| file_name_for(url));
    let category = category
        .map(|c| c.to_string())
        .unwrap_or_else(|| category_for(&base_name).to_string());
    // category routing (IDM-style): the category's own folder applies
    // when the caller asked for the ordinary default folder
    let dir = route_category_dir(state, &category, resolve_dir(dir));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create folder: {e}"))?;
    let (name, out_path) = unique_out_path(state, &dir, &base_name);
    let id = format!("dl-{}", state.seq.fetch_add(1, Ordering::Relaxed) + 1);
    let stub = Arc::new(Entry {
        id: id.clone(),
        name: name.clone(),
        url: url.to_string(),
        out_path,
        category,
        added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        download: None,
        video: None,
        kind: "http".to_string(),
        selector: String::new(),
        cookies: cookies_token.to_string(),
        referer: referer.to_string(),
        audio: false,
        restored: Mutex::new(None),
        queued: AtomicBool::new(false),
        sample: Mutex::new((Instant::now(), 0, 0.0)),
        emitting: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        removed: AtomicBool::new(false),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(String::new()),
        queue: Mutex::new(queue.to_string()),
        login: login.to_string(),
        password: password.to_string(),
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
    if entry.kind == "video"
        && video::find_tool("yt-dlp", &app.state::<AppState>().components()).is_none()
    {
        // The video engine itself is still downloading in the
        // background. Dying here with an error helps nobody — wait in
        // the queue instead; the setup worker pumps the queue the
        // moment the engine lands.
        entry.queued.store(true, Ordering::Relaxed);
        {
            let mut r = entry.restored.lock().unwrap();
            if r.is_none() {
                *r = Some(Restored {
                    total: 0,
                    done_bytes: 0,
                    blocks: Vec::new(),
                    resumable: false,
                    status: "queued".to_string(),
                    error: String::new(),
                });
            }
        }
        let state = app.state::<AppState>();
        let _ = app.emit("progress", payload_of(&entry));
        persist(&state);
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
            &entry.name,
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
            let prev_restored = entry.restored.lock().unwrap().clone();
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
                restored: Mutex::new(prev_restored),
                queued: AtomicBool::new(false),
                sample: Mutex::new((Instant::now(), 0, 0.0)),
                emitting: AtomicBool::new(false),
                starting: AtomicBool::new(false),
                removed: AtomicBool::new(entry.removed.load(Ordering::Relaxed)),
                pause_requested: AtomicBool::new(false),
                completion: Mutex::new(entry.completion.lock().unwrap().clone()),
                queue: Mutex::new(entry_queue(&entry)),
                login: entry.login.clone(),
                password: entry.password.clone(),
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
            Err(e) => {
                entry.starting.store(false, Ordering::SeqCst);
                // mirror the HTTP path: a failed start must surface as a
                // visible error, not sit silently on a stale status —
                // with the reason attached, so the user can act on it
                *entry.restored.lock().unwrap() = Some(Restored {
                    total: 0,
                    done_bytes: 0,
                    blocks: Vec::new(),
                    resumable: false,
                    status: "error".to_string(),
                    error: e,
                });
                let _ = app.emit("progress", payload_of(&entry));
                persist(&state);
            }
        }
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(start_err) = start_engine(app.clone(), &entry).await {
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
                        error: start_err,
                    });
                    let _ = app.emit("progress", payload_of(&e));
                }
            }
            persist(&state);
        }
    });
}

/// The pump must not start a queue's downloads outside that queue's
/// scheduler window — unless the user pressed "Start Queue Now" for
/// it (the override lasts until the window next starts or ends, or
/// "Stop Queue Now" clears it). Windows are per queue (IDM's
/// Scheduler); a queue without a schedule never blocks.
fn scheduler_blocks(state: &AppState, queue: &str) -> bool {
    let s = current_settings(state);
    let cfg = queue_cfg(&s, queue);
    if !cfg.use_schedule {
        return false;
    }
    if state.sched_override.lock().unwrap().contains(queue) {
        return false;
    }
    let now = chrono::Local::now().format("%H:%M").to_string();
    !window_open(&cfg.start, &cfg.stop, &now)
}

/// Should a new/queued download in this queue wait right now?
/// (Its queue's window or the download quota says so.)
fn entry_blocks(state: &AppState, queue: &str) -> bool {
    scheduler_blocks(state, queue) || quota_blocks(state)
}

/// How many downloads of one queue are running or starting.
fn count_active_for(state: &AppState, queue: &str) -> usize {
    let order = state.order.lock().unwrap().clone();
    order
        .iter()
        .filter_map(|id| state.get(id))
        .filter(|e| {
            entry_queue(&e) == queue
                && (e.starting.load(Ordering::Relaxed) || payload_of(e).status == "downloading")
        })
        .count()
}

/// Fill free simultaneous-download slots from the queues. The cap is
/// per queue (IDM: each queue downloads N files at a time).
fn pump_queue(app: &AppHandle) {
    let state = app.state::<AppState>();
    let s = current_settings(&state);
    let max = s.max_simultaneous;
    let order = state.order.lock().unwrap().clone();
    for cfg in &s.queues {
        if entry_blocks(&state, &cfg.name) {
            continue;
        }
        if max == 0 {
            for id in &order {
                if let Some(e) = state.get(id) {
                    if entry_queue(&e) == cfg.name && e.queued.swap(false, Ordering::SeqCst) {
                        start_entry(app, e);
                    }
                }
            }
            continue;
        }
        let mut active = count_active_for(&state, &cfg.name);
        if active >= max {
            continue;
        }
        for id in &order {
            if active >= max {
                break;
            }
            let Some(e) = state.get(id) else { continue };
            if entry_queue(&e) != cfg.name {
                continue;
            }
            // atomic claim: two pumps running at once must never start
            // the same entry twice (two engines, one output file)
            if e.queued.swap(false, Ordering::SeqCst) {
                start_entry(app, e);
                active += 1;
            }
        }
    }
}

/// Start every queued entry of one queue (or of all queues when
/// `queue` is None), regardless of the simultaneous cap.
fn start_all_queued(app: &AppHandle, queue: Option<&str>) {
    let state = app.state::<AppState>();
    let order = state.order.lock().unwrap().clone();
    for id in order {
        if let Some(e) = state.get(&id) {
            if queue.is_some_and(|q| entry_queue(&e) != q) {
                continue;
            }
            if e.queued.swap(false, Ordering::SeqCst) {
                start_entry(app, e);
            }
        }
    }
}

/// Pause the currently-downloading entries of one queue (or of all
/// queues when `queue` is None). With `requeue` (a queue's own
/// window-end pause), entries that were actually downloading are
/// handed back to the queue so the next window start claims them
/// again — without it they sat paused forever while merely-queued
/// entries around them started. A manual Stop Queue Now passes
/// false: the user's stop is not the window's.
fn pause_all(app: &AppHandle, requeue: bool, queue: Option<&str>) {
    let state = app.state::<AppState>();
    let order = state.order.lock().unwrap().clone();
    for id in order {
        if let Some(e) = state.get(&id) {
            if queue.is_some_and(|q| entry_queue(&e) != q) {
                continue;
            }
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
    *state.settings.lock().unwrap() = s.clone();
    save_settings(&state);
    // an entry whose queue was deleted falls back to the Main queue
    {
        let names: Vec<String> = s.queues.iter().map(|q| q.name.clone()).collect();
        let entries: Vec<Arc<Entry>> = state.entries.lock().unwrap().values().cloned().collect();
        let mut moved = false;
        for e in entries {
            if !names.contains(&entry_queue(&e)) {
                *e.queue.lock().unwrap() = main_queue_name();
                moved = true;
            }
        }
        if moved {
            persist(&state);
        }
    }
    // the pump itself respects every queue's window, quota and cap
    // (including "no cap"), so saving settings just gives it a chance
    // to fill whatever slots the new settings freed
    pump_queue(&app);
    Ok(())
}

/// Move an entry to another queue (IDM: right-click → Add to queue).
#[tauri::command]
fn set_entry_queue(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    queue: String,
) -> Result<(), String> {
    let s = current_settings(&state);
    if !s.queues.iter().any(|q| q.name == queue) {
        return Err("unknown queue".to_string());
    }
    let entry = state.get(&id).ok_or("unknown download id")?;
    *entry.queue.lock().unwrap() = queue;
    persist(&state);
    let _ = app.emit("progress", payload_of(&entry));
    pump_queue(&app);
    Ok(())
}

#[tauri::command]
fn scheduler_start_now(app: AppHandle, state: State<'_, AppState>, queue: Option<String>) {
    let q = queue.unwrap_or_else(main_queue_name);
    state.sched_override.lock().unwrap().insert(q.clone());
    if current_settings(&state).max_simultaneous == 0 {
        start_all_queued(&app, Some(&q));
    } else {
        pump_queue(&app);
    }
}

#[tauri::command]
fn scheduler_stop_now(app: AppHandle, state: State<'_, AppState>, queue: Option<String>) {
    let q = queue.unwrap_or_else(main_queue_name);
    state.sched_override.lock().unwrap().remove(&q);
    pause_all(&app, false, Some(&q));
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
    force: Option<bool>,
    queue: Option<String>,
    login: Option<String>,
    password: Option<String>,
) -> Result<String, String> {
    if state.update_required.load(Ordering::SeqCst) {
        return Err("NDM needs its latest update before it can download. Please install the update first.".to_string());
    }
    if !force.unwrap_or(false) {
        if let Some(existing) = find_active_duplicate(&state, "http", &url) {
            return Err(format!("duplicate:{existing}"));
        }
    }
    let stub = register_http_stub(
        &state,
        &url,
        &dir,
        category.as_deref(),
        name.as_deref(),
        cookies.as_deref().unwrap_or(""),
        referer.as_deref().unwrap_or(""),
        queue.as_deref().unwrap_or(&main_queue_name()),
        login.as_deref().unwrap_or(""),
        password.as_deref().unwrap_or(""),
    )?;
    let id = stub.id.clone();
    let is_later = later.unwrap_or(false);
    let max = current_settings(&state).max_simultaneous;
    let must_wait = is_later
        || entry_blocks(&state, &entry_queue(&stub))
        || (max > 0 && count_active_for(&state, &entry_queue(&stub)) >= max);
    if must_wait {
        if is_later {
            // "Download Later" stays paused until the user explicitly starts/resumes it
            stub.queued.store(false, Ordering::Relaxed);
            *stub.restored.lock().unwrap() = Some(Restored {
                total: 0,
                done_bytes: 0,
                blocks: Vec::new(),
                resumable: true,
                status: "paused".to_string(),
                error: String::new(),
            });
        } else {
            // Scheduler or simultaneous cap: register as queued so the pump starts it when a slot frees up
            stub.queued.store(true, Ordering::Relaxed);
        }
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
                        error: String::new(),
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
    pump_queue(&app);
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
                &entry.name,
                state.cookies_path(&entry.cookies),
                &state.components(),
            ) {
                Ok(j) => j,
                Err(e) => {
                    entry.starting.store(false, Ordering::SeqCst);
                    return Err(e);
                }
            };
            let prev_restored = entry.restored.lock().unwrap().clone();
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
                restored: Mutex::new(prev_restored),
                queued: AtomicBool::new(false),
                sample: Mutex::new((Instant::now(), 0, 0.0)),
                emitting: AtomicBool::new(false),
                starting: AtomicBool::new(false),
                removed: AtomicBool::new(entry.removed.load(Ordering::Relaxed)),
                pause_requested: AtomicBool::new(false),
                completion: Mutex::new(entry.completion.lock().unwrap().clone()),
                queue: Mutex::new(entry_queue(&entry)),
                login: entry.login.clone(),
                password: entry.password.clone(),
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
    state.retries.lock().unwrap().remove(&id);
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
        if folder {
            entry.out_path.clone()
        } else {
            resolve_video_file(&entry)
        }
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
        if result.is_ok() {
            // video downloads that arrived while the engine was still
            // downloading are waiting in the queue — start them now
            pump_queue(&app);
        }
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
        restored: Mutex::new(Some(Restored {
            total,
            done_bytes: 0,
            blocks: Vec::new(),
            resumable: true,
            status: "paused".to_string(),
            error: String::new(),
        })),
        queued: AtomicBool::new(false),
        sample: Mutex::new((Instant::now(), 0, 0.0)),
        emitting: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        removed: AtomicBool::new(false),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(entry.completion.lock().unwrap().clone()),
        queue: Mutex::new(entry_queue(&entry)),
        login: entry.login.clone(),
        password: entry.password.clone(),
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

#[tauri::command]
async fn probe_file(
    state: State<'_, AppState>,
    url: String,
    cookies: Option<String>,
    referer: Option<String>,
    login: Option<String>,
    password: Option<String>,
) -> Result<serde_json::Value, String> {
    let url_trimmed = url.trim().to_string();
    if !(url_trimmed.starts_with("http://") || url_trimmed.starts_with("https://")) {
        return Err("URL must start with http:// or https://".to_string());
    }
    let cookie = cookies
        .as_deref()
        .and_then(|t| state.cookies_path(t))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| cookie_header_from_netscape(&text));
    let referer = referer.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
    let authorization = basic_auth_header(
        login.as_deref().unwrap_or(""),
        password.as_deref().unwrap_or(""),
    );
    let auth = ndm::Auth {
        cookie,
        referer,
        authorization,
    };
    let u2 = url_trimmed.clone();
    let probe = tauri::async_runtime::spawn_blocking(move || ndm::probe_with_auth(&u2, &auth))
        .await
        .map_err(|e| format!("internal join error: {e}"))??;
    let suggested = probe
        .suggested_name
        .unwrap_or_else(|| file_name_for(&url_trimmed));
    let category = category_for(&suggested).to_string();
    Ok(serde_json::json!({
        "total": probe.total,
        "resumable": probe.range_ok,
        "unknown_size": probe.unknown_size,
        "suggested_name": suggested,
        "category": category,
    }))
}

#[tauri::command]
async fn probe_video(
    state: State<'_, AppState>,
    url: String,
    cookies: Option<String>,
) -> Result<video::VideoInfo, String> {
    let components = state.components();
    let cookies = state.cookies_path(cookies.as_deref().unwrap_or(""));
    tauri::async_runtime::spawn_blocking(move || {
        video::probe(&url, &components, cookies.as_deref())
    })
        .await
        .map_err(|e| format!("internal join error: {e}"))?
}

fn unique_video_name(state: &AppState, dir: &Path, raw_title: &str) -> String {
    let base_stem = video::safe_filename_stem(raw_title);
    if base_stem.is_empty() {
        return raw_title.trim().to_string();
    }
    let taken = |cand: &str| -> bool {
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
        state.entries.lock().unwrap().values().any(|e| {
            e.kind == "video"
                && e.out_path == dir
                && !e.removed.load(Ordering::Relaxed)
                && (e.name == cand || video::safe_filename_stem(&e.name) == cand)
        })
    };
    if !taken(&base_stem) {
        return base_stem;
    }
    for n in 1..10_000 {
        let cand = format!("{base_stem} ({n})");
        if !taken(&cand) {
            return cand;
        }
    }
    base_stem
}

#[allow(clippy::too_many_arguments)]
fn do_add_video(
    app: &AppHandle,
    state: &AppState,
    url: String,
    dir: String,
    selector: String,
    audio: bool,
    title: String,
    cookies: Option<String>,
    later: bool,
    total_hint: u64,
    force: bool,
    queue: String,
) -> Result<String, String> {
    if state.update_required.load(Ordering::SeqCst) {
        return Err("please install the required update before starting new downloads".to_string());
    }
    // category routing (IDM-style) happens BEFORE the duplicate
    // check, so a re-add compares against the folder the first add
    // was actually routed to
    let dir = route_category_dir(
        state,
        if audio { "Music" } else { "Video" },
        resolve_dir(&dir),
    );
    let cookies_token = cookies
        .filter(|t| state.cookies_path(t).is_some())
        .unwrap_or_default();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create folder: {e}"))?;
    if !force {
        let entries = state.entries.lock().unwrap();
        let dupe = entries.values().find(|e| {
            e.kind == "video"
                && e.url == url
                && e.out_path == dir
                && e.selector == selector
                && e.audio == audio
                && !matches!(
                    payload_of(e).status.as_str(),
                    "error" | "finished" | "cancelled"
                )
        });
        if let Some(existing) = dupe {
            // IDM asks instead of silently refusing or duplicating:
            // the caller decides (dialog confirm / "Download again")
            // and retries with force to get a second copy.
            return Err(format!("duplicate:{}", existing.id));
        }
    }
    let raw_name = if title.trim().is_empty() {
        file_name_for(&url)
    } else {
        title.trim().to_string()
    };
    let name = unique_video_name(state, &dir, &raw_name);
    let category = if audio {
        "Music".to_string()
    } else {
        "Video".to_string()
    };
    let id = format!("dl-{}", state.seq.fetch_add(1, Ordering::Relaxed) + 1);
    let max = current_settings(state).max_simultaneous;
    // A video that arrives while the engine itself is still
    // downloading waits in the queue like any other not-now download —
    // it must not be rejected for a condition the user cannot fix.
    let engine_missing = video::find_tool("yt-dlp", &state.components()).is_none();
    let should_wait = later
        || engine_missing
        || entry_blocks(state, &queue)
        || (max > 0 && count_active_for(state, &queue) >= max);
    if should_wait {
        let initial_status = if later { "paused" } else { "queued" };
        let entry = Arc::new(Entry {
            id: id.clone(),
            name,
            url,
            out_path: dir,
            category,
            added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            download: None,
            video: None,
            kind: "video".to_string(),
            selector,
            cookies: cookies_token,
            referer: String::new(),
            audio,
            restored: Mutex::new(Some(Restored {
                total: total_hint,
                done_bytes: 0,
                blocks: Vec::new(),
                resumable: true,
                status: initial_status.to_string(),
                error: String::new(),
            })),
            queued: AtomicBool::new(!later),
            sample: Mutex::new((Instant::now(), 0, 0.0)),
            emitting: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            completion: Mutex::new(String::new()),
            queue: Mutex::new(queue.clone()),
            login: String::new(),
            password: String::new(),
        });
        state
            .entries
            .lock()
            .unwrap()
            .insert(id.clone(), entry.clone());
        state.order.lock().unwrap().push(id.clone());
        let _ = app.emit("progress", payload_of(&entry));
        persist(state);
        return Ok(id);
    }
    let job = video::VideoJob::start(
        &url,
        &dir,
        &selector,
        audio,
        &name,
        state.cookies_path(&cookies_token),
        &state.components(),
    )?;
    let initial_restored = if total_hint > 0 {
        Some(Restored {
            total: total_hint,
            done_bytes: 0,
            blocks: Vec::new(),
            resumable: true,
            status: "downloading".to_string(),
            error: String::new(),
        })
    } else {
        None
    };
    let entry = Arc::new(Entry {
        id: id.clone(),
        name,
        url,
        out_path: dir,
        category,
        added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        download: None,
        video: Some(job),
        kind: "video".to_string(),
        selector,
        cookies: cookies_token,
        referer: String::new(),
        audio,
        restored: Mutex::new(initial_restored),
        queued: AtomicBool::new(false),
        sample: Mutex::new((Instant::now(), 0, 0.0)),
        emitting: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        removed: AtomicBool::new(false),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(String::new()),
        queue: Mutex::new(queue.clone()),
        login: String::new(),
        password: String::new(),
    });
    state
        .entries
        .lock()
        .unwrap()
        .insert(id.clone(), entry.clone());
    state.order.lock().unwrap().push(id.clone());
    spawn_emit_loop(app.clone(), entry);
    persist(state);
    Ok(id)
}

#[tauri::command]
// a Tauri command's parameters ARE its IPC surface (url, folder,
// format choice, captured-session token) — named inputs are the
// honest shape here, not a code smell
#[allow(clippy::too_many_arguments)]
async fn add_video(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    dir: String,
    selector: String,
    audio: bool,
    title: String,
    cookies: Option<String>,
    later: Option<bool>,
    total: Option<u64>,
    force: Option<bool>,
    queue: Option<String>,
) -> Result<String, String> {
    do_add_video(
        &app,
        &state,
        url,
        dir,
        selector,
        audio,
        title,
        cookies,
        later.unwrap_or(false),
        total.unwrap_or(0),
        force.unwrap_or(false),
        queue.unwrap_or_else(main_queue_name),
    )
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
        // the retired catch-all category name maps to "General"
        let s = if s.category == "All Downloads" {
            SavedEntry {
                category: "General".to_string(),
                ..s
            }
        } else {
            s
        };
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
                    error: s.error.clone(),
                })),
                queued: AtomicBool::new(s.status == "queued" && !finished),
                sample: Mutex::new((Instant::now(), 0, 0.0)),
                emitting: AtomicBool::new(false),
                starting: AtomicBool::new(false),
                removed: AtomicBool::new(false),
                pause_requested: AtomicBool::new(false),
                completion: Mutex::new(s.completion.clone()),
                queue: Mutex::new(s.queue.clone()),
                login: s.login.clone(),
                password: s.password.clone(),
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
                error: s.error.clone(),
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
                    error: s.error.clone(),
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
                    error: s.error.clone(),
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
            restored: Mutex::new(Some(restored)),
            queued: AtomicBool::new(s_queued),
            sample: Mutex::new((Instant::now(), 0, 0.0)),
            emitting: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            completion: Mutex::new(s.completion.clone()),
            queue: Mutex::new(s.queue.clone()),
            login: s.login.clone(),
            password: s.password.clone(),
        });
        state.entries.lock().unwrap().insert(s.id.clone(), entry);
        state.order.lock().unwrap().push(s.id.clone());
    }
    state.seq.store(max_seq + 1, Ordering::Relaxed);
}

/// Process the browser extension's incoming queue file. A leftover
/// `incoming.processing` from a crashed run is drained first, so queued
/// links are never silently lost.
fn drain_incoming(handle: &AppHandle, queue_dir: &Path) {
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
        handle_incoming_line(handle, line);
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

/// Reliably raise the main NDM window above the browser on X11 & Wayland.
fn raise_main_window(handle: &AppHandle) {
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_always_on_top(true);
        let _ = w.set_focus();
        let w2 = w.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            let _ = w2.set_always_on_top(false);
        });
    }
}

fn sanitize_ack_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return None;
    }
    if trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn write_ack_and_result(handle: &AppHandle, ack_id: Option<&str>, result: serde_json::Value) {
    let Some(id) = ack_id.and_then(sanitize_ack_id) else {
        return;
    };
    let Ok(base) = handle.path().app_data_dir() else {
        return;
    };
    let res_dir = base.join("results");
    let ack_dir = base.join("acks");
    let _ = std::fs::create_dir_all(&res_dir);
    let _ = std::fs::create_dir_all(&ack_dir);
    let _ = std::fs::write(res_dir.join(format!("{id}.json")), result.to_string());
    let _ = std::fs::write(ack_dir.join(format!("{id}.ack")), b"ok");
}

fn prune_acks(queue_dir: &Path) {
    let now = std::time::SystemTime::now();
    for sub in ["acks", "results"] {
        if let Ok(rd) = std::fs::read_dir(queue_dir.join(sub)) {
            for e in rd.flatten() {
                let stale = e
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| now.duration_since(t).ok())
                    .map(|d| d.as_secs() > 120)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// One queued extension link:
/// - `video_direct` / `file_direct`: user already selected options inside
///   the in-browser IDM popup, so start (or queue) the download silently
///   in the background WITHOUT raising the NDM window or stealing focus!
/// - `show_app`: user explicitly clicked "Show in NDM" -> raise NDM window.
/// - `video`: open the in-app Video Download dialog.
/// - `file`: open the in-app File Download Info dialog.
/// Discard an entry completely — engine/job cancelled, list entry,
/// resume state and the partial file all go. This is the browser
/// dialog's Cancel for a download that already started while the
/// dialog was open: IDM consumes the click, so nothing of the
/// download may remain. (Deliberately different from removing a row
/// in the app, which keeps the user's partial file.)
fn discard_entry(app: &AppHandle, state: &AppState, entry: &Arc<Entry>) {
    entry.removed.store(true, Ordering::SeqCst);
    state.retries.lock().unwrap().remove(&entry.id);
    if let Some(dl) = &entry.download {
        dl.control.cancel();
        // wait for the engine to actually stop: its shutdown writes
        // the resume state one last time, and deleting the files
        // before that would leave an orphan state file behind (a
        // later download of the same name could trust stale blocks)
        for _ in 0..40 {
            if dl.is_terminal() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if let Some(job) = &entry.video {
        job.cancel();
    }
    state.entries.lock().unwrap().remove(&entry.id);
    state.order.lock().unwrap().retain(|x| x != &entry.id);
    // the engine's final state write can land just after the cancel:
    // keep removing the resume state until it stays gone, so no
    // orphan survives to be trusted by a future same-name download
    let sp = ndm::state_path_for(&entry.out_path);
    for _ in 0..20 {
        let _ = std::fs::remove_file(&sp);
        std::thread::sleep(Duration::from_millis(100));
        if !sp.exists() {
            std::thread::sleep(Duration::from_millis(150));
            if !sp.exists() {
                break;
            }
        }
    }
    if entry.kind == "video" {
        // sweep this video's part files from its folder
        let stem = entry
            .name
            .rsplit_once('.')
            .map(|(s, _)| s.to_string())
            .unwrap_or_else(|| entry.name.clone());
        if let Ok(rd) = std::fs::read_dir(&entry.out_path) {
            for f in rd.flatten() {
                let n = f.file_name().to_string_lossy().to_string();
                if n.starts_with(&format!(".{stem}.ndm_stream"))
                    || (n.starts_with(&stem) && (n.ends_with(".part") || n.contains(".f")))
                {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
    } else if payload_of(entry).status != "finished" {
        let _ = std::fs::remove_file(&entry.out_path);
    }
    if let Some(p) = state.cookies_path(&entry.cookies) {
        let _ = std::fs::remove_file(p);
    }
    persist(state);
    pump_queue(app);
}

fn handle_incoming_line(handle: &AppHandle, line: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let ack_id = v["id"].as_str().and_then(sanitize_ack_id);
    let kind = v["kind"].as_str().unwrap_or("file");
    if kind == "show_app" {
        raise_main_window(handle);
        write_ack_and_result(handle, ack_id.as_deref(), serde_json::json!({ "ok": true }));
        return;
    }
    if kind == "pause_entry" || kind == "cancel_entry" {
        // the browser dialog's Later / Cancel for a download that
        // started while the dialog was open
        let entry_id = v["entry"].as_str().unwrap_or("");
        let state = handle.state::<AppState>();
        let result = match state.get(entry_id) {
            Some(entry) => {
                if kind == "pause_entry" {
                    let _ = pause_download(handle.clone(), state, entry_id.to_string());
                } else {
                    discard_entry(handle, &state, &entry);
                }
                serde_json::json!({ "ok": true })
            }
            None => serde_json::json!({ "ok": false, "error": "no such entry" }),
        };
        write_ack_and_result(handle, ack_id.as_deref(), result);
        return;
    }
    let url = v["url"].as_str().unwrap_or("");
    if url.is_empty() {
        write_ack_and_result(
            handle,
            ack_id.as_deref(),
            serde_json::json!({ "ok": false, "error": "empty url" }),
        );
        return;
    }
    let mut token = String::new();
    if let Some(text) = v["cookies"].as_str().filter(|t| !t.is_empty() && t.len() <= 600_000) {
        let state = handle.state::<AppState>();
        let dir = state.cookies_dir();
        let candidate = format!("ck{}", state.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let file = dir.join(format!("{candidate}.txt"));
        if std::fs::create_dir_all(&dir).is_ok() {
            #[cfg(unix)]
            let written = {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&file)
                    .and_then(|mut f| f.write_all(text.as_bytes()))
                    .is_ok()
            };
            #[cfg(not(unix))]
            let written = std::fs::write(&file, text).is_ok();
            if written {
                token = candidate;
            }
        }
    }
    if kind == "video_direct" {
        // Run silently in the background — NEVER steal focus from the browser!
        let state = handle.state::<AppState>();
        let dir = v["dir"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("~/Downloads")
            .to_string();
        let selector = v["selector"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b")
            .to_string();
        let audio = v["audio"].as_bool().unwrap_or(false);
        let later = v["later"].as_bool().unwrap_or(false);
        let total_hint = v["total"].as_u64().unwrap_or(0);
        let title = v["title"].as_str().unwrap_or("").to_string();
        let cookies_opt = if token.is_empty() { None } else { Some(token) };
        let force = v["force"].as_bool().unwrap_or(false);
        match do_add_video(
            handle,
            &state,
            url.to_string(),
            dir,
            selector,
            audio,
            title,
            cookies_opt,
            later,
            total_hint,
            force,
            main_queue_name(),
        ) {
            Ok(entry_id) => write_ack_and_result(
                handle,
                ack_id.as_deref(),
                serde_json::json!({ "ok": true, "id": entry_id }),
            ),
            Err(err) => write_ack_and_result(
                handle,
                ack_id.as_deref(),
                serde_json::json!({ "ok": false, "error": err }),
            ),
        }
        return;
    }
    if kind == "file_direct" {
        // Run silently in the background — NEVER steal focus from the browser!
        let state = handle.state::<AppState>();
        if state.update_required.load(Ordering::SeqCst) {
            write_ack_and_result(
                handle,
                ack_id.as_deref(),
                serde_json::json!({
                    "ok": false,
                    "error": "please install the required update before starting new downloads"
                }),
            );
            return;
        }
        let dir = v["dir"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("~/Downloads");
        let later = v["later"].as_bool().unwrap_or(false);
        let total_hint = v["total"].as_u64().unwrap_or(0);
        let title = v["title"].as_str().unwrap_or("").trim();
        let page_url = v["pageUrl"].as_str().unwrap_or("");
        let name_hint = if title.is_empty() { None } else { Some(title) };
        // IDM asks about duplicate links; the silent path instead
        // points at the entry that is already there (no second copy)
        if !v["force"].as_bool().unwrap_or(false) {
            if let Some(existing) = find_active_duplicate(&state, "http", url) {
                write_ack_and_result(
                    handle,
                    ack_id.as_deref(),
                    serde_json::json!({ "ok": true, "id": existing, "dupe": true }),
                );
                return;
            }
        }
        match register_http_stub(
            &state,
            url,
            dir,
            None,
            name_hint,
            &token,
            page_url,
            &main_queue_name(),
            "",
            "",
        ) {
            Ok(stub) => {
                let entry_id = stub.id.clone();
                let max = current_settings(&state).max_simultaneous;
                let must_wait = later
                    || entry_blocks(&state, &entry_queue(&stub))
                    || (max > 0 && count_active_for(&state, &entry_queue(&stub)) >= max);
                if must_wait {
                    if later {
                        stub.queued.store(false, Ordering::Relaxed);
                        *stub.restored.lock().unwrap() = Some(Restored {
                            total: total_hint,
                            done_bytes: 0,
                            blocks: Vec::new(),
                            resumable: true,
                            status: "paused".to_string(),
                            error: String::new(),
                        });
                    } else {
                        stub.queued.store(true, Ordering::Relaxed);
                    }
                    let _ = handle.emit("progress", payload_of(&stub));
                    persist(&state);
                } else {
                    start_entry(handle, stub);
                }
                write_ack_and_result(
                    handle,
                    ack_id.as_deref(),
                    serde_json::json!({ "ok": true, "id": entry_id }),
                );
            }
            Err(err) => {
                write_ack_and_result(
                    handle,
                    ack_id.as_deref(),
                    serde_json::json!({ "ok": false, "error": err }),
                );
            }
        }
        return;
    }
    if kind == "video" {
        raise_main_window(handle);
        let payload = serde_json::json!({
            "url": url,
            "title": v["title"].as_str().unwrap_or(""),
            "cookies": token,
        });
        let state = handle.state::<AppState>();
        state.pending_videos.lock().unwrap().push(payload.clone());
        let _ = handle.emit("incoming_video", payload);
        write_ack_and_result(handle, ack_id.as_deref(), serde_json::json!({ "ok": true }));
        return;
    }
    raise_main_window(handle);
    let payload = serde_json::json!({
        "url": url,
        "title": v["title"].as_str().unwrap_or(""),
        "pageUrl": v["pageUrl"].as_str().unwrap_or(""),
        "cookies": token,
    });
    let state = handle.state::<AppState>();
    state.pending_files.lock().unwrap().push(payload.clone());
    let _ = handle.emit("incoming_file", payload);
    write_ack_and_result(handle, ack_id.as_deref(), serde_json::json!({ "ok": true }));
}

/// The installed ndm-host binary, resolved from this app's own path:
/// next to the app in a dev build, ../lib/ndm in the .deb layout.
fn native_host_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for cand in [
        dir.join("ndm-host"),
        dir.join("../../../target/release/ndm-host"),
        dir.join("../lib/ndm/ndm-host"),
    ] {
        if cand.is_file() {
            return Some(cand.canonicalize().unwrap_or(cand));
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
            sched_override: Mutex::new(std::collections::HashSet::new()),
            quota_state: Mutex::new((0, 0, false)),
            quota_file: Mutex::new(PathBuf::new()),
            pending_videos: Mutex::new(Vec::new()),
            pending_files: Mutex::new(Vec::new()),
            components_busy: AtomicBool::new(false),
            retries: Mutex::new(HashMap::new()),
            had_active: AtomicBool::new(false),
            pending_update: Mutex::new(None),
            update_required: AtomicBool::new(false),
            update_busy: AtomicBool::new(false),
        })
        .setup(|app| {
            if let Some(w) = app.get_webview_window("main") {
                if std::env::args().any(|a| a == "--background") {
                    let _ = w.hide();
                } else {
                    let _ = w.show();
                }
            }
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
                *state.quota_file.lock().unwrap() = dir.join("quota.json");
                load_settings(&state);
                load_quota(&state);
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
                    // queue every pending entry and let the pump
                    // decide per queue: a blocked queue's entries stay
                    // queued for their window, the rest start now
                    // (respecting each queue's cap)
                    for e in &pending {
                        e.queued.store(true, Ordering::Relaxed);
                    }
                    persist(&state);
                    pump_queue(app.handle());
                }
            }
            // scheduler: every 30 s, start the queue inside the configured
            // window and pause everything when the window ends
            let handle4 = app.handle().clone();
            std::thread::spawn(move || {
                let mut was_in_window: std::collections::HashMap<String, bool> =
                    std::collections::HashMap::new();
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    let state = handle4.state::<AppState>();
                    let s = current_settings(&state);
                    // automatic retries (IDM queue semantics): start
                    // every failed entry whose retry is due and which
                    // is still sitting in error
                    {
                        let due: Vec<String> = {
                            let mut map = state.retries.lock().unwrap();
                            let now = Instant::now();
                            let mut due = Vec::new();
                            map.retain(|id, (_, at)| {
                                if *at <= now {
                                    due.push(id.clone());
                                    false
                                } else {
                                    true
                                }
                            });
                            due
                        };
                        for id in due {
                            let entry = state.entries.lock().unwrap().get(&id).cloned();
                            if let Some(e) = entry {
                                if payload_of(&e).status == "error" {
                                    start_entry(&handle4, e);
                                }
                            }
                        }
                    }
                    // per-queue scheduler windows (IDM's Scheduler):
                    // each queue starts at its window opening and
                    // pauses (re-queued) at its close
                    let now = chrono::Local::now().format("%H:%M").to_string();
                    for cfg in &s.queues {
                        if !cfg.use_schedule {
                            was_in_window.remove(&cfg.name);
                            continue;
                        }
                        let in_win = window_open(&cfg.start, &cfg.stop, &now);
                        let was = was_in_window.get(&cfg.name).copied().unwrap_or(false);
                        if in_win && !was {
                            // the window governs again; the manual
                            // override for this queue ends
                            state.sched_override.lock().unwrap().remove(&cfg.name);
                            if s.max_simultaneous == 0 {
                                start_all_queued(&handle4, Some(&cfg.name));
                            } else {
                                pump_queue(&handle4);
                            }
                        } else if !in_win && was {
                            state.sched_override.lock().unwrap().remove(&cfg.name);
                            pause_all(&handle4, true, Some(&cfg.name));
                        }
                        was_in_window.insert(cfg.name.clone(), in_win);
                    }
                }
            });
            // clipboard monitoring (IDM): copying a download link
            // raises the Add URL dialog with the link filled in —
            // the same incoming_file flow the browser extension uses.
            // Each distinct link is offered once per session.
            let handle5 = app.handle().clone();
            std::thread::spawn(move || {
                let mut last_seen = String::new();
                let mut prompted: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut cb: Option<arboard::Clipboard> = None;
                loop {
                    std::thread::sleep(Duration::from_millis(900));
                    let state = handle5.state::<AppState>();
                    if !current_settings(&state).clipboard_monitor {
                        continue;
                    }
                    if cb.is_none() {
                        cb = arboard::Clipboard::new().ok();
                    }
                    let Some(c) = cb.as_mut() else { continue };
                    let Ok(text) = c.get_text() else { continue };
                    let text = text.trim().to_string();
                    if text == last_seen {
                        continue;
                    }
                    last_seen = text.clone();
                    if !clipboard_url_qualifies(&text) {
                        continue;
                    }
                    if !prompted.insert(text.clone()) {
                        continue;
                    }
                    if prompted.len() > 200 {
                        prompted.clear();
                        prompted.insert(text.clone());
                    }
                    let payload = serde_json::json!({
                        "url": text,
                        "title": "",
                        "pageUrl": "",
                        "cookies": "",
                    });
                    state.pending_files.lock().unwrap().push(payload.clone());
                    raise_main_window(&handle5);
                    let _ = handle5.emit("incoming_file", payload);
                }
            });
            // periodic snapshot so even a hard power cut keeps a fresh list
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(5));
                let state = handle.state::<AppState>();
                persist(&state);
                // download quota (IDM's Download limits): the moment
                // the window's budget runs out, everything pauses —
                // re-queued, so the next window resumes it — and the
                // user is told once per window
                if quota_blocks(&state) {
                    let mut q = state.quota_state.lock().unwrap();
                    if !q.2 {
                        q.2 = true;
                        drop(q);
                        pause_all(&handle, true, None);
                        let _ = handle.emit("quota_exceeded", serde_json::json!({}));
                    }
                }
            });
            // incoming queue from the browser extension (via ndm-host):
            // drain immediately on startup and poll every 200ms so browser
            // clicks feel instantaneous
            let handle2 = app.handle().clone();
            let queue_dir = dir.clone();
            std::thread::spawn(move || {
                prune_acks(&queue_dir);
                drain_incoming(&handle2, &queue_dir);
                let mut ticks = 0u64;
                loop {
                    std::thread::sleep(Duration::from_millis(200));
                    drain_incoming(&handle2, &queue_dir);
                    ticks += 1;
                    if ticks % 300 == 0 {
                        prune_acks(&queue_dir);
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add_download,
            pause_download,
            resume_download,
            remove_download,
            list_downloads,
            probe_file,
            probe_video,
            add_video,
            grab_page,
            get_settings,
            set_settings,
            set_entry_queue,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_preserve_user_connections_8_and_16_across_restarts() {
        let dir = std::env::temp_dir().join(format!("ndm-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings_path = dir.join("settings.json");

        let state = AppState {
            entries: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            seq: AtomicU64::new(1),
            data_file: Mutex::new(PathBuf::new()),
            components_dir: Mutex::new(PathBuf::new()),
            cookies_dir: Mutex::new(PathBuf::new()),
            save_lock: Mutex::new(()),
            settings: Mutex::new(Settings {
                max_connections: 8,
                ..Settings::default()
            }),
            settings_file: Mutex::new(settings_path.clone()),
            sched_override: Mutex::new(std::collections::HashSet::new()),
            quota_state: Mutex::new((0, 0, false)),
            quota_file: Mutex::new(PathBuf::new()),
            pending_videos: Mutex::new(Vec::new()),
            pending_files: Mutex::new(Vec::new()),
            components_busy: AtomicBool::new(false),
            pending_update: Mutex::new(None),
            update_required: AtomicBool::new(false),
            update_busy: AtomicBool::new(false),
        };
        save_settings(&state);
        state.settings.lock().unwrap().max_connections = 32;
        load_settings(&state);
        assert_eq!(state.settings.lock().unwrap().max_connections, 8);

        state.settings.lock().unwrap().max_connections = 16;
        save_settings(&state);
        state.settings.lock().unwrap().max_connections = 32;
        load_settings(&state);
        assert_eq!(state.settings.lock().unwrap().max_connections, 16);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_ack_id_rejects_traversal_and_accepts_valid_ids() {
        assert_eq!(sanitize_ack_id("12345-67890-0"), Some("12345-67890-0".to_string()));
        assert_eq!(sanitize_ack_id("../etc/passwd"), None);
        assert_eq!(sanitize_ack_id(""), None);
    }

    #[test]
    fn persist_writes_live_downloading_status_not_fake_paused() {
        let dir = std::env::temp_dir().join(format!("ndm-persist-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let data_file = dir.join("downloads.json");

        let state = AppState {
            entries: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            seq: AtomicU64::new(1),
            data_file: Mutex::new(data_file.clone()),
            components_dir: Mutex::new(PathBuf::new()),
            cookies_dir: Mutex::new(PathBuf::new()),
            save_lock: Mutex::new(()),
            settings: Mutex::new(Settings::default()),
            settings_file: Mutex::new(PathBuf::new()),
            sched_override: Mutex::new(std::collections::HashSet::new()),
            quota_state: Mutex::new((0, 0, false)),
            quota_file: Mutex::new(PathBuf::new()),
            pending_videos: Mutex::new(Vec::new()),
            pending_files: Mutex::new(Vec::new()),
            components_busy: AtomicBool::new(false),
            pending_update: Mutex::new(None),
            update_required: AtomicBool::new(false),
            update_busy: AtomicBool::new(false),
        };

        let entry = Arc::new(Entry {
            id: "dl-1".to_string(),
            name: "test.iso".to_string(),
            url: "https://example.com/test.iso".to_string(),
            out_path: dir.join("test.iso"),
            category: "Programs".to_string(),
            added: "2026-10-29 12:00".to_string(),
            download: None,
            video: None,
            kind: "http".to_string(),
            selector: String::new(),
            cookies: String::new(),
            referer: String::new(),
            audio: false,
            restored: Mutex::new(Some(Restored {
                total: 1000,
                done_bytes: 500,
                blocks: vec![1, 0],
                resumable: true,
                status: "downloading".to_string(),
                error: String::new(),
            })),
            queued: AtomicBool::new(false),
            sample: Mutex::new((Instant::now(), 0, 0.0)),
            emitting: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            completion: Mutex::new(String::new()),
            queue: Mutex::new(String::new()),
            login: String::new(),
            password: String::new(),
        });
        state.entries.lock().unwrap().insert("dl-1".to_string(), entry);
        state.order.lock().unwrap().push("dl-1".to_string());

        persist(&state);
        let text = std::fs::read_to_string(&data_file).unwrap();
        assert!(text.contains("\"status\":\"downloading\""));
        assert!(!text.contains("\"status\":\"paused\""));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

