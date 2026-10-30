// Nill Download Manager (app backend)
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
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

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
    /// the user's own note for this download (IDM's Description)
    description: String,
    /// this download's own speed limit in bytes/sec (0 = none)
    speed_limit: u64,
    /// where the file is saved (shown in the status dialog, IDM's
    /// "Save To"); for videos this is the folder
    save_to: String,
    /// the page this download came from ("" = none)
    referer: String,
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
    /// the user's own note for this download (IDM's Description,
    /// shown in its own list column and editable in Properties)
    #[serde(default)]
    description: String,
    /// this download's own speed limit in bytes/sec, 0 = none
    /// (IDM's per-download Speed Limiter tab)
    #[serde(default)]
    speed_limit: u64,
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

/// One saved server/site login (IDM's Options -> Site Logins table):
/// when a download's address matches `site` (host, optionally with a
/// path prefix), this login is offered automatically.
#[derive(Clone, Serialize, Deserialize)]
struct SiteLogin {
    #[serde(default)]
    site: String,
    #[serde(default)]
    login: String,
    #[serde(default)]
    password: String,
}

/// One saved Site Grabber project (IDM's Grabber projects): the whole
/// wizard state, restorable from the Grabber dialog and listed under
/// the sidebar's "Grabber projects".
#[derive(Clone, Serialize, Deserialize)]
struct GrabProject {
    #[serde(default)]
    name: String,
    #[serde(default)]
    start_url: String,
    #[serde(default)]
    template: String,
    #[serde(default)]
    save_dir: String,
    #[serde(default)]
    use_subfolders: bool,
    #[serde(default)]
    add_as_queue: bool,
    #[serde(default)]
    depth_same: u32,
    #[serde(default)]
    depth_other: u32,
    #[serde(default)]
    whole_site: bool,
    #[serde(default)]
    no_parent: bool,
    #[serde(default)]
    same_site_files_only: bool,
    #[serde(default)]
    include_exts: Vec<String>,
    #[serde(default)]
    exclude_exts: Vec<String>,
    #[serde(default)]
    min_kb: u64,
    #[serde(default)]
    max_mb: u64,
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
    /// IDM's Proxy/Socks tab, in full: how the proxy is chosen
    /// ("none" | "system" | "pac" | "manual"; "" = the legacy single
    /// `proxy` field above, migrated by sanitize). `proxy` itself is
    /// kept as the EFFECTIVE proxy URL the engine consumes, recomputed
    /// by sanitize() from these fields on every load and save.
    #[serde(default)]
    proxy_mode: String,
    #[serde(default)]
    proxy_scheme: String,
    #[serde(default)]
    proxy_host: String,
    #[serde(default)]
    proxy_port: u16,
    #[serde(default)]
    proxy_user: String,
    #[serde(default)]
    proxy_pass: String,
    #[serde(default = "def_true")]
    proxy_http: bool,
    #[serde(default = "def_true")]
    proxy_https: bool,
    #[serde(default = "def_true")]
    proxy_ftp: bool,
    #[serde(default)]
    proxy_pac_url: String,
    /// use the system (browser-environment) proxy as a one-time
    /// fallback when a browser-caught download cannot even be probed
    /// (IDM's "Use proxy/socks from a browser's request if there are
    /// download errors for the downloads intercepted from the browser")
    #[serde(default)]
    proxy_from_browser_on_error: bool,
    /// Options -> Site Logins: saved logins, applied automatically to
    /// downloads whose address matches (see site_login_for)
    #[serde(default)]
    site_logins: Vec<SiteLogin>,
    /// what the computer does when all downloads finish (IDM's
    /// Scheduler "Turn off computer when done"): "" | "shutdown" |
    /// "restart" | "sleep" | "hibernate" | "logoff"
    #[serde(default)]
    power_when_done: String,
    /// force running programs to close with the power action
    #[serde(default)]
    power_force: bool,
    /// saved Site Grabber projects (managed from the Grabber wizard)
    #[serde(default)]
    grab_projects: Vec<GrabProject>,
    /// IDM-style category routing: category name → its own default
    /// folder. Applies when a download is headed for the ordinary
    /// default folder; an explicitly chosen folder always wins.
    #[serde(default)]
    category_dirs: std::collections::HashMap<String, String>,
    /// user-edited category rules (Options → Categories): category
    /// name → the extensions that belong to it. An extension listed
    /// here wins over the built-in table in `category_for`; empty =
    /// built-in table only. The browser extension keeps its own
    /// built-in copy (it cannot read app settings), so this steers
    /// the app's own classification and probing.
    #[serde(default)]
    category_exts: std::collections::HashMap<String, Vec<String>>,
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
    /// Add URL dialog (IDM's clipboard monitoring). IDM's equivalent
    /// option is OFF by default — clipboard capture is opt-in, so a
    /// fresh install does not raise the app over the user's work on
    /// every copied link. A value already saved by the user (either
    /// way) is always respected.
    #[serde(default)]
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
    // a queue object missing its name used to fail the WHOLE
    // settings parse and silently factory-reset every preference —
    // it now just becomes the Main queue
    #[serde(default = "main_queue_name")]
    name: String,
    #[serde(default)]
    use_schedule: bool,
    #[serde(default = "def_sched_start")]
    start: String,
    #[serde(default = "def_sched_stop")]
    stop: String,
    #[serde(default)]
    exit_when_done: bool,
    /// this queue's own "turn off computer when done" action
    /// ("" = fall back to the global Options power setting)
    #[serde(default)]
    power_when_done: String,
    /// this queue's own simultaneous-download cap (IDM's Scheduler
    /// "Max simultaneous downloads" is per queue); 0 = fall back to
    /// the global Options value
    #[serde(default)]
    max_simultaneous: usize,
}

impl Default for QueueCfg {
    fn default() -> Self {
        QueueCfg {
            name: main_queue_name(),
            use_schedule: false,
            start: def_sched_start(),
            stop: def_sched_stop(),
            exit_when_done: false,
            power_when_done: String::new(),
            max_simultaneous: 0,
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
            proxy_mode: String::new(),
            proxy_scheme: String::new(),
            proxy_host: String::new(),
            proxy_port: 0,
            proxy_user: String::new(),
            proxy_pass: String::new(),
            proxy_http: true,
            proxy_https: true,
            proxy_ftp: true,
            proxy_pac_url: String::new(),
            proxy_from_browser_on_error: false,
            site_logins: Vec::new(),
            power_when_done: String::new(),
            power_force: false,
            grab_projects: Vec::new(),
            category_dirs: std::collections::HashMap::new(),
            category_exts: std::collections::HashMap::new(),
            conn_exceptions: Vec::new(),
            exit_when_done: false,
            sound_events: true,
            show_complete_dialog: true,
            queues: vec![QueueCfg::default()],
            quota_mb: 0,
            quota_hours: def_quota_hours(),
            clipboard_monitor: false,
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
        self.sanitize_proxy();
        // site logins: trimmed, host required, no exact duplicates
        {
            let mut seen = std::collections::HashSet::new();
            self.site_logins.retain(|l| {
                let site = l.site.trim().to_lowercase();
                !site.is_empty()
                    && !l.login.trim().is_empty()
                    && seen.insert(format!("{site}|{}", l.login.trim().to_lowercase()))
            });
            for l in &mut self.site_logins {
                l.site = l.site.trim().to_string();
                l.login = l.login.trim().to_string();
            }
        }
        // the global power action must be one of the known actions
        if !POWER_ACTIONS.contains(&self.power_when_done.as_str()) {
            self.power_when_done = String::new();
        }
        for q in &mut self.queues {
            if !POWER_ACTIONS.contains(&q.power_when_done.as_str()) {
                q.power_when_done = String::new();
            }
        }
        // grabber projects: named, addressed, depths clamped, unique names
        {
            let mut seen = std::collections::HashSet::new();
            self.grab_projects.retain(|p| {
                !p.name.trim().is_empty()
                    && !p.start_url.trim().is_empty()
                    && seen.insert(p.name.trim().to_lowercase())
            });
            for p in &mut self.grab_projects {
                p.name = p.name.trim().to_string();
                p.start_url = p.start_url.trim().to_string();
                p.depth_same = p.depth_same.min(10);
                p.depth_other = p.depth_other.min(10);
            }
        }
        self.category_dirs = self
            .category_dirs
            .drain()
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, v)| !k.is_empty() && !v.is_empty())
            .collect();
        // category rules: normalize extensions (lowercase, no dots,
        // no blanks, no duplicates within a category)
        self.category_exts = self
            .category_exts
            .drain()
            .map(|(k, v)| {
                let mut seen = std::collections::HashSet::new();
                let exts: Vec<String> = v
                    .iter()
                    .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|e| !e.is_empty() && seen.insert(e.clone()))
                    .collect();
                (k.trim().to_string(), exts)
            })
            // an explicitly EMPTIED category keeps its (empty)
            // entry: category_for_with reads a present-but-empty
            // list as "matches nothing" instead of resurrecting
            // the built-in list the user just cleared
            .filter(|(k, _)| !k.is_empty())
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
                power_when_done: String::new(),
                max_simultaneous: 0,
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
            q.max_simultaneous = q.max_simultaneous.min(10);
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
        // unchecked quota arithmetic on hand-edited values could
        // overflow and make the limit behave randomly — clamp to
        // sane bounds (1 TB per window, at most a week per window)
        self.quota_mb = self.quota_mb.min(1_000_000);
        self.quota_hours = self.quota_hours.min(24 * 7);
    }
}

const POWER_ACTIONS: [&str; 6] = ["", "shutdown", "restart", "sleep", "hibernate", "logoff"];

/// Percent-encode the characters that would break a proxy URL's
/// userinfo part (a password containing "@" must not move the host).
fn pct_encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-decode a proxy userinfo part (the inverse of
/// pct_encode_userinfo): the parser must hand sanitize the RAW
/// credentials, or sanitize's re-encoding would encode the escapes
/// a second time ("%40" -> "%2540") and proxy authentication would
/// silently break the first time a legacy proxy URL is migrated.
fn pct_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse "scheme://[user[:pass]@]host[:port]" into the manual fields.
fn parse_proxy_url(url: &str) -> Option<(String, String, u16, String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };
    let hostport = hostport.split('/').next().unwrap_or("");
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(0)),
        None => (hostport, 0),
    };
    if host.is_empty() {
        return None;
    }
    let (user, pass) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((a, b)) => (pct_decode(a), pct_decode(b)),
            None => (pct_decode(u), String::new()),
        },
        None => (String::new(), String::new()),
    };
    Some((scheme.to_string(), host.to_string(), port, user, pass))
}

/// The system proxy as the desktop environment reports it: the
/// standard proxy environment variables (what browsers launched from
/// the same session inherit, and what ureq itself honours).
fn system_proxy_url() -> String {
    for key in ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    String::new()
}

/// The proxy the engine should actually use under these settings.
/// Every mode except PAC has its effective URL composed by sanitize
/// into `proxy`; a PAC script is resolved here — at the points where
/// the app applies the proxy to the engine — and the engine-global
/// value then serves as the cache until the settings change again.
fn effective_proxy(s: &Settings) -> String {
    if s.proxy_mode == "pac" {
        ndm::resolve_pac_proxy(&s.proxy_pac_url)
    } else {
        s.proxy.clone()
    }
}

/// Push the settings' proxy configuration into the engine globals.
fn apply_proxy_to_engine(s: &Settings) {
    ndm::set_global_proxy(&effective_proxy(s));
    ndm::set_proxy_protocols(s.proxy_http, s.proxy_https, s.proxy_ftp);
}

impl Settings {
    /// Recompute the effective `proxy` from the Proxy-tab fields and
    /// normalize those fields (legacy migration included): a settings
    /// file written before the full Proxy tab existed carries only the
    /// single `proxy` string — that becomes a manual configuration.
    fn sanitize_proxy(&mut self) {
        let mode = self.proxy_mode.trim().to_ascii_lowercase();
        match mode.as_str() {
            "" => {
                if !self.proxy.is_empty() {
                    if let Some((scheme, host, port, user, pass)) = parse_proxy_url(&self.proxy) {
                        self.proxy_mode = "manual".to_string();
                        self.proxy_scheme = scheme;
                        self.proxy_host = host;
                        self.proxy_port = port;
                        self.proxy_user = user;
                        self.proxy_pass = pass;
                    }
                }
            }
            "none" => {
                self.proxy = String::new();
                self.proxy_mode = "none".to_string();
            }
            "system" => {
                self.proxy = system_proxy_url();
                self.proxy_mode = "system".to_string();
            }
            "pac" => {
                self.proxy_pac_url = self.proxy_pac_url.trim().to_string();
                // resolving the PAC script fetches it over the
                // network and its answer is per-URL — neither belongs
                // in sanitize, which runs on every load and save, and
                // the result must not be frozen into the persisted
                // `proxy` field. The effective proxy is resolved once
                // per settings change where the settings are applied
                // to the engine (effective_proxy below).
                self.proxy = String::new();
                self.proxy_mode = "pac".to_string();
            }
            "manual" => {
                let scheme = if self.proxy_scheme.trim().is_empty() {
                    "http".to_string()
                } else {
                    self.proxy_scheme.trim().to_ascii_lowercase()
                };
                self.proxy_scheme = scheme.clone();
                self.proxy_host = self.proxy_host.trim().to_string();
                if self.proxy_host.is_empty() {
                    self.proxy = String::new();
                } else {
                    let auth = if self.proxy_user.is_empty() {
                        String::new()
                    } else if self.proxy_pass.is_empty() {
                        format!("{}@", pct_encode_userinfo(&self.proxy_user))
                    } else {
                        format!(
                            "{}:{}@",
                            pct_encode_userinfo(&self.proxy_user),
                            pct_encode_userinfo(&self.proxy_pass)
                        )
                    };
                    let port = if self.proxy_port > 0 {
                        format!(":{}", self.proxy_port)
                    } else {
                        String::new()
                    };
                    self.proxy = format!("{scheme}://{auth}{}{port}", self.proxy_host);
                }
                self.proxy_mode = "manual".to_string();
            }
            _ => {
                self.proxy_mode = "none".to_string();
                self.proxy = String::new();
            }
        }
    }
}

/// The saved login that applies to `url`, if any (IDM's Site Logins):
/// the site field is a host, optionally with a path prefix; the
/// longest matching site wins, so a deeper path can override a
/// host-wide login.
/// Split an authority ("host" or "host:port") into its host and
/// port parts: the last colon only introduces a port when what
/// follows it is all digits, so a colon inside a host name is safe.
fn split_host_port(authority: &str) -> (&str, Option<&str>) {
    match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(p)),
        _ => (authority, None),
    }
}

fn site_login_for(s: &Settings, url: &str) -> Option<(String, String)> {
    let after = url.split("://").nth(1).unwrap_or("");
    let (authority, path) = match after.find('/') {
        Some(i) => (
            &after[..i],
            after[i..].split(['?', '#']).next().unwrap_or("").to_string(),
        ),
        None => (after, "/".to_string()),
    };
    // an embedded "user:pass@" in the URL must not poison the host
    // comparison, and the comparison itself is by HOST (IDM's rule):
    // a saved site without a port matches the host on any port,
    // while one that names a port must match that port too
    let authority = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let (host, url_port) = split_host_port(&authority);
    let mut best: Option<(&SiteLogin, usize)> = None;
    for l in &s.site_logins {
        let site = l
            .site
            .trim()
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_ascii_lowercase();
        let (shost, spath) = match site.find('/') {
            Some(i) => (&site[..i], &site[i..]),
            None => (site.as_str(), ""),
        };
        let (shost, site_port) = split_host_port(shost);
        if shost != host {
            continue;
        }
        if let Some(p) = site_port {
            if url_port != Some(p) {
                continue;
            }
        }
        if !spath.is_empty() && spath != "/" && !path.starts_with(spath) {
            continue;
        }
        let score = site.len();
        if best.map(|(_, sc)| score > sc).unwrap_or(true) {
            best = Some((l, score));
        }
    }
    best.map(|(l, _)| (l.login.clone(), l.password.clone()))
}

/// Run one "turn off computer when done" power action. The
/// NDM_POWER_DRY_RUN environment variable (a file path) turns the
/// action into a marker write instead of a real power call — the
/// test harness uses it, and it doubles as a safety valve.
fn do_power_action(action: &str, force: bool) -> bool {
    if !POWER_ACTIONS.contains(&action) || action.is_empty() {
        return false;
    }
    if let Ok(marker) = std::env::var("NDM_POWER_DRY_RUN") {
        if !marker.is_empty() {
            let _ = std::fs::write(&marker, format!("{action} force={force}"));
            return true;
        }
    }
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("shutdown");
        match action {
            "shutdown" => {
                cmd.args(["/s", "/t", "5"]);
                if force {
                    cmd.arg("/f");
                }
            }
            "restart" => {
                cmd.args(["/r", "/t", "5"]);
                if force {
                    cmd.arg("/f");
                }
            }
            "logoff" => {
                cmd.args(["/l"]);
                if force {
                    cmd.arg("/f");
                }
            }
            "sleep" | "hibernate" => {
                let hib = if action == "hibernate" { "1" } else { "0" };
                cmd = std::process::Command::new("rundll32.exe");
                cmd.args(["powrprof.dll,SetSuspendState", hib, "1", "0"]);
            }
            _ => return false,
        }
        return cmd.spawn().is_ok();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let (prog, args): (&str, Vec<&str>) = match action {
            "shutdown" => ("systemctl", vec!["poweroff"]),
            "restart" => ("systemctl", vec!["reboot"]),
            "sleep" => ("systemctl", vec!["suspend"]),
            "hibernate" => ("systemctl", vec!["hibernate"]),
            "logoff" => ("loginctl", vec!["terminate-session", "self"]),
            _ => return false,
        };
        let mut cmd = std::process::Command::new(prog);
        cmd.args(&args);
        if force && action != "logoff" {
            cmd.arg("--force");
        }
        if cmd.spawn().is_ok() {
            return true;
        }
        // fallbacks for systems without systemd session control
        let fallback: (&str, Vec<&str>) = match action {
            "shutdown" => ("shutdown", vec!["-h", "now"]),
            "restart" => ("shutdown", vec!["-r", "now"]),
            "logoff" => ("gnome-session-quit", vec!["--logout", "--no-prompt"]),
            _ => return false,
        };
        let mut cmd2 = std::process::Command::new(fallback.0);
        cmd2.args(&fallback.1);
        cmd2.spawn().is_ok()
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
        let parsed = serde_json::from_slice::<Settings>(&raw).ok().or_else(|| {
            // tolerant second pass: one malformed element inside the
            // queues array must not discard EVERY preference (the
            // downloads store already parses per-record; settings now
            // gets the same mercy — keep the queues that do parse)
            let mut v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
            if let Some(arr) = v.get_mut("queues").and_then(|q| q.as_array_mut()) {
                arr.retain(|el| serde_json::from_value::<QueueCfg>(el.clone()).is_ok());
            }
            serde_json::from_value::<Settings>(v).ok()
        });
        if let Some(mut s) = parsed {
            s.sanitize();
            ndm::set_global_limit(s.speed_limit_bps);
            ndm::set_global_connections(s.max_connections);
            apply_proxy_to_engine(&s);
            *state.settings.lock().unwrap() = s;
        }
    }
}

fn save_settings(state: &AppState) {
    // Unlike persist (serialized by save_lock), this writer had NO
    // lock: the UI's set_settings and the 12 s install-count thread
    // write the same shared .tmp path concurrently, and one side can
    // rename the other's half-written file — the next load then fails
    // and silently reverts every preference to defaults.
    static SETTINGS_SAVE_LOCK: Mutex<()> = Mutex::new(());
    let _guard = SETTINGS_SAVE_LOCK.lock().unwrap();
    let path = state.settings_file.lock().unwrap().clone();
    if path.as_os_str().is_empty() {
        return;
    }
    let settings = current_settings(state);
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = create_private(&tmp) {
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
    /// a simultaneous-cap slot was reserved for this entry's pending
    /// start (closes the check-then-start race between concurrent
    /// adds: the reservation is visible to other cap checks before
    /// start_engine's own claim runs)
    slot_preclaimed: AtomicBool,
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
    /// the user's own note for this download (IDM's Description)
    description: Mutex<String>,
    /// this download's own speed limit in bytes/sec, 0 = none
    speed_limit: Mutex<u64>,
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
    /// bytes downloaded this window by entries that have since been
    /// REMOVED from the list — the quota meter is total traffic in
    /// the window, so deleting entries must not refund the budget
    /// (IDM meters traffic, not list contents)
    quota_removed: Mutex<u64>,
    /// where quota.json lives (next to downloads.json)
    quota_file: Mutex<PathBuf>,
    /// video links from the extension that arrived before the webview
    /// was listening; the frontend drains this once on load
    pending_videos: Mutex<Vec<serde_json::Value>>,
    pending_files: Mutex<Vec<serde_json::Value>>,
    /// "Download all links" requests from the extension that arrived
    /// before the webview was listening; drained once on load like
    /// pending_videos
    pending_grabs: Mutex<Vec<serde_json::Value>>,
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
        "exe" | "msi" | "msix" | "msixbundle" | "deb" | "rpm" | "dmg" | "pkg" | "appimage" | "apk"
        | "bin" | "run" | "cab" | "jar" | "war" | "vmdk" | "qcow2" | "vdi" | "iso" | "img"
        | "flatpakref" => "Programs",
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" | "ts" => "Video",
        "mp3" | "m4a" | "m4b" | "flac" | "wav" | "ogg" | "opus" | "aac" | "wma" => "Music",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "csv"
        | "rtf" | "epub" | "mobi" => "Documents",
        _ => "General",
    }
}

/// Settings-aware classification: an extension the user assigned to
/// a category in Options wins over the built-in table.
fn category_for_with(name: &str, s: &Settings) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if !ext.is_empty() {
        for (cat, exts) in &s.category_exts {
            if exts.iter().any(|e| e == &ext) {
                return cat.clone();
            }
        }
    }
    let builtin = category_for(name);
    // a category the user explicitly emptied in Options (the key is
    // present with an empty list) matches NOTHING: its built-in
    // extensions fall through to General rather than the built-in
    // list silently coming back. Only a MISSING key falls back.
    if s.category_exts
        .get(builtin)
        .map(|v| v.is_empty())
        .unwrap_or(false)
    {
        return "General".to_string();
    }
    builtin.to_string()
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
        // state honesty (F-15): if the job's thread died mid-download
        // its last sampled speed would otherwise be reported as a live
        // rate forever — no byte progress for a few seconds means the
        // rate is 0, whatever the stale sample says
        let speed_bps = if p.status == "downloading"
            && p.last_progress_at.elapsed() > Duration::from_secs(3)
        {
            0.0
        } else {
            p.speed_bps
        };
        return ProgressPayload {
            id: entry.id.clone(),
            name: display_name,
            url: entry.url.clone(),
            total,
            done_bytes,
            speed_bps,
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
            description: entry.description.lock().unwrap().clone(),
            speed_limit: *entry.speed_limit.lock().unwrap(),
            save_to: entry.out_path.to_string_lossy().to_string(),
            referer: entry.referer.clone(),
        };
    }
    let (total, done_bytes, rate, conns, status, blocks, resumable, error_text, conns_detail) =
        match &entry.download {
        Some(dl) => {
            let p = dl.progress();
            let rate = if p.status == ndm::Status::Downloading {
                let s = entry.sample.lock().unwrap();
                // state honesty (F-15): the smoothed sample is a LIVE
                // rate only while bytes are actually moving. If no
                // byte has arrived past the sample's mark for a few
                // seconds — a stalled engine, a dead emit loop — the
                // displayed rate decays to 0 instead of presenting a
                // frozen number as a healthy live transfer forever.
                if p.done_bytes <= s.1 && s.0.elapsed() > Duration::from_secs(3) {
                    0.0
                } else {
                    s.2
                }
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
        description: entry.description.lock().unwrap().clone(),
        speed_limit: *entry.speed_limit.lock().unwrap(),
        save_to: entry.out_path.to_string_lossy().to_string(),
        referer: entry.referer.clone(),
    }
}

/// Snapshot the whole list to disk (write-then-rename, crash-safe).
/// Create a temp file for an atomic rename-write with owner-only
/// permissions on Unix: downloads.json now holds saved site
/// passwords, so the stores are not world-readable (the session
/// cookie files already use 0600 for the same reason).
fn create_private(tmp: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::create(tmp)
    }
}

/// Set when downloads.json's top level was unparseable at startup
/// and the original was quarantined: persist must then never write
/// an EMPTY list over the (new) file, or the quarantine is the only
/// copy of the user's history and the app looks factory-fresh.
static DOWNLOADS_CORRUPT: AtomicBool = AtomicBool::new(false);

fn persist(state: &AppState) {
    let _guard = state.save_lock.lock().unwrap();
    let path = state.data_file.lock().unwrap().clone();
    if path.as_os_str().is_empty() {
        return;
    }
    if DOWNLOADS_CORRUPT.load(Ordering::Relaxed) && state.entries.lock().unwrap().is_empty() {
        return;
    }
    let order = state.order.lock().unwrap().clone();
    let saved: Vec<SavedEntry> = order
        .iter()
        .filter_map(|id| state.get(id))
        .filter(|e| {
            // a just-registered stub whose first start is still in
            // flight — no engine, no job, no restored record yet —
            // is a placeholder, not a download: snapshotting it
            // would persist a total=0 row that a crash mid-probe
            // restores as a phantom entry (R3). It joins the
            // snapshot the moment its start resolves, one way or
            // the other: a live handle, an error record, or the
            // queued/paused record of a deliberate later start.
            // QUEUED is one of those resolved states, not a
            // placeholder trait: an entry the cap/scheduler
            // deliberately parked (queued flag set, start already
            // decided as "later") must reach downloads.json the
            // moment it is queued — without this arm it matched
            // the placeholder shape (no handle, no record when no
            // size hint rode along) and silently vanished from
            // the list file, restart included (battery19 t1/t3).
            e.download.is_some()
                || e.video.is_some()
                || e.restored.lock().unwrap().is_some()
                || e.queued.load(Ordering::Relaxed)
        })
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
                description: e.description.lock().unwrap().clone(),
                speed_limit: *e.speed_limit.lock().unwrap(),
            }
        })
        .collect();
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = create_private(&tmp) {
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
        // the engine's own wording for the same dead links (probe
        // and block-fetch failures) — a gone file must not burn the
        // whole retry budget either
        || error.contains("status code 404")
        || error.contains("status code 410")
        || error.contains("no playable video stream")
        // post-download verdicts that re-downloading the same bytes
        // cannot change — the shrink guard's mismatch and a merge
        // the tools could not perform. These used to burn the whole
        // retry budget re-downloading the full file again and again
        // (R2-EXT-03 / N-10); a manual retry stays available.
        || error.contains("much smaller than what was downloaded")
        || error.contains("could not be merged into one playable file")
        || error.contains("required update")
}

/// The user-facing form of a probe/start failure. A definitive
/// HTTP answer from the server (404, 410, …) is NOT a reachability
/// problem, and presenting it as one ("check your connection")
/// sends the user debugging the wrong thing (N-07 — a batch of
/// playlist links at wrong paths was reported as a network
/// failure). Translate it into what the server actually said; the
/// wording deliberately avoids the dialog's network-error trigger
/// words, so the accurate text is what the user reads. Anything
/// else — genuine transport failures — passes through unchanged.
fn plain_server_error(err: &str) -> String {
    let Some(pos) = err.find("status code ") else {
        return err.to_string();
    };
    let code: String = err[pos + "status code ".len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    match code.as_str() {
        "404" => {
            "The server answered 404 (Not Found) — there is no file at that address."
                .to_string()
        }
        "410" => {
            "The server answered 410 (Gone) — that file has been removed.".to_string()
        }
        "403" => "The server answered 403 (Forbidden) — it will not serve that file; \
                  it may need a login."
            .to_string(),
        "401" => "The server answered 401 — that file needs a login.".to_string(),
        "" => err.to_string(),
        _ => format!("The server answered {code} for that address."),
    }
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
/// Snapshots the entry list first: payload_of calls into engine code,
/// and holding the central entries lock across it stalls every other
/// path (and would poison the central lock if engine code panicked).
fn total_done_bytes(state: &AppState) -> u64 {
    let entries: Vec<Arc<Entry>> = state
        .entries
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    entries.iter().map(|e| payload_of(e).done_bytes).sum()
}

/// The quota meter: ALL bytes downloaded in the window — entries
/// still in the list plus entries removed mid-window (their bytes
/// move into quota_removed at removal time, so deleting a finished
/// entry never refunds the budget).
fn quota_meter(state: &AppState) -> u64 {
    total_done_bytes(state) + *state.quota_removed.lock().unwrap()
}

/// Record an entry's downloaded bytes into the removed-bytes meter
/// just before it leaves the list (see quota_meter).
fn quota_note_removed(state: &AppState, entry: &Entry) {
    let done = payload_of(entry).done_bytes;
    if done > 0 {
        *state.quota_removed.lock().unwrap() += done;
        save_quota(state);
    }
}

fn save_quota(state: &AppState) {
    let path = state.quota_file.lock().unwrap().clone();
    if path.as_os_str().is_empty() {
        return;
    }
    let (baseline, start, warned) = *state.quota_state.lock().unwrap();
    let removed = *state.quota_removed.lock().unwrap();
    // tmp+rename like the other stores: a bare write torn by a crash
    // used to silently reset the whole window's accounting
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::json!({
        "baseline": baseline,
        "removed": removed,
        "start": start,
        "warned": warned,
    })
    .to_string();
    if let Ok(mut f) = create_private(&tmp) {
        use std::io::Write;
        if f.write_all(body.as_bytes()).is_ok() && f.flush().is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
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
                let warned = v["warned"].as_bool().unwrap_or(false);
                *state.quota_state.lock().unwrap() = (baseline, start, warned);
                *state.quota_removed.lock().unwrap() =
                    v["removed"].as_u64().unwrap_or(0);
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
        q.1 == 0 || now - q.1 >= (s.quota_hours as i64).saturating_mul(3600)
    };
    if expired {
        *state.quota_state.lock().unwrap() = (quota_meter(state), now, false);
        save_quota(state);
        return false;
    }
    let baseline = state.quota_state.lock().unwrap().0;
    quota_meter(state).saturating_sub(baseline)
        >= (s.quota_mb as u64).saturating_mul(1024 * 1024)
}

/// IDM's "Exit when done": when it is on (globally, or on the queue
/// the finished entry belonged to), downloads ran this session, and
/// nothing is active or queued anymore, the app closes itself.
fn maybe_exit_when_done(app: &AppHandle, state: &AppState, finished_queue: &str) {
    let s = current_settings(state);
    // "Turn off computer when done" (the queue's own action wins over
    // the global one) shares the exit-when-done trigger conditions:
    // downloads ran, nothing is active, queued or awaiting retry
    let qcfg = queue_cfg(&s, finished_queue);
    let power = if !qcfg.power_when_done.is_empty() {
        qcfg.power_when_done.clone()
    } else {
        s.power_when_done.clone()
    };
    if power.is_empty() && !s.exit_when_done && !qcfg.exit_when_done {
        return;
    }
    if !state.had_active.load(Ordering::Relaxed) {
        return;
    }
    // a failed download awaiting its automatic retry is unfinished
    // queue work, not "done": the app used to close itself over a
    // pending retry (and over a user's cancel of the last file)
    if !state.retries.lock().unwrap().is_empty() {
        return;
    }
    // an entry the user PAUSED is unfinished work, not "done":
    // powering down (or closing) over it would strand a download
    // they fully intend to resume tomorrow
    let any_paused = {
        let entries: Vec<Arc<Entry>> =
            state.entries.lock().unwrap().values().cloned().collect();
        entries.iter().any(|e| payload_of(e).status == "paused")
    };
    if any_paused {
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
        if !power.is_empty() {
            // audit trail (F-19): this is the queue/global
            // power-when-done setting (the finished entry's queue
            // wins over the global one) — distinct from the
            // per-entry completion action above
            eprintln!("[ndm] power-when-done firing: {power} (queue: {finished_queue})");
            // a dry-run (test/safety valve) writes its marker but
            // must not then exit as if the machine were going down
            let dry = std::env::var("NDM_POWER_DRY_RUN")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            do_power_action(&power, s.power_force);
            if dry {
                return;
            }
        }
        app.exit(0);
    }
}

/// The per-download "Options on completion" action, run once at finish.
fn run_completion_action(entry: &Entry) {
    let action = entry.completion.lock().unwrap().clone();
    if !action.is_empty() {
        // audit trail (F-19): the value acted on is THIS entry's own
        // stored field — written per entry by set_completion and
        // carried verbatim through every entry rebuild (swap_live,
        // demote_terminal_live, the video relaunch) — never a shared
        // or global slot, so one download's choice cannot fire for
        // another. Log which action fires, for which entry.
        eprintln!(
            "[ndm] completion action for {} ({}): {action}",
            entry.id, entry.name
        );
    }
    // IDM's "Options on completion" also offers turning the computer
    // off (or restarting it) when this download finishes
    match action.as_str() {
        "turn_off_pc" => {
            do_power_action("shutdown", false);
            return;
        }
        "restart_pc" => {
            do_power_action("restart", false);
            return;
        }
        _ => {}
    }
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
        let mut last_progress_write: Option<Instant> = None;
        loop {
            if entry.removed.load(Ordering::Relaxed) {
                break;
            }
            update_sample(&entry);
            let payload = payload_of(&entry);
            // a discard that landed while this payload was being
            // sampled ends the loop BEFORE anything is published:
            // a late progress-file write (or a finish-arm persist)
            // would resurrect the entry the discard just removed
            // (R2-EXT-05)
            if entry.removed.load(Ordering::Relaxed) {
                break;
            }
            let status = payload.status.clone();
            let total_now = payload.total;
            // keep the browser panel's progress file in step with
            // the event stream: ~2 writes/sec while bytes move,
            // and ALWAYS on a status transition (every non-downloading
            // status ends this loop, so the final state must land)
            let progress_due = last_progress_write
                .map(|t| t.elapsed() >= Duration::from_millis(500))
                .unwrap_or(true);
            if status != "downloading" || progress_due {
                write_progress_file(&app, &payload);
                last_progress_write = Some(Instant::now());
            }
            let _ = app.emit("progress", payload);
            if status == "downloading" {
                app.state::<AppState>()
                    .had_active
                    .store(true, Ordering::Relaxed);
            }
            match status.as_str() {
                "finished" => {
                    // "finished" is a claim about a file on disk.
                    // A video job whose output is not there (a
                    // cancel won the finalize race and the file was
                    // removed) must not fire completion actions or
                    // persist as a finished download (R2-EXT-05).
                    if entry.kind == "video" && !resolve_video_file(&entry).is_file() {
                        if let Some(job) = &entry.video {
                            let mut p = job.progress.lock().unwrap();
                            p.status = "error".to_string();
                            p.error = Some(
                                "the download finished but its file is missing from the save folder"
                                    .to_string(),
                            );
                            p.speed_bps = 0.0;
                            p.conns = 0;
                        }
                        let state = app.state::<AppState>();
                        persist(&state);
                        pump_queue(&app);
                        break;
                    }
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
                                "url": entry.url,
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
                    // NOTE: no maybe_exit_when_done here — a user
                    // CANCEL (or an error) is not a "done" transition;
                    // only a genuine finish may close the app or fire
                    // the power action
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
            std::thread::sleep(Duration::from_millis(200));
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
                slot_preclaimed: AtomicBool::new(false),
        removed: AtomicBool::new(stub.removed.load(Ordering::Relaxed)),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(stub.completion.lock().unwrap().clone()),
        queue: Mutex::new(entry_queue(&stub)),
        login: stub.login.clone(),
        password: stub.password.clone(),
        description: Mutex::new(stub.description.lock().unwrap().clone()),
        speed_limit: Mutex::new(*stub.speed_limit.lock().unwrap()),
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
    // racing a manual resume) must not probe and spawn in parallel. A
    // cap-slot reservation made by the add path (slot_preclaimed) is
    // adopted instead of treated as a rival claim.
    let preclaimed = stub.slot_preclaimed.swap(false, Ordering::SeqCst);
    if stub.starting.swap(true, Ordering::SeqCst) && !preclaimed {
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
    // the entry's own speed limit (IDM's per-download limiter tab)
    let own_limit = *stub.speed_limit.lock().unwrap();
    if own_limit > 0 {
        if let Some(d) = &live.download {
            d.set_limit(own_limit);
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
    // 1,000 collisions: the old fallthrough returned the TAKEN first
    // path, putting two entries on one output file and one engine
    // state file. A timestamped name preserves the invariant.
    let stamp = chrono::Utc::now().timestamp_millis();
    let candidate = format!("{stem}-{stamp}{ext}");
    let p = dir.join(&candidate);
    (candidate, p)
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
    // snapshot first, then compute payloads unlocked (payload_of
    // calls into engine code; holding the central lock across it
    // stalls — and could poison — every other path)
    let entries: Vec<Arc<Entry>> = state
        .entries
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    entries
        .iter()
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

/// An entry for the same URL that is FINISHED — backend-verified,
/// file on disk at the recorded size (entry_is_finished is the
/// authority). The active-duplicate check above deliberately
/// skips finished entries, which let the file paths silently
/// re-download a completed file under a "(1)" name (R2-EXT-07;
/// the video path's own check in do_add_video already counts
/// finished copies). Callers surface this exactly like an active
/// duplicate: the existing entry is the answer, not a second copy.
fn find_finished_duplicate(state: &AppState, kind: &str, url: &str) -> Option<String> {
    // snapshot first, payloads/files checked unlocked (see
    // find_active_duplicate for why the lock must not be held)
    let entries: Vec<Arc<Entry>> = state
        .entries
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    entries
        .iter()
        .find(|e| e.kind == kind && e.url == url && entry_is_finished(e))
        .map(|e| e.id.clone())
}

/// An entry for the same URL that sits in ERROR state. A re-add
/// used to slip past both duplicate checks above — the active
/// check excludes errored entries, the finished check only
/// matches finished ones — and silently mint a "(2)" copy,
/// orphaning the failed entry's partial progress (R2-EXT-04).
/// Callers surface the errored entry exactly like the other
/// duplicates: the browser flyout renders its error with a Retry
/// button, the app dialog asks. This check never restarts or
/// replaces the entry itself.
fn find_errored_duplicate(state: &AppState, kind: &str, url: &str) -> Option<String> {
    // snapshot first, payloads computed unlocked (see
    // find_active_duplicate for why the lock must not be held)
    let entries: Vec<Arc<Entry>> = state
        .entries
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    entries
        .iter()
        .find(|e| e.kind == kind && e.url == url && payload_of(e).status == "error")
        .map(|e| e.id.clone())
}

/// Give a just-minted entry id back to the shared sequence when an
/// add fails before the entry ever became a download (R3: every
/// failed add used to burn a number, leaving gaps like dl-1, dl-3,
/// dl-7 missing from the profile). Minting is
/// `fetch_add(1) + 1`, so right after minting `dl-<n>` the counter
/// holds exactly n; the number is reclaimed only when the counter
/// still holds it — a compare-exchange on the high-water mark. A
/// concurrent mint (another entry, a captured-cookies token) makes
/// the exchange fail and the gap simply stays: the scheme prefers
/// a harmless gap over ever reissuing a live entry's id.
fn release_entry_id(state: &AppState, id: &str) {
    let Some(n) = id
        .strip_prefix("dl-")
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return;
    };
    let _ = state
        .seq
        .compare_exchange(n, n - 1, Ordering::Relaxed, Ordering::Relaxed);
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
    // a saved Site Login for this address applies automatically when
    // the caller supplied no credentials of its own — centrally, here,
    // so EVERY registration path (dialog, silent browser catch,
    // grabber, batch) behaves the same, like IDM
    let owned_login: String;
    let owned_pass: String;
    let (login, password) = if login.is_empty() {
        match site_login_for(&current_settings(state), url) {
            Some((l, p)) => {
                owned_login = l;
                owned_pass = p;
                (owned_login.as_str(), owned_pass.as_str())
            }
            None => (login, password),
        }
    } else {
        (login, password)
    };
    let category = category
        .map(|c| c.to_string())
        .unwrap_or_else(|| category_for_with(&base_name, &current_settings(state)));
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
                slot_preclaimed: AtomicBool::new(false),
        removed: AtomicBool::new(false),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(String::new()),
        queue: Mutex::new(queue.to_string()),
        login: login.to_string(),
        password: password.to_string(),
        description: Mutex::new(String::new()),
        speed_limit: Mutex::new(0),
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

/// A live entry whose engine reached a terminal state (error, or a
/// cancel that left the entry in the list) holds a DEAD handle: its
/// workers have exited, so control.resume() restarts nothing — in
/// 1.8.2 that made Resume and every automatic retry on an errored
/// download a silent no-op. Demote it back to a stub carrying its
/// last known progress, so the normal start path builds a fresh
/// engine that resumes from the on-disk state file. Returns the stub
/// when a demotion happened, None otherwise.
fn demote_terminal_live(state: &AppState, entry: &Arc<Entry>) -> Option<Arc<Entry>> {
    let dl = entry.download.as_ref()?;
    if !dl.is_terminal() {
        return None;
    }
    let p = payload_of(entry);
    if p.status == "finished" {
        return None;
    }
    // the display snapshot comes from the engine's own state file
    // when it exists (real block map); the payload's blocks are a
    // downsampled string and cannot seed a Restored record
    let (snap_total, snap_done, snap_blocks, snap_resumable) =
        match ndm::peek_state(&entry.url, &entry.out_path) {
            Some(pk) => (pk.total, pk.done_bytes, pk.blocks, true),
            None => (p.total, p.done_bytes, Vec::new(), false),
        };
    let stub = Arc::new(Entry {
        id: entry.id.clone(),
        name: entry.name.clone(),
        url: entry.url.clone(),
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
            total: snap_total,
            done_bytes: snap_done,
            blocks: snap_blocks,
            resumable: snap_resumable,
            status: "paused".to_string(),
            error: String::new(),
        })),
        queued: AtomicBool::new(entry.queued.load(Ordering::Relaxed)),
        sample: Mutex::new((Instant::now(), snap_done, 0.0)),
        emitting: AtomicBool::new(false),
        starting: AtomicBool::new(false),
                slot_preclaimed: AtomicBool::new(false),
        removed: AtomicBool::new(entry.removed.load(Ordering::Relaxed)),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(entry.completion.lock().unwrap().clone()),
        queue: Mutex::new(entry_queue(entry)),
        login: entry.login.clone(),
        password: entry.password.clone(),
        description: Mutex::new(entry.description.lock().unwrap().clone()),
        speed_limit: Mutex::new(*entry.speed_limit.lock().unwrap()),
    });
    let mut entries = state.entries.lock().unwrap();
    let still_current = entries
        .get(&entry.id)
        .map(|e| Arc::ptr_eq(e, entry))
        .unwrap_or(false);
    if !still_current || entry.removed.load(Ordering::SeqCst) {
        return None;
    }
    entries.insert(entry.id.clone(), stub.clone());
    drop(entries);
    Some(stub)
}

/// Start one entry that is not currently running: a queued/restored HTTP
/// entry gets the engine; a restored video entry gets a fresh yt-dlp job
/// (which continues its .part files); a live-but-paused one just resumes.
fn start_entry(app: &AppHandle, entry: Arc<Entry>) {
    // a finished entry must never be (re)started: resuming its dead
    // handle would spawn a fresh emit loop whose first tick runs
    // the completion action a second time
    if payload_of(&entry).status == "finished" {
        return;
    }
    // a (re)start supersedes the previous attempt's failure: the
    // stored error clears NOW, so progress payloads (and the
    // browser progress file mirrored from them) never carry a
    // stale error alongside a live "downloading" state — the
    // video jobs clear theirs in spawn()/resume(), this covers
    // the restored record every stub start reads from
    if let Some(r) = entry.restored.lock().unwrap().as_mut() {
        r.error.clear();
    }
    if entry.download.is_some() {
        let state = app.state::<AppState>();
        if let Some(stub) = demote_terminal_live(&state, &entry) {
            start_entry(app, stub);
            return;
        }
    }
    if let Some(dl) = &entry.download {
        dl.control.resume();
        // the emit loop died when it observed the pause — without a
        // fresh one this download would report no progress and fire
        // NONE of the finish side-effects (no persist-at-finish, no
        // queue pump, no Complete dialog, no retry registration)
        spawn_emit_loop(app.clone(), entry);
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
        job.set_limit(*entry.speed_limit.lock().unwrap());
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
            &entry.login,
            &entry.password,
            &state.components(),
        ) {
            Ok(job) => {
            job.set_limit(*entry.speed_limit.lock().unwrap());
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
                slot_preclaimed: AtomicBool::new(false),
                removed: AtomicBool::new(entry.removed.load(Ordering::Relaxed)),
                pause_requested: AtomicBool::new(false),
                completion: Mutex::new(entry.completion.lock().unwrap().clone()),
                queue: Mutex::new(entry_queue(&entry)),
                login: entry.login.clone(),
                password: entry.password.clone(),
                description: Mutex::new(entry.description.lock().unwrap().clone()),
                speed_limit: Mutex::new(*entry.speed_limit.lock().unwrap()),
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
                // a transient failure (server down, connection
                // refused) retries itself like any other failure;
                // permanent ones are filtered inside register_retry
                register_retry(&state, &entry);
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
                    if e.pause_requested.load(Ordering::SeqCst) {
                        // the user paused this entry while it was
                        // still probing: the pause wins over the
                        // start failure — the entry records "paused",
                        // not an error, and no automatic retry is
                        // queued behind the user's back
                        *e.restored.lock().unwrap() = Some(Restored {
                            total: 0,
                            done_bytes: 0,
                            blocks: Vec::new(),
                            resumable: false,
                            status: "paused".to_string(),
                            error: String::new(),
                        });
                        let _ = app.emit("progress", payload_of(&e));
                    } else {
                        *e.restored.lock().unwrap() = Some(Restored {
                            total: 0,
                            done_bytes: 0,
                            blocks: Vec::new(),
                            resumable: false,
                            status: "error".to_string(),
                            error: start_err,
                        });
                        let _ = app.emit("progress", payload_of(&e));
                        // probe/start failures (a server that is down or
                        // refused the connection) never reach an emit
                        // loop, so nothing ever registered them for the
                        // automatic retry every other failure gets —
                        // they sat dead until a manual Resume. Register
                        // here; dead links (404/410) stay excluded by
                        // error_is_permanent.
                        register_retry(&state, &e);
                    }
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

/// The simultaneous-download cap that applies to one queue: the
/// queue's own cap when it sets one, else the global setting —
/// the same rule pump_queue applies, so direct adds, the pump and
/// the scheduler triggers never disagree about a queue's limit.
fn effective_cap(s: &Settings, queue: &str) -> usize {
    let cfg = queue_cfg(s, queue);
    if cfg.max_simultaneous > 0 {
        cfg.max_simultaneous
    } else {
        s.max_simultaneous
    }
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
                && (e.starting.load(Ordering::Relaxed)
                    || e.slot_preclaimed.load(Ordering::Relaxed)
                    || payload_of(e).status == "downloading")
        })
        .count()
}

/// The backend's own verdict on whether an entry is finished: the
/// live engine/job state first, then the restored record checked
/// against the file actually on disk (a download that completed
/// just before a crash can sit in the list as "paused" with every
/// byte present). Callers that act on "finished" — file deletion,
/// dialog discards — must use THIS, never a status cached by the
/// UI or the browser (F-21, F-EXT-12).
fn entry_is_finished(entry: &Entry) -> bool {
    let payload = payload_of(entry);
    if payload.status == "finished" {
        // "finished" is a claim about a file on disk, whoever made
        // it — a live engine that just completed, or a record
        // restored from downloads.json. The restored claim used to
        // be trusted outright here, so the disk check below was
        // never reached for exactly the records it exists for: a
        // file deleted while the app was closed restored as
        // "Finished", and demote_phantom_finished was a no-op
        // (R3). The claim is honoured only when the output file
        // verifiably exists at the recorded size.
        return finished_file_on_disk(entry, payload.total);
    }
    if entry.download.is_some() || entry.video.is_some() {
        return false;
    }
    let (total, done) = match &*entry.restored.lock().unwrap() {
        Some(r) => (r.total, r.done_bytes),
        None => (0, 0),
    };
    if total == 0 || done < total {
        return false;
    }
    finished_file_on_disk(entry, total)
}

/// The disk half of the finished verdict: the entry's output file
/// exists and matches the recorded total. A video's merged output
/// may legitimately be smaller than the recorded total — the
/// record can predate the finalize step that rewrites the total
/// to the merged size (a crash in between, a record written by an
/// older build) — so for video a present, non-empty file at or
/// under the total counts. A missing or empty file never does,
/// for any kind.
fn finished_file_on_disk(entry: &Entry, total: u64) -> bool {
    let path = if entry.kind == "video" {
        resolve_video_file(entry)
    } else {
        entry.out_path.clone()
    };
    let Ok(m) = path.metadata() else { return false };
    if !m.is_file() || m.len() == 0 {
        return false;
    }
    if total == 0 || m.len() == total {
        return true;
    }
    entry.kind == "video" && m.len() < total
}

/// The terminal snapshot of a live handle whose engine/job already
/// reached a terminal state: (status, total, done_bytes, error).
fn terminal_snapshot(entry: &Entry) -> Option<(String, u64, u64, String)> {
    if let Some(dl) = &entry.download {
        if !dl.is_terminal() {
            return None;
        }
        let p = dl.progress();
        let status = p.status.as_str().to_string();
        if status == "downloading" || status == "paused" {
            return None;
        }
        return Some((status, p.total, p.done_bytes, p.error.unwrap_or_default()));
    }
    if let Some(job) = &entry.video {
        let p = job.progress.lock().unwrap().clone();
        if matches!(p.status.as_str(), "finished" | "error" | "cancelled") {
            return Some((
                p.status.clone(),
                p.total,
                p.done,
                p.error.clone().unwrap_or_default(),
            ));
        }
    }
    None
}

/// State-honesty sweep, run at the top of every pump (F-15/F-16):
/// an entry whose recorded state claims something no live handle
/// backs — or whose live handle reached a terminal state nobody
/// processed because its emit loop died first — is reconciled with
/// reality instead of sitting in a fake-live state forever.
fn reconcile_wedged(app: &AppHandle, state: &AppState) {
    let entries: Vec<Arc<Entry>> =
        state.entries.lock().unwrap().values().cloned().collect();
    let mut dirty = false;
    for e in &entries {
        if e.removed.load(Ordering::Relaxed) || e.starting.load(Ordering::Relaxed) {
            continue;
        }
        if e.download.is_none() && e.video.is_none() {
            // no live handle at all: a record claiming a live state
            // ("downloading", or "queued" with the flag lost) can
            // never move on its own — hand it back to the queue so
            // the claim loop below starts it (a failed start then
            // surfaces as a visible error, not a silent wedge). A
            // pause the user asked for is never overridden.
            if !e.queued.load(Ordering::Relaxed) && !e.pause_requested.load(Ordering::SeqCst) {
                let st = e
                    .restored
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|r| r.status.clone());
                if matches!(st.as_deref(), Some("downloading") | Some("queued")) {
                    e.queued.store(true, Ordering::Relaxed);
                }
            }
            continue;
        }
        // live handle, terminal engine/job, but the terminal state
        // was never recorded (the emit loop exited before its finish
        // arm — e.g. around a pause/resume inside the detail
        // dialog): persist the truth now, so the list, the payload
        // stream and downloads.json stop disagreeing (F-16).
        if let Some((status, total, done, error)) = terminal_snapshot(e) {
            let mut r = e.restored.lock().unwrap();
            let recorded = r
                .as_ref()
                .map(|x| x.status == status && x.done_bytes == done)
                .unwrap_or(false);
            if !recorded {
                *r = Some(Restored {
                    total,
                    done_bytes: done,
                    blocks: Vec::new(),
                    resumable: true,
                    status,
                    error,
                });
                drop(r);
                let _ = app.emit("progress", payload_of(e));
                dirty = true;
            }
        }
    }
    if dirty {
        persist(state);
    }
}

/// Does this queue carry an enabled scheduler window? (A queue's
/// window only exists when the user ticked "Use schedule" for it in
/// the Scheduler dialog; start/stop always hold values, enabled or
/// not, so the flag is the whole test.)
fn queue_has_schedule(s: &Settings, queue: &str) -> bool {
    queue_cfg(s, queue).use_schedule
}

/// "Download Later" answers "not NOW", not "never": when the target
/// queue has an enabled schedule, a later-entry belongs to that
/// schedule and is created QUEUED, so the pump claims it the moment
/// the window opens — IDM's primary scheduler workflow (stock the
/// queue now, the schedule starts it later). The pump refuses
/// out-of-window claims via entry_blocks, so a queued later-entry
/// can never start early. Only a queue with NO schedule keeps the
/// parked behaviour: paused until the user starts it by hand (R5-5a:
/// later-entries used to be created un-queued unconditionally, so a
/// scheduled queue's own schedule could never see them at all).
fn later_entry_queued(later: bool, queue_scheduled: bool) -> bool {
    later && queue_scheduled
}

/// Was this entry parked before its first start and never run since?
/// This is the state that separates "Download Later" from "the user
/// paused it": a download the user paused mid-flight either still
/// holds its live handle or has bytes on the record (done_bytes > 0);
/// a parked later-entry has neither — no handle, a "paused" record,
/// zero bytes. The pause latch is part of the test in-session: the
/// one deliberate press that can land on a parked entry (Pause in
/// the list) sets it, and a latched entry is the user's explicit
/// stop, never the schedule's to adopt. (After an app restart the
/// latch is gone — restores clear it — so a pre-restart pause of a
/// zero-byte entry is indistinguishable from a fresh Download Later;
/// that residue is accepted, the alternative would need a new
/// persisted field for a corner the in-session state already covers.)
fn never_started_parked(e: &Entry) -> bool {
    if e.download.is_some() || e.video.is_some() || e.queued.load(Ordering::Relaxed) {
        return false;
    }
    if e.pause_requested.load(Ordering::SeqCst) {
        return false;
    }
    match &*e.restored.lock().unwrap() {
        Some(r) => r.status == "paused" && r.done_bytes == 0,
        None => false,
    }
}

/// Bring a scheduled queue's parked Download-Later entries under its
/// schedule: they become queued, and the existing window machinery
/// (the pump outside the window refuses, the window-open edge starts)
/// takes it from there. Covers the entries the add-path rule cannot
/// reach: parked before the queue had a schedule, or moved into the
/// queue while parked. Only never-started entries are adopted (see
/// never_started_parked) — a file the user paused after it ran is
/// never auto-started by this.
fn adopt_parked_entries(app: &AppHandle, state: &AppState, s: &Settings, queue: &str) -> bool {
    if !queue_has_schedule(s, queue) {
        return false;
    }
    let order = state.order.lock().unwrap().clone();
    let mut adopted = false;
    for id in &order {
        if let Some(e) = state.get(id) {
            if entry_queue(&e) != queue || !never_started_parked(&e) {
                continue;
            }
            e.queued.store(true, Ordering::Relaxed);
            if let Some(r) = e.restored.lock().unwrap().as_mut() {
                r.status = "queued".to_string();
            }
            let _ = app.emit("progress", payload_of(&e));
            adopted = true;
        }
    }
    if adopted {
        persist(state);
    }
    adopted
}

/// Claim a queued entry for a pump/scheduler start. A successful
/// claim also clears the pause latch: every legitimate pause of a
/// handle-less entry either dequeues it (pause_download) or is
/// consumed by the start path itself the moment the entry goes live,
/// so a latch still set on an entry that stayed queued until claim
/// time is stale by construction — exactly the R5-5b poison, where a
/// schedule edit latched a merely-queued entry and its own window
/// then paused it after a single block.
fn claim_queued(e: &Entry) -> bool {
    if e.queued.swap(false, Ordering::SeqCst) {
        e.pause_requested.store(false, Ordering::SeqCst);
        true
    } else {
        false
    }
}

/// Fill free simultaneous-download slots from the queues. The cap is
/// per queue (IDM: each queue downloads N files at a time).
fn pump_queue(app: &AppHandle) {
    let state = app.state::<AppState>();
    reconcile_wedged(app, &state);
    let s = current_settings(&state);
    let order = state.order.lock().unwrap().clone();
    for cfg in &s.queues {
        if entry_blocks(&state, &cfg.name) {
            continue;
        }
        // a queue's own cap wins; 0 falls back to the global cap
        // (IDM's Scheduler sets the simultaneous count per queue)
        let max = if cfg.max_simultaneous > 0 {
            cfg.max_simultaneous
        } else {
            s.max_simultaneous
        };
        if max == 0 {
            for id in &order {
                if let Some(e) = state.get(id) {
                    if entry_queue(&e) == cfg.name && claim_queued(&e) {
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
            if claim_queued(&e) {
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
            if claim_queued(&e) {
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
            // The pause latch exists for ONE case: a start genuinely
            // in flight (probe running, live handle not installed
            // yet) — it makes that start honour the pause the moment
            // the entry goes live, instead of downloading outside
            // the window this pause was meant to close. A MERELY
            // QUEUED or parked entry must NOT be latched (R5-5b):
            // its queued state already keeps it from starting
            // outside the window (the pump refuses out-of-window
            // claims), and the stale latch survived into its own
            // window, pausing it immediately after the first block
            // when the scheduler claimed it there.
            let start_in_flight = e.starting.load(Ordering::Relaxed)
                || e.slot_preclaimed.load(Ordering::Relaxed);
            if e.download.is_none() && e.video.is_none() && start_in_flight {
                e.pause_requested.store(true, Ordering::SeqCst);
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
            if !requeue
                && e.download.is_none()
                && e.video.is_none()
                && !start_in_flight
                && e.queued.swap(false, Ordering::Relaxed)
            {
                // a manual Stop Queue Now on a merely-queued entry:
                // dequeue it and park it paused (the same shape
                // pause_download uses for a not-running entry), so
                // the stop holds — without the latch trick, the pump
                // would simply claim it again on the next tick
                let mut r = e.restored.lock().unwrap();
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
    apply_proxy_to_engine(&s);
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
    // a schedule switched on (or pointed at a queue that already
    // holds parked Download-Later entries) adopts those entries now
    // (R5-5a): they join the queue and wait for their window
    for cfg in &s.queues {
        adopt_parked_entries(&app, &state, &s, &cfg.name);
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
    *entry.queue.lock().unwrap() = queue.clone();
    persist(&state);
    let _ = app.emit("progress", payload_of(&entry));
    // an entry moved INTO a scheduled queue while parked as Download
    // Later joins that queue's schedule (R5-5a) instead of sitting
    // paused in it forever
    adopt_parked_entries(&app, &state, &s, &queue);
    pump_queue(&app);
    Ok(())
}

/// Reorder an entry within its own queue (the Scheduler's "Files in
/// the queue" tab — IDM starts queued files in list order, so the
/// order IS the priority). Swaps with the previous/next entry of the
/// same queue in the global order list and persists.
#[tauri::command]
fn move_in_queue(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    up: bool,
) -> Result<(), String> {
    let entry = state.get(&id).ok_or("unknown download id")?;
    let q = entry_queue(&entry);
    let mut order = state.order.lock().unwrap();
    let pos = order
        .iter()
        .position(|x| x == &id)
        .ok_or("entry is not in the order list")?;
    // find the nearest neighbour in the same queue, in the requested
    // direction (entries of other queues in between don't matter —
    // the pump claims per queue in this order)
    let neighbour = if up {
        (0..pos)
            .rev()
            .find(|&i| state.get(&order[i]).map(|e| entry_queue(&e) == q).unwrap_or(false))
    } else {
        (pos + 1..order.len())
            .find(|&i| state.get(&order[i]).map(|e| entry_queue(&e) == q).unwrap_or(false))
    };
    if let Some(n) = neighbour {
        order.swap(pos, n);
    }
    drop(order);
    persist(&state);
    let _ = app.emit("progress", payload_of(&entry));
    Ok(())
}

/// A link dropped on the drop basket (or sent by it): offered in the
/// main window's Add URL dialog, exactly like an extension catch.
#[tauri::command]
fn basket_add(app: AppHandle, state: State<'_, AppState>, url: String) -> Result<(), String> {
    let url = url.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("not a web link".to_string());
    }
    let payload = serde_json::json!({
        "url": url,
        "title": "",
        "pageUrl": "",
        "cookies": "",
        "source": "basket",
    });
    state.pending_files.lock().unwrap().push(payload.clone());
    raise_main_window(&app);
    let _ = app.emit("incoming_file", payload);
    Ok(())
}

/// Show or hide the drop-basket window (View menu / basket's own
/// close button).
#[tauri::command]
fn set_basket_visible(app: AppHandle, visible: bool) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("basket") {
        if visible {
            let _ = w.show();
        } else {
            let _ = w.hide();
        }
    }
    Ok(())
}

/// Set one download's own speed limit (bytes/sec, 0 = none) — IDM's
/// per-download Speed Limiter tab. Applies to the live engine/job
/// immediately and persists with the entry.
#[tauri::command]
fn set_entry_limit(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    limit: u64,
) -> Result<(), String> {
    let entry = state.get(&id).ok_or("unknown download id")?;
    *entry.speed_limit.lock().unwrap() = limit;
    if let Some(dl) = &entry.download {
        dl.set_limit(limit);
    }
    if let Some(job) = &entry.video {
        job.set_limit(limit);
    }
    persist(&state);
    let _ = app.emit("progress", payload_of(&entry));
    Ok(())
}

/// Edit one download's Description (IDM's File Properties field).
#[tauri::command]
fn set_entry_description(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    description: String,
) -> Result<(), String> {
    let entry = state.get(&id).ok_or("unknown download id")?;
    *entry.description.lock().unwrap() = description.chars().take(500).collect::<String>();
    persist(&state);
    let _ = app.emit("progress", payload_of(&entry));
    Ok(())
}

#[tauri::command]
fn scheduler_start_now(app: AppHandle, state: State<'_, AppState>, queue: Option<String>) {
    let q = queue.unwrap_or_else(main_queue_name);
    state.sched_override.lock().unwrap().insert(q.clone());
    // "Start Queue Now" runs the queue — its parked Download-Later
    // entries are part of the queue, so adopt them first (R5-5a);
    // a no-op for queues without a schedule
    adopt_parked_entries(&app, &state, &current_settings(&state), &q);
    // the QUEUE's effective cap decides, not the global value: a
    // queue with its own cap fills its slots through the pump
    // instead of flooding past its simultaneous limit
    if effective_cap(&current_settings(&state), &q) == 0 {
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

/// Serializes "cap check + slot reservation" across every direct
/// start path: two starts landing together must not both pass the
/// simultaneous-cap check before either has claimed its slot.
static START_GATE: Mutex<()> = Mutex::new(());

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
    description: Option<String>,
    total: Option<u64>,
) -> Result<String, String> {
    if state.update_required.load(Ordering::SeqCst) {
        return Err("NDM needs its latest update before it can download. Please install the update first.".to_string());
    }
    if !force.unwrap_or(false) {
        if let Some(existing) = find_active_duplicate(&state, "http", &url) {
            return Err(format!("duplicate:{existing}"));
        }
        // a FINISHED copy is a duplicate too (R2-EXT-07): the
        // dialog asks about it exactly like an active one instead
        // of silently minting a "(1)" file
        if let Some(existing) = find_finished_duplicate(&state, "http", &url) {
            return Err(format!("duplicate:{existing}"));
        }
        // and an ERRORED copy (R2-EXT-04): the dialog asks about
        // the entry that already failed instead of minting a "(2)"
        // copy that orphans its partial progress
        if let Some(existing) = find_errored_duplicate(&state, "http", &url) {
            return Err(format!("duplicate:{existing}"));
        }
    }
    // a saved Site Login for this address applies automatically when
    // the caller did not supply its own credentials (IDM behaviour)
    let (login, password) = if login.as_deref().unwrap_or("").is_empty() {
        match site_login_for(&current_settings(&state), &url) {
            Some((l, p)) => (Some(l), Some(p)),
            None => (login, password),
        }
    } else {
        (login, password)
    };
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
    if let Some(d) = &description {
        *stub.description.lock().unwrap() = d.chars().take(500).collect();
    }
    // the dialog already probed this link before asking: carry the
    // probed size into the entry at creation, so the list shows it
    // immediately — including for Download Later, whose entries used
    // to sit at "Unknown" until their first start (F-03)
    let probed_total = total.unwrap_or(0);
    if probed_total > 0 {
        let mut r = stub.restored.lock().unwrap();
        match r.as_mut() {
            Some(r) => {
                if r.total == 0 {
                    r.total = probed_total;
                }
            }
            None => {
                *r = Some(Restored {
                    total: probed_total,
                    done_bytes: 0,
                    blocks: Vec::new(),
                    resumable: true,
                    status: "queued".to_string(),
                    error: String::new(),
                });
            }
        }
    }
    let id = stub.id.clone();
    let is_later = later.unwrap_or(false);
    // a Download Later aimed at a queue WITH an enabled schedule
    // joins that schedule instead of parking (see later_entry_queued,
    // R5-5a): created queued below, claimed by the pump when the
    // window opens — never now, never early
    let later_joins_schedule = later_entry_queued(
        is_later,
        queue_has_schedule(&current_settings(&state), &entry_queue(&stub)),
    );
    let max = effective_cap(&current_settings(&state), &entry_queue(&stub));
    // cap check + slot reservation under one gate: two adds landing
    // together used to both pass the check before either start
    // claimed its slot, overshooting the queue's cap
    let must_wait = {
        let _gate = START_GATE.lock().unwrap();
        let wait = is_later
            || entry_blocks(&state, &entry_queue(&stub))
            || (max > 0 && count_active_for(&state, &entry_queue(&stub)) >= max);
        if !wait {
            stub.slot_preclaimed.store(true, Ordering::SeqCst);
        }
        wait
    };
    if must_wait {
        if is_later && !later_joins_schedule {
            // "Download Later" into a queue with NO schedule stays
            // paused until the user explicitly starts/resumes it
            stub.queued.store(false, Ordering::Relaxed);
            *stub.restored.lock().unwrap() = Some(Restored {
                total: probed_total,
                done_bytes: 0,
                blocks: Vec::new(),
                resumable: true,
                status: "paused".to_string(),
                error: String::new(),
            });
        } else {
            // Scheduler or simultaneous cap: register as queued so the pump starts it when a slot frees up.
            // A Download Later that joined its queue's schedule lands
            // here too — queued, and the pump claims it at the window.
            stub.queued.store(true, Ordering::Relaxed);
            // the queued state IS the entry's record (same shape
            // the video path and Download Later use): without one,
            // an unprobed queued entry existed only as the flag —
            // persist now keeps it via that flag too, but the
            // record keeps the in-memory state identical to what a
            // restart restores
            let mut r = stub.restored.lock().unwrap();
            if r.is_none() {
                *r = Some(Restored {
                    total: probed_total,
                    done_bytes: 0,
                    blocks: Vec::new(),
                    resumable: true,
                    status: "queued".to_string(),
                    error: String::new(),
                });
            }
        }
        let _ = app.emit("progress", payload_of(&stub));
        persist(&state);
        return Ok(id);
    }
    let first_start = start_engine(app.clone(), &stub).await;
    let first_start = match first_start {
        Err(e1) => {
            // IDM's Proxy-tab fallback: for a download intercepted
            // from the browser, one retry through the system proxy
            // when the configured route could not reach the server
            let s = current_settings(&state);
            let sys = system_proxy_url();
            if s.proxy_from_browser_on_error
                && !sys.is_empty()
                && sys != effective_proxy(&s)
                && (!stub.cookies.is_empty() || !stub.referer.is_empty())
            {
                ndm::set_global_proxy(&sys);
                match start_engine(app.clone(), &stub).await {
                    Ok(v) => {
                        // the started engine captured the system-
                        // proxy agent when it was built; the session
                        // proxy goes back NOW, on success too — it
                        // used to be restored only on failure, so one
                        // fallback quietly rerouted every later
                        // download through the browser's proxy
                        ndm::set_global_proxy(&effective_proxy(&s));
                        Ok(v)
                    }
                    Err(_) => {
                        ndm::set_global_proxy(&effective_proxy(&s));
                        Err(e1)
                    }
                }
            } else {
                Err(e1)
            }
        }
        ok => ok,
    };
    match first_start {
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
                // the add never became a download: hand its id
                // number back, so a failed add leaves neither a
                // placeholder entry nor a gap in the numbering
                release_entry_id(&state, &id);
            }
            persist(&state);
            // a definitive server answer (404/410/…) is reported
            // as what it is, not as a reachability failure (N-07)
            Err(plain_server_error(&e))
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
        // Record the press UNCONDITIONALLY: a start may already be
        // claimed by the pump but not yet inside start_engine's own
        // claim, and a pause landing in that gap used to be silently
        // overwritten when the live entry was installed. The start
        // paths honour pause_requested the moment the download goes
        // live; an explicit Resume clears it first.
        entry.pause_requested.store(true, Ordering::SeqCst);
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
    if state.update_required.load(Ordering::SeqCst) {
        // the mandatory-update block used to leak through Resume
        // (and the retry thread) while add paths enforced it
        return Err("NDM needs its latest update before it can download. Please install the update first.".to_string());
    }
    // a manual Resume always takes the entry out of the queue
    entry.queued.store(false, Ordering::Relaxed);
    entry.pause_requested.store(false, Ordering::SeqCst);
    // an errored live entry holds a dead engine handle: demote it to
    // a stub so the start below builds a fresh engine (control.resume
    // on the dead handle restarted nothing)
    let entry = match demote_terminal_live(&state, &entry) {
        Some(stub) => stub,
        None => entry,
    };
    if let Some(dl) = &entry.download {
        dl.control.resume();
        let _ = app.emit("progress", payload_of(&entry));
        // same emit-loop revival as start_entry: a resumed HTTP
        // download must keep reporting and must finish properly
        spawn_emit_loop(app, entry);
        Ok(())
    } else if entry.kind == "video" {
        // live job resumes in place; a restored entry starts a fresh job
        // with the same arguments and yt-dlp continues its .part files
        if let Some(job) = &entry.video {
            job.set_limit(*entry.speed_limit.lock().unwrap());
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
                &entry.login,
                &entry.password,
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
                slot_preclaimed: AtomicBool::new(false),
                removed: AtomicBool::new(entry.removed.load(Ordering::Relaxed)),
                pause_requested: AtomicBool::new(false),
                completion: Mutex::new(entry.completion.lock().unwrap().clone()),
                queue: Mutex::new(entry_queue(&entry)),
                login: entry.login.clone(),
                password: entry.password.clone(),
                description: Mutex::new(entry.description.lock().unwrap().clone()),
                speed_limit: Mutex::new(*entry.speed_limit.lock().unwrap()),
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
        match start_engine(app.clone(), &entry).await {
            Ok(_) => Ok(()),
            Err(e) => {
                // mirror start_entry's failure handling: a failed
                // start must surface on the entry as a visible error
                // with its reason and an automatic retry — it used
                // to vanish into the caller, leaving a stale status
                *entry.restored.lock().unwrap() = Some(Restored {
                    total: 0,
                    done_bytes: 0,
                    blocks: Vec::new(),
                    resumable: false,
                    status: "error".to_string(),
                    error: e.clone(),
                });
                let _ = app.emit("progress", payload_of(&entry));
                register_retry(&state, &entry);
                persist(&state);
                Err(e)
            }
        }
    }
}

#[tauri::command]
fn remove_download(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    delete_file: Option<bool>,
) -> Result<(), String> {
    state.retries.lock().unwrap().remove(&id);
    delete_progress_file(&app, &id);
    if let Some(entry) = state.get(&id) {
        // judged BEFORE anything is cancelled: cancelling a video
        // job rewrites its status to "cancelled", which would make
        // a finished entry look unfinished to the check below
        let was_finished = entry_is_finished(&entry);
        // its downloaded bytes stay on the quota meter even though
        // the entry leaves the list (deleting must not refund quota)
        quota_note_removed(&state, &entry);
        entry.removed.store(true, Ordering::SeqCst);
        // settle BEFORE any file is touched: the engines write
        // their resume state one last time on the way out, and the
        // old cancel-then-immediately-delete raced that write —
        // the journal survived the delete and unique_out_path then
        // treated the name as taken forever (R3). discard_entry
        // (the browser dialog's path) already settled this way;
        // the app's own removal now gets the same treatment.
        settle_entry_engines(&entry);
        // the engine's state file is our own bookkeeping: leaving it
        // behind would make unique_out_path treat the path as taken
        // forever (phantom "name (1)" on every future re-add). The
        // partial data file itself is the user's — it stays.
        sweep_engine_state_file(&entry);
        // a video entry's intermediates are NDM's own working
        // files, not user data: deleting a mid-download video used
        // to orphan a full-size .ndm_stream part + its journal in
        // the save folder (R3). The sweep covers exactly the
        // discard set (stream parts, mux/audio temps, yt-dlp
        // parts — journals ride along by prefix); the entry's
        // final file follows the ordinary rule below.
        if entry.kind == "video" && !was_finished {
            sweep_video_temps(&entry);
        }
        // a captured-cookies file belongs to this entry alone and holds
        // the user's browser session: it must not outlive the entry
        if let Some(path) = state.cookies_path(&entry.cookies) {
            let _ = std::fs::remove_file(path);
        }
        // IDM's delete confirmation offers "also delete the file":
        // only then does the downloaded data itself go (the plain
        // list removal above always keeps it). The finished half of
        // that decision is the BACKEND's, taken from the entry's
        // live truth — never from a status the caller cached: a
        // batch delete keyed on the UI's stale "Paused" silently
        // kept a finished 800 MB file the user had explicitly asked
        // to delete (F-21). An unfinished entry's partial stays,
        // exactly like a plain removal.
        if delete_file.unwrap_or(false) && was_finished {
            if entry.kind == "video" {
                let f = resolve_video_file(&entry);
                let _ = std::fs::remove_file(&f);
            } else {
                let _ = std::fs::remove_file(&entry.out_path);
            }
        }
        // the emit loop is stopped and the entry is leaving: its
        // progress mirror must not survive it (the delete at the
        // top of this command ran before the settle, when a last
        // emit could still rewrite it — discard_entry does the
        // same at its end)
        delete_progress_file(&app, &id);
    }
    state.entries.lock().unwrap().remove(&id);
    state.order.lock().unwrap().retain(|x| x != &id);
    // same removal signal discard_entry emits: this command is
    // normally invoked BY the window (which drops the row itself),
    // but the list must never depend on which path removed the
    // entry (R3 X-REF)
    let _ = app.emit("entry_removed", serde_json::json!({ "id": id }));
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
    if !matches!(
        action.as_str(),
        "" | "open_file" | "open_folder" | "turn_off_pc" | "restart_pc"
    ) {
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
        // panic-isolated: a panic in the setup loop must not leave
        // components_busy latched true (video would silently never
        // become available until a restart)
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loop {
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
        }))
        .unwrap_or_else(|_| Err("the video component setup failed unexpectedly".to_string()));
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
    let dir = home_dir().join("Downloads");
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
        // panic-isolated: a panic anywhere in the download/install
        // chain must not leave update_busy latched true (which would
        // silently disable updates for the rest of the session)
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ndm::updater::download_file(&info.deb_url, &dest, &info.deb_sha256, &cb)
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
            })
        }))
        .unwrap_or_else(|_| Err("the update failed unexpectedly".to_string()));
        {
            let state = app.state::<AppState>();
            state.update_busy.store(false, Ordering::SeqCst);
            if matches!(outcome, Ok("installed")) {
                // the mandatory-update block has served its purpose:
                // clear it in-process (it used to stay latched, so the
                // still-running old binary kept refusing downloads
                // with no hint that a restart completes the update),
                // and tell the UI to offer that restart
                state.update_required.store(false, Ordering::SeqCst);
                let _ = app.emit(
                    "update_installed",
                    serde_json::json!({ "version": info.version }),
                );
            }
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

#[tauri::command]
fn take_pending_grabs(state: State<'_, AppState>) -> Vec<serde_json::Value> {
    std::mem::take(&mut state.pending_grabs.lock().unwrap())
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
    // the refreshed entry restarts from byte 0: the bytes it already
    // downloaded stay on the quota meter (the same accounting the
    // remove paths use), or refreshing would refund spent budget
    quota_note_removed(&state, &entry);
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
                slot_preclaimed: AtomicBool::new(false),
        removed: AtomicBool::new(false),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(entry.completion.lock().unwrap().clone()),
        queue: Mutex::new(entry_queue(&entry)),
        login: entry.login.clone(),
        password: entry.password.clone(),
                description: Mutex::new(entry.description.lock().unwrap().clone()),
                speed_limit: Mutex::new(*entry.speed_limit.lock().unwrap()),
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
        .map_err(|e| format!("internal join error: {e}"))?
        // a definitive server answer is reported as what it is,
        // not as a reachability failure (N-07)
        .map_err(|e| plain_server_error(&e))?;
    let suggested = probe
        .suggested_name
        .unwrap_or_else(|| file_name_for(&url_trimmed));
    let category = category_for_with(&suggested, &current_settings(&state));
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

fn unique_video_name(state: &AppState, dir: &Path, raw_title: &str, audio: bool) -> String {
    let base_stem = video::safe_filename_stem(raw_title);
    if base_stem.is_empty() {
        return raw_title.trim().to_string();
    }
    // A stem is taken only by this entry's OWN final filename: a
    // same-stem file or live entry of the other kind (the video half
    // of a download-all pair vs its audio half) writes a different
    // filename and must not force a spurious "(1)" (R3-EXT-04).
    let taken = |cand: &str| -> bool {
        video::stem_taken_on_disk(dir, cand, audio)
            || state.entries.lock().unwrap().values().any(|e| {
                e.kind == "video"
                    && e.audio == audio
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
        // snapshot first, payloads computed unlocked (see
        // find_active_duplicate for why the lock must not be held)
        let entries: Vec<Arc<Entry>> = state
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let dupe = entries.iter().find(|e| {
            e.kind == "video"
                && e.url == url
                && e.out_path == dir
                && e.selector == selector
                && e.audio == audio
                // a FINISHED copy counts as a duplicate too
                // (F-EXT-07): re-adding it used to slip past this
                // check and silently re-download everything under a
                // "(1)" name, so the browser card's "already in your
                // download list" state could never appear. An
                // ERRORED copy counts as well (R2-EXT-04): the
                // caller surfaces the existing entry — the browser
                // flyout shows its error with a Retry button —
                // instead of silently minting a "(2)" copy that
                // orphans the failed entry's partial progress. Only
                // cancelled entries fall through: a cancel was a
                // deliberate discard, so a fresh add is a fresh
                // start. (Plain file downloads keep their
                // rename-on-re-add behaviour in
                // find_active_duplicate's callers.)
                && !matches!(payload_of(e).status.as_str(), "cancelled")
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
    let name = unique_video_name(state, &dir, &raw_name, audio);
    let category = if audio {
        "Music".to_string()
    } else {
        "Video".to_string()
    };
    let id = format!("dl-{}", state.seq.fetch_add(1, Ordering::Relaxed) + 1);
    // a saved Site Login for this address applies to video
    // downloads too: it is stored on the entry and travels into
    // the job, which passes it to yt-dlp (--username/--password),
    // so a login-gated page resolves the way a browser tab would
    let (login, password) =
        site_login_for(&current_settings(state), &url).unwrap_or_default();
    let max = effective_cap(&current_settings(state), &queue);
    // A video that arrives while the engine itself is still
    // downloading waits in the queue like any other not-now download —
    // it must not be rejected for a condition the user cannot fix.
    let engine_missing = video::find_tool("yt-dlp", &state.components()).is_none();
    // a Download Later aimed at a queue WITH an enabled schedule
    // joins that schedule instead of parking (see later_entry_queued,
    // R5-5a): created queued below, claimed by the pump at the window
    let joins_schedule = later_entry_queued(later, queue_has_schedule(&current_settings(state), &queue));
    let should_wait = later
        || engine_missing
        || entry_blocks(state, &queue)
        || (max > 0 && count_active_for(state, &queue) >= max);
    if should_wait {
        let initial_status = if later && !joins_schedule { "paused" } else { "queued" };
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
            queued: AtomicBool::new(!later || joins_schedule),
            sample: Mutex::new((Instant::now(), 0, 0.0)),
            emitting: AtomicBool::new(false),
            starting: AtomicBool::new(false),
                slot_preclaimed: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            completion: Mutex::new(String::new()),
            queue: Mutex::new(queue.clone()),
            login: login.clone(),
            password: password.clone(),
            description: Mutex::new(String::new()),
            speed_limit: Mutex::new(0),
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
    let job = match video::VideoJob::start(
        &url,
        &dir,
        &selector,
        audio,
        &name,
        state.cookies_path(&cookies_token),
        &login,
        &password,
        &state.components(),
    ) {
        Ok(job) => job,
        Err(e) => {
            // the entry was never inserted: the add failed before
            // it became a download, so its id number goes back too
            release_entry_id(state, &id);
            return Err(e);
        }
    };
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
                slot_preclaimed: AtomicBool::new(false),
        removed: AtomicBool::new(false),
        pause_requested: AtomicBool::new(false),
        completion: Mutex::new(String::new()),
        queue: Mutex::new(queue.clone()),
        login,
        password,
            description: Mutex::new(String::new()),
            speed_limit: Mutex::new(0),
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
    description: Option<String>,
) -> Result<String, String> {
    let id = do_add_video(
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
    )?;
    if let Some(d) = &description {
        if let Some(e) = state.get(&id) {
            *e.description.lock().unwrap() = d.chars().take(500).collect();
            persist(&state);
        }
    }
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
async fn grab_page(state: State<'_, AppState>, url: String) -> Result<GrabPageDto, String> {
    let settings = current_settings(&state);
    let page = tauri::async_runtime::spawn_blocking(move || ndm::grab_page(&url))
        .await
        .map_err(|e| format!("internal join error: {e}"))??;
    Ok(GrabPageDto {
        title: page.title,
        items: page
            .items
            .into_iter()
            .map(|it| GrabItemDto {
                kind: category_for_with(&it.name, &settings),
                url: it.url,
                name: it.name,
                size: it.size,
                label: it.label,
            })
            .collect(),
    })
}

#[derive(Clone, Deserialize)]
struct GrabExploreArgs {
    url: String,
    #[serde(default)]
    depth_same: u32,
    #[serde(default)]
    depth_other: u32,
    #[serde(default)]
    whole_site: bool,
    #[serde(default)]
    no_parent: bool,
    #[serde(default)]
    same_site_files_only: bool,
    #[serde(default)]
    include_exts: Vec<String>,
    #[serde(default)]
    exclude_exts: Vec<String>,
    #[serde(default)]
    min_kb: u64,
    #[serde(default)]
    max_mb: u64,
}

#[derive(Clone, Serialize)]
struct GrabSiteDto {
    title: String,
    pages_visited: usize,
    items: Vec<GrabItemDto>,
}

/// The IDM Grabber exploration: follow page links level by level from
/// the start page and collect the files passing the project filters.
#[tauri::command]
async fn grab_explore(
    state: State<'_, AppState>,
    args: GrabExploreArgs,
) -> Result<GrabSiteDto, String> {
    let settings = current_settings(&state);
    let opts = ndm::GrabOptions {
        depth_same: args.depth_same,
        depth_other: args.depth_other,
        whole_site: args.whole_site,
        no_parent: args.no_parent,
        same_site_files_only: args.same_site_files_only,
        include_exts: args.include_exts,
        exclude_exts: args.exclude_exts,
        min_size: if args.min_kb > 0 {
            Some(args.min_kb.saturating_mul(1024))
        } else {
            None
        },
        max_size: if args.max_mb > 0 {
            Some(args.max_mb.saturating_mul(1024 * 1024))
        } else {
            None
        },
    };
    let url = args.url.clone();
    let site = tauri::async_runtime::spawn_blocking(move || ndm::grab_site(&url, &opts))
        .await
        .map_err(|e| format!("internal join error: {e}"))??;
    Ok(GrabSiteDto {
        title: site.title,
        pages_visited: site.pages_visited,
        items: site
            .items
            .into_iter()
            .map(|it| GrabItemDto {
                kind: category_for_with(&it.name, &settings),
                url: it.url,
                name: it.name,
                size: it.size,
                label: it.label,
            })
            .collect(),
    })
}

/// Rebuild the in-memory list from downloads.json + engine state files.
/// A restored record that claims "finished" is only believed when
/// the file it names is actually on disk at the recorded size
/// (entry_is_finished is the authority). A claim that fails the
/// check — the file was deleted, moved, or never really written —
/// comes back as an honest error instead of a list row pointing
/// at nothing (R2-EXT-05).
fn demote_phantom_finished(entry: &Entry) {
    if entry_is_finished(entry) {
        return;
    }
    let mut r = entry.restored.lock().unwrap();
    if let Some(rec) = r.as_mut() {
        if rec.status == "finished" {
            rec.status = "error".to_string();
            rec.error =
                "NDM recorded this download as finished, but its file is missing from the save folder."
                    .to_string();
        }
    }
}

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
        Err(_) => {
            // The TOP LEVEL itself is unparseable (truncated by an
            // editor, a disk error, a non-array value). Returning with
            // an empty list here used to be fatal: the 5 s snapshot
            // then overwrote downloads.json with that empty list and
            // the user's whole history was gone. Quarantine the bad
            // file for inspection and block persist from writing an
            // empty list over the name until real entries exist.
            let stamp = chrono::Utc::now().timestamp();
            let _ = std::fs::rename(
                &path,
                path.with_extension(format!("json.corrupt-{stamp}")),
            );
            DOWNLOADS_CORRUPT.store(true, Ordering::Relaxed);
            return;
        }
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
        // normalize the queue name against the settings list: queue
        // matching elsewhere is case-sensitive and unknown names fall
        // back silently in some paths and strand the entry in others —
        // a hand-edited or case-drifted name must land in the queue it
        // obviously means (or Main), visibly and persistently
        let s = {
            let settings = current_settings(state);
            let canonical = settings
                .queues
                .iter()
                .find(|q| q.name.eq_ignore_ascii_case(&s.queue))
                .map(|q| q.name.clone())
                .unwrap_or_else(|| main_queue_name());
            if canonical != s.queue {
                SavedEntry {
                    queue: canonical,
                    ..s
                }
            } else {
                s
            }
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
                slot_preclaimed: AtomicBool::new(false),
                removed: AtomicBool::new(false),
                pause_requested: AtomicBool::new(false),
                completion: Mutex::new(s.completion.clone()),
                queue: Mutex::new(s.queue.clone()),
                login: s.login.clone(),
                password: s.password.clone(),
                description: Mutex::new(s.description.clone()),
                speed_limit: Mutex::new(s.speed_limit),
            });
            demote_phantom_finished(&entry);
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
                slot_preclaimed: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            completion: Mutex::new(s.completion.clone()),
            queue: Mutex::new(s.queue.clone()),
            login: s.login.clone(),
            password: s.password.clone(),
                description: Mutex::new(s.description.clone()),
                speed_limit: Mutex::new(s.speed_limit),
        });
        demote_phantom_finished(&entry);
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

/// Mirror one entry's payload into <app data>/progress/<id>.json —
/// the file the native host reads back for the browser panel's live
/// progress card (the app's own "progress" event stream is not
/// visible outside the app). Only the fields the card renders are
/// written; eta_secs is derived here because the payload itself
/// carries no ETA (0 = unknown, which the card renders as absent).
fn write_progress_file(handle: &AppHandle, payload: &ProgressPayload) {
    let Some(id) = sanitize_ack_id(&payload.id) else {
        return;
    };
    let Ok(base) = handle.path().app_data_dir() else {
        return;
    };
    let dir = base.join("progress");
    let _ = std::fs::create_dir_all(&dir);
    let eta_secs = if payload.speed_bps > 0.0 && payload.total > payload.done_bytes {
        ((payload.total - payload.done_bytes) as f64 / payload.speed_bps).ceil() as u64
    } else {
        0
    };
    let body = serde_json::json!({
        "status": payload.status,
        "done_bytes": payload.done_bytes,
        "total": payload.total,
        "rate_bps": payload.speed_bps,
        "eta_secs": eta_secs,
        "error": payload.error,
    });
    let _ = std::fs::write(dir.join(format!("{id}.json")), body.to_string());
}

/// Remove one entry's browser-panel progress mirror: once the entry
/// is gone, a leftover file keeps answering the host's progress
/// queries with a phantom live download for minutes (F-EXT-08).
fn delete_progress_file(app: &AppHandle, id: &str) {
    let Some(id) = sanitize_ack_id(id) else {
        return;
    };
    if let Ok(base) = app.path().app_data_dir() {
        let _ = std::fs::remove_file(base.join("progress").join(format!("{id}.json")));
    }
}

fn prune_stale_files(dir: &Path, max_age_secs: u64) {
    let now = std::time::SystemTime::now();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let stale = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| now.duration_since(t).ok())
                .map(|d| d.as_secs() > max_age_secs)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn prune_acks(queue_dir: &Path) {
    for sub in ["acks", "results"] {
        prune_stale_files(&queue_dir.join(sub), 120);
    }
    // browser-panel progress mirrors: a live entry's file is
    // rewritten ~twice a second while it downloads, so a file
    // untouched for 120 s belongs to an entry nobody polls anymore
    prune_stale_files(&queue_dir.join("progress"), 120);
    // captured browser sessions whose dialog was never acted on:
    // a day is far longer than any dialog sits open, and a live
    // session file must not linger on disk indefinitely
    prune_stale_files(&queue_dir.join("cookies"), 24 * 3600);
}

/// Delete a captured browser-session file whose incoming path is
/// returning WITHOUT handing the token to an entry or a dialog:
/// left behind, live session files would accumulate in the cookies
/// folder forever (the periodic 24 h prune is only the backstop).
fn drop_cookies_token(state: &AppState, token: &str) {
    if let Some(p) = state.cookies_path(token) {
        let _ = std::fs::remove_file(p);
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
/// Cancel an entry's engine/job and wait until they have fully
/// stopped — every shutdown write (the engine's final resume-state
/// save, the job's last progress update) has landed, so a file
/// sweep that follows cannot be raced by a last-moment rewrite.
/// Shared by every removal path: sweeping while the engine is
/// still winding down is exactly how orphan journals and stream
/// parts survived deletes (R3). A paused engine's workers are
/// already gone — nothing is left to write, so there is nothing
/// to wait for on that side.
fn settle_entry_engines(entry: &Entry) {
    if let Some(dl) = &entry.download {
        dl.control.cancel();
        // Wait for SETTLED, not merely terminal: the engine's
        // supervisor publishes the terminal status FIRST and only
        // then syncs the file and writes the final resume state,
        // so a terminal-only wait let the sweeps race that last
        // write — the journal reappeared after the delete, on the
        // app path and the extension Cancel path alike (R4 item
        // 2). The cap only guards a pathological engine; on expiry
        // the sweeps below still run, exactly as before.
        for _ in 0..100 {
            if dl.is_settled() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if let Some(job) = &entry.video {
        job.cancel();
        for _ in 0..100 {
            let st = job.progress.lock().unwrap().status.clone();
            if matches!(st.as_str(), "cancelled" | "error" | "finished") && job.is_settled() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Remove an entry's engine resume-state file, and keep removing
/// it until it stays gone: an engine that was mid-flight when the
/// removal began writes its state one last time on the way out,
/// and a single-pass removal loses that race — the surviving
/// journal then makes unique_out_path treat the name as taken
/// forever (phantom "(1)" names on re-add). Callers settle the
/// engines first; this loop is the post-settle final sweep.
fn sweep_engine_state_file(entry: &Entry) {
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
}

/// Sweep a video entry's intermediate files from its save folder —
/// STRICTLY its own intermediates. The old pattern (stem prefix +
/// any ".f" anywhere) could delete FINISHED, unrelated user files:
/// a video titled "my.film" has stem "my", and the completed
/// "my.film.mp4" itself matched. Only the engine's hidden parts
/// and yt-dlp's "{stem}.f<digits>.*.part" files go. The engine
/// parts' resume-state files (".…ndm_stream0.mp4.part.ndm.json")
/// match the same prefix and go too — a single immediate pass
/// used to miss them because the job's engines write their final
/// state on the way out, AFTER the sweep had already run
/// (F-EXT-08), so the sweep repeats until a full pass finds
/// nothing left to remove.
fn sweep_video_temps(entry: &Entry) {
    let raw_stem = entry
        .name
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| entry.name.clone());
    let job_stem = video::safe_filename_stem(&entry.name);
    let stems: Vec<&str> = if job_stem.is_empty() || job_stem == raw_stem {
        vec![raw_stem.as_str()]
    } else {
        vec![raw_stem.as_str(), job_stem.as_str()]
    };
    for pass in 0..10 {
        let mut removed_any = false;
        if let Ok(rd) = std::fs::read_dir(&entry.out_path) {
            for f in rd.flatten() {
                let n = f.file_name().to_string_lossy().to_string();
                let engine_part = stems
                    .iter()
                    .any(|s| n.starts_with(&format!(".{s}.ndm_stream")));
                // the audio-pairing temps (the fetched rendition
                // and the mux output), if the removal landed
                // mid-pairing
                let mux_temp = stems
                    .iter()
                    .any(|s| n.starts_with(&format!(".{s}.ndm_mux")));
                let pair_temp = stems
                    .iter()
                    .any(|s| n.starts_with(&format!(".{s}.ndm_audio")));
                // yt-dlp working files: "{stem}.f<digits>…" for
                // split formats AND "{stem}.<ext>…" for the
                // single-format template, as .part OR .ytdl — the
                // old pattern knew only the first shape and only
                // .part, so .ytdl files and single-format parts
                // survived the sweep (F-EXT-08)
                let ytdlp_part = (n.ends_with(".part") || n.ends_with(".ytdl"))
                    && stems.iter().any(|s| {
                        let split = n
                            .strip_prefix(&format!("{s}.f"))
                            .map(|rest| {
                                rest.chars()
                                    .next()
                                    .map(|c| c.is_ascii_digit())
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        let single = n
                            .strip_prefix(&format!("{s}."))
                            .map(|rest| {
                                ["mp4", "m4a", "mp3", "webm", "mkv", "ts"].iter().any(|e| {
                                    rest == *e || rest.starts_with(&format!("{e}."))
                                })
                            })
                            .unwrap_or(false);
                        split || single
                    });
                if engine_part || mux_temp || pair_temp || ytdlp_part {
                    removed_any = std::fs::remove_file(f.path()).is_ok() || removed_any;
                }
            }
        }
        if !removed_any && pass >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn discard_entry(app: &AppHandle, state: &AppState, entry: &Arc<Entry>) {
    // a finished download is the user's completed file, not a
    // dialog session: the browser dialog's Cancel/Esc arriving
    // after the download already finished must never delete its
    // list entry while the file stays on disk (F-EXT-12). No-op —
    // the caller still answers ok, and the entry (and file) stay.
    if entry_is_finished(entry) {
        return;
    }
    // A video job SETTLES before anything is removed. Its engine
    // streams write their resume state on the way out; sweeping
    // immediately raced those final writes and an orphan sidecar
    // survived on disk (F-EXT-08) — the HTTP branch below already
    // waits for its engine the same way. And if the job finishes
    // during the wait, the finish has WON against the cancel: the
    // discard aborts with nothing touched, under the same rule as
    // the finished guard above (R2-EXT-05).
    if entry.video.is_some() {
        settle_entry_engines(entry);
        if entry_is_finished(entry) {
            return;
        }
    }
    entry.removed.store(true, Ordering::SeqCst);
    state.retries.lock().unwrap().remove(&entry.id);
    quota_note_removed(state, entry);
    if entry.download.is_some() {
        // wait for the engine to actually stop: its shutdown writes
        // the resume state one last time, and deleting the files
        // before that would leave an orphan state file behind (a
        // later download of the same name could trust stale blocks)
        settle_entry_engines(entry);
    }
    state.entries.lock().unwrap().remove(&entry.id);
    state.order.lock().unwrap().retain(|x| x != &entry.id);
    // the engine's final state write can land just after the
    // cancel: the sweep keeps removing the resume state until it
    // stays gone, so no orphan survives to be trusted by a future
    // same-name download
    sweep_engine_state_file(entry);
    if entry.kind == "video" && !entry_is_finished(entry) {
        // the file the job itself produced and recorded — e.g. a
        // merge kept for a resume that will now never come — is
        // this download's own output, and a discard leaves nothing
        // of the download behind. (A FINISHED download never
        // reaches this sweep: the guards above return first.)
        if let Some(job) = &entry.video {
            if let Some(fp) = job.progress.lock().unwrap().final_path.clone() {
                let _ = std::fs::remove_file(&fp);
            }
        }
        sweep_video_temps(entry);
    } else if !entry_is_finished(entry) {
        let _ = std::fs::remove_file(&entry.out_path);
    }
    if let Some(p) = state.cookies_path(&entry.cookies) {
        let _ = std::fs::remove_file(p);
    }
    // the entry is gone: its progress mirror must go too, or the
    // browser panel keeps reporting a phantom live download (F-EXT-08)
    delete_progress_file(app, &entry.id);
    // the window's list is add-only — it merges progress events
    // and list_downloads pulls, and rows leave it only through
    // this event. A discard arrives from the browser (host path),
    // behind the window's back: without the signal the row stayed
    // behind as a ghost, counted in the list while downloads.json
    // no longer held the entry (R3 X-REF). The listener contract:
    // payload { "id": <entry id> } — drop that row.
    let _ = app.emit(
        "entry_removed",
        serde_json::json!({ "id": entry.id }),
    );
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
    if kind == "pause_entry" || kind == "cancel_entry" || kind == "resume_entry" {
        // the browser dialog's Later / Cancel for a download that
        // started while the dialog was open, and the panel progress
        // card's Pause / Resume / Cancel
        let entry_id = v["entry"].as_str().unwrap_or("");
        let state = handle.state::<AppState>();
        let result = match state.get(entry_id) {
            Some(entry) => {
                if kind == "pause_entry" {
                    let _ = pause_download(handle.clone(), state, entry_id.to_string());
                    serde_json::json!({ "ok": true })
                } else if kind == "cancel_entry" {
                    discard_entry(handle, &state, &entry);
                    serde_json::json!({ "ok": true })
                } else {
                    // the SAME resume the app's own Resume button
                    // runs — no second implementation to drift. This
                    // handler runs on the queue-drain thread (never
                    // inside a runtime task), so block_on is safe.
                    match tauri::async_runtime::block_on(resume_download(
                        handle.clone(),
                        state,
                        entry_id.to_string(),
                    )) {
                        Ok(()) => serde_json::json!({ "ok": true }),
                        Err(e) => serde_json::json!({ "ok": false, "error": e }),
                    }
                }
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
        let cookies_opt = if token.is_empty() {
            None
        } else {
            Some(token.clone())
        };
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
            Err(err) => {
                drop_cookies_token(&state, &token);
                write_ack_and_result(
                    handle,
                    ack_id.as_deref(),
                    serde_json::json!({ "ok": false, "error": err }),
                );
            }
        }
        return;
    }
    if kind == "file_direct" {
        // Run silently in the background — NEVER steal focus from the browser!
        let state = handle.state::<AppState>();
        if state.update_required.load(Ordering::SeqCst) {
            drop_cookies_token(&state, &token);
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
                drop_cookies_token(&state, &token);
                write_ack_and_result(
                    handle,
                    ack_id.as_deref(),
                    serde_json::json!({ "ok": true, "id": existing, "dupe": true }),
                );
                return;
            }
            // a FINISHED copy counts too (R2-EXT-07): the context
            // menu path used to re-download it as "(1)" with no
            // word — the existing entry is the answer here as well
            if let Some(existing) = find_finished_duplicate(&state, "http", url) {
                drop_cookies_token(&state, &token);
                write_ack_and_result(
                    handle,
                    ack_id.as_deref(),
                    serde_json::json!({ "ok": true, "id": existing, "dupe": true }),
                );
                return;
            }
            // an ERRORED copy counts too (R2-EXT-04): the flyout
            // renders the existing entry's error with a Retry
            // button — a silent "(2)" re-add would orphan the
            // failed entry's partial progress instead
            if let Some(existing) = find_errored_duplicate(&state, "http", url) {
                drop_cookies_token(&state, &token);
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
                let max = effective_cap(&current_settings(&state), &entry_queue(&stub));
                // the same gate + slot pre-claim discipline as the
                // dialog path: several catches landing in one drain
                // batch used to all pass the cap check before any of
                // them became visible to the counter
                let must_wait = {
                    let _gate = START_GATE.lock().unwrap();
                    let wait = later
                        || entry_blocks(&state, &entry_queue(&stub))
                        || (max > 0 && count_active_for(&state, &entry_queue(&stub)) >= max);
                    if !wait {
                        stub.slot_preclaimed.store(true, Ordering::SeqCst);
                    }
                    wait
                };
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
                        // the browser's size hint rides along so a
                        // queued catch shows its size, not "Unknown"
                        // (F-03, same rule as the dialog path). The
                        // record is written even with NO hint: a
                        // queued catch whose only trace was the flag
                        // never reached downloads.json and was lost
                        // on restart (battery19 t1/t3) — persist
                        // also keeps flag-only queued entries now,
                        // and the record keeps memory and disk in
                        // the same shape.
                        let mut r = stub.restored.lock().unwrap();
                        if r.is_none() {
                            *r = Some(Restored {
                                total: total_hint,
                                done_bytes: 0,
                                blocks: Vec::new(),
                                resumable: true,
                                status: "queued".to_string(),
                                error: String::new(),
                            });
                        }
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
                drop_cookies_token(&state, &token);
                write_ack_and_result(
                    handle,
                    ack_id.as_deref(),
                    serde_json::json!({ "ok": false, "error": err }),
                );
            }
        }
        return;
    }
    if kind == "grab" {
        // "Download all links" from the browser: open the in-app
        // Site Grabber preloaded with this page (the grab
        // counterpart of the video/file dialog kinds below)
        raise_main_window(handle);
        let state = handle.state::<AppState>();
        // a grab starts no download itself, so a captured session
        // file has no consumer on this path and must not linger
        drop_cookies_token(&state, &token);
        let payload = serde_json::json!({
            "url": url,
            "pageUrl": v["pageUrl"].as_str().unwrap_or(""),
            "title": v["title"].as_str().unwrap_or(""),
        });
        state.pending_grabs.lock().unwrap().push(payload.clone());
        let _ = handle.emit("incoming_grab", payload);
        write_ack_and_result(handle, ack_id.as_deref(), serde_json::json!({ "ok": true }));
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
/// start, (re)write the native-messaging host manifests for every
/// known browser (creating the config dirs when needed), using the
/// extension's fixed ID (pinned by
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
        "description": "Nill Download Manager native messaging host",
        "path": host,
        "type": "stdio",
        "allowed_origins": ["chrome-extension://njjmngpfmblaghapeenfdfkahhdocbic/"],
    })
    .to_string();
    // create the NativeMessagingHosts directories unconditionally
    // (F-EXT-06): writing only into config dirs that already exist
    // silently skipped every browser the user had not launched yet —
    // on a fresh machine the app ran, "registered", and the extension
    // still reported the host missing, sending the user in a circle.
    // write_if_changed creates the parent dirs itself.
    for cfg in [
        "google-chrome", "google-chrome-beta", "chromium", "chromium-browser",
        "BraveSoftware/Brave-Browser", "microsoft-edge", "vivaldi", "opera",
    ] {
        let base = config.join(cfg);
        write_if_changed(
            &base.join("NativeMessagingHosts").join("com.ndm.host.json"),
            &chrome,
        );
    }
    let moz = home.join(".mozilla");
    let firefox = serde_json::json!({
        "name": "com.ndm.host",
        "description": "Nill Download Manager native messaging host",
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
            quota_removed: Mutex::new(0),
            quota_file: Mutex::new(PathBuf::new()),
            pending_videos: Mutex::new(Vec::new()),
            pending_files: Mutex::new(Vec::new()),
            pending_grabs: Mutex::new(Vec::new()),
            components_busy: AtomicBool::new(false),
            retries: Mutex::new(HashMap::new()),
            had_active: AtomicBool::new(false),
            pending_update: Mutex::new(None),
            update_required: AtomicBool::new(false),
            update_busy: AtomicBool::new(false),
        })
        .on_window_event(|window, event| {
            // N-12: closing the main window IS the user leaving —
            // there is no tray and no close-to-tray mode anywhere
            // in the app. Tauri only requests an exit when its
            // window map becomes EMPTY, and the hidden drop-basket
            // window keeps that map non-empty forever, so closing
            // main used to leave a windowless zombie process (its
            // WebKit children, bridge sockets and "downloading"
            // claims all alive) with no visible way back. An
            // explicit close of the main window now exits fully,
            // exactly like File → Exit / Ctrl+Q (quit_app).
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { .. } = event {
                    window.app_handle().exit(0);
                }
            }
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
                } else {
                    // restored QUEUED entries (they were waiting for
                    // a cap slot when the app closed) must be pumped
                    // once even with auto-resume off: "queued" is a
                    // live state in IDM, not a parking state — they
                    // used to sit forever until an unrelated event
                    // happened to pump. Blocked queues stay blocked
                    // via entry_blocks inside the pump.
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
                    // panic-isolated per tick: a panic here used to
                    // kill scheduling AND automatic retries for the
                    // whole session with no sign anything was wrong
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let state = handle4.state::<AppState>();
                    let s = current_settings(&state);
                    // automatic retries (IDM queue semantics): a
                    // failed file goes back into its queue and is
                    // retried BY THE QUEUE PROCESSOR — it stays
                    // subject to the queue's window, the download
                    // quota and the simultaneous cap. The old code
                    // started retries directly, bypassing all three.
                    if !state.update_required.load(Ordering::Relaxed) {
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
                        let mut any = false;
                        for id in due {
                            let entry = state.entries.lock().unwrap().get(&id).cloned();
                            if let Some(e) = entry {
                                if payload_of(&e).status == "error" {
                                    e.queued.store(true, Ordering::Relaxed);
                                    any = true;
                                }
                            }
                        }
                        if any {
                            pump_queue(&handle4);
                        }
                    }
                    // the periodic promoter (F-18): queued entries
                    // are re-offered to the pump on every tick, not
                    // only when some unrelated event happens to pump —
                    // a queue whose moment was missed (a transient
                    // block, a wedged neighbour holding a cap slot)
                    // still drains on its own. The pump honours each
                    // queue's window, the quota and the caps, and it
                    // claims queued entries regardless of whether
                    // their size is known yet — the start path probes
                    // anyway, so an unknown total is never a reason
                    // to leave an entry sitting in "Queued".
                    pump_queue(&handle4);
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
                            // parked Download-Later entries join the
                            // queue before it starts (R5-5a) — also
                            // the catch-all for entries restored from
                            // disk parked, which no add/move/settings
                            // path has touched this session
                            adopt_parked_entries(&handle4, &state, &s, &cfg.name);
                            if effective_cap(&s, &cfg.name) == 0 && !quota_blocks(&state) {
                                // start-all respects Download limits:
                                // an exhausted quota is not overridden
                                // by a queue window opening
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
                    }));
                }
            });
            // clipboard monitoring (IDM): copying a download link
            // raises the Add URL dialog with the link filled in —
            // the same incoming_file flow the browser extension uses.
            // Each distinct link is offered once per session.
            let handle5 = app.handle().clone();
            std::thread::spawn(move || {
                let mut last_seen = String::new();
                let mut was_enabled = false;
                let mut prompted: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut cb: Option<arboard::Clipboard> = None;
                loop {
                    std::thread::sleep(Duration::from_millis(900));
                    // panic-isolated: one bad tick must not kill
                    // clipboard monitoring for the whole session
                    let tick = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let state = handle5.state::<AppState>();
                    if !current_settings(&state).clipboard_monitor {
                        was_enabled = false;
                        return;
                    }
                    if cb.is_none() {
                        cb = arboard::Clipboard::new().ok();
                    }
                    let Some(c) = cb.as_mut() else { return };
                    if !was_enabled {
                        // off→on transition: whatever is on the
                        // clipboard RIGHT NOW was copied while the
                        // monitor was off — seed it as already-seen
                        // instead of "discovering" it as a new link
                        was_enabled = true;
                        if let Ok(t) = c.get_text() {
                            last_seen = t.trim().to_string();
                        }
                        return;
                    }
                    let Ok(text) = c.get_text() else { return };
                    let text = text.trim().to_string();
                    if text == last_seen {
                        return;
                    }
                    last_seen = text.clone();
                    if !clipboard_url_qualifies(&text) {
                        return;
                    }
                    if !prompted.insert(text.clone()) {
                        return;
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
                        "source": "clipboard",
                    });
                    state.pending_files.lock().unwrap().push(payload.clone());
                    raise_main_window(&handle5);
                    let _ = handle5.emit("incoming_file", payload);
                    }));
                    let _ = tick;
                }
            });
            // periodic snapshot so even a hard power cut keeps a fresh list
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut was_blocked = false;
                loop {
                    std::thread::sleep(Duration::from_secs(5));
                    // panic-isolated: this thread also carries quota
                    // enforcement — a panic must not kill both quietly
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let state = handle.state::<AppState>();
                        persist(&state);
                        // download quota (IDM's Download limits): the
                        // moment the window's budget runs out,
                        // everything pauses — re-queued, so the next
                        // window resumes it — and the user is told
                        // once per window
                        let blocked = quota_blocks(&state);
                        if blocked {
                            let mut q = state.quota_state.lock().unwrap();
                            if !q.2 {
                                q.2 = true;
                                drop(q);
                                save_quota(&state);
                                pause_all(&handle, true, None);
                                let _ = handle.emit("quota_exceeded", serde_json::json!({}));
                            }
                        }
                        if was_blocked && !blocked {
                            // the window rolled over: restart what
                            // the quota paused. Nothing else ever
                            // pumped here, so quota-paused downloads
                            // used to stall until an unrelated event.
                            pump_queue(&handle);
                        }
                        was_blocked = blocked;
                    }));
                }
            });
            // incoming queue from the browser extension (via ndm-host):
            // drain immediately on startup and poll every 200ms so browser
            // clicks feel instantaneous
            let handle2 = app.handle().clone();
            let queue_dir = dir.clone();
            std::thread::spawn(move || {
                // panic-isolated per tick: this thread is the ONLY
                // path by which browser clicks reach the app — its
                // silent death would look like "NDM stopped working"
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prune_acks(&queue_dir);
                    drain_incoming(&handle2, &queue_dir);
                }));
                let mut ticks = 0u64;
                loop {
                    std::thread::sleep(Duration::from_millis(200));
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        drain_incoming(&handle2, &queue_dir);
                    }));
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
            grab_explore,
            get_settings,
            set_settings,
            set_entry_queue,
            set_entry_limit,
            set_entry_description,
            move_in_queue,
            basket_add,
            set_basket_visible,
            scheduler_start_now,
            scheduler_stop_now,
            open_path,
            set_completion,
            refresh_url,
            take_pending_videos,
            take_pending_files,
            take_pending_grabs,
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
    fn post_download_verdicts_are_permanent_for_auto_retry() {
        // R2-EXT-03 / N-10: these errors re-downloading the same
        // bytes cannot change — they must never enter the retry map
        assert!(error_is_permanent(
            "the merged file is much smaller than what was downloaded (79165 of 100663296 bytes) — the source streams may be incomplete"
        ));
        assert!(error_is_permanent(
            "the video and audio downloaded but could not be merged into one playable file — merging needs ffmpeg. Reinstall NDM with apt (ffmpeg is a package dependency), then try again"
        ));
        // transient failures still retry
        assert!(!error_is_permanent("probe failed: connection timed out"));
        assert!(!error_is_permanent("the server closed the connection early"));
    }

    #[test]
    fn server_answers_are_translated_not_blamed_on_the_connection() {
        // N-07: the probe's raw wording for a definitive HTTP answer
        let raw = "probe failed: http://127.0.0.1:8951/master.m3u8: status code 404";
        let plain = plain_server_error(raw);
        assert!(plain.contains("404"), "{plain}");
        // the dialog maps these trigger words to "could not reach
        // that address" — the translation must contain none of them
        let lower = plain.to_lowercase();
        for trigger in ["probe", "connect", "network", "timeout", "timed out", "dns", "refused"] {
            assert!(!lower.contains(trigger), "{plain} contains {trigger}");
        }
        // transport failures pass through untouched
        let transport = "probe failed: http://x.test/f.bin: Connection Failed";
        assert_eq!(plain_server_error(transport), transport);
    }

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
            quota_removed: Mutex::new(0),
            quota_file: Mutex::new(PathBuf::new()),
            pending_videos: Mutex::new(Vec::new()),
            pending_files: Mutex::new(Vec::new()),
            pending_grabs: Mutex::new(Vec::new()),
            components_busy: AtomicBool::new(false),
            pending_update: Mutex::new(None),
            update_required: AtomicBool::new(false),
            update_busy: AtomicBool::new(false),
            retries: Mutex::new(HashMap::new()),
            had_active: AtomicBool::new(false),
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
    fn manual_proxy_composes_effective_url_with_encoded_userinfo() {
        let mut s = Settings {
            proxy_mode: "manual".to_string(),
            proxy_scheme: "http".to_string(),
            proxy_host: "127.0.0.1".to_string(),
            proxy_port: 8080,
            proxy_user: "u@x".to_string(),
            proxy_pass: "p:ss".to_string(),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.proxy, "http://u%40x:p%3Ass@127.0.0.1:8080");
        assert_eq!(s.proxy_mode, "manual");
    }

    #[test]
    fn legacy_single_proxy_migrates_to_manual_fields() {
        let mut s = Settings {
            proxy: "socks5://bob:secret@proxy.example.com:1080".to_string(),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.proxy_mode, "manual");
        assert_eq!(s.proxy_scheme, "socks5");
        assert_eq!(s.proxy_host, "proxy.example.com");
        assert_eq!(s.proxy_port, 1080);
        assert_eq!(s.proxy_user, "bob");
        // and the effective URL round-trips back to the same address
        assert_eq!(s.proxy, "socks5://bob:secret@proxy.example.com:1080");
    }

    #[test]
    fn site_login_for_picks_the_longest_matching_site() {
        let s = Settings {
            site_logins: vec![
                SiteLogin {
                    site: "example.com".to_string(),
                    login: "hostuser".to_string(),
                    password: "p1".to_string(),
                },
                SiteLogin {
                    site: "example.com/members".to_string(),
                    login: "memberuser".to_string(),
                    password: "p2".to_string(),
                },
            ],
            ..Settings::default()
        };
        assert_eq!(
            site_login_for(&s, "https://example.com/members/file.zip"),
            Some(("memberuser".to_string(), "p2".to_string()))
        );
        assert_eq!(
            site_login_for(&s, "https://example.com/public/file.zip"),
            Some(("hostuser".to_string(), "p1".to_string()))
        );
        assert_eq!(site_login_for(&s, "https://other.com/members/file.zip"), None);
    }

    #[test]
    fn unknown_power_actions_are_cleared_by_sanitize() {
        let mut s = Settings {
            power_when_done: "explode".to_string(),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.power_when_done, "");
        let mut s2 = Settings {
            power_when_done: "shutdown".to_string(),
            ..Settings::default()
        };
        s2.sanitize();
        assert_eq!(s2.power_when_done, "shutdown");
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
            quota_removed: Mutex::new(0),
            quota_file: Mutex::new(PathBuf::new()),
            pending_videos: Mutex::new(Vec::new()),
            pending_files: Mutex::new(Vec::new()),
            pending_grabs: Mutex::new(Vec::new()),
            components_busy: AtomicBool::new(false),
            pending_update: Mutex::new(None),
            update_required: AtomicBool::new(false),
            update_busy: AtomicBool::new(false),
            retries: Mutex::new(HashMap::new()),
            had_active: AtomicBool::new(false),
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
                slot_preclaimed: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            completion: Mutex::new(String::new()),
            queue: Mutex::new(String::new()),
            login: String::new(),
            password: String::new(),
            description: Mutex::new(String::new()),
            speed_limit: Mutex::new(0),
        });
        state.entries.lock().unwrap().insert("dl-1".to_string(), entry);
        state.order.lock().unwrap().push("dl-1".to_string());

        persist(&state);
        let text = std::fs::read_to_string(&data_file).unwrap();
        assert!(text.contains("\"status\":\"downloading\""));
        assert!(!text.contains("\"status\":\"paused\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn restored_entry(name: &str, kind: &str, total: u64, out_path: PathBuf) -> Entry {
        Entry {
            id: "dl-1".to_string(),
            name: name.to_string(),
            url: "https://example.com/f".to_string(),
            out_path,
            category: "General".to_string(),
            added: "2026-10-30 12:00".to_string(),
            download: None,
            video: None,
            kind: kind.to_string(),
            selector: String::new(),
            cookies: String::new(),
            referer: String::new(),
            audio: false,
            restored: Mutex::new(Some(Restored {
                total,
                done_bytes: total,
                blocks: Vec::new(),
                resumable: true,
                status: "finished".to_string(),
                error: String::new(),
            })),
            queued: AtomicBool::new(false),
            sample: Mutex::new((Instant::now(), 0, 0.0)),
            emitting: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            slot_preclaimed: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            completion: Mutex::new(String::new()),
            queue: Mutex::new(String::new()),
            login: String::new(),
            password: String::new(),
            description: Mutex::new(String::new()),
            speed_limit: Mutex::new(0),
        }
    }

    #[test]
    fn restored_finished_claim_needs_the_file_on_disk() {
        // R3 (restart phantom-finished): a restored record that
        // claims "finished" is only believed when the file is
        // really on disk at the recorded size — the stored status
        // alone used to be trusted, so a file deleted while the
        // app was closed restored as "Finished" and the demote
        // below could never fire.
        let dir = std::env::temp_dir().join(format!("ndm-phantom-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("movie.mp4");

        // file deleted while the app was closed → NOT finished,
        // and the demote turns the record into an honest error
        let entry = restored_entry("movie.mp4", "http", 5, file.clone());
        assert!(!entry_is_finished(&entry));
        demote_phantom_finished(&entry);
        let guard = entry.restored.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        assert_eq!(rec.status, "error");
        assert!(rec.error.contains("missing"));
        drop(guard);

        // file present at the recorded size → finished, demote
        // leaves the record alone
        std::fs::write(&file, b"12345").unwrap();
        let entry = restored_entry("movie.mp4", "http", 5, file.clone());
        assert!(entry_is_finished(&entry));
        demote_phantom_finished(&entry);
        assert_eq!(
            entry.restored.lock().unwrap().as_ref().unwrap().status,
            "finished"
        );

        // file present but truncated (HTTP is exact) → not finished
        std::fs::write(&file, b"123").unwrap();
        let entry = restored_entry("movie.mp4", "http", 5, file.clone());
        assert!(!entry_is_finished(&entry));

        // video tolerance: a merged file smaller than the recorded
        // total still counts (the record can predate the finalize
        // step that rewrites the total to the merged size)
        let vfile = dir.join("clip.mp4");
        std::fs::write(&vfile, b"123").unwrap();
        let entry = restored_entry("clip.mp4", "video", 5, dir.clone());
        assert!(entry_is_finished(&entry));
        // …but a missing video file never does
        let _ = std::fs::remove_file(&vfile);
        let entry = restored_entry("clip.mp4", "video", 5, dir.clone());
        assert!(!entry_is_finished(&entry));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_entry_id_only_reclaims_the_high_water_mark() {
        // R3 (failed adds burn ids): a failed add hands its number
        // back only while it is still the counter's latest — a
        // later mint (another entry, a cookies token) keeps it.
        let state = AppState {
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
            quota_removed: Mutex::new(0),
            quota_file: Mutex::new(PathBuf::new()),
            pending_videos: Mutex::new(Vec::new()),
            pending_files: Mutex::new(Vec::new()),
            pending_grabs: Mutex::new(Vec::new()),
            components_busy: AtomicBool::new(false),
            pending_update: Mutex::new(None),
            update_required: AtomicBool::new(false),
            update_busy: AtomicBool::new(false),
            retries: Mutex::new(HashMap::new()),
            had_active: AtomicBool::new(false),
        };
        let mint = |st: &AppState| st.seq.fetch_add(1, Ordering::Relaxed) + 1;
        // mint dl-2, release it → the next mint reuses 2
        assert_eq!(mint(&state), 2);
        release_entry_id(&state, "dl-2");
        assert_eq!(mint(&state), 2);
        // mint dl-3 on top: dl-2 is no longer the high-water mark
        assert_eq!(mint(&state), 3);
        release_entry_id(&state, "dl-2");
        assert_eq!(state.seq.load(Ordering::Relaxed), 3);
        // releasing the actual high-water mark still works
        release_entry_id(&state, "dl-3");
        assert_eq!(state.seq.load(Ordering::Relaxed), 2);
        // foreign id shapes are ignored
        release_entry_id(&state, "ck9");
        assert_eq!(state.seq.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn persist_skips_unborn_placeholder_stubs() {
        // R3 (failed adds): a registered stub whose first start is
        // still in flight must never reach downloads.json — a
        // crash mid-probe would restore it as a total=0 phantom.
        // The moment it has a restored record it persists normally.
        let dir = std::env::temp_dir().join(format!("ndm-unborn-test-{}", std::process::id()));
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
            quota_removed: Mutex::new(0),
            quota_file: Mutex::new(PathBuf::new()),
            pending_videos: Mutex::new(Vec::new()),
            pending_files: Mutex::new(Vec::new()),
            pending_grabs: Mutex::new(Vec::new()),
            components_busy: AtomicBool::new(false),
            pending_update: Mutex::new(None),
            update_required: AtomicBool::new(false),
            update_busy: AtomicBool::new(false),
            retries: Mutex::new(HashMap::new()),
            had_active: AtomicBool::new(false),
        };

        let stub = restored_entry("f.bin", "http", 0, dir.join("f.bin"));
        *stub.restored.lock().unwrap() = None;
        let stub = Arc::new(stub);
        state
            .entries
            .lock()
            .unwrap()
            .insert("dl-1".to_string(), stub.clone());
        state.order.lock().unwrap().push("dl-1".to_string());

        persist(&state);
        let text = std::fs::read_to_string(&data_file).unwrap();
        assert!(!text.contains("dl-1"), "placeholder persisted: {text}");

        *stub.restored.lock().unwrap() = Some(Restored {
            total: 0,
            done_bytes: 0,
            blocks: Vec::new(),
            resumable: true,
            status: "paused".to_string(),
            error: String::new(),
        });
        persist(&state);
        let text = std::fs::read_to_string(&data_file).unwrap();
        assert!(text.contains("dl-1"), "real entry missing: {text}");

        // battery19 t1/t3: a deliberately QUEUED entry is a resolved
        // state, not an unborn placeholder — with no handle and no
        // restored record (a catch that carried no size hint) it
        // must still be persisted, as "queued", the moment it is
        // queued, so it survives a restart and the startup pump
        // can start it.
        let mut q = restored_entry("g.bin", "http", 0, dir.join("g.bin"));
        q.id = "dl-2".to_string();
        *q.restored.lock().unwrap() = None;
        q.queued.store(true, Ordering::Relaxed);
        let q = Arc::new(q);
        state
            .entries
            .lock()
            .unwrap()
            .insert("dl-2".to_string(), q);
        state.order.lock().unwrap().push("dl-2".to_string());
        persist(&state);
        let text = std::fs::read_to_string(&data_file).unwrap();
        assert!(text.contains("dl-2"), "queued entry missing: {text}");
        assert!(
            text.contains("\"status\":\"queued\""),
            "queued entry not saved as queued: {text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn later_entry_queued_only_into_a_scheduled_queue() {
        // R5-5a: "Download Later" joins the queue only when that
        // queue has an enabled schedule; every other combination
        // keeps today's behaviour
        assert!(later_entry_queued(true, true));
        assert!(!later_entry_queued(true, false));
        assert!(!later_entry_queued(false, true));
        assert!(!later_entry_queued(false, false));
    }

    #[test]
    fn queue_has_schedule_reads_the_use_schedule_flag() {
        let mut s = Settings::default();
        s.queues = vec![
            QueueCfg {
                name: "Night".to_string(),
                use_schedule: true,
                ..QueueCfg::default()
            },
            QueueCfg {
                name: "Plain".to_string(),
                use_schedule: false,
                ..QueueCfg::default()
            },
        ];
        assert!(queue_has_schedule(&s, "Night"));
        assert!(!queue_has_schedule(&s, "Plain"));
        // an unknown name falls back to the Main queue's config,
        // which carries no schedule here
        assert!(!queue_has_schedule(&s, "Nope"));
    }

    fn parked_entry(done: u64, status: &str) -> Entry {
        let mut e = restored_entry("f.bin", "http", 1000, PathBuf::from("/tmp/f.bin"));
        *e.restored.lock().unwrap() = Some(Restored {
            total: 1000,
            done_bytes: done,
            blocks: Vec::new(),
            resumable: true,
            status: status.to_string(),
            error: String::new(),
        });
        e
    }

    #[test]
    fn never_started_parked_matches_only_the_download_later_shape() {
        // the Download Later shape: parked paused, zero bytes
        assert!(never_started_parked(&parked_entry(0, "paused")));
        // paused AFTER bytes moved = a user pause, never adopted
        assert!(!never_started_parked(&parked_entry(500, "paused")));
        // an explicit Pause press on a parked entry latches it, and
        // the latch is the user's stop — not adoptable either
        let latched = parked_entry(0, "paused");
        latched.pause_requested.store(true, Ordering::SeqCst);
        assert!(!never_started_parked(&latched));
        // already queued entries are claimable as they are
        let queued = parked_entry(0, "paused");
        queued.queued.store(true, Ordering::Relaxed);
        assert!(!never_started_parked(&queued));
        // other parked states are not Download Later
        assert!(!never_started_parked(&parked_entry(0, "error")));
        let bare = parked_entry(0, "paused");
        *bare.restored.lock().unwrap() = None;
        assert!(!never_started_parked(&bare));
    }

    #[test]
    fn claim_queued_clears_a_stale_pause_latch() {
        // R5-5b belt: an entry claimed by the pump/scheduler starts
        // clean — a latch left over from a schedule edit must not
        // pause it after its first block
        let e = parked_entry(0, "queued");
        e.queued.store(true, Ordering::Relaxed);
        e.pause_requested.store(true, Ordering::SeqCst);
        assert!(claim_queued(&e));
        assert!(!e.queued.load(Ordering::Relaxed));
        assert!(!e.pause_requested.load(Ordering::SeqCst));
        // a second claim finds nothing to claim
        assert!(!claim_queued(&e));
    }
}

