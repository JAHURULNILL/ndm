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

static MAX_CONN: AtomicU64 = AtomicU64::new(16);

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


/// Attach the configured proxy (if any and parseable) to an agent builder.
fn apply_proxy(builder: ureq::AgentBuilder) -> ureq::AgentBuilder {
    let url = global_proxy();
    if url.is_empty() {
        return builder;
    }
    match ureq::Proxy::new(&url) {
        Ok(p) => builder.proxy(p),
        Err(_) => builder, // a bad proxy string must never break downloads
    }
}

/// Charge `n` downloaded bytes against the global pacing clock (GCRA-style
/// slot reservation): every charge reserves n/rate seconds on a shared
/// timeline and the caller sleeps until its slot begins. Aggregate throughput
/// converges to exactly the configured rate, with no burst-and-starve cycle.
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
}

fn save_state(shared: &Shared, force: bool) {
    let mut last = shared.last_save.lock().unwrap();
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
        if serde_json::to_writer(&f, &snapshot).is_ok() && f.sync_all().is_ok() {
            saved = std::fs::rename(&tmp, &shared.state_path).is_ok();
        }
    }
    if saved {
        // best-effort directory fsync so the rename itself survives an OS crash
        if let Some(dir) = shared.state_path.parent() {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        // only a successful save resets the throttle counters — a failed
        // one (disk full, permissions) must be retried at the next block,
        // not silently deferred by a full interval
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
}

const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
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
}

fn apply_auth(req: ureq::Request, auth: &Auth) -> ureq::Request {
    let mut req = req;
    if let Some(c) = &auth.cookie {
        req = req.set("Cookie", c);
    }
    if let Some(r) = &auth.referer {
        req = req.set("Referer", r);
    }
    req
}

fn build_agent() -> ureq::Agent {
    apply_proxy(ureq::AgentBuilder::new())
        .timeout(Duration::from_secs(60))
        .user_agent(BROWSER_UA)
        .build()
}

pub fn probe(url: &str) -> Result<Probe, String> {
    probe_with(&probe_agent(), url, &Auth::default())
}

/// Probing must not hang a download start for minutes: a dedicated agent
/// with a short timeout, separate from the transfer agent.
fn probe_agent() -> ureq::Agent {
    apply_proxy(ureq::AgentBuilder::new())
        .timeout(Duration::from_secs(15))
        .user_agent(BROWSER_UA)
        .build()
}

/// Parse a `Content-Range: bytes <start>-<end>/<total>` header value.
fn parse_content_range(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().strip_prefix("bytes ")?.trim_start();
    let (range, total) = v.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((
        start.trim().parse().ok()?,
        end.trim().parse().ok()?,
        total.trim().parse().ok()?,
    ))
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
                .map(|(s, e, t)| s == 0 && e == 0 && t == total)
                .unwrap_or(false)
        }
        // a definitive HTTP refusal settles it (the claim is false); only
        // a transport failure leaves the question open — trust the header
        Err(ureq::Error::Status(..)) => false,
        Err(_) => header_claim,
    }
}

fn probe_with(agent: &ureq::Agent, url: &str, auth: &Auth) -> Result<Probe, String> {
    if let Ok(resp) = apply_auth(agent.head(url), auth).set("Accept-Encoding", "identity").call() {
        let etag = resp.header("ETag").map(|x| x.to_string());
        let last_modified = resp.header("Last-Modified").map(|x| x.to_string());
        if let Some(total) = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
        {
            if total > 0 {
                let header_claim = resp
                .header("Accept-Ranges")
                .map(|v| v.eq_ignore_ascii_case("bytes"))
                .unwrap_or(false);
                let range_ok = verify_range(agent, url, total, header_claim, auth);
                return Ok(Probe {
                    total,
                    range_ok,
                    unknown_size: false,
                    etag,
                    last_modified,
                });
            }
            // HEAD said Content-Length: 0 — some frameworks/proxies report
            // 0 on HEAD while GET serves the real body, so a zero is not
            // trusted: fall through to the GET probe below (416 there is
            // what actually proves the file is empty)
        }
    }
    let resp = match apply_auth(agent.get(url), auth)
        .set("Range", "bytes=0-0")
        .set("Accept-Encoding", "identity")
        .call()
    {
        Ok(r) => r,
        // 416 to a 1-byte range request means the file is empty: that
        // range is satisfiable for any non-empty file
        Err(ureq::Error::Status(416, _)) => {
            return Ok(Probe {
                total: 0,
                range_ok: false,
                unknown_size: false,
                etag: None,
                last_modified: None,
            });
        }
        Err(e) => return Err(format!("probe failed: {e}")),
    };
    if resp.status() == 206 {
        let (s, _e, total) = resp
            .header("Content-Range")
            .and_then(parse_content_range)
            .ok_or("probe: no usable Content-Range")?;
        if s != 0 {
            return Err(
                "probe: server answered a range request from the wrong offset".to_string(),
            );
        }
        return Ok(Probe {
            total,
            range_ok: true,
            unknown_size: false,
            etag: resp.header("ETag").map(|s| s.to_string()),
            last_modified: resp.header("Last-Modified").map(|s| s.to_string()),
        });
    }
    let etag = resp.header("ETag").map(|s| s.to_string());
    let last_modified = resp.header("Last-Modified").map(|s| s.to_string());
    let total: u64 = match resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
    {
        Some(t) => t,
        // no length anywhere (chunked / generated responses): still
        // downloadable single-stream, just with an unknown total
        None => {
            return Ok(Probe {
                total: 0,
                range_ok: false,
                unknown_size: true,
                etag,
                last_modified,
            });
        }
    };
    Ok(Probe {
        total,
        range_ok: false,
        unknown_size: false,
        etag,
        last_modified,
    })
}

enum BlockError {
    Retry(String),
    Paused,
    Cancelled,
}

fn download_block(
    agent: &ureq::Agent,
    url: &str,
    file: &File,
    start: u64,
    end: u64,
    shared: &Shared,
    worker_id: usize,
) -> Result<(), BlockError> {
    let total = shared.state.lock().unwrap().total;
    let resp = apply_auth(agent.get(url), &shared.auth)
        .set("Range", &format!("bytes={start}-{end}"))
        .set("Accept-Encoding", "identity")
        .call()
        .map_err(|e| {
            if matches!(e, ureq::Error::Status(403, _) | ureq::Error::Status(429, _)) {
                shared.throttle_events.fetch_add(1, Ordering::Relaxed);
            }
            BlockError::Retry(format!("http: {e}"))
        })?;
    if resp.status() != 206 {
        return Err(BlockError::Retry(format!(
            "server answered {} to a range request",
            resp.status()
        )));
    }
    // the answer must be exactly the range we asked for, from a file of
    // exactly the probed size: a server that streams from a different
    // offset (some broken frameworks/CDNs answer every Range request
    // with the file's first bytes) would otherwise fill this block with
    // the wrong bytes at the right length — silent corruption
    let range_ok = resp
        .header("Content-Range")
        .and_then(parse_content_range)
        .map(|(s, e, t)| s == start && e == end && t == total)
        .unwrap_or(false);
    if !range_ok {
        return Err(BlockError::Retry(
            "server answered a range request with the wrong range".to_string(),
        ));
    }
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut pos = start;
    loop {
        if shared.control.cancelled.load(Ordering::Relaxed) {
            return Err(BlockError::Cancelled);
        }
        if shared.control.paused.load(Ordering::Relaxed) {
            return Err(BlockError::Paused);
        }
        // never read past the block's end: a server that streams more
        // than the requested range would otherwise overwrite the NEXT
        // block's region (possibly already marked done) or grow the file
        // past its true size — silent corruption
        let remaining = end + 1 - pos;
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
        write_at(file, &buf[..n], pos).map_err(|e| BlockError::Retry(format!("write: {e}")))?;
        pos += n as u64;
        shared.partial[worker_id].store(pos - start, Ordering::Relaxed);
        shared.fetched_session.fetch_add(n as u64, Ordering::Relaxed);
        shared.since_save.fetch_add(n as u64, Ordering::Relaxed);
        throttle_global(n as u64, read_at);
    }
    if pos != end + 1 {
        return Err(BlockError::Retry(format!(
            "short block: got {} of {} bytes",
            pos - start,
            end - start + 1
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
        // adaptive gate: workers above the current limit wait instead of claiming
        // work — but must still exit once every block is complete
        if id >= shared.active_limit.load(Ordering::Relaxed) {
            if shared.done_count.load(Ordering::Relaxed) >= total_blocks {
                return;
            }
            std::thread::sleep(Duration::from_millis(80));
            continue;
        }
        let idx = shared.next_block.fetch_add(1, Ordering::Relaxed);
        if idx >= total_blocks {
            return;
        }
        {
            let st = shared.state.lock().unwrap();
            if st.done[idx] == 1 {
                continue;
            }
        }
        let (start, end) = {
            let st = shared.state.lock().unwrap();
            let start = idx as u64 * st.block_size;
            let end = (start + st.block_size - 1).min(st.total - 1);
            (start, end)
        };
        shared.block_state[idx].store(2, Ordering::Relaxed);
        let mut attempt = 0;
        loop {
            match download_block(&agent, &url, &file, start, end, &shared, id) {
                Ok(()) => {
                    shared.partial[id].store(0, Ordering::Relaxed);
                    {
                        let mut st = shared.state.lock().unwrap();
                        st.done[idx] = 1;
                    }
                    shared.done_count.fetch_add(1, Ordering::Relaxed);
                    shared.block_state[idx].store(1, Ordering::Relaxed);
                    save_state(&shared, false);
                    break;
                }
                Err(BlockError::Paused) => {
                    shared.block_state[idx].store(0, Ordering::Relaxed);
                    if !wait_while_paused(&shared) {
                        return;
                    }
                    shared.block_state[idx].store(2, Ordering::Relaxed);
                    // retry the same block; a pause is not a failure attempt
                }
                Err(BlockError::Cancelled) => {
                    shared.block_state[idx].store(0, Ordering::Relaxed);
                    shared.partial[id].store(0, Ordering::Relaxed);
                    return;
                }
                Err(BlockError::Retry(e)) => {
                    attempt += 1;
                    if attempt >= 5 {
                        let mut err = shared.error.lock().unwrap();
                        *err = Some(format!("block {idx}: {e}"));
                        shared.failed.store(true, Ordering::Relaxed);
                        shared.block_state[idx].store(0, Ordering::Relaxed);
                        return;
                    }
                    // exponential back-off with jitter: 300ms, 600ms,
                    // 1.2s, 2.4s — a fixed rhythm lets every worker hit a
                    // struggling server in lockstep
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
        let blocks: Vec<u8> = if self.resumable {
            self.shared
                .block_state
                .iter()
                .map(|b| b.load(Ordering::Relaxed))
                .collect()
        } else {
            Vec::new()
        };
        Progress {
            total: self.total,
            done_bytes: if self.resumable {
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
            speed_bps: if elapsed > 0.0 { fetched as f64 / elapsed } else { 0.0 },
            conns: self.shared.active_limit.load(Ordering::Relaxed),
            status,
            blocks,
            resumable: self.resumable,
            error: self.shared.error.lock().unwrap().clone(),
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
    let agent = build_agent();
    let probed = probe_with(&probe_agent(), url, auth)?;
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
    let state = match std::fs::read(&state_path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<State>(&raw).ok())
        .filter(|s| {
            s.url == url
                && s.total == probed.total
                && s.block_size == block_size
                && s.done.len() == total_blocks
                && (s.etag.is_none() || probed.etag.is_none() || s.etag == probed.etag)
                && (s.last_modified.is_none()
                    || probed.last_modified.is_none()
                    || s.last_modified == probed.last_modified)
                && out.exists()
        }) {
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
        active_limit: AtomicUsize::new(1),
        throttle_events: AtomicU64::new(0),
        failed: AtomicBool::new(false),
        error: Mutex::new(None),
        control: control.clone(),
        block_state,
        status: Mutex::new(Status::Downloading),
        partial: (0..max_conn).map(|_| AtomicU64::new(0)).collect(),
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
        let u = url.to_string();
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
                set_status(&s, Status::Finished);
                let _ = std::fs::remove_file(&s.state_path);
            } else if s.control.cancelled.load(Ordering::Relaxed) {
                set_status(&s, Status::Cancelled);
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
        throttle_events: AtomicU64::new(0),
        failed: AtomicBool::new(false),
        error: Mutex::new(None),
        control: control.clone(),
        block_state: vec![AtomicU8::new(1)],
        status: Mutex::new(Status::Finished),
        partial: vec![AtomicU64::new(0)],
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
        throttle_events: AtomicU64::new(0),
        failed: AtomicBool::new(false),
        error: Mutex::new(None),
        control: control.clone(),
        block_state: vec![AtomicU8::new(0)],
        status: Mutex::new(Status::Downloading),
        partial: vec![AtomicU64::new(0)],
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
        let agent = build_agent();
        let result: Result<(), String> = (|| {
            let resp = apply_auth(agent.get(&url), &auth)
                .set("Accept-Encoding", "identity")
                .call()
                .map_err(|e| format!("http: {e}"))?;
            let mut reader = resp.into_reader();
            let f = File::create(&out).map_err(|e| format!("create: {e}"))?;
            let mut buf = vec![0u8; 64 * 1024];
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
                throttle_global(n as u64, read_at);
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
                if !shared.control.cancelled.load(Ordering::Relaxed) {
                    shared.done_count.store(1, Ordering::Relaxed);
                    set_status(&shared, Status::Finished);
                }
            }
            Err(e) => {
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

fn grab_agent(timeout_secs: u64) -> ureq::Agent {
    apply_proxy(ureq::AgentBuilder::new())
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
    let agent = grab_agent(25);
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
    let probe_agent = grab_agent(8);
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
