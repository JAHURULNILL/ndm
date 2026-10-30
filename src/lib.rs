// NDM download engine — library form of the Phase 1 prototype.
// Same proven core (dynamic block claiming, preallocated offset writes,
// crash-safe resume state, adaptive connection ramp with throttle back-off),
// plus UI controls: pause / resume / cancel and a progress callback.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt as WinFileExt;

fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        file.write_all_at(buf, offset)
    }
    #[cfg(windows)]
    {
        let mut pos = offset;
        let mut rest = buf;
        while !rest.is_empty() {
            let n = file.seek_write(rest, pos)?;
            pos += n as u64;
            rest = &rest[n..];
        }
        Ok(())
    }
}

pub mod components;
pub mod updater;
pub mod video_probe;

pub const DEFAULT_BLOCK: u64 = 1 << 20; // 1 MiB block map, per engine spec A
const SAVE_EVERY_BYTES: u64 = 4 << 20;
const SAVE_EVERY: Duration = Duration::from_secs(1);

// ---------- global speed limit (GCRA pacing clock, shared by every download) ----------

static LIMIT_RATE: AtomicU64 = AtomicU64::new(0); // bytes/sec, 0 = unlimited
static LIMIT_STATE: Mutex<Option<Instant>> = Mutex::new(None); // next free pacing slot

/// Set the global download speed limit in bytes/second (0 = unlimited).
/// Applies to all running and future HTTP-engine downloads in this process.
pub fn set_global_limit(bytes_per_sec: u64) {
    LIMIT_RATE.store(bytes_per_sec, Ordering::Relaxed);
    // reset the pacing clock so a new rate starts clean
    *LIMIT_STATE.lock().unwrap() = None;
}

pub fn global_limit() -> u64 {
    LIMIT_RATE.load(Ordering::Relaxed)
}

static MAX_CONN: AtomicU64 = AtomicU64::new(32);

/// The user's per-download connection setting, mirrored process-wide so
/// sidecar tools (yt-dlp fragment downloads) can honour it too.
pub fn set_global_connections(n: usize) {
    MAX_CONN.store(n.clamp(1, 32) as u64, Ordering::Relaxed);
}

pub fn global_connections() -> usize {
    MAX_CONN.load(Ordering::Relaxed) as usize
}

static PROXY_URL: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

/// The user's optional outbound proxy ("http://host:port", "socks5://…"),
/// mirrored process-wide. Applies to agents built after the call; empty
/// string means a direct connection.
pub fn set_global_proxy(url: &str) {
    *PROXY_URL.write().unwrap() = url.trim().to_string();
}

pub fn global_proxy() -> String {
    PROXY_URL.read().unwrap().clone()
}

/// Which protocols the configured proxy serves (IDM's Proxy tab has a
/// checkbox per protocol: HTTP / HTTPS / FTP). Default: all three.
static PROXY_PROTOCOLS: std::sync::RwLock<(bool, bool, bool)> =
    std::sync::RwLock::new((true, true, true));

pub fn set_proxy_protocols(http: bool, https: bool, ftp: bool) {
    *PROXY_PROTOCOLS.write().unwrap() = (http, https, ftp);
}

/// The proxy to use for one URL: empty when its protocol is unchecked
/// on the Proxy tab (the connection then goes direct, exactly like IDM).
pub fn proxy_for_url(url: &str) -> String {
    let (http, https, ftp) = *PROXY_PROTOCOLS.read().unwrap();
    let enabled = if url.starts_with("https://") {
        https
    } else if url.starts_with("ftp://") {
        ftp
    } else {
        http
    };
    if enabled {
        global_proxy()
    } else {
        String::new()
    }
}

/// Attach the configured proxy (if any and parseable) to an agent builder.
pub(crate) fn apply_proxy(builder: ureq::AgentBuilder) -> ureq::AgentBuilder {
    apply_proxy_url(builder, &global_proxy())
}

/// Attach the proxy that applies to `url` (protocol-gated).
pub(crate) fn apply_proxy_for(builder: ureq::AgentBuilder, url: &str) -> ureq::AgentBuilder {
    let proxy = proxy_for_url(url);
    apply_proxy_url(builder, &proxy)
}

fn apply_proxy_url(builder: ureq::AgentBuilder, url: &str) -> ureq::AgentBuilder {
    if url.is_empty() {
        return builder;
    }
    match ureq::Proxy::new(url) {
        Ok(p) => builder.proxy(p),
        Err(_) => builder, // a bad proxy string must never break downloads
    }
}

/// Resolve a PAC (automatic configuration) address to a concrete
/// proxy: fetch the script and take its first PROXY/SOCKS directive.
/// Deliberately simple — it covers the overwhelmingly common PAC shape
/// (a fixed proxy for everything) and fails to "direct", never to a
/// guessed address.
pub fn resolve_pac_proxy(pac_url: &str) -> String {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(4))
        .build();
    let Ok(resp) = agent.get(pac_url).call() else {
        return String::new();
    };
    let Ok(body) = resp.into_string() else {
        return String::new();
    };
    // find "PROXY host:port" / "SOCKS host:port" pairs in order
    let words: Vec<&str> = body
        .split(|c: char| c == ';' || c == '"' || c == '\'' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .collect();
    for w in words.windows(2) {
        let kind = w[0].to_ascii_uppercase();
        if (kind == "PROXY" || kind == "SOCKS" || kind == "SOCKS5") && w[1].contains(':') {
            let scheme = if kind == "PROXY" { "http" } else { "socks5" };
            return format!("{scheme}://{}", w[1]);
        }
    }
    String::new()
}

/// Charge `n` downloaded bytes against the global pacing clock (GCRA-style
/// slot reservation): every charge reserves n/rate seconds on a shared
/// timeline and the caller sleeps until its slot begins. Aggregate throughput
/// converges to exactly the configured rate, with no burst-and-starve cycle.
/// Per-download pacing (IDM's Speed Limiter tab is per download):
/// same slot algorithm as the global limiter, but scoped to one
/// download's Shared state. Both throttles apply — a download obeys
/// its own limit AND the global one.
fn throttle_own(shared: &Shared, n: u64, since: Instant) {
    let rate = shared.own_limit.load(Ordering::Relaxed);
    if rate == 0 || n == 0 {
        return;
    }
    let sleep_for = {
        let mut st = shared.own_limit_state.lock().unwrap();
        let now = Instant::now();
        let slot = (*st).unwrap_or(now).max(since);
        let cost = Duration::from_secs_f64(n as f64 / rate as f64);
        *st = Some(slot + cost);
        slot.saturating_duration_since(now)
    };
    if !sleep_for.is_zero() {
        std::thread::sleep(sleep_for);
    }
}

fn throttle_global(n: u64, since: Instant) {
    let rate = LIMIT_RATE.load(Ordering::Relaxed);
    if rate == 0 || n == 0 {
        return;
    }
    let sleep_for = {
        let mut st = LIMIT_STATE.lock().unwrap();
        let now = Instant::now();
        // credit the time the bytes spent in flight: if the pacing queue has
        // drained, the slot may start at the read's start time instead of now
        let slot = (*st).unwrap_or(now).max(since);
        let cost = Duration::from_secs_f64(n as f64 / rate as f64);
        *st = Some(slot + cost);
        slot.saturating_duration_since(now)
    };
    if !sleep_for.is_zero() {
        std::thread::sleep(sleep_for);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Downloading,
    Paused,
    Finished,
    Error,
    Cancelled,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Downloading => "downloading",
            Status::Paused => "paused",
            Status::Finished => "finished",
            Status::Error => "error",
            Status::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub total: u64,
    pub done_bytes: u64,
    pub fetched_session: u64,
    pub speed_bps: f64,
    pub conns: usize,
    pub status: Status,
    /// one byte per block: 0 pending, 1 done, 2 active
    pub blocks: Vec<u8>,
    pub resumable: bool,
    pub error: Option<String>,
    /// cumulative bytes fetched by each connection this session —
    /// the data behind IDM's per-connection progress table
    pub conn_bytes: Vec<u64>,
}

#[derive(Serialize, Deserialize, Clone)]
struct State {
    url: String,
    total: u64,
    block_size: u64,
    etag: Option<String>,
    /// second validator for servers without ETags (older saves lack it)
    #[serde(default)]
    last_modified: Option<String>,
    done: Vec<u8>,
}

impl State {
    fn done_bytes(&self) -> u64 {
        let full = self.done.iter().filter(|&&b| b == 1).count() as u64;
        // saturating: a crafted state file (huge block_size) must never
        // overflow this multiply — the guard below rejects those files,
        // but this function must stay safe for any State value
        full.saturating_mul(self.block_size).min(self.total)
    }
}

pub struct Control {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

impl Control {
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
    }
}

struct Shared {
    state: Mutex<State>,
    state_path: PathBuf,
    next_block: AtomicUsize,
    done_count: AtomicUsize,
    fetched_session: AtomicU64,
    since_save: AtomicU64,
    last_save: Mutex<Instant>,
    active_limit: AtomicUsize,
    active_workers: AtomicUsize,
    throttle_events: AtomicU64,
    failed: AtomicBool,
    error: Mutex<Option<String>>,
    control: Arc<Control>,
    auth: Auth,
    block_state: Vec<AtomicU8>,
    status: Mutex<Status>,
    /// bytes each worker has written toward its current, unfinished block
    /// (indexed by worker id) — lets progress show partial blocks without
    /// ever double-counting re-fetched bytes
    partial: Vec<AtomicU64>,
    /// cumulative bytes fetched per worker this session (never reset
    /// mid-session) — IDM's per-connection "Downloaded" column
    conn_bytes: Vec<AtomicU64>,
    /// (instant, fetched_session) samples for the sliding-window
    /// transfer rate shown to the user — IDM displays the CURRENT
    /// rate, not a session average diluted by pauses and slow starts
    rate_hist: Mutex<std::collections::VecDeque<(Instant, u64)>>,
    /// this download's own speed limit in bytes/sec (0 = none) —
    /// IDM's per-download Speed Limiter; composes with the global one
    own_limit: AtomicU64,
    own_limit_state: Mutex<Option<Instant>>,
}

fn save_state(shared: &Shared, force: bool) {
    let mut last = if force {
        shared.last_save.lock().unwrap()
    } else {
        match shared.last_save.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        }
    };
    if !force
        && last.elapsed() < SAVE_EVERY
        && shared.since_save.load(Ordering::Relaxed) < SAVE_EVERY_BYTES
    {
        return;
    }
    let snapshot = shared.state.lock().unwrap().clone();
    // write-then-rename keeps the on-disk state valid even if killed mid-save
    let tmp = shared.state_path.with_extension("json.tmp");
    let mut saved = false;
    if let Ok(f) = File::create(&tmp) {
        let write_ok = serde_json::to_writer(&f, &snapshot).is_ok();
        let sync_ok = !force || f.sync_all().is_ok();
        if write_ok && sync_ok {
            saved = std::fs::rename(&tmp, &shared.state_path).is_ok();
        }
    }
    if saved {
        if force {
            if let Some(dir) = shared.state_path.parent() {
                if let Ok(d) = File::open(dir) {
                    let _ = d.sync_all();
                }
            }
        }
        shared.since_save.store(0, Ordering::Relaxed);
        *last = Instant::now();
    }
}

pub struct Probe {
    pub total: u64,
    pub range_ok: bool,
    /// the server gives no length at all (chunked/generated responses):
    /// the file can still be fetched single-stream, size unknown upfront
    pub unknown_size: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub suggested_name: Option<String>,
    pub final_url: String,
}

/// Extract a clean filename from a `Content-Disposition` header value
/// (`filename*=UTF-8''...` or `filename="..."`) or, failing that, from
/// the final post-redirect URL when it differs from `original_url`.
fn extract_suggested_name(resp: &ureq::Response, original_url: &str) -> Option<String> {
    if let Some(cd) = resp.header("Content-Disposition") {
        // 1. RFC 5987 filename*=UTF-8''...
        for part in cd.split(';').map(|s| s.trim()) {
            if let Some(rest) = part
                .strip_prefix("filename*=")
                .or_else(|| part.strip_prefix("FILENAME*="))
            {
                let val = rest.trim().trim_matches('"');
                let encoded = val
                    .split_once("''")
                    .map(|(_, r)| r)
                    .unwrap_or(val);
                let decoded = percent_decode(encoded);
                let cleaned = sanitize_suggested_name(&decoded);
                if !cleaned.is_empty() {
                    return Some(cleaned);
                }
            }
        }
        // 2. Standard filename="..."
        for part in cd.split(';').map(|s| s.trim()) {
            if let Some(rest) = part
                .strip_prefix("filename=")
                .or_else(|| part.strip_prefix("FILENAME="))
            {
                let val = rest.trim().trim_matches(|c| c == '"' || c == '\'');
                let decoded = percent_decode(val);
                let cleaned = sanitize_suggested_name(&decoded);
                if !cleaned.is_empty() {
                    return Some(cleaned);
                }
            }
        }
    }
    let final_url = resp.get_url();
    if !final_url.is_empty() && final_url != original_url {
        let from_final = file_name_of(final_url);
        if from_final != "index" {
            return Some(from_final);
        }
    }
    None
}

fn sanitize_suggested_name(raw: &str) -> String {
    let tail = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = tail
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        String::new()
    } else {
        trimmed.to_string()
    }
}

pub(crate) const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

/// Extra request headers captured from the user's own browser session
/// for a download the browser handed to NDM (IDM-style catching):
/// sites that gate a file behind a login/session cookie or a referer
/// check refuse a bare request, so the probe and every transfer
/// request repeat them, exactly as the browser itself would send.
#[derive(Clone, Default)]
pub struct Auth {
    pub cookie: Option<String>,
    pub referer: Option<String>,
    /// full Authorization header value (e.g. "Basic …") from the
    /// Add URL dialog's "Use authorization" fields (IDM parity)
    pub authorization: Option<String>,
}

fn apply_auth(req: ureq::Request, auth: &Auth) -> ureq::Request {
    let mut req = req;
    if let Some(c) = &auth.cookie {
        req = req.set("Cookie", c);
    }
    if let Some(r) = &auth.referer {
        req = req.set("Referer", r);
    }
    if let Some(a) = &auth.authorization {
        req = req.set("Authorization", a);
    }
    req
}

fn build_agent_for(url: &str) -> ureq::Agent {
    apply_proxy_for(ureq::AgentBuilder::new(), url)
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .max_idle_connections(256)
        .max_idle_connections_per_host(64)
        .user_agent(BROWSER_UA)
        .build()
}

fn build_agent() -> ureq::Agent {
    apply_proxy(ureq::AgentBuilder::new())
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .max_idle_connections(256)
        .max_idle_connections_per_host(64)
        .user_agent(BROWSER_UA)
        .build()
}

pub fn probe(url: &str) -> Result<Probe, String> {
    probe_with(&probe_agent_for(url), url, &Auth::default())
}

pub fn probe_with_auth(url: &str, auth: &Auth) -> Result<Probe, String> {
    probe_with(&probe_agent_for(url), url, auth)
}

/// Probing must not hang a download start for minutes: a dedicated agent
/// with a short timeout, separate from the transfer agent.
fn probe_agent_for(url: &str) -> ureq::Agent {
    apply_proxy_for(ureq::AgentBuilder::new(), url)
        .timeout(Duration::from_secs(15))
        .user_agent(BROWSER_UA)
        .build()
}

/// Parse a `Content-Range: bytes <start>-<end>/<total>` header value.
fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let v = v.trim().strip_prefix("bytes ")?.trim_start();
    let (range, total) = v.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start_num: u64 = start.trim().parse().ok()?;
    let end_num: u64 = end.trim().parse().ok()?;
    let total_num: Option<u64> = total.trim().parse().ok();
    Some((start_num, end_num, total_num))
}

/// Confirm with a real 1-byte request that the server honours Range —
/// some servers advertise `Accept-Ranges: bytes` but answer 200 to actual
/// range requests; trusting the header alone bricks those downloads.
fn verify_range(agent: &ureq::Agent, url: &str, total: u64, header_claim: bool, auth: &Auth) -> bool {
    match apply_auth(agent.get(url), auth)
        .set("Range", "bytes=0-0")
        .set("Accept-Encoding", "identity")
        .call()
    {
        Ok(resp) => {
            if resp.status() != 206 {
                return false;
            }
            // the answer must be exactly the byte we asked for, from a
            // file of exactly the probed size — a server that streams
            // from a different offset is not range-capable for our use
            resp.header("Content-Range")
                .and_then(parse_content_range)
                .map(|(s, e, t)| s == 0 && e == 0 && (t.is_none() || t == Some(total)))
                .unwrap_or(false)
        }
        // a definitive HTTP refusal settles it (the claim is false); only
        // a transport failure leaves the question open — trust the header
        Err(ureq::Error::Status(..)) => false,
        Err(_) => header_claim,
    }
}

fn probe_with(agent: &ureq::Agent, url: &str, auth: &Auth) -> Result<Probe, String> {
    let mut head_suggested: Option<String> = None;
    let mut head_final_url: Option<String> = None;
    if let Ok(resp) = apply_auth(agent.head(url), auth).set("Accept-Encoding", "identity").call() {
        let resolved = resp.get_url().to_string();
        let etag = resp.header("ETag").map(|x| x.to_string());
        let last_modified = resp.header("Last-Modified").map(|x| x.to_string());
        let suggested_name = extract_suggested_name(&resp, url);
        if let Some(total) = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
        {
            if total > 0 {
                let header_claim = resp
                    .header("Accept-Ranges")
                    .map(|v| v.eq_ignore_ascii_case("bytes"))
                    .unwrap_or(false);
                let range_ok = verify_range(agent, &resolved, total, header_claim, auth);
                return Ok(Probe {
                    total,
                    range_ok,
                    unknown_size: false,
                    etag,
                    last_modified,
                    suggested_name,
                    final_url: resolved,
                });
            }
        }
        head_suggested = suggested_name;
        head_final_url = Some(resolved);
    }
    let resp = match apply_auth(agent.get(url), auth)
        .set("Range", "bytes=0-0")
        .set("Accept-Encoding", "identity")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(416, _)) => {
            return Ok(Probe {
                total: 0,
                range_ok: false,
                unknown_size: false,
                etag: None,
                last_modified: None,
                suggested_name: head_suggested,
                final_url: head_final_url.unwrap_or_else(|| url.to_string()),
            });
        }
        Err(e) => return Err(format!("probe failed: {e}")),
    };
    let resolved = resp.get_url().to_string();
    let suggested_name = extract_suggested_name(&resp, url).or(head_suggested);
    if resp.status() == 206 {
        let (s, _e, total_opt) = resp
            .header("Content-Range")
            .and_then(parse_content_range)
            .ok_or("probe: no usable Content-Range")?;
        if s != 0 {
            return Err(
                "probe: server answered a range request from the wrong offset".to_string(),
            );
        }
        let (total, range_ok, unknown_size) = match total_opt {
            Some(t) => (t, true, false),
            // Content-Range "bytes 0-0/*": the total is indeterminate.
            // NEVER derive it from this response's Content-Length — that
            // is the length of the 1-byte partial body, and trusting it
            // once produced a "finished" 1-byte file. Unknown size means
            // a streamed download, exactly like IDM treats it.
            None => (0, false, true),
        };
        return Ok(Probe {
            total,
            range_ok,
            unknown_size,
            etag: resp.header("ETag").map(|s| s.to_string()),
            last_modified: resp.header("Last-Modified").map(|s| s.to_string()),
            suggested_name,
            final_url: resolved,
        });
    }
    let etag = resp.header("ETag").map(|s| s.to_string());
    let last_modified = resp.header("Last-Modified").map(|s| s.to_string());
    let total: u64 = match resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
    {
        Some(t) => t,
        None => {
            return Ok(Probe {
                total: 0,
                range_ok: false,
                unknown_size: true,
                etag,
                last_modified,
                suggested_name,
                final_url: resolved,
            });
        }
    };
    Ok(Probe {
        total,
        range_ok: false,
        unknown_size: false,
        etag,
        last_modified,
        suggested_name,
        final_url: resolved,
    })
}

enum BlockError {
    Retry(String),
    Paused,
    Cancelled,
}

struct ActiveWorkerGuard<'a>(&'a AtomicUsize);
impl<'a> ActiveWorkerGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}
impl Drop for ActiveWorkerGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn download_block_run(
    agent: &ureq::Agent,
    url: &str,
    file: &File,
    cur_block: &mut usize,
    b_end_excl: usize,
    block_size: u64,
    total: u64,
    end: u64,
    pos: &mut u64,
    shared: &Shared,
    worker_id: usize,
) -> Result<(), BlockError> {
    if *pos > end {
        return Ok(());
    }
    let _active_guard = ActiveWorkerGuard::new(&shared.active_workers);
    let req_start = *pos;
    let is_gvideo = url.contains("googlevideo.com/videoplayback")
        && !url.contains("&range=")
        && !url.contains("?range=");
    let req_url = if is_gvideo {
        let sep = if url.contains('?') { '&' } else { '?' };
        format!("{url}{sep}range={req_start}-{end}")
    } else {
        url.to_string()
    };
    let mut req = apply_auth(agent.get(&req_url), &shared.auth)
        .set("Accept-Encoding", "identity");
    if !is_gvideo {
        req = req.set("Range", &format!("bytes={req_start}-{end}"));
    }
    let resp = req.call().map_err(|e| {
        if matches!(e, ureq::Error::Status(403, _) | ureq::Error::Status(429, _)) {
            shared.throttle_events.fetch_add(1, Ordering::Relaxed);
        }
        BlockError::Retry(format!("http: {e}"))
    })?;
    let status = resp.status();
    // the end of THIS response — a server may legally answer a
    // shorter range than the run requested; the run's own `end`
    // stays the target and the worker re-calls us for the remainder
    let mut resp_end = end;
    if is_gvideo && status == 200 {
        // googlevideo &range= query parameter returns 200 OK for the
        // requested byte slice — but a 200 can also be the WHOLE file
        // (range ignored, redirect to a generic mirror), and writing
        // that body at this run's offset would silently corrupt the
        // file and still report Finished. The slice length is the one
        // trustworthy check: require Content-Length to match it
        // exactly; anything else is a retry, never assembled data.
        let want = end - req_start + 1;
        let cl_ok = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|cl| cl == want)
            .unwrap_or(false);
        if !cl_ok {
            return Err(BlockError::Retry(
                "googlevideo 200 response length does not match the requested slice".to_string(),
            ));
        }
    } else if status == 206 {
        // RFC 7233 lets a server return FEWER bytes than requested.
        // Accept a shorter range that starts at the right offset:
        // this call fills only up to resp_end and returns Ok with
        // the block cursor short of the run end, and the worker
        // calls again for the remainder (progress, not a retry).
        // Only a wrong start offset or a mismatched total fails.
        let parsed = resp.header("Content-Range").and_then(parse_content_range);
        match parsed {
            Some((s, e, t)) if s == req_start && e <= end && (t.is_none() || t == Some(total)) => {
                resp_end = e;
            }
            _ => {
                return Err(BlockError::Retry(
                    "server answered a range request with the wrong range".to_string(),
                ));
            }
        }
    } else {
        return Err(BlockError::Retry(format!(
            "server answered {status} to a range request"
        )));
    }

    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 512 * 1024];
    loop {
        if shared.control.cancelled.load(Ordering::Relaxed) {
            return Err(BlockError::Cancelled);
        }
        if shared.control.paused.load(Ordering::Relaxed) {
            return Err(BlockError::Paused);
        }
        let remaining = resp_end + 1 - *pos;
        if remaining == 0 {
            break;
        }
        let want = (buf.len() as u64).min(remaining) as usize;
        let read_at = Instant::now();
        let n = reader
            .read(&mut buf[..want])
            .map_err(|e| BlockError::Retry(format!("read: {e}")))?;
        if n == 0 {
            break;
        }
        write_at(file, &buf[..n], *pos).map_err(|e| BlockError::Retry(format!("write: {e}")))?;
        *pos += n as u64;
        shared.fetched_session.fetch_add(n as u64, Ordering::Relaxed);
        if let Some(cb) = shared.conn_bytes.get(worker_id) {
            cb.fetch_add(n as u64, Ordering::Relaxed);
        }
        shared.since_save.fetch_add(n as u64, Ordering::Relaxed);

        // Mark any completed blocks in this contiguous run immediately
        while *cur_block < b_end_excl {
            let b_limit = ((*cur_block as u64 + 1) * block_size).min(total);
            if *pos >= b_limit {
                {
                    let mut st = shared.state.lock().unwrap();
                    if st.done[*cur_block] == 0 {
                        st.done[*cur_block] = 1;
                        shared.done_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
                shared.block_state[*cur_block].store(1, Ordering::Relaxed);
                *cur_block += 1;
                if *cur_block < b_end_excl {
                    shared.block_state[*cur_block].store(2, Ordering::Relaxed);
                }
            } else {
                break;
            }
        }
        // persist completions continuously (save_state throttles
        // itself to 1 s / 4 MiB and try-locks): a crash or power loss
        // must lose at most the in-flight block, never a whole claim
        // run of up to 256 blocks whose marks existed only in RAM
        save_state(shared, false);
        if *cur_block < b_end_excl {
            let b_base = (*cur_block as u64) * block_size;
            shared.partial[worker_id].store((*pos).saturating_sub(b_base), Ordering::Relaxed);
        } else {
            shared.partial[worker_id].store(0, Ordering::Relaxed);
        }
        throttle_global(n as u64, read_at);
        throttle_own(shared, n as u64, read_at);
    }
    if *pos != resp_end + 1 {
        return Err(BlockError::Retry(format!(
            "short block run: got {} bytes, expected to reach {}",
            *pos,
            resp_end + 1
        )));
    }
    Ok(())
}

fn wait_while_paused(shared: &Shared) -> bool {
    // returns false when cancelled while waiting
    while shared.control.paused.load(Ordering::Relaxed) {
        if shared.control.cancelled.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !shared.control.cancelled.load(Ordering::Relaxed)
}

fn worker(
    id: usize,
    agent: ureq::Agent,
    url: String,
    file: Arc<File>,
    shared: Arc<Shared>,
    total_blocks: usize,
) {
    let (block_size, total) = {
        let st = shared.state.lock().unwrap();
        (st.block_size, st.total)
    };
    loop {
        if shared.failed.load(Ordering::Relaxed)
            || shared.control.cancelled.load(Ordering::Relaxed)
        {
            return;
        }
        if shared.control.paused.load(Ordering::Relaxed) {
            if !wait_while_paused(&shared) {
                return;
            }
            continue;
        }
        let active = shared.active_limit.load(Ordering::Relaxed).max(1);
        if id >= active {
            if shared.done_count.load(Ordering::Relaxed) >= total_blocks {
                return;
            }
            std::thread::sleep(Duration::from_millis(80));
            continue;
        }
        // Claim a contiguous run of blocks so each worker keeps its TCP window
        // saturated over a multi-megabyte stream, shrinking automatically to 1
        // block at the end of the file so all workers share the tail.
        let cur_next = shared.next_block.load(Ordering::Relaxed);
        if cur_next >= total_blocks {
            return;
        }
        let rem_blocks = total_blocks.saturating_sub(cur_next);
        let run_len = (rem_blocks / active).clamp(1, 256);
        let claim_start = shared.next_block.fetch_add(run_len, Ordering::Relaxed);
        if claim_start >= total_blocks {
            return;
        }
        let claim_end = (claim_start + run_len).min(total_blocks);

        // Process any undone sub-runs inside [claim_start, claim_end)
        let mut cursor = claim_start;
        while cursor < claim_end {
            let (b_first, b_last) = {
                let st = shared.state.lock().unwrap();
                while cursor < claim_end && st.done[cursor] == 1 {
                    cursor += 1;
                }
                if cursor >= claim_end {
                    break;
                }
                let first = cursor;
                while cursor < claim_end && st.done[cursor] == 0 {
                    cursor += 1;
                }
                (first, cursor)
            };

            let start = b_first as u64 * block_size;
            let end = ((b_last as u64) * block_size - 1).min(total - 1);
            let mut cur_block = b_first;
            let mut cur_pos = start;
            let mut attempt = 0;
            shared.block_state[cur_block].store(2, Ordering::Relaxed);

            loop {
                match download_block_run(
                    &agent,
                    &url,
                    &file,
                    &mut cur_block,
                    b_last,
                    block_size,
                    total,
                    end,
                    &mut cur_pos,
                    &shared,
                    id,
                ) {
                    Ok(()) => {
                        shared.partial[id].store(0, Ordering::Relaxed);
                        save_state(&shared, false);
                        if cur_block >= b_last {
                            break;
                        }
                        // partial run: the server answered a shorter
                        // range than the sub-run — loop and fetch the
                        // next chunk of the SAME sub-run. This is
                        // progress, not a failure: the attempt counter
                        // must not move, or a server capping responses
                        // at 512 KiB would burn the retry budget on a
                        // multi-megabyte run and fail the download.
                    }
                    Err(BlockError::Paused) => {
                        if cur_block < b_last {
                            shared.block_state[cur_block].store(0, Ordering::Relaxed);
                        }
                        if !wait_while_paused(&shared) {
                            shared.partial[id].store(0, Ordering::Relaxed);
                            return;
                        }
                        if cur_block < b_last {
                            shared.block_state[cur_block].store(2, Ordering::Relaxed);
                        }
                    }
                    Err(BlockError::Cancelled) => {
                        if cur_block < b_last {
                            shared.block_state[cur_block].store(0, Ordering::Relaxed);
                        }
                        shared.partial[id].store(0, Ordering::Relaxed);
                        return;
                    }
                    Err(BlockError::Retry(e)) => {
                        attempt += 1;
                        if attempt >= 5 {
                            shared.partial[id].store(0, Ordering::Relaxed);
                            let mut err = shared.error.lock().unwrap();
                            *err = Some(format!("block {cur_block}: {e}"));
                            shared.failed.store(true, Ordering::Relaxed);
                            if cur_block < b_last {
                                shared.block_state[cur_block].store(0, Ordering::Relaxed);
                            }
                            return;
                        }
                        let backoff = (300u64 << (attempt - 1)).min(5_000);
                        let jitter = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| (d.as_nanos() % 150) as u64)
                            .unwrap_or(0);
                        std::thread::sleep(Duration::from_millis(backoff + jitter));
                    }
                }
            }
        }
    }
}

fn governor(shared: Arc<Shared>, max_conn: usize, total_blocks: usize) {
    let mut last_bytes = 0u64;
    let mut last_check = Instant::now();
    let mut last_throttles = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(1500));
        if shared.done_count.load(Ordering::Relaxed) >= total_blocks
            || shared.failed.load(Ordering::Relaxed)
            || shared.control.cancelled.load(Ordering::Relaxed)
        {
            return;
        }
        let now_bytes = shared.fetched_session.load(Ordering::Relaxed);
        let rate = (now_bytes - last_bytes) as f64 / last_check.elapsed().as_secs_f64();
        last_bytes = now_bytes;
        last_check = Instant::now();
        let throttles = shared.throttle_events.load(Ordering::Relaxed);
        let limit = shared.active_limit.load(Ordering::Relaxed);
        if throttles > last_throttles {
            last_throttles = throttles;
            if limit > 1 {
                shared.active_limit.store((limit / 2).max(1), Ordering::Relaxed);
            }
        } else if limit < max_conn && rate > 0.0 {
            shared
                .active_limit
                .store((limit * 2).min(max_conn), Ordering::Relaxed);
        }
    }
}

pub struct Download {
    pub control: Arc<Control>,
    shared: Arc<Shared>,
    total: u64,
    resumable: bool,
    started: Instant,
}

impl Download {
    pub fn progress(&self) -> Progress {
        let elapsed = self.started.elapsed().as_secs_f64();
        let fetched = self.shared.fetched_session.load(Ordering::Relaxed);
        let status = *self.shared.status.lock().unwrap();
        let blocks: Vec<u8> = if status == Status::Finished {
            vec![1u8; self.shared.block_state.len().max(1)]
        } else if self.resumable {
            self.shared
                .block_state
                .iter()
                .map(|b| b.load(Ordering::Relaxed))
                .collect()
        } else {
            Vec::new()
        };
        let conns = if status == Status::Downloading {
            let live = self.shared.active_workers.load(Ordering::Relaxed);
            let active_blocks = blocks.iter().filter(|&&b| b == 2).count();
            live.max(active_blocks).max(1)
        } else {
            0
        };
        Progress {
            total: self.total,
            done_bytes: if status == Status::Finished && self.total > 0 {
                self.total
            } else if self.resumable {
                // completed blocks + what each worker has already written
                // toward its current block: smooth, and never ahead of the
                // bytes actually on disk
                let persisted = self.shared.state.lock().unwrap().done_bytes();
                let partial: u64 = self
                    .shared
                    .partial
                    .iter()
                    .map(|p| p.load(Ordering::Relaxed))
                    .sum();
                persisted.saturating_add(partial).min(self.total)
            } else {
                fetched
            },
            fetched_session: fetched,
            speed_bps: if status == Status::Downloading {
                // current transfer rate over a ~6 s sliding window
                // (IDM shows the live rate); the session average would
                // decay toward zero across every pause and slow start
                let mut hist = self.shared.rate_hist.lock().unwrap();
                let now = Instant::now();
                if hist.back().map(|(_, b)| *b) != Some(fetched) {
                    hist.push_back((now, fetched));
                }
                while hist.len() > 2
                    && now.duration_since(hist[0].0) > Duration::from_secs(6)
                {
                    hist.pop_front();
                }
                match hist.front() {
                    Some((t0, b0)) => {
                        let dt = now.duration_since(*t0).as_secs_f64();
                        if dt >= 1.0 && fetched > *b0 {
                            (fetched - *b0) as f64 / dt
                        } else if elapsed > 0.0 {
                            fetched as f64 / elapsed
                        } else {
                            0.0
                        }
                    }
                    None => 0.0,
                }
            } else {
                0.0
            },
            conns,
            status,
            blocks,
            resumable: self.resumable,
            error: self.shared.error.lock().unwrap().clone(),
            conn_bytes: self
                .shared
                .conn_bytes
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .collect(),
        }
    }

    /// Set this download's own speed limit (bytes/sec, 0 = none) —
    /// IDM's per-download Speed Limiter tab. Applies immediately to a
    /// running download and composes with the global limit.
    pub fn set_limit(&self, bytes_per_sec: u64) {
        self.shared.own_limit.store(bytes_per_sec, Ordering::Relaxed);
        if bytes_per_sec == 0 {
            *self.shared.own_limit_state.lock().unwrap() = None;
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            *self.shared.status.lock().unwrap(),
            Status::Finished | Status::Error | Status::Cancelled
        )
    }

    pub fn status(&self) -> Status {
        *self.shared.status.lock().unwrap()
    }
}

fn set_status(shared: &Shared, s: Status) {
    *shared.status.lock().unwrap() = s;
}



/// Start a download in the background. Progress is polled via `Download::progress`
/// (the CLI polls and prints; the Tauri backend polls and emits events).
pub fn start(
    url: &str,
    out: &Path,
    max_conn: usize,
    block_size: u64,
) -> Result<Arc<Download>, String> {
    start_impl(url, out, max_conn, block_size, &Auth::default())
}

/// Start a download that repeats the browser session headers in
/// `auth` on the probe and on every transfer request (caught browser
/// downloads whose links are login- or referer-gated).
pub fn start_with_auth(
    url: &str,
    out: &Path,
    max_conn: usize,
    block_size: u64,
    auth: Auth,
) -> Result<Arc<Download>, String> {
    start_impl(url, out, max_conn, block_size, &auth)
}

fn start_impl(
    url: &str,
    out: &Path,
    max_conn: usize,
    block_size: u64,
    auth: &Auth,
) -> Result<Arc<Download>, String> {
    if block_size == 0 {
        return Err("block size must be positive".to_string());
    }
    // an absurd block size is clamped, not trusted (overflow bait)
    let block_size = block_size.min(64 * 1024 * 1024);
    let max_conn = max_conn.clamp(1, 64);
    let agent = build_agent_for(url);
    let probed = probe_with(&probe_agent_for(url), url, auth)?;
    let state_path = state_path_for(out);

    let control = Arc::new(Control {
        paused: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
    });

    if probed.total == 0 && !probed.unknown_size {
        // empty file: create it and finish immediately
        File::create(out).map_err(|e| format!("cannot create output file: {e}"))?;
        return Ok(finished_empty_download(control, state_path));
    }

    if !probed.range_ok {
        return start_single_stream(url, out, probed, control, state_path, auth);
    }

    let existing_state = std::fs::read(&state_path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<State>(&raw).ok())
        .filter(|s| {
            s.url == url
                && s.total == probed.total
                && s.block_size > 0
                && s.block_size <= 64 * 1024 * 1024
                && (s.block_size == block_size || block_size == DEFAULT_BLOCK)
                && s.done.len() == s.total.div_ceil(s.block_size) as usize
                // block values are a closed set: anything but 0/1 makes
                // the worker's run scan wedge (a value >1 yields an
                // empty run that completes without progress, forever)
                && s.done.iter().all(|&b| b <= 1)
                && (s.etag.is_none() || probed.etag.is_none() || s.etag == probed.etag)
                && (s.last_modified.is_none()
                    || probed.last_modified.is_none()
                    || s.last_modified == probed.last_modified)
                && out.exists()
                // the partial file must still be the preallocated full
                // size: a truncated/recreated file would make set_len
                // zero-extend regions the state calls downloaded
                // (peek_state applies this same guard)
                && out.metadata().map(|m| m.len()).unwrap_or(0) == s.total
        });

    let block_size = match &existing_state {
        Some(s) => s.block_size,
        None if block_size == DEFAULT_BLOCK => {
            if probed.total > 0 && probed.total < (max_conn as u64) * DEFAULT_BLOCK {
                ((probed.total / max_conn as u64) & !(64 * 1024 - 1)).max(256 * 1024)
            } else if probed.total >= 2 * 1024 * 1024 * 1024 {
                4 * 1024 * 1024
            } else if probed.total >= 512 * 1024 * 1024 {
                2 * 1024 * 1024
            } else {
                DEFAULT_BLOCK
            }
        }
        None => block_size,
    };

    let total_blocks = probed.total.div_ceil(block_size) as usize;
    // the total comes from the network: a hostile or broken server can
    // claim an absurd size, and the block map below allocates from it —
    // refuse before any allocation happens (2^24 blocks at the default
    // 1 MiB is a 16 PiB ceiling, far above any real file)
    if total_blocks > (1 << 24) {
        return Err(format!(
            "server reported an implausible file size ({} bytes)",
            probed.total
        ));
    }
    let mut resumed_blocks = 0usize;
    let state = match existing_state {
        Some(s) => {
            resumed_blocks = s.done.iter().filter(|&&b| b == 1).count();
            s
        }
        None => State {
            url: url.to_string(),
            total: probed.total,
            block_size,
            etag: probed.etag.clone(),
            last_modified: probed.last_modified.clone(),
            done: vec![0u8; total_blocks],
        },
    };

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // never truncate: a resumed download keeps its existing bytes and
        // is only re-sized by the explicit set_len below when it differs
        .truncate(false)
        .open(out)
        .map_err(|e| format!("cannot open output file: {e}"))?;
    if file.metadata().map(|m| m.len()).unwrap_or(0) != probed.total {
        file.set_len(probed.total)
            .map_err(|e| format!("cannot preallocate file: {e}"))?;
    }
    let file = Arc::new(file);

    let block_state: Vec<AtomicU8> = state
        .done
        .iter()
        .map(|&b| AtomicU8::new(b))
        .collect();

    let shared = Arc::new(Shared {
        state: Mutex::new(state),
        state_path,
        next_block: AtomicUsize::new(0),
        done_count: AtomicUsize::new(resumed_blocks),
        fetched_session: AtomicU64::new(0),
        since_save: AtomicU64::new(0),
        last_save: Mutex::new(Instant::now()),
        active_limit: AtomicUsize::new(max_conn.min(total_blocks).max(1)),
        active_workers: AtomicUsize::new(0),
        throttle_events: AtomicU64::new(0),
        failed: AtomicBool::new(false),
        error: Mutex::new(None),
        control: control.clone(),
        block_state,
        status: Mutex::new(Status::Downloading),
        partial: (0..max_conn).map(|_| AtomicU64::new(0)).collect(),
        conn_bytes: (0..max_conn).map(|_| AtomicU64::new(0)).collect(),
        rate_hist: Mutex::new(std::collections::VecDeque::new()),
        own_limit: AtomicU64::new(0),
        own_limit_state: Mutex::new(None),
        auth: auth.clone(),
    });

    let download = Arc::new(Download {
        control,
        shared: shared.clone(),
        total: probed.total,
        resumable: true,
        started: Instant::now(),
    });

    {
        let s = shared.clone();
        std::thread::spawn(move || governor(s, max_conn, total_blocks));
    }
    let mut handles = Vec::new();
    for id in 0..max_conn.max(1) {
        let s = shared.clone();
        let f = file.clone();
        let u = probed.final_url.clone();
        let a = agent.clone();
        handles.push(std::thread::spawn(move || worker(id, a, u, f, s, total_blocks)));
    }
    // supervisor: waits for workers, then settles the final status
    {
        let s = shared.clone();
        std::thread::spawn(move || {
            for h in handles {
                let _ = h.join();
            }
            let _ = file.sync_all();
            save_state(&s, true);
            if s.failed.load(Ordering::Relaxed) {
                set_status(&s, Status::Error);
            } else if s.done_count.load(Ordering::Relaxed) >= total_blocks {
                // byte-complete is Finished even if a cancel landed in
                // the same instant — the file on disk IS the full file
                for b in &s.block_state {
                    b.store(1, Ordering::Relaxed);
                }
                set_status(&s, Status::Finished);
                let _ = std::fs::remove_file(&s.state_path);
            } else if s.control.cancelled.load(Ordering::Relaxed) {
                set_status(&s, Status::Cancelled);
            } else if s.control.paused.load(Ordering::Relaxed) {
                set_status(&s, Status::Paused);
            } else {
                set_status(&s, Status::Error);
                let mut err = s.error.lock().unwrap();
                if err.is_none() {
                    *err = Some("download stopped before completion".to_string());
                }
            }
        });
    }
    // status tracker: keeps Paused/Downloading fresh while running
    {
        let s = shared.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(200));
            // check-and-write under ONE lock hold: a separate check and
            // write let the supervisor's terminal state be overwritten by
            // a stale "downloading" in between, wedging the entry forever
            let mut st = s.status.lock().unwrap();
            if matches!(*st, Status::Finished | Status::Error | Status::Cancelled) {
                return;
            }
            *st = if s.control.cancelled.load(Ordering::Relaxed) {
                Status::Cancelled
            } else if s.failed.load(Ordering::Relaxed) {
                Status::Error
            } else if s.control.paused.load(Ordering::Relaxed) {
                Status::Paused
            } else {
                Status::Downloading
            };
        });
    }
    Ok(download)
}

/// A completed download handle for a zero-byte file (no threads needed).
fn finished_empty_download(control: Arc<Control>, state_path: PathBuf) -> Arc<Download> {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            url: String::new(),
            total: 0,
            block_size: 1,
            etag: None,
            last_modified: None,
            done: vec![1u8; 1],
        }),
        state_path,
        next_block: AtomicUsize::new(0),
        done_count: AtomicUsize::new(1),
        fetched_session: AtomicU64::new(0),
        since_save: AtomicU64::new(0),
        last_save: Mutex::new(Instant::now()),
        active_limit: AtomicUsize::new(1),
        active_workers: AtomicUsize::new(0),
        throttle_events: AtomicU64::new(0),
        failed: AtomicBool::new(false),
        error: Mutex::new(None),
        control: control.clone(),
        block_state: vec![AtomicU8::new(1)],
        status: Mutex::new(Status::Finished),
        partial: vec![AtomicU64::new(0)],
        conn_bytes: vec![AtomicU64::new(0)],
        rate_hist: Mutex::new(std::collections::VecDeque::new()),
        own_limit: AtomicU64::new(0),
        own_limit_state: Mutex::new(None),
        auth: Auth::default(),
    });
    Arc::new(Download {
        control,
        shared,
        total: 0,
        resumable: false,
        started: Instant::now(),
    })
}

fn start_single_stream(
    url: &str,
    out: &Path,
    probed: Probe,
    control: Arc<Control>,
    state_path: PathBuf,
    auth: &Auth,
) -> Result<Arc<Download>, String> {
    // fallback for servers without Range support: no resume, no acceleration
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            url: url.to_string(),
            total: probed.total,
            block_size: 1,
            etag: probed.etag.clone(),
            last_modified: probed.last_modified.clone(),
            done: vec![0u8; 1],
        }),
        state_path,
        next_block: AtomicUsize::new(0),
        done_count: AtomicUsize::new(0),
        fetched_session: AtomicU64::new(0),
        since_save: AtomicU64::new(0),
        last_save: Mutex::new(Instant::now()),
        active_limit: AtomicUsize::new(1),
        active_workers: AtomicUsize::new(1),
        throttle_events: AtomicU64::new(0),
        failed: AtomicBool::new(false),
        error: Mutex::new(None),
        control: control.clone(),
        block_state: vec![AtomicU8::new(0)],
        status: Mutex::new(Status::Downloading),
        partial: vec![AtomicU64::new(0)],
        conn_bytes: vec![AtomicU64::new(0)],
        rate_hist: Mutex::new(std::collections::VecDeque::new()),
        own_limit: AtomicU64::new(0),
        own_limit_state: Mutex::new(None),
        auth: auth.clone(),
    });
    let download = Arc::new(Download {
        control,
        shared: shared.clone(),
        total: probed.total,
        resumable: false,
        started: Instant::now(),
    });
    let expected_total = probed.total;
    let url = url.to_string();
    let out = out.to_path_buf();
    let auth = auth.clone();
    std::thread::spawn(move || {
        let agent = build_agent_for(&url);
        let result: Result<(), String> = (|| {
            shared.active_workers.store(1, Ordering::Relaxed);
            let resp = apply_auth(agent.get(&url), &auth)
                .set("Accept-Encoding", "identity")
                .call()
                .map_err(|e| format!("http: {e}"))?;
            let mut reader = resp.into_reader();
            let f = File::create(&out).map_err(|e| format!("create: {e}"))?;
            let mut buf = vec![0u8; 512 * 1024];
            let mut pos = 0u64;
            loop {
                if shared.control.cancelled.load(Ordering::Relaxed) {
                    set_status(&shared, Status::Cancelled);
                    return Ok(());
                }
                if shared.control.paused.load(Ordering::Relaxed) {
                    set_status(&shared, Status::Paused);
                    if !wait_while_paused(&shared) {
                        set_status(&shared, Status::Cancelled);
                        return Ok(());
                    }
                    set_status(&shared, Status::Downloading);
                }
                let read_at = Instant::now();
                let n = reader.read(&mut buf).map_err(|e| format!("read: {e}"))?;
                if n == 0 {
                    break;
                }
                write_at(&f, &buf[..n], pos).map_err(|e| format!("write: {e}"))?;
                pos += n as u64;
                // the server is sending MORE than the probed size (a
                // HEAD/GET disagreement): stop now instead of writing
                // the whole over-long body and reporting it backwards
                if expected_total > 0 && pos > expected_total {
                    return Err(format!(
                        "server sent more data than the probed size ({expected_total} bytes)"
                    ));
                }
                shared.fetched_session.fetch_add(n as u64, Ordering::Relaxed);
                if let Some(cb) = shared.conn_bytes.first() {
                    cb.fetch_add(n as u64, Ordering::Relaxed);
                }
                throttle_global(n as u64, read_at);
                throttle_own(&shared, n as u64, read_at);
            }
            // a clean early EOF must not pose as a finished file
            if expected_total > 0 && pos != expected_total {
                return Err(format!(
                    "short download: got {pos} of {expected_total} bytes"
                ));
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                shared.active_workers.store(0, Ordering::Relaxed);
                if !shared.control.cancelled.load(Ordering::Relaxed) {
                    shared.done_count.store(1, Ordering::Relaxed);
                    if let Some(b) = shared.block_state.first() {
                        b.store(1, Ordering::Relaxed);
                    }
                    set_status(&shared, Status::Finished);
                }
            }
            Err(e) => {
                shared.active_workers.store(0, Ordering::Relaxed);
                let mut err = shared.error.lock().unwrap();
                *err = Some(e);
                shared.failed.store(true, Ordering::Relaxed);
                set_status(&shared, Status::Error);
            }
        }
    });
    Ok(download)
}

/// What a previous session left on disk for this file, if anything — used by
/// the app to restore the download list after a restart without any network I/O.
pub struct Peeked {
    pub total: u64,
    pub done_bytes: u64,
    /// one byte per block: 0 pending, 1 done
    pub blocks: Vec<u8>,
}

pub fn peek_state(url: &str, out: &Path) -> Option<Peeked> {
    let raw = std::fs::read(state_path_for(out)).ok()?;
    let s = serde_json::from_slice::<State>(&raw).ok()?;
    if s.url != url || !out.exists() {
        return None;
    }
    // the same poison guard start() applies: a state whose block map
    // does not match its own block size must never be rendered or used
    if s.block_size == 0
        || s.block_size > 64 * 1024 * 1024
        || s.done.len() != s.total.div_ceil(s.block_size) as usize
        || s.done.iter().any(|&b| b > 1)
    {
        return None;
    }
    if out.metadata().map(|m| m.len()).unwrap_or(0) != s.total {
        return None;
    }
    Some(Peeked {
        total: s.total,
        done_bytes: s.done_bytes(),
        blocks: s.done.clone(),
    })
}

pub fn state_path_for(out: &Path) -> PathBuf {
    let ext = out
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    if ext.is_empty() {
        out.with_extension("ndm.json")
    } else {
        out.with_extension(format!("{ext}.ndm.json"))
    }
}

pub fn sha256_of(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

pub fn fmt_mb(b: u64) -> f64 {
    b as f64 / (1u64 << 20) as f64
}

// ---------- site grabber ----------
// Fetch a web page, extract every downloadable link (files, images, media,
// archives, linked pages), resolve them against the page URL, and probe each
// one's size. Static HTML only — pages built by JavaScript are the browser
// extension's territory, exactly like IDM splits the two jobs.

#[derive(Clone, Serialize)]
pub struct GrabItem {
    pub url: String,
    pub name: String,
    pub label: String,
    pub size: Option<u64>,
}

#[derive(Clone, Serialize)]
pub struct GrabPage {
    pub title: String,
    pub items: Vec<GrabItem>,
}

const MAX_GRAB_ITEMS: usize = 300;

fn grab_agent_for(url: &str, timeout_secs: u64) -> ureq::Agent {
    apply_proxy_for(ureq::AgentBuilder::new(), url)
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent(BROWSER_UA)
        .build()
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Find `attr="value"` (single, double, or unquoted) inside one tag's text.
fn find_attr(tag: &str, attr: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let needle = attr.as_bytes();
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if bytes[i].eq_ignore_ascii_case(&needle[0])
            && tag[i..]
                .get(..attr.len())
                .map(|w| w.eq_ignore_ascii_case(attr))
                .unwrap_or(false)
            && (i == 0 || bytes[i - 1].is_ascii_whitespace())
        {
            let mut j = i + needle.len();
            // the character after the name must not extend the attribute name
            if j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
                i += 1;
                continue;
            }
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                    let q = bytes[j];
                    let start = j + 1;
                    let mut k = start;
                    while k < bytes.len() && bytes[k] != q {
                        k += 1;
                    }
                    return Some(tag[start..k].to_string());
                }
                let start = j;
                let mut k = start;
                while k < bytes.len() && !bytes[k].is_ascii_whitespace() && bytes[k] != b'>' {
                    k += 1;
                }
                return Some(tag[start..k].to_string());
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    None
}

/// Strip inner tags and collapse whitespace (for anchor labels).
fn plain_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract (raw link, label) pairs from page HTML.
fn extract_links(html: &str) -> Vec<(String, String)> {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // read the tag name
        let mut j = i + 1;
        if j < bytes.len() && bytes[j] == b'/' {
            i = j;
            continue;
        }
        let name_start = j;
        while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j == name_start {
            i += 1;
            continue;
        }
        let tag_name = &lower[name_start..j];
        // find the tag's end, respecting quotes
        let mut k = j;
        let mut quote = 0u8;
        while k < bytes.len() {
            let c = bytes[k];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            k += 1;
        }
        let tag_text = &html[i..k.min(bytes.len())];
        let attr = match tag_name {
            "a" | "link" => Some("href"),
            "img" | "script" | "video" | "audio" | "source" | "embed" | "track" => Some("src"),
            _ => None,
        };
        if let Some(attr) = attr {
            if let Some(raw) = find_attr(tag_text, attr) {
                let raw = decode_entities(raw.trim());
                if !raw.is_empty() {
                    let mut label = String::new();
                    if tag_name == "a" {
                        // anchor text up to the matching close tag (the
                        // scan is bounded — an unclosed <a> on a huge page
                        // must not turn this into an O(n^2) crawl)
                        let scan_end = (k + 4096).min(lower.len());
                        let close = lower[k..scan_end]
                            .find("</a")
                            .map(|p| k + p)
                            .unwrap_or(k);
                        if close > k {
                            label = plain_text(&decode_entities(&html[k + 1..close]));
                            if label.chars().count() > 80 {
                                label = label.chars().take(80).collect();
                            }
                        }
                    }
                    out.push((raw, label));
                }
            }
        } else if tag_name == "meta" {
            let prop = find_attr(tag_text, "property")
                .or_else(|| find_attr(tag_text, "name"))
                .unwrap_or_default()
                .to_ascii_lowercase();
            if prop.contains("og:video") || prop.contains("og:image") || prop.contains("og:audio")
                || prop.contains("twitter:image") || prop.contains("twitter:player:stream")
            {
                if let Some(content) = find_attr(tag_text, "content") {
                    let content = decode_entities(content.trim());
                    if !content.is_empty() {
                        out.push((content, prop));
                    }
                }
            }
        }
        i = k.max(i + 1);
    }
    out
}

/// Resolve `href` against the page URL; None for non-HTTP(S) or empty links.
fn resolve_url(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let href = href.split('#').next().unwrap_or("");
    if href.is_empty() {
        return None;
    }
    // absolute URL with a scheme
    if let Some(colon) = href.find(':') {
        let first_sep = href.find(['/', '?']).unwrap_or(usize::MAX);
        if colon < first_sep {
            let scheme = href[..colon].to_ascii_lowercase();
            if scheme == "http" || scheme == "https" {
                return Some(href.to_string());
            }
            return None; // mailto:, javascript:, data:, tel:, blob: …
        }
    }
    // split base into scheme / origin / path
    let scheme_end = base.find("://")?;
    let scheme = &base[..scheme_end];
    let after = &base[scheme_end + 3..];
    let host_end = after.find(['/', '?']).unwrap_or(after.len());
    let origin = format!("{scheme}://{}", &after[..host_end]);
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    // relative to the page's directory
    let base_path = &after[host_end..];
    let base_path = base_path.split(['?', '#']).next().unwrap_or("");
    let dir = match base_path.rfind('/') {
        Some(p) => &base_path[..=p],
        None => "/",
    };
    let joined = format!("{dir}{}", href.split('?').next().unwrap_or(""));
    let query = href
        .find('?')
        .map(|p| &href[p..])
        .unwrap_or("");
    // normalise "." and ".." segments
    let mut stack: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    Some(format!("{origin}/{}{}", stack.join("/"), query))
}

pub fn file_name_of(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let raw = base.rsplit('/').next().unwrap_or("");
    // percent-decode (%20 etc.) so saved names read the way users expect
    let decoded = percent_decode(raw);
    // a derived name must never escape the download folder or be a dot
    // segment (a URL ending in /.. would otherwise produce "..")
    let cleaned: String = decoded
        .chars()
        .map(|c| {
            // path separators, and the shell metacharacters that would
            // matter if the name ever reaches a command line (Windows
            // open uses cmd): a hostile URL must not smuggle them in
            if matches!(c, '/' | '\\' | '"' | '%' | '^' | '&' | '|' | '<' | '>') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = cleaned.trim();
    if name.is_empty() || name == "." || name == ".." {
        "index".to_string()
    } else {
        name.to_string()
    }
}

fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn probe_size(agent: &ureq::Agent, url: &str) -> Option<u64> {
    if let Ok(resp) = agent.head(url).set("Accept-Encoding", "identity").call() {
        if let Some(len) = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
        {
            if len > 0 {
                return Some(len);
            }
        }
    }
    // fall back to a 1-byte range request and read the total from Content-Range
    if let Ok(resp) = agent.get(url).set("Range", "bytes=0-0").set("Accept-Encoding", "identity").call() {
        if let Some(cr) = resp.header("Content-Range") {
            if let Some(total) = cr.rsplit('/').next().and_then(|v| v.parse::<u64>().ok()) {
                return Some(total);
            }
        }
        if resp.status() == 200 {
            if let Some(len) = resp
                .header("Content-Length")
                .and_then(|v| v.parse::<u64>().ok())
            {
                return Some(len);
            }
        }
    }
    None
}

pub fn grab_page(url: &str) -> Result<GrabPage, String> {
    let agent = grab_agent_for(url, 25);
    let resp = agent
        .get(url)
        .set("Accept-Encoding", "identity")
        .call()
        .map_err(|e| format!("could not fetch the page: {e}"))?;
    let final_url = resp.get_url().to_string();
    let mut body = Vec::new();
    {
        use std::io::Read as _;
        resp.into_reader()
            .take(8 * 1024 * 1024)
            .read_to_end(&mut body)
            .map_err(|e| format!("could not read the page: {e}"))?;
    }
    let html = String::from_utf8_lossy(&body).to_string();

    let title = {
        let lower = html.to_ascii_lowercase();
        let found = (|| {
            let start = lower.find("<title")?;
            let gt = lower[start..].find('>')? + start + 1;
            let end = lower[gt..].find("</title>")? + gt;
            Some(plain_text(&decode_entities(&html[gt..end])))
        })();
        found.unwrap_or_default()
    };

    let mut seen = std::collections::HashSet::new();
    let mut items: Vec<GrabItem> = Vec::new();
    for (raw, label) in extract_links(&html) {
        let Some(resolved) = resolve_url(&final_url, &raw) else {
            continue;
        };
        if !seen.insert(resolved.clone()) {
            continue;
        }
        items.push(GrabItem {
            name: file_name_of(&resolved),
            url: resolved,
            label,
            size: None,
        });
        if items.len() >= MAX_GRAB_ITEMS {
            break;
        }
    }

    // probe sizes in parallel (8 workers over a shared index), with a
    // global time budget so a page full of dead links cannot hold the
    // command open for many minutes — late items just keep size None
    let probe_deadline = Instant::now() + Duration::from_secs(120);
    let probe_agent = grab_agent_for(url, 8);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<u64>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= items.len() {
                    break;
                }
                if Instant::now() > probe_deadline {
                    continue;
                }
                *slots[i].lock().unwrap() = probe_size(&probe_agent, &items[i].url);
            });
        }
    });
    for (item, slot) in items.iter_mut().zip(slots) {
        item.size = *slot.lock().unwrap();
    }

    Ok(GrabPage { title, items })
}


// ---------- site grabber: multi-level exploration (IDM Grabber) ----------
// IDM's Grabber is not a one-page link list: starting from a project's
// start page it EXPLORES — following page links level by level, inside
// the site and (with its own, usually smaller, level budget) outside
// it — and collects the files that pass the project's filters. This is
// that exploration. `grab_page` above stays as the one-page special
// case used by older callers.

#[derive(Clone, Default)]
pub struct GrabOptions {
    /// link levels to explore within the start site (0 = start page only)
    pub depth_same: u32,
    /// link levels to explore on other sites
    pub depth_other: u32,
    /// explore the whole site (depth_same treated as very large; the
    /// page cap still applies)
    pub whole_site: bool,
    /// do not explore pages above the start page's directory
    pub no_parent: bool,
    /// collect files only from the start site
    pub same_site_files_only: bool,
    /// if non-empty, only files with one of these extensions are kept
    pub include_exts: Vec<String>,
    /// files with one of these extensions are never kept
    pub exclude_exts: Vec<String>,
    /// size window in bytes, applied after probing (unknown size keeps
    /// the file — a size cannot disprove what it never reported)
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
}

#[derive(Clone, Serialize)]
pub struct GrabSite {
    pub title: String,
    pub items: Vec<GrabItem>,
    pub pages_visited: usize,
}

const MAX_GRAB_PAGES: usize = 60;
const MAX_SITE_ITEMS: usize = 1000;

fn url_host(url: &str) -> String {
    let after = url.split("://").nth(1).unwrap_or("");
    after
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn url_path(url: &str) -> String {
    let after = url.split("://").nth(1).unwrap_or("");
    match after.find('/') {
        Some(i) => after[i..]
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .to_string(),
        None => "/".to_string(),
    }
}

fn url_ext(url: &str) -> String {
    let path = url_path(url);
    let last = path.rsplit('/').next().unwrap_or("");
    match last.rfind('.') {
        Some(i) if i > 0 => last[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// A link worth EXPLORING (an HTML-ish page) vs a file to collect.
fn is_page_link(url: &str) -> bool {
    let path = url_path(url);
    if path.ends_with('/') {
        return true;
    }
    let ext = url_ext(url);
    ext.is_empty()
        || matches!(
            ext.as_str(),
            "html" | "htm" | "php" | "asp" | "aspx" | "jsp" | "shtml" | "xhtml"
        )
}

fn fetch_page(agent: &ureq::Agent, url: &str) -> Option<(String, String)> {
    let resp = agent.get(url).set("Accept-Encoding", "identity").call().ok()?;
    let final_url = resp.get_url().to_string();
    let mut body = Vec::new();
    {
        use std::io::Read as _;
        resp.into_reader()
            .take(8 * 1024 * 1024)
            .read_to_end(&mut body)
            .ok()?;
    }
    Some((final_url, String::from_utf8_lossy(&body).to_string()))
}

pub fn grab_site(start_url: &str, opts: &GrabOptions) -> Result<GrabSite, String> {
    let start_host = url_host(start_url);
    if start_host.is_empty() {
        return Err("the start page address is not a valid URL".to_string());
    }
    let start_path = url_path(start_url);
    let start_dir = match start_path.rfind('/') {
        Some(p) => start_path[..=p].to_string(),
        None => "/".to_string(),
    };
    let depth_same = if opts.whole_site { 10 } else { opts.depth_same.min(10) };
    let depth_other = opts.depth_other.min(10);
    let include: Vec<String> = opts
        .include_exts
        .iter()
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    let exclude: Vec<String> = opts
        .exclude_exts
        .iter()
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    let agent = grab_agent_for(start_url, 25);
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<(String, u32)> = std::collections::VecDeque::new();
    queue.push_back((start_url.to_string(), 0));
    visited.insert(start_url.to_string());
    let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut items: Vec<GrabItem> = Vec::new();
    let mut title = String::new();
    let mut pages_visited = 0usize;

    while let Some((page_url, depth)) = queue.pop_front() {
        if pages_visited >= MAX_GRAB_PAGES {
            break;
        }
        let Some((final_url, html)) = fetch_page(&agent, &page_url) else {
            continue;
        };
        pages_visited += 1;
        if pages_visited == 1 {
            let lower = html.to_ascii_lowercase();
            title = (|| {
                let start = lower.find("<title")?;
                let gt = lower[start..].find('>')? + start + 1;
                let end = lower[gt..].find("</title>")? + gt;
                Some(plain_text(&decode_entities(&html[gt..end])))
            })()
            .unwrap_or_default();
        }
        for (raw, label) in extract_links(&html) {
            let Some(resolved) = resolve_url(&final_url, &raw) else {
                continue;
            };
            let same_site = url_host(&resolved) == start_host;
            if is_page_link(&resolved) {
                // exploration budget: same-site and other-site links
                // have their own level allowance, like IDM Step 3
                let limit = if same_site { depth_same } else { depth_other };
                if depth >= limit {
                    continue;
                }
                if opts.no_parent && same_site && !url_path(&resolved).starts_with(&start_dir) {
                    continue;
                }
                if visited.insert(resolved.clone()) {
                    queue.push_back((resolved, depth + 1));
                }
            } else {
                if opts.same_site_files_only && !same_site {
                    continue;
                }
                let ext = url_ext(&resolved);
                if !include.is_empty() && !include.contains(&ext) {
                    continue;
                }
                if exclude.contains(&ext) {
                    continue;
                }
                if seen_files.insert(resolved.clone()) {
                    items.push(GrabItem {
                        name: file_name_of(&resolved),
                        url: resolved,
                        label,
                        size: None,
                    });
                    if items.len() >= MAX_SITE_ITEMS {
                        break;
                    }
                }
            }
        }
        if items.len() >= MAX_SITE_ITEMS {
            break;
        }
    }
    if pages_visited == 0 {
        return Err("could not fetch the start page".to_string());
    }

    // probe sizes in parallel, with the same global budget as grab_page
    let probe_deadline = Instant::now() + Duration::from_secs(120);
    let probe_agent = grab_agent_for(start_url, 8);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<u64>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= items.len() {
                    break;
                }
                if Instant::now() > probe_deadline {
                    continue;
                }
                *slots[i].lock().unwrap() = probe_size(&probe_agent, &items[i].url);
            });
        }
    });
    for (item, slot) in items.iter_mut().zip(slots) {
        item.size = *slot.lock().unwrap();
    }
    if opts.min_size.is_some() || opts.max_size.is_some() {
        items.retain(|it| match it.size {
            Some(sz) => {
                opts.min_size.map(|m| sz >= m).unwrap_or(true)
                    && opts.max_size.map(|m| sz <= m).unwrap_or(true)
            }
            None => true,
        });
    }

    Ok(GrabSite {
        title,
        items,
        pages_visited,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_of_decodes_and_sanitizes_safely() {
        assert_eq!(
            file_name_of("https://example.com/files/Ubuntu%2024.04%20LTS.iso?token=1"),
            "Ubuntu 24.04 LTS.iso"
        );
        assert_eq!(file_name_of("https://example.com/.."), "index");
        assert_eq!(file_name_of("https://example.com/"), "index");
    }

    #[test]
    fn finished_download_reports_zero_active_conns_and_all_done_blocks() {
        let control = Arc::new(Control {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        let dl = finished_empty_download(control, PathBuf::from("/tmp/nonexistent.ndm.json"));
        let p = dl.progress();
        assert_eq!(p.status, Status::Finished);
        assert_eq!(p.conns, 0);
        assert!(!p.blocks.is_empty());
        assert!(p.blocks.iter().all(|&b| b == 1));
    }

    #[test]
    fn parse_content_range_handles_known_and_indeterminate_totals() {
        assert_eq!(
            parse_content_range("bytes 0-1048575/104857600"),
            Some((0, 1048575, Some(104857600)))
        );
        assert_eq!(parse_content_range("bytes 0-512/*"), Some((0, 512, None)));
        assert_eq!(parse_content_range("invalid"), None);
    }

    // --- 1.9.0 completion: per-protocol proxy gating + Grabber crawl ---

    #[test]
    fn proxy_for_url_respects_protocol_toggles() {
        set_global_proxy("http://127.0.0.1:9999");
        set_proxy_protocols(true, true, true);
        assert_eq!(proxy_for_url("http://a.com/f"), "http://127.0.0.1:9999");
        set_proxy_protocols(false, true, true);
        assert_eq!(proxy_for_url("http://a.com/f"), "");
        assert_eq!(proxy_for_url("https://a.com/f"), "http://127.0.0.1:9999");
        set_proxy_protocols(true, true, true);
        set_global_proxy("");
    }

    /// A tiny canned web site on a loopback port: two "hosts"
    /// (127.0.0.1 and localhost) sharing one server, pages at two
    /// depths, files of known sizes.
    fn canned_site() -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("/").to_string();
                    // drain headers
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    let zip = vec![b'z'; 100];
                    let pdf = vec![b'p'; 200];
                    let bin = vec![b'b'; 300];
                    let idx = format!(
                        "<html><title>Root</title><a href=\"/site/a.zip\">zip</a><a href=\"/site/sub/page.html\">sub</a><a href=\"http://localhost:{port}/ext/page2.html\">ext</a></html>"
                    );
                    let sub = "<html><title>Sub</title><a href=\"/site/files/deep.pdf\">pdf</a><a href=\"/site/index.html\">back</a></html>".to_string();
                    let ext = "<html><title>Ext</title><a href=\"/ext/deep.bin\">bin</a></html>".to_string();
                    let (ctype, body): (&str, Vec<u8>) = match path.as_str() {
                        "/site/index.html" => ("text/html", idx.into_bytes()),
                        "/site/sub/page.html" => ("text/html", sub.into_bytes()),
                        "/ext/page2.html" => ("text/html", ext.into_bytes()),
                        "/site/a.zip" => ("application/zip", zip),
                        "/site/files/deep.pdf" => ("application/pdf", pdf),
                        "/ext/deep.bin" => ("application/octet-stream", bin),
                        _ => {
                            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            return;
                        }
                    };
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                                .as_bytes(),
                        );
                    } else {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                                .as_bytes(),
                        );
                        let _ = s.write_all(&body);
                    }
                });
            }
        });
        port
    }

    fn grab_names(site: &GrabSite) -> Vec<String> {
        let mut v: Vec<String> = site.items.iter().map(|i| i.name.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn grab_site_depth_and_filters_follow_idm_rules() {
        let port = canned_site();
        let root = format!("http://127.0.0.1:{port}/site/index.html");

        // depth 0: only the start page is read
        let r = grab_site(&root, &GrabOptions::default()).unwrap();
        assert_eq!(grab_names(&r), vec!["a.zip".to_string()], "depth0: {:?}", grab_names(&r));
        assert_eq!(r.pages_visited, 1);

        // one same-site level: the sub page's file appears; the other
        // site's page is not explored at depth_other = 0
        let r = grab_site(
            &root,
            &GrabOptions { depth_same: 1, ..Default::default() },
        )
        .unwrap();
        assert_eq!(
            grab_names(&r),
            vec!["a.zip".to_string(), "deep.pdf".to_string()],
            "depth1: {:?}",
            grab_names(&r)
        );

        // one other-site level too: the external file appears
        let r = grab_site(
            &root,
            &GrabOptions { depth_same: 1, depth_other: 1, ..Default::default() },
        )
        .unwrap();
        assert_eq!(
            grab_names(&r),
            vec!["a.zip".to_string(), "deep.bin".to_string(), "deep.pdf".to_string()],
            "other1: {:?}",
            grab_names(&r)
        );

        // same-site-files-only keeps the external PAGE explored out of
        // the file list even when it is explored
        let r = grab_site(
            &root,
            &GrabOptions {
                depth_same: 1,
                depth_other: 1,
                same_site_files_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            grab_names(&r),
            vec!["a.zip".to_string(), "deep.pdf".to_string()]
        );

        // extension include filter + size window (a.zip is 100 B)
        let r = grab_site(
            &root,
            &GrabOptions {
                depth_same: 1,
                depth_other: 1,
                include_exts: vec!["pdf".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(grab_names(&r), vec!["deep.pdf".to_string()]);
        let r = grab_site(
            &root,
            &GrabOptions {
                depth_same: 1,
                min_size: Some(150),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(grab_names(&r), vec!["deep.pdf".to_string()]);
    }

    #[test]
    fn sanitize_suggested_name_preserves_legal_special_chars() {
        assert_eq!(
            sanitize_suggested_name("Document & Report (100% complete).pdf"),
            "Document & Report (100% complete).pdf"
        );
        assert_eq!(
            sanitize_suggested_name("../../bad:file*name?.txt"),
            "bad_file_name_.txt"
        );
    }
}

