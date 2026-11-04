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

/// F8: the plain sentence for a disk-full-class write failure, or
/// None for any other error. A full disk must SAY it is full — the
/// raw OS wording ("os error 28") names no action the user can
/// take. Callers append the original error text after the
/// sentence, so every substring classification that keys off the
/// raw wording keeps working exactly as before.
fn disk_full_sentence(e: &io::Error) -> Option<&'static str> {
    let raw = e.raw_os_error();
    #[cfg(unix)]
    let full = matches!(raw, Some(28) | Some(122) | Some(30)); // ENOSPC, EDQUOT, EROFS
    #[cfg(windows)]
    let full = matches!(raw, Some(112) | Some(39)); // ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL
    #[cfg(not(any(unix, windows)))]
    let full = false;
    if full {
        Some("The disk is full — free some space, then resume the download")
    } else {
        None
    }
}

/// A write failure in the form the terminal error surfaces:
/// disk-full errors lead with the plain sentence; every other
/// error keeps its exact historical `write: {e}` shape.
fn plain_write_error(e: &io::Error) -> String {
    match disk_full_sentence(e) {
        Some(s) => format!("{s} (write: {e})"),
        None => format!("write: {e}"),
    }
}

/// How a parallel worker's write failure is classified
/// (QA-220 m2, 2.3.1): disk-full is Permanent — the download
/// fails at once with the plain sentence — because as a
/// retryable block error it looped silently for minutes (the
/// pool re-fetched the same bytes into the same full disk).
/// Every other write error keeps the historical Retry shape.
fn write_block_error(e: &io::Error) -> BlockError {
    if disk_full_sentence(e).is_some() {
        BlockError::Permanent(plain_write_error(e))
    } else {
        BlockError::Retry(plain_write_error(e))
    }
}

pub mod components;
pub mod ftp;
pub mod updater;
pub mod video_probe;

pub const DEFAULT_BLOCK: u64 = 1 << 20; // 1 MiB block map, per engine spec A
const SAVE_EVERY_BYTES: u64 = 4 << 20;
const SAVE_EVERY: Duration = Duration::from_secs(1);
/// F7: minimum spacing between PERIODIC journal saves, no matter
/// how many bytes flowed — the 4 MiB byte trigger alone fires
/// ~25 saves a second at LAN speed, each cloning and serialising
/// the whole block map against the same mutex the workers mark
/// blocks through. Durable/terminal saves are not periodic and
/// are never delayed by this.
const SAVE_MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Whether a periodic (non-forced) journal save is due, as a pure
/// decision: never more often than SAVE_MIN_INTERVAL, and beyond
/// that on the long-standing triggers (SAVE_EVERY elapsed, or
/// SAVE_EVERY_BYTES of new data).
fn periodic_save_due(elapsed: Duration, since_save_bytes: u64) -> bool {
    elapsed >= SAVE_MIN_INTERVAL
        && (elapsed >= SAVE_EVERY || since_save_bytes >= SAVE_EVERY_BYTES)
}

// ---------- global speed limit (GCRA pacing clock, shared by every download) ----------

static LIMIT_RATE: AtomicU64 = AtomicU64::new(0); // bytes/sec, 0 = unlimited
static LIMIT_STATE: Mutex<Option<Instant>> = Mutex::new(None); // next free pacing slot
static LIMIT_GEN: AtomicU64 = AtomicU64::new(0); // bumped on every limit change
static LIMIT_OLDEST_LIVE: AtomicU64 = AtomicU64::new(0); // D7-v2 stale gate watermark (see pace_reserve)

/// Set the global download speed limit in bytes/second (0 = unlimited).
/// Applies to all running and future HTTP-engine downloads in this process.
pub fn set_global_limit(bytes_per_sec: u64) {
    LIMIT_RATE.store(bytes_per_sec, Ordering::Relaxed);
    // reset the pacing clock so a new rate starts clean, and bump
    // the generation so waits parked under the old rate re-reserve
    // under the new one instead of sleeping the old slot out (M-14).
    // The bump first records its PRE-BUMP generation as the
    // timeline's oldest-live watermark (D7-v2): only a charge that
    // predates that watermark is stale enough to earn the re-base
    // clamp in pace_reserve; a charge made under the watermark
    // generation itself is live and re-reserves unclamped.
    *plock(&LIMIT_STATE) = None;
    LIMIT_OLDEST_LIVE.store(LIMIT_GEN.load(Ordering::Relaxed), Ordering::Relaxed);
    LIMIT_GEN.fetch_add(1, Ordering::Relaxed);
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

/// The connection split for a video+audio stream pair, returned as
/// (video, audio). Hosts like googlevideo cap throughput PER
/// CONNECTION, and in a pair the video stream carries ~90% of the
/// bytes — the old even split (4 + 4 at the default 8) measured the
/// video stream 3.1× slower than giving it the connections
/// (Finding 2 of the speed investigation). The video stream now
/// gets `max_conn - 2` and the audio stream 2: the audio is small
/// and finishes early either way. Small counts stay near-even —
/// neither stream may drop below 1, and at max_conn ≤ 4 halving was
/// never the bottleneck: 1 → (1, 1), 2 → (1, 1), 3 → (2, 1),
/// 4 → (2, 2), then 5 → (3, 2), 8 → (6, 2), 32 → (30, 2).
pub fn video_pair_connections(max_conn: usize) -> (usize, usize) {
    let max_conn = max_conn.max(1);
    if max_conn >= 5 {
        (max_conn - 2, 2)
    } else {
        (max_conn.div_ceil(2), (max_conn / 2).max(1))
    }
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

/// Remove `/* */` and `//` comments from a script's text. The PAC
/// token scan below is deliberately naive, so commented-out example
/// directives must be gone before it runs — a `// PROXY old:1` line
/// (or a block comment with a sample config) otherwise wins over the
/// script's real return value.
fn strip_js_comments(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push(b' ');
        } else if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Resolve a PAC (automatic configuration) address to a concrete
/// proxy: fetch the script and take its first PROXY/SOCKS directive.
/// Deliberately simple — it covers the overwhelmingly common PAC shape
/// (a fixed proxy for everything) and fails to "direct", never to a
/// guessed address.
pub fn resolve_pac_proxy(pac_url: &str) -> String {
    let agent = ureq::AgentBuilder::new()
        .redirect_auth_headers(ureq::RedirectAuthHeaders::Never)
        .timeout(Duration::from_secs(4))
        .build();
    let Ok(resp) = agent.get(pac_url).call() else {
        return String::new();
    };
    if response_encoding(&resp).is_some() {
        return String::new();
    }
    let Ok(body) = resp.into_string() else {
        return String::new();
    };
    let body = strip_js_comments(&body);
    // find "PROXY host:port" / "HTTPS host:port" / "SOCKS host:port"
    // pairs in order (HTTPS is the secure-web-proxy directive)
    let words: Vec<&str> = body
        .split(|c: char| c == ';' || c == '"' || c == '\'' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .collect();
    for w in words.windows(2) {
        let kind = w[0].to_ascii_uppercase();
        if (kind == "PROXY" || kind == "HTTPS" || kind == "SOCKS" || kind == "SOCKS5")
            && w[1].contains(':')
        {
            let scheme = match kind.as_str() {
                "PROXY" => "http",
                "HTTPS" => "https",
                _ => "socks5",
            };
            return format!("{scheme}://{}", w[1]);
        }
    }
    String::new()
}

/// Pacing (GCRA-style slot reservation, M-13/M-14 rework).
///
/// The old pacer charged bytes AFTER they were read: the slot could
/// start at the read's own start time, so on an idle timeline every
/// connection's first 512 KiB read escaped at wire speed (~18.5 MiB
/// across a pool before the first wait ever bit), and a mid-flight
/// limit change only took effect once the far-future slots reserved
/// at the old rate had drained (≥15 s, then a catch-up burst).
///
/// The reworked pacer reserves BEFORE the read: reserving `n` bytes
/// pushes the shared virtual finish line out by n/rate and the
/// caller waits until its slot BEGINS before reading a single byte,
/// so the limit holds from byte 0 — the only uncharged bytes in
/// flight are the one bounded slice each reservation grants.
/// Reservations are capped at PACE_SLICE so no single charge can
/// push the finish line (or a peer's wait) far out. A short read
/// refunds the un-read remainder. Limit changes bump a generation
/// counter and reset the timeline (the setters already did the
/// reset): a parked wait notices within one 50 ms slice, abandons
/// its stale reservation and re-reserves under the new rate — the
/// change is effective within ~2 s and there is no old-debt
/// catch-up burst, because the debt was discarded with the reset.
/// Both throttles apply — a download obeys its own limit AND the
/// global one (composition = min, the cleared F2 behaviour).
const PACE_SLICE: u64 = 32 * 1024;

enum PaceWait {
    Reached,
    Halted,
    Changed,
    Cleared,
}

/// Wait until `slot` in slices of at most 50 ms, re-checking the
/// halt flags and the CURRENT limit on every slice (finding #9:
/// one bare sleep could park a worker for a whole slot — at
/// 1 KB/s a 512 KiB charge is ~8 minutes). A generation bump
/// (limit changed) or a cleared limit ends the wait at once.
fn pace_wait(
    shared: &Shared,
    slot: Instant,
    charged_rate: u64,
    rate_now: impl Fn() -> u64,
    gen_now: impl Fn() -> u64,
    charged_gen: u64,
) -> PaceWait {
    let mut wait_left = slot.saturating_duration_since(Instant::now());
    let mut last_rate = charged_rate;
    while !wait_left.is_zero() {
        if shared.failed.load(Ordering::Relaxed)
            || shared.control.cancelled.load(Ordering::Relaxed)
            || shared.control.paused.load(Ordering::Relaxed)
            || shared.superseded.load(Ordering::Relaxed)
        {
            return PaceWait::Halted;
        }
        if gen_now() != charged_gen {
            return PaceWait::Changed;
        }
        let rate = rate_now();
        if rate == 0 {
            return PaceWait::Cleared;
        }
        if rate != last_rate && last_rate > 0 {
            wait_left = Duration::from_secs_f64(
                wait_left.as_secs_f64() * last_rate as f64 / rate as f64,
            );
            last_rate = rate;
        }
        let step = wait_left.min(Duration::from_millis(50));
        std::thread::sleep(step);
        wait_left = wait_left.saturating_sub(step);
        // a wait the rate change left still far out must not be
        // slept through nominally: recompute from the real clock
        // every slice so timer overshoot never accumulates (#9)
        let real_left = slot.saturating_duration_since(Instant::now());
        if real_left < wait_left {
            wait_left = real_left;
        }
    }
    PaceWait::Reached
}

/// Reserve `amount` bytes on one pacing timeline and wait until
/// the reserved slot begins. Returns false only when a halt flag
/// landed during the wait (the caller settles pause/cancel through
/// its normal paths). A limit change mid-wait loops and
/// re-reserves under the new rate — the stale charge vanished with
/// the setter's timeline reset, so nothing is double-charged.
/// The re-reserve is a RE-BASE, not a fresh queue join (D7), but
/// ONLY through the stale-generation gate (D7-v2): the clamp keys
/// on the generation this call's ABANDONED charge was reserved
/// under, compared against the timeline's oldest-live watermark
/// (recorded by the setters at every generation bump as the
/// pre-bump generation). The abandoned charge earns the clamp
/// only when it PREDATES the watermark — it is stale. A charge
/// made under the watermark generation itself is live: its
/// re-reserve takes the normal unclamped path and earns no
/// credit. Keying on "this call saw a change" instead (v1) let a
/// waiter clamp again on EVERY bump — under repeated bumps (a
/// user dragging a limit slider, each set resetting the timeline)
/// each clamp granted up to one slice-cost of credit, so the
/// aggregate limit was no longer a bound; the gate grants at
/// most one clamp per stale epoch. When the clamp does fire the
/// slot is at most one slice-cost ahead of NOW, so the waiter is
/// never thrown back behind finish-line debt stacked by peers
/// re-reserving in the same instant (at low rates that stack is
/// seconds–minutes of parked wait on an already-open
/// connection). The stored finish line itself never moves
/// backward — the clamped slice rides within debt already
/// reserved, a bounded one-slice credit per stale waiter.
/// Steady-state reservations (no change seen by this call) are
/// unclamped, so aggregate pacing is exactly as before.
fn pace_reserve(
    shared: &Shared,
    state: &Mutex<Option<Instant>>,
    amount: u64,
    rate_now: impl Fn() -> u64,
    gen_now: impl Fn() -> u64,
    oldest_live_now: impl Fn() -> u64,
) -> bool {
    if amount == 0 {
        return true;
    }
    // the generation the charge this call most recently abandoned
    // to a limit change is reserved under; None until the first
    // PaceWait::Changed — a fresh reservation can never clamp
    let mut abandoned_gen: Option<u64> = None;
    loop {
        let rate = rate_now();
        if rate == 0 {
            return true;
        }
        let gen = gen_now();
        // D7-v2 gate, read at re-reserve time: clamp only when the
        // abandoned charge predates the timeline's CURRENT
        // oldest-live watermark; a charge under a live generation
        // (abandoned_gen >= watermark) re-reserves unclamped
        let rebased = matches!(abandoned_gen, Some(g) if g < oldest_live_now());
        let slot = {
            let mut st = plock(state);
            let now = Instant::now();
            // no in-flight credit: an idle timeline starts at NOW,
            // never in the past, so a drained queue grants exactly
            // one slice immediately and paces everything after it
            let base = (*st).unwrap_or(now).max(now);
            let cost = Duration::from_secs_f64(amount as f64 / rate as f64);
            let slot = if rebased {
                base.min(now + cost)
            } else {
                base
            };
            let finish = slot + cost;
            *st = Some(match *st {
                Some(f) if f > finish => f,
                _ => finish,
            });
            slot
        };
        match pace_wait(shared, slot, rate, &rate_now, &gen_now, gen) {
            PaceWait::Reached | PaceWait::Cleared => return true,
            PaceWait::Halted => return false,
            PaceWait::Changed => {
                abandoned_gen = Some(gen);
                continue;
            }
        }
    }
}

/// Give back the un-read part of a reservation (a short read, or a
/// halt that landed between two limiters' reservations): the
/// virtual finish line moves back by the refunded cost, so the
/// aggregate charge always equals the bytes actually read.
fn pace_refund(state: &Mutex<Option<Instant>>, amount: u64, rate: u64) {
    if amount == 0 || rate == 0 {
        return;
    }
    let mut st = plock(state);
    if let Some(t) = *st {
        let cost = Duration::from_secs_f64(amount as f64 / rate as f64);
        *st = t.checked_sub(cost);
    }
}

/// The read size a worker may take right now: unbounded (the
/// caller's normal buffer) when no limit is set, one PACE_SLICE
/// when any limit is active.
fn pace_grant_size(shared: &Shared, want: usize) -> usize {
    if LIMIT_RATE.load(Ordering::Relaxed) == 0
        && shared.own_limit.load(Ordering::Relaxed) == 0
    {
        want
    } else {
        want.min(PACE_SLICE as usize)
    }
}

/// Reserve `amount` bytes on BOTH pacing timelines (global, then
/// this download's own) and wait until the later slot begins.
/// Returns false when a halt flag landed during the wait.
fn pace_before_read(shared: &Shared, amount: u64) -> bool {
    let global_rate = LIMIT_RATE.load(Ordering::Relaxed);
    if global_rate > 0
        && !pace_reserve(
            shared,
            &LIMIT_STATE,
            amount,
            || LIMIT_RATE.load(Ordering::Relaxed),
            || LIMIT_GEN.load(Ordering::Relaxed),
            || LIMIT_OLDEST_LIVE.load(Ordering::Relaxed),
        )
    {
        return false;
    }
    if shared.own_limit.load(Ordering::Relaxed) > 0
        && !pace_reserve(
            shared,
            &shared.own_limit_state,
            amount,
            || shared.own_limit.load(Ordering::Relaxed),
            || shared.own_limit_gen.load(Ordering::Relaxed),
            || shared.own_limit_oldest_live.load(Ordering::Relaxed),
        )
    {
        // the global timeline keeps a charge for bytes that were
        // never read — hand it back so the aggregate stays honest
        pace_refund(&LIMIT_STATE, amount, global_rate);
        return false;
    }
    true
}

/// Refund the un-read remainder of a `pace_before_read` grant on
/// both timelines (short read / EOF).
fn pace_refund_both(shared: &Shared, amount: u64) {
    if amount == 0 {
        return;
    }
    pace_refund(
        &LIMIT_STATE,
        amount,
        LIMIT_RATE.load(Ordering::Relaxed),
    );
    pace_refund(
        &shared.own_limit_state,
        amount,
        shared.own_limit.load(Ordering::Relaxed),
    );
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
    /// bytes in fully completed ("sealed") blocks only — unlike
    /// `done_bytes`, which also counts what workers have written
    /// toward their current unfinished blocks, this never claims a
    /// byte a restart would have to re-fetch (finding #7: persisted
    /// progress must be the sealed figure, or a restart visibly
    /// regresses the bar to the last sealed block boundary)
    pub sealed_done_bytes: u64,
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
    /// per-connection phase for the progress dialog's Info column
    /// (G1): 0 = waiting for a claim, 1 = request sent, awaiting
    /// response headers ("Send GET…"), 2 = reading the body
    /// ("Receiving data…"). Session-only; never journalled.
    pub conn_states: Vec<u8>,
    /// bytes each connection has written toward its current,
    /// unfinished block (the fractional fill of the segment strip)
    pub conn_partial: Vec<u64>,
    /// the block index each connection is currently working on,
    /// -1 when it holds no block (strip start markers, G1)
    pub conn_block: Vec<i64>,
    /// WS-A (audit-232 S5): at least one connection was answered
    /// 403/410 — the signed ADDRESS died mid-download, as opposed
    /// to the file being gone (404 stays Permanent). Typed signal
    /// for the job layer (core), which owns the heal: re-probe the
    /// original URL and resume on the fresh effective address.
    pub auth_dead: bool,
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
    /// B-4 (audit-233): per-block PARTIAL progress — for each
    /// unsealed block, how many of its leading bytes are already
    /// written in the output file at the block's offset. Partial
    /// bytes are written at offset as they arrive (see
    /// download_block_run's write_at), so the journal only needs
    /// the counts to resume MID-BLOCK instead of re-fetching the
    /// whole in-flight block (up to a full block per connection
    /// per restart). Serde-default empty = every pre-B-4 journal:
    /// old files load exactly as before, and old binaries ignore
    /// the field. Values are never trusted raw: start_impl
    /// normalizes (length, per-block cap, sealed ⇒ 0) and, when
    /// no validator pair is usable, sample-verifies the prefixes
    /// like sealed blocks.
    #[serde(default)]
    partial: Vec<u64>,
}

impl State {
    fn done_bytes(&self) -> u64 {
        let full = self.done.iter().filter(|&&b| b == 1).count() as u64;
        // saturating: a crafted state file (huge block_size) must never
        // overflow this multiply — the guard below rejects those files,
        // but this function must stay safe for any State value
        full.saturating_mul(self.block_size).min(self.total)
    }

    /// bytes held by unsealed blocks' written prefixes, each
    /// clamped to its block's real length — safe for any State
    /// value, normalized or not
    fn partial_bytes(&self) -> u64 {
        let mut sum: u64 = 0;
        for (i, &p) in self.partial.iter().enumerate() {
            if self.done.get(i).copied().unwrap_or(0) != 0 || p == 0 {
                continue;
            }
            let start = (i as u64).saturating_mul(self.block_size);
            let len = ((i as u64 + 1)
                .saturating_mul(self.block_size)
                .min(self.total))
            .saturating_sub(start);
            sum = sum.saturating_add(p.min(len));
        }
        sum.min(self.total)
    }

    /// B-4: make `partial` safe to index and trust: exactly one
    /// entry per block, no more than that block's length, and 0
    /// for sealed blocks. Anything else in a journal is dropped
    /// to 0 for the affected entries — the sealed map is never
    /// discarded over partial bookkeeping.
    fn normalize_partial(&mut self) {
        let n = self.done.len();
        if self.partial.len() != n {
            self.partial.resize(n, 0);
        }
        for i in 0..n {
            if self.done[i] != 0 {
                self.partial[i] = 0;
                continue;
            }
            let start = (i as u64).saturating_mul(self.block_size);
            let len = ((i as u64 + 1)
                .saturating_mul(self.block_size)
                .min(self.total))
            .saturating_sub(start);
            if self.partial[i] > len {
                self.partial[i] = 0;
            }
        }
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

/// WS-D D1: one in-flight block split at a byte offset — the
/// owner keeps [block_start, split), a helper takes
/// [split, block_end). See `byte_requeue` / `tail_splits` on Shared.
#[derive(Clone, Copy)]
struct TailSplit {
    split: u64,
    owner_done: bool,
    helper_done: bool,
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
    /// WS-D D2: the governor's live ceiling for active_limit, and
    /// how many workers were spawned. Normally both equal the
    /// start's connection count; a video+audio pair member starts
    /// with the ceiling at its pair share while its sibling lives
    /// (all of the pair's workers exist, the gate holds the excess
    /// parked), and set_connection_limit raises both when the
    /// sibling finishes — the survivor inherits the whole pool
    /// without a restart.
    conn_cap: AtomicUsize,
    worker_count: usize,
    /// WS-D D2: set on a video+audio pair's surviving stream when
    /// its sibling finishes (with the connection-limit raise).
    /// The survivor's tail byte-split gate relaxes for it (see
    /// tail_split_due_survivor): its helpers are its own parked
    /// workers, dedicated and already waiting in its claim wait —
    /// not the churn-busy pool that made uniform-speed splitting
    /// a loss for ordinary downloads (see tail_split_due).
    pair_survivor: AtomicBool,
    active_workers: AtomicUsize,
    /// when each 403/429 landed (F3): the governor shrinks the
    /// pool only when throttling is BROAD — at least half the
    /// pool throttled inside the window — while a lone event just
    /// cools down its own connection in the worker's retry path
    throttle_times: Mutex<Vec<Instant>>,
    /// range requests answered 200 since the last validated 206
    /// (F6): a server that stopped honouring Range mid-download
    /// surfaces here; at RANGE_LOSS_LIMIT the job restarts its
    /// transfer in single-stream mode (once, see below)
    range_200_count: AtomicUsize,
    /// F6 mode switch: set once range loss is declared; worker 0
    /// re-fetches the file in one plain stream while the rest of
    /// the pool stands down
    single_fallback: AtomicBool,
    /// F6 once-guard: the single-stream restart may happen at
    /// most once per job
    fallback_used: AtomicBool,
    failed: AtomicBool,
    /// WS-A: a 403/410 was seen on some connection of this job —
    /// the address died, not necessarily the download (see
    /// Progress::auth_dead). Set by the block-run and
    /// single-stream error paths, never cleared within a run: a
    /// later success on other ranges does not un-die the address
    /// for the ranges that were refused.
    auth_dead: AtomicBool,
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
    /// WS-D D1: per-connection delivery rate (B/s) as sampled by
    /// the governor's 1.5 s tick, and the highest such rate any
    /// connection has shown this download. The tail byte-split's
    /// straggler gate reads them: a split only pays when the
    /// block's owner is demonstrably slower than what this pool
    /// can do — at uniform per-connection speeds splitting is a
    /// measured LOSS (see tail_split_due).
    conn_rate_bps: Vec<AtomicU64>,
    peak_conn_rate_bps: AtomicU64,
    /// (instant, fetched_session) samples for the sliding-window
    /// transfer rate shown to the user — IDM displays the CURRENT
    /// rate, not a session average diluted by pauses and slow starts
    rate_hist: Mutex<std::collections::VecDeque<(Instant, u64)>>,
    /// this download's own speed limit in bytes/sec (0 = none) —
    /// IDM's per-download Speed Limiter; composes with the global one
    own_limit: AtomicU64,
    own_limit_state: Mutex<Option<Instant>>,
    /// when any worker last wrote a byte — the pool-wide progress
    /// clock behind the failure policy: a download fails only when
    /// NO worker has moved a byte for STALL_LIMIT, so one sick
    /// connection can never kill the healthy ones
    last_progress: Mutex<Instant>,
    /// block ranges [first, last) a worker handed back after
    /// repeated failures; other workers (or the same one, later)
    /// pick them up again — a range leaves the pool only by being
    /// downloaded or by the whole download settling
    requeue: Mutex<Vec<(usize, usize)>>,
    /// WS-D D1: byte ranges carved out of an in-flight block by a
    /// tail split — (start, end_exclusive) pairs, each wholly
    /// inside one block, published by that block's owner at the
    /// moment it caps its own run at the split offset (see
    /// `tail_split_point`). A range is published ONLY by its owner
    /// and ONLY before the owner reads another byte, so a helper's
    /// range and the owner's remaining range are disjoint by
    /// construction: no byte range is ever owned by two workers.
    byte_requeue: Mutex<Vec<(u64, u64)>>,
    /// WS-D D1: per-block tail-split bookkeeping (indexed by block,
    /// None = never split). A split block is sealed only when BOTH
    /// halves have landed (note_tail_owner_done /
    /// note_tail_helper_done) — neither half alone may mark the
    /// block, so the journal can never credit a block whose tail
    /// bytes have not landed.
    tail_splits: Mutex<Vec<Option<TailSplit>>>,
    /// workers parked in the claim wait right now because nothing
    /// was claimable — the signal that lets a busy worker split the
    /// unstarted tail of its claim instead of leaving peers idle
    /// while its own connection crawls (Finding 1: with no
    /// work-stealing, the finish time equalled the slowest claimed
    /// run — 3.3× worse than the aggregate bound on uneven lanes)
    waiting_workers: AtomicUsize,
    /// when the last tail split was executed. Splits are
    /// rate-limited pool-wide: each one abandons an in-flight
    /// response, so a momentary idle blip must not be able to shred
    /// every run at once
    last_split: Mutex<Option<Instant>>,
    /// the most recent journal (resume-state) save failure, if any
    /// (finding #8a: save_state used to swallow every error, so a
    /// download whose journal could never be written — an unwritable
    /// folder, an over-long journal name — ran on with no resume
    /// protection and no signal anywhere). Cleared by the next
    /// successful save.
    last_journal_error: Mutex<Option<String>>,
    /// set by the engine's last writer once the terminal status is
    /// settled AND the final resume-state save has landed on disk.
    /// The supervisor settles the status FIRST and only then syncs
    /// + saves (F-15), so `is_terminal` alone races that save —
    /// removal paths wait for `Download::is_settled` before
    /// sweeping the journal (R4 item 2)
    settled: AtomicBool,
    /// set by the supervisor when it settles WITHOUT a worker — the
    /// worker was parked on a server-held connection past the
    /// settle grace (W1 F13 / C-2). A detached worker that wakes
    /// later checks this before every write and block mark, so it
    /// can never write into a file a settled (or superseding)
    /// download now owns.
    superseded: AtomicBool,
    /// per-worker progress-dialog surface (G1): phase 0 waiting /
    /// 1 request sent / 2 receiving, and the current block index
    /// (usize::MAX = none). Session-only, never journalled.
    conn_phase: Vec<AtomicU8>,
    conn_block: Vec<AtomicUsize>,
    /// bumped whenever this download's own limit changes, so a
    /// pacing wait parked under the old rate re-reserves under the
    /// new one instead of sleeping out the old slot (M-14)
    own_limit_gen: AtomicU64,
    /// the own timeline's oldest-live watermark for the D7-v2
    /// stale-generation gate: set_limit records the PRE-BUMP value
    /// of own_limit_gen here at every bump (the global timeline's
    /// equivalent is LIMIT_OLDEST_LIVE). A pace_reserve charge
    /// abandoned from a generation OLDER than this is stale and
    /// may re-base with the clamp; a charge from this generation
    /// itself is live and re-reserves unclamped.
    own_limit_oldest_live: AtomicU64,
    /// the lying-Content-Encoding-header proof passed for this job
    /// (DIAG fix): block runs skip the header veto because the
    /// bodies were proven identity. True encoders never set this —
    /// their proof fails on the encoded length alone. Atomic
    /// because a tentative (F5) start runs the same proof in the
    /// first worker that meets an encoded answer, mid-download.
    ignore_encoding: AtomicBool,
    /// the job's session cookie jar (F1): every request of this
    /// job — probe, verify, workers, single-stream — captures
    /// Set-Cookie into it and replays it per host (see CookieJar)
    jar: Arc<CookieJar>,
    /// journal sync policy state (W1 F15): when the data file was
    /// last synced to stable storage, and how many bytes workers
    /// have written since. A periodic journal save may only sync
    /// the journal itself once it has FIRST synced the data file
    /// (see save_state), so the journal never durably claims bytes
    /// the data file does not hold.
    last_data_sync: Mutex<Instant>,
    bytes_since_sync: AtomicU64,
}

/// How long the whole worker pool may move zero bytes before the
/// download is declared failed (permanent errors fail immediately,
/// without waiting for this window).
const STALL_LIMIT: Duration = Duration::from_secs(30);

/// How long the supervisor waits for workers to wind down once a
/// terminal intent exists before it settles without them (C-2):
/// settlement is bounded by STALL_LIMIT + SETTLE_GRACE (+ one poll
/// tick), never by a parked worker's connection.
const SETTLE_GRACE: Duration = Duration::from_secs(5);

/// Lock a Shared mutex, recovering the guard if a previous holder
/// panicked while holding it. A poisoned mutex must degrade to
/// "keep going with the state as it stands", never cascade: before
/// this, one panicking thread poisoned `state`/`status`, every
/// worker and the supervisor then died on their next `.unwrap()`,
/// and the download sat non-terminal forever — no connections, no
/// error, a supervisor that never settled (the F-15 shape). The
/// data behind these mutexes is plain download state; a torn read
/// is recoverable, a dead pool is not.
fn plock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Evaluate the pool-wide failure policy from a waiting path: if
/// no worker has moved a byte for STALL_LIMIT while blocks remain,
/// declare the download failed and return true. The policy used to
/// live only inside the transfer error handler — a pool parked
/// anywhere else (waiting for a claim, gated by the connection
/// limit) never evaluated it, so a genuine stall could sit
/// non-terminal forever instead of ending in an error (F-15).
fn fail_if_stalled(shared: &Shared, total_blocks: usize) -> bool {
    if shared.control.paused.load(Ordering::Relaxed) {
        // a paused download waits on the user, not the network —
        // the stall clock only runs against an active pool
        return false;
    }
    if shared.done_count.load(Ordering::Relaxed) >= total_blocks {
        return false;
    }
    if plock(&shared.last_progress).elapsed() <= STALL_LIMIT {
        return false;
    }
    let mut err = plock(&shared.error);
    if err.is_none() {
        *err = Some(format!(
            "no progress for {}s — the server stopped sending data",
            STALL_LIMIT.as_secs()
        ));
    }
    shared.failed.store(true, Ordering::Relaxed);
    true
}

/// Work-stealing split shape (Finding 1). When a peer is idle, a
/// busy owner whose run still has at least SPLIT_KEEP_BLOCKS +
/// SPLIT_MIN_STEAL unstarted blocks releases everything past the
/// first SPLIT_KEEP_BLOCKS of them back to the pool: the owner
/// keeps enough to stay busy until the released tail is picked up,
/// the released part is at least SPLIT_MIN_STEAL blocks, and a run
/// whose tail is smaller than the sum — a run already nearly done —
/// is never split.
const SPLIT_KEEP_BLOCKS: usize = 2;
const SPLIT_MIN_STEAL: usize = 2;

/// Pool-wide minimum spacing between executed splits.
const SPLIT_COOLDOWN: Duration = Duration::from_secs(1);

/// The split point for a run whose first unwritten block is
/// `first_unwritten` and whose end (exclusive) is `end`: the owner
/// keeps [first_unwritten, split) and releases [split, end).
/// `None` when the unstarted tail is too small to split. Pure
/// arithmetic — the gating (an idle peer, the cooldown) lives in
/// `split_allowed`; both must pass before a split is armed.
fn split_point(first_unwritten: usize, end: usize) -> Option<usize> {
    let tail = end.checked_sub(first_unwritten)?;
    if tail < SPLIT_KEEP_BLOCKS + SPLIT_MIN_STEAL {
        return None;
    }
    Some(first_unwritten + SPLIT_KEEP_BLOCKS)
}

/// The split point a pair survivor uses (WS-D D2): when a
/// video+audio pair's other side finished, this download
/// inherited the whole pool — but the inherited workers can
/// only take work the current claims RELEASE, and the normal
/// split shape (keep 2, steal at least 2) releases nothing from
/// the short claims a pair member holds. Measured on the audit
/// fixture, donation without release gained ~0.4 s of 2 s
/// available: the blocks stayed locked in the original claims
/// until they drained naturally. A survivor's owner therefore
/// keeps only its first unwritten block and releases everything
/// past it, so the inherited pool redistributes the remaining
/// blocks at once. Only a flagged survivor takes this branch —
/// ordinary downloads keep the Finding-1 shape exactly.
fn shed_point(shared: &Shared, first_unwritten: usize, end: usize) -> Option<usize> {
    if shared.pair_survivor.load(Ordering::Relaxed) && end > first_unwritten + 1 {
        return Some(first_unwritten + 1);
    }
    split_point(first_unwritten, end)
}

/// Whether a tail split may be armed right now: a peer must
/// actually be waiting for work (splitting into an empty room only
/// fragments the claim and abandons a healthy response), and the
/// pool-wide cooldown must have elapsed.
fn split_allowed(shared: &Shared) -> bool {
    if shared.waiting_workers.load(Ordering::Relaxed) == 0 {
        return false;
    }
    match *plock(&shared.last_split) {
        Some(t) => t.elapsed() >= SPLIT_COOLDOWN,
        None => true,
    }
}

/// WS-D D1 tail byte-splitting (audit-232 S6). Block-granular
/// stealing cannot help the end-game: the final blocks are each
/// IN FLIGHT on one slow connection (a run's unstarted tail is
/// below SPLIT_MIN_STEAL, so `split_point` rightly refuses), while
/// the rest of the pool idles. The fix splits the in-flight block
/// itself at a byte offset. Safety is by construction, mirroring
/// the block split's ownership rule — the OWNER executes the
/// split: at a chunk boundary (never mid-write) it picks the
/// split from its own true write cursor, publishes the helper
/// range [split, block_end) to `byte_requeue`, and caps its own
/// remaining reads at split. The helper range therefore starts
/// exactly where the owner's writes provably stop, and the two
/// ranges can never overlap. The block seals only when both
/// halves report done (`note_tail_half_done`).

/// A tail split must leave both sides worth a connection: the
/// block remainder at the split decision, and each side of it.
const TAIL_SPLIT_MIN_REMAINDER: u64 = 256 * 1024;
const TAIL_SPLIT_MIN_SIDE: u64 = 64 * 1024;

/// The byte offset splitting the current block's remainder
/// [pos, block_end) in two, or None when the remainder is too
/// small for a second connection to pay off. Pure arithmetic.
fn tail_split_point(pos: u64, block_end: u64) -> Option<u64> {
    let rem = block_end.checked_sub(pos)?;
    if rem < TAIL_SPLIT_MIN_REMAINDER {
        return None;
    }
    let split = pos + rem / 2;
    if split - pos < TAIL_SPLIT_MIN_SIDE || block_end - split < TAIL_SPLIT_MIN_SIDE {
        return None;
    }
    Some(split)
}

/// Whether a tail byte-split may be armed right now. This is
/// deliberately the CONSERVATIVE shape (WS-D D1): the audit's
/// uniform-cap rig showed that arming whenever a waiter merely
/// exists can lose time — in that end-game the "waiting" workers
/// are mostly churning through recycled hand-backs, helper ranges
/// get claimed late and serially, and the split download finished
/// ~10% SLOWER (124 s vs 111 s at 100 MiB / 16 conns / 64 KiB/s
/// per conn). The win shape is narrower and provable: the split
/// block is among the very last unfinished blocks (at most
/// TAIL_SPLIT_MAX_REMAINING), the idle pool is at least as large
/// as ALL the work that remains (so a helper is a genuinely
/// spare connection, not a transient one), no earlier helper
/// range is still unclaimed, and the pool-wide split cooldown
/// (shared with block splits) has elapsed.
const TAIL_SPLIT_MAX_REMAINING: usize = 4;

fn tail_split_due(shared: &Shared) -> bool {
    let waiting = shared.waiting_workers.load(Ordering::Relaxed);
    if waiting == 0 {
        return false;
    }
    if !plock(&shared.byte_requeue).is_empty() {
        return false;
    }
    let total_blocks = shared.block_state.len();
    let remaining = total_blocks
        .saturating_sub(shared.done_count.load(Ordering::Relaxed));
    if remaining == 0 || remaining > TAIL_SPLIT_MAX_REMAINING || waiting < remaining {
        return false;
    }
    match *plock(&shared.last_split) {
        Some(t) => t.elapsed() >= SPLIT_COOLDOWN,
        None => true,
    }
}

/// The pair-survivor variant of the tail-split gate (WS-D D2):
/// when a video+audio pair's other side has finished, this
/// download's remaining blocks are the whole job's finish line,
/// and the workers that would help are this download's OWN —
/// parked at its gate until the donation, then dedicated in its
/// claim wait. The uniform-speed objection to splitting (owner
/// and helper equally fast, split adds only coordination) does
/// not hold here: the helpers were contributing NOTHING while
/// parked, so a split block finishing on two connections is a
/// strict gain as long as enough blocks remain to matter and at
/// least one helper is actually waiting. Bounds: at most 8
/// unfinished blocks, a waiter present, no helper range
/// outstanding, shared split cooldown respected.
fn tail_split_due_survivor(shared: &Shared) -> bool {
    if !shared.pair_survivor.load(Ordering::Relaxed) {
        return false;
    }
    if shared.waiting_workers.load(Ordering::Relaxed) == 0 {
        return false;
    }
    if !plock(&shared.byte_requeue).is_empty() {
        return false;
    }
    let total_blocks = shared.block_state.len();
    let remaining = total_blocks
        .saturating_sub(shared.done_count.load(Ordering::Relaxed));
    if remaining == 0 || remaining > 8 {
        return false;
    }
    match *plock(&shared.last_split) {
        Some(t) => t.elapsed() >= SPLIT_COOLDOWN,
        None => true,
    }
}

/// The straggler gate (WS-D D1): the block's owner is currently
/// delivering at under half the best rate any connection in this
/// pool has shown. Without it, splitting fires at uniform speeds
/// too — where the audit rig measured it a net LOSS (owner and
/// helper are equally fast, so the split only adds coordination:
/// the abandoned response, the helper-claim latency, the
/// both-halves seal). With it, splitting is reserved for the
/// shape it provably wins: one decayed/throttled connection
/// holding the finish while the pool demonstrably can do better.
/// No data yet (fresh download, owner not yet sampled) reads as
/// NOT a straggler — the conservative direction.
fn tail_owner_is_straggler(shared: &Shared, worker_id: usize) -> bool {
    let peak = shared.peak_conn_rate_bps.load(Ordering::Relaxed);
    if peak == 0 {
        return false;
    }
    let owner = shared
        .conn_rate_bps
        .get(worker_id)
        .map(|r| r.load(Ordering::Relaxed))
        .unwrap_or(0);
    owner > 0 && owner * 2 < peak
}

/// Seal a split block whose two halves have both landed.
fn seal_tail_block(shared: &Shared, block: usize) {
    {
        let mut st = plock(&shared.state);
        if block < st.done.len() && st.done[block] == 0 {
            st.done[block] = 1;
            // B-4: a sealed block carries no partial claim
            if let Some(p) = st.partial.get_mut(block) {
                *p = 0;
            }
            shared.done_count.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(b) = shared.block_state.get(block) {
        b.store(1, Ordering::Relaxed);
    }
}

/// The owner half of a split block has landed (the owner stopped
/// exactly at the split offset): flag it, and seal the block if
/// the helper half beat the owner to the finish.
fn note_tail_owner_done(shared: &Shared, block: usize) {
    let seal = {
        let mut splits = plock(&shared.tail_splits);
        match splits.get_mut(block) {
            Some(Some(ts)) => {
                ts.owner_done = true;
                ts.helper_done
            }
            _ => false,
        }
    };
    if seal {
        seal_tail_block(shared, block);
    }
}

/// The helper half of a split block has landed: flag it, and seal
/// the block if the owner half is already in.
fn note_tail_helper_done(shared: &Shared, block: usize) {
    let seal = {
        let mut splits = plock(&shared.tail_splits);
        match splits.get_mut(block) {
            Some(Some(ts)) => {
                ts.helper_done = true;
                ts.owner_done
            }
            _ => false,
        }
    };
    if seal {
        seal_tail_block(shared, block);
    }
}

/// Counts a worker as waiting-for-work for exactly the scope that
/// holds it (same pattern as ActiveWorkerGuard).
struct WaiterGuard<'a>(&'a AtomicUsize);
impl<'a> WaiterGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}
impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One attempt to claim work: a fresh contiguous range so each
/// worker keeps its TCP window saturated, or — once the fresh
/// range is exhausted — a range another worker handed back after
/// repeated failures. `None` means nothing is claimable RIGHT NOW,
/// not that nothing is left: a range can re-enter the pool at any
/// moment, so callers wait and retry instead of treating this as
/// the end of the download.
fn try_claim(shared: &Shared, total_blocks: usize) -> Option<(usize, usize)> {
    let active = shared.active_limit.load(Ordering::Relaxed).max(1);
    let cur_next = shared.next_block.load(Ordering::Relaxed);
    if cur_next < total_blocks {
        let rem_blocks = total_blocks.saturating_sub(cur_next);
        let run_len = (rem_blocks / active).clamp(1, 256);
        let cs = shared.next_block.fetch_add(run_len, Ordering::Relaxed);
        if cs < total_blocks {
            return Some((cs, (cs + run_len).min(total_blocks)));
        }
        // raced past the end — fall through to the pool
    }
    let mut pool = plock(&shared.requeue);
    let (s, e) = pool.pop()?;
    // A handed-back range is shared with the workers currently
    // waiting: the first claimant takes a fair slice and leaves the
    // rest in the pool, instead of one worker swallowing a range
    // the whole pool could be chewing. The released tail of a slow
    // run is exactly such a range — Finding 1's redistribution
    // depends on it spreading across the idle workers at once, not
    // cascading claim by claim.
    let waiting = shared.waiting_workers.load(Ordering::Relaxed);
    let len = e - s;
    let take = if waiting > 0 {
        (len / (waiting + 1)).max(1)
    } else {
        len
    };
    if take < len {
        pool.push((s + take, e));
        return Some((s, s + take));
    }
    Some((s, e))
}

/// Durable-point cadence for periodic journal saves (W1 F15 +
/// DIAG speed report): a periodic save syncs at most once per
/// 16 MiB of new data or 5 s, whichever comes first — every-save
/// fsync was a measurable speed tax on consumer disks, and syncing
/// the JOURNAL without the DATA was a durability hole (on power
/// loss the journal could durably claim bytes the data file never
/// held). Forced/terminal saves are always durable.
const SYNC_EVERY_BYTES: u64 = 16 << 20;
const SYNC_EVERY: Duration = Duration::from_secs(5);

/// The sync policy for one journal save, as a pure decision so it
/// can be unit-tested without a disk: `durable` saves sync data
/// file → journal file → directory, in that order, so a durable
/// journal never claims bytes the data file does not hold;
/// non-durable saves write the journal with no sync at all.
fn sync_is_durable(force: bool, bytes_since_sync: u64, since_sync: Duration) -> bool {
    force || bytes_since_sync >= SYNC_EVERY_BYTES || since_sync >= SYNC_EVERY
}

fn save_state(shared: &Shared, force: bool, data_file: Option<&File>) {
    let mut last = if force {
        plock(&shared.last_save)
    } else {
        match shared.last_save.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        }
    };
    if !force
        && !periodic_save_due(
            last.elapsed(),
            shared.since_save.load(Ordering::Relaxed),
        )
    {
        return;
    }
    let durable = sync_is_durable(
        force,
        shared.bytes_since_sync.load(Ordering::Relaxed),
        plock(&shared.last_data_sync).elapsed(),
    );
    let snapshot = plock(&shared.state).clone();
    // write-then-rename keeps the on-disk state valid even if killed mid-save
    let tmp = shared.state_path.with_extension("json.tmp");
    // The tmp handle is CLOSED before the rename below: on Windows,
    // renaming a file whose write handle is still open can fail with
    // a sharing violation, and the save that matters most — the
    // supervisor's final save, immediately followed by deleting the
    // state file — is exactly the one that must not silently fail.
    //
    // Sync policy (W1 F15): a DURABLE save syncs the DATA file
    // first, then the journal, then its directory — the journal
    // must never be the most durable copy of a byte. A non-durable
    // periodic save syncs nothing: it is a crash hint for the page
    // cache era, and the next durable point (≤16 MiB / ≤5 s away,
    // or the forced terminal save) restores the invariant. A
    // failure anywhere is RECORDED on the job (finding #8a) instead
    // of swallowed — a journal that can never be written means this
    // download has no resume protection, and that must be visible.
    let save_result: io::Result<()> = (|| {
        if durable {
            if let Some(df) = data_file {
                df.sync_all()?;
            }
        }
        {
            let f = File::create(&tmp)?;
            serde_json::to_writer(&f, &snapshot).map_err(io::Error::from)?;
            if durable {
                f.sync_all()?;
            }
        } // f drops here — the handle is closed before the rename
        std::fs::rename(&tmp, &shared.state_path)?;
        if durable {
            if let Some(dir) = shared.state_path.parent() {
                if let Ok(d) = File::open(dir) {
                    let _ = d.sync_all();
                }
            }
        }
        Ok(())
    })();
    match save_result {
        Ok(()) => {
            *plock(&shared.last_journal_error) = None;
            shared.since_save.store(0, Ordering::Relaxed);
            if durable {
                shared.bytes_since_sync.store(0, Ordering::Relaxed);
                *plock(&shared.last_data_sync) = Instant::now();
            }
            *last = Instant::now();
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            *plock(&shared.last_journal_error) =
                Some(format!("could not save resume state: {e}"));
        }
    }
}

/// Delete a file, tolerating the brief window on Windows where a
/// just-written, just-closed file is still locked (the OS or a virus
/// scanner holding it open for a scan): a single fire-and-forget
/// remove loses that race and leaves a finished download's state
/// file behind, its error discarded. Bounded (~0.5 s worst case)
/// and only a backstop — callers close their handles first and
/// order their writes so the first attempt normally succeeds.
fn remove_file_bounded(path: &Path) {
    for attempt in 0..20 {
        match std::fs::remove_file(path) {
            Ok(()) => return,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(_) if attempt < 19 => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return,
        }
    }
}

#[derive(Clone, Debug)]
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
    /// the server labels responses with a Content-Encoding header
    /// but the bodies were PROVEN identity (the DIAG lying-header
    /// proof in verify_range / probe_once): transfer paths for this
    /// job skip the header veto. Never set for a true encoder.
    pub ignore_encoding_header: bool,
    /// the session cookies the probe collected on the way (F1):
    /// a start handed this probe continues the SAME jar, so a
    /// cookie the probe's redirect chain set is still there when
    /// the workers ask for the file
    pub cookies: Arc<CookieJar>,
    /// the range verdict is TENTATIVE (F5 download probe): the
    /// server's HEAD claimed range support and no verify GET was
    /// spent; the first worker responses are the verification,
    /// with the encoding proof and the single-stream restart as
    /// the fallbacks on contradiction. Never set by the full
    /// probe (probe / probe_with_auth).
    pub range_unverified: bool,
}

/// The filename inside one Content-Disposition value
/// (`filename*=UTF-8''...` preferred, else `filename="..."`),
/// sanitized; None when the value carries no usable name.
fn name_from_disposition(cd: &str) -> Option<String> {
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
    None
}

/// A URL-derived name is plausible as a file name when it is not
/// the bare-handler fallback and carries an extension. A signed
/// CDN URL's path tail — a bare asset UUID with no extension —
/// is NOT plausible: the name the user was offered lives on the
/// original (feed) URL, never on the redirect target (2.3.1:
/// update assets added through the browser arrived named
/// "4c62eef7-…" — the release-asset UUID — instead of the
/// .deb/.zip names from the URLs that were clicked).
fn plausible_file_name(name: &str) -> bool {
    name != "index" && name.contains('.')
}

/// Extract a clean filename from a `Content-Disposition` header value
/// (`filename*=UTF-8''...` or `filename="..."`) or, failing that, from
/// the final post-redirect URL when it differs from `original_url`.
fn extract_suggested_name(resp: &ureq::Response, original_url: &str) -> Option<String> {
    if let Some(cd) = resp.header("Content-Disposition") {
        if let Some(name) = name_from_disposition(cd) {
            return Some(name);
        }
    }
    let final_url = resp.get_url();
    if !final_url.is_empty() && final_url != original_url {
        // Signed CDN URLs (GitHub release assets, S3) carry the
        // real name in the response-content-disposition QUERY
        // parameter — the name the origin promised the browser —
        // while their path tail is an opaque asset id.
        if let Some(query) = final_url.split('?').nth(1) {
            for pair in query.split('&') {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                if k == "response-content-disposition" {
                    let cd = percent_decode(v);
                    if let Some(name) = name_from_disposition(&cd) {
                        return Some(name);
                    }
                }
            }
        }
        // The original URL's own plausible name beats an
        // implausible redirect tail (the asset-UUID case above);
        // a plausible redirect tail still wins for short-link
        // flows whose original URL names nothing.
        let from_original = file_name_of(original_url);
        let from_final = file_name_of(final_url);
        if plausible_file_name(&from_final) {
            return Some(from_final);
        }
        if plausible_file_name(&from_original) {
            return Some(from_original);
        }
        if from_final != "index" {
            return Some(from_final);
        }
    }
    // No Content-Disposition and no usable redirect target: the
    // original URL's query string may still carry the real name
    // (/dl?file=report.pdf) — without it the file dialog probes
    // this endpoint as a nameless "dl" and saves it extensionless
    // in the wrong category (F-EXT-11). Content-Disposition above
    // still wins whenever present.
    query_file_name(original_url)
}

pub fn sanitize_suggested_name(raw: &str) -> String {
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
        return String::new();
    }
    // F-6: a server-suggested name can be arbitrarily long; cap it
    // like every other name source before it reaches the filesystem
    let name = cap_file_name(trimmed);
    if cfg!(target_os = "windows") {
        // Windows reserves the DOS device names with ANY extension:
        // a download literally named "nul.mp3" or "CON" cannot be
        // created there at all. Defuse it with a leading underscore.
        // (cfg! keeps every other platform's names byte-identical.)
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", //
            "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", //
            "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        if RESERVED.contains(&stem.as_str()) {
            return cap_file_name(&format!("_{name}"));
        }
    }
    name
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
    /// the User-Agent of the browser that captured this download
    /// (browser-capture door only): a session CDN that bound the
    /// session to the browser refuses the matching cookies under
    /// a foreign UA, so capture-sourced requests repeat the
    /// browser's own UA instead of the BROWSER_UA constant
    /// (audit-232 S3). None → the constant, as before.
    pub user_agent: Option<String>,
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
    if let Some(ua) = &auth.user_agent {
        // a per-request User-Agent wins over the agent's
        // BROWSER_UA default (ureq only writes its own when the
        // request carries none)
        req = req.set("User-Agent", ua);
    }
    req
}

/// The host part of a URL, lower-cased, without any port — the key
/// credential scoping compares (a CDN mirror on another host is a
/// different origin even when it serves the same file).
fn host_key(url: &str) -> String {
    url_host(url)
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

/// `apply_auth` for a request whose target may differ from the URL
/// the credentials were captured for (finding #13): Cookie and
/// Authorization are credentials for ONE origin, so when the target
/// host is not the origin host they are dropped, never replayed to
/// the other host. The Referer is not a credential and still
/// applies. (Redirects inside ureq are covered separately: every
/// agent builder in this crate sets
/// `redirect_auth_headers(RedirectAuthHeaders::Never)`, and ureq
/// additionally strips Cookie on any redirect hop.)
fn apply_auth_scoped(
    req: ureq::Request,
    auth: &Auth,
    target_url: &str,
    origin_url: &str,
) -> ureq::Request {
    if host_key(target_url) == host_key(origin_url) {
        return apply_auth(req, auth);
    }
    let mut req = req;
    if let Some(r) = &auth.referer {
        req = req.set("Referer", r);
    }
    if let Some(ua) = &auth.user_agent {
        // not a credential either: the browser sends its UA to
        // every host, and a session CDN on a mirror host is
        // exactly where the UA binding bites
        req = req.set("User-Agent", ua);
    }
    req
}

/// One cookie held by the jar (F1).
#[derive(Debug, Clone)]
struct JarCookie {
    /// host that set the cookie (lower-case, no port)
    host: String,
    /// the Domain attribute, when one was sent AND the setter
    /// belongs to it: delivery widens to that domain and its
    /// subdomains. Without one the cookie is host-only — it goes
    /// to the exact host that set it and nowhere else, mirroring
    /// the cross-host credential scoping of finding #13.
    domain: Option<String>,
    path: String,
    name: String,
    value: String,
}

/// Parse one Set-Cookie header value into (name, value, domain,
/// path, expired). Pure, so the parsing rules are unit-testable
/// without a fabricated ureq response. `Max-Age=0` (or a negative
/// age) is the server's delete instruction and reports expired.
fn parse_set_cookie(raw: &str) -> Option<(String, String, Option<String>, String, bool)> {
    let mut parts = raw.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() {
        return None;
    }
    let mut domain = None;
    let mut path = "/".to_string();
    let mut expired = false;
    for attr in parts {
        let attr = attr.trim();
        if let Some((k, v)) = attr.split_once('=') {
            let v = v.trim();
            if k.trim().eq_ignore_ascii_case("domain") {
                let d = v.trim_start_matches('.').to_ascii_lowercase();
                if !d.is_empty() {
                    domain = Some(d);
                }
            } else if k.trim().eq_ignore_ascii_case("path") {
                if v.starts_with('/') {
                    path = v.to_string();
                }
            } else if k.trim().eq_ignore_ascii_case("max-age")
                && v.parse::<i64>().map(|n| n <= 0).unwrap_or(false) {
                    expired = true;
                }
        }
    }
    Some((name.to_string(), value.to_string(), domain, path, expired))
}

/// The per-job session cookie jar (F1). ureq is built without its
/// cookies feature, so a cookie a host sets — classically on the
/// redirect that leads to the file — used to be dropped on the
/// floor, and cookie-gated hosts (302 sets the session cookie,
/// the target 403s without it) could not even be probed. Every
/// request of a job runs through `jar_call`, which captures each
/// response's Set-Cookie headers here (redirect hops included)
/// and replays the jar per host on the next request.
#[derive(Debug, Default)]
pub struct CookieJar {
    entries: Mutex<Vec<JarCookie>>,
}

impl CookieJar {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Store every Set-Cookie a response carried, keyed by the
    /// host of the URL that was actually requested (a redirect
    /// hop's cookies belong to the hop's host, not the origin's).
    fn capture(&self, resp: &ureq::Response, url: &str) {
        let host = host_key(url);
        if host.is_empty() {
            return;
        }
        for raw in resp.all("Set-Cookie") {
            let Some((name, value, domain, path, expired)) = parse_set_cookie(raw) else {
                continue;
            };
            // a Domain attribute counts only when the setter
            // actually belongs to that domain — a host may not
            // plant cookies for a stranger's domain
            let domain = domain.filter(|d| host == *d || host.ends_with(&format!(".{d}")));
            let mut entries = plock(&self.entries);
            entries.retain(|c| !(c.host == host && c.name == name && c.domain == domain));
            if !expired {
                entries.push(JarCookie {
                    host: host.clone(),
                    domain,
                    path,
                    name,
                    value,
                });
            }
        }
    }

    /// The Cookie header value for a request to `url`: cookies
    /// the URL's own host set (host-only), plus cookies whose
    /// Domain attribute names the host's parent domain. Nothing
    /// else — a cookie never crosses to a host that did not set
    /// it or name it via Domain.
    fn header_for(&self, url: &str) -> Option<String> {
        let host = host_key(url);
        let path = url_path(url);
        let path = if path.is_empty() { "/".to_string() } else { path };
        let entries = plock(&self.entries);
        let pairs: Vec<String> = entries
            .iter()
            .filter(|c| {
                let host_ok = match &c.domain {
                    Some(d) => host == *d || host.ends_with(&format!(".{d}")),
                    None => host == c.host,
                };
                host_ok && path.starts_with(&c.path)
            })
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }
}

/// The Cookie header for one request: the job jar's cookies for
/// the target host merged with the static (extension-captured)
/// Auth cookie. On a name clash the static value wins — it is
/// the fresher, user-session truth — and the jar adds the rest.
fn merged_cookie_header(
    jar: &CookieJar,
    url: &str,
    static_cookie: Option<&str>,
) -> Option<String> {
    match (static_cookie, jar.header_for(url)) {
        (None, j) => j,
        (Some(s), None) => Some(s.to_string()),
        (Some(s), Some(j)) => {
            let static_names: Vec<String> = s
                .split(';')
                .filter_map(|p| {
                    p.split_once('=')
                        .map(|(n, _)| n.trim().to_ascii_lowercase())
                })
                .collect();
            let extra: Vec<&str> = j
                .split("; ")
                .filter(|p| {
                    let n = p.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
                    !static_names.contains(&n)
                })
                .collect();
            if extra.is_empty() {
                Some(s.to_string())
            } else {
                Some(format!("{s}; {}", extra.join("; ")))
            }
        }
    }
}

/// One request through the job's cookie jar, following redirects
/// MANUALLY — the probe and transfer agents are built with ureq's
/// own following disabled, because that follower strips Cookie on
/// every hop and never exposes a hop's Set-Cookie; the gate
/// cookie set by a 302 therefore never reached the file it
/// unlocked (F1). Here each hop captures its response's cookies
/// into the jar and sends the jar's cookies for ITS OWN host, so
/// a cross-host hop receives only what that host set itself.
/// Auth is re-scoped per hop against the job's origin, exactly
/// as one-shot requests scope it (finding #13), and `headers`
/// (Range, Accept-Encoding, …) are re-sent verbatim on every
/// hop, as ureq's follower re-sent them. Only GET and HEAD are
/// routed here — every engine request is one of the two.
#[allow(clippy::too_many_arguments)]
fn jar_call(
    agent: &ureq::Agent,
    jar: &CookieJar,
    auth: &Auth,
    origin_url: &str,
    method: &str,
    start_url: &str,
    headers: &[(&str, &str)],
) -> Result<ureq::Response, ureq::Error> {
    let mut url = start_url.to_string();
    for hop in 0..=5u32 {
        let mut req = if method == "HEAD" {
            agent.head(&url)
        } else {
            agent.get(&url)
        };
        req = apply_auth_scoped(req, auth, &url, origin_url);
        // apply_auth_scoped may already have set the static
        // Cookie (same-host hops); the merged header is the final
        // word for THIS hop's host
        let static_cookie = if host_key(&url) == host_key(origin_url) {
            auth.cookie.as_deref()
        } else {
            None
        };
        if let Some(c) = merged_cookie_header(jar, &url, static_cookie) {
            req = req.set("Cookie", &c);
        }
        for (k, v) in headers {
            req = req.set(k, v);
        }
        match req.call() {
            Ok(resp) => {
                jar.capture(&resp, &url);
                if !(300..400).contains(&resp.status()) || hop == 5 {
                    return Ok(resp);
                }
                let Some(loc) = resp.header("Location").map(|s| s.to_string()) else {
                    return Ok(resp);
                };
                let Some(next) = resolve_url(&url, &loc) else {
                    return Ok(resp);
                };
                url = next;
            }
            Err(e) => {
                // an HTTP error response can still set cookies the
                // job needs (a login gate answering 403 with a
                // fresh session cookie): capture before returning
                if let ureq::Error::Status(_, resp) = &e {
                    jar.capture(resp, &url);
                }
                return Err(e);
            }
        }
    }
    unreachable!("the hop loop always returns by hop 5")
}

/// The response's Content-Encoding when it is a real encoding —
/// every transfer path in this crate requests `identity`, and ureq
/// is built without an auto-decode feature, so a server that answers
/// with gzip/deflate anyway would have its compressed bytes saved
/// as the finished file (finding #3). Callers must reject a response
/// this flags: decode-or-fail is not an option without a decoder,
/// so failing cleanly is the only non-corrupting outcome.
pub(crate) fn response_encoding(resp: &ureq::Response) -> Option<String> {
    let enc = resp.header("Content-Encoding")?.trim().to_ascii_lowercase();
    if enc.is_empty() || enc == "identity" {
        None
    } else {
        Some(enc)
    }
}

fn build_agent_for(url: &str) -> ureq::Agent {
    // redirects(0): transfer requests follow redirects manually
    // through jar_call so every hop's cookies are captured and
    // replayed per host (F1) — ureq's follower strips Cookie on
    // hops and hides hop Set-Cookie headers entirely
    apply_proxy_for(ureq::AgentBuilder::new()
        .redirect_auth_headers(ureq::RedirectAuthHeaders::Never)
        .redirects(0), url)
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .max_idle_connections(256)
        .max_idle_connections_per_host(64)
        .user_agent(BROWSER_UA)
        .build()
}

fn build_agent() -> ureq::Agent {
    apply_proxy(ureq::AgentBuilder::new()
        .redirect_auth_headers(ureq::RedirectAuthHeaders::Never))
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .max_idle_connections(256)
        .max_idle_connections_per_host(64)
        .user_agent(BROWSER_UA)
        .build()
}

pub fn probe(url: &str) -> Result<Probe, String> {
    if ftp::is_ftp_url(url) {
        return ftp::probe_ftp(url, &Auth::default());
    }
    probe_with(&probe_agent_for(url), url, &Auth::default(), &CookieJar::new())
}

pub fn probe_with_auth(url: &str, auth: &Auth) -> Result<Probe, String> {
    if ftp::is_ftp_url(url) {
        return ftp::probe_ftp(url, auth);
    }
    probe_with(&probe_agent_for(url), url, auth, &CookieJar::new())
}

/// The probe a DOWNLOAD START runs when the caller has not
/// already probed (F5): one HEAD, and when that HEAD is a clean
/// answer — a usable total, no Content-Encoding, an explicit
/// `Accept-Ranges: bytes` claim — its verdict is taken
/// tentatively (range_unverified) and the separate verify GET
/// round trip is skipped: the first worker range responses are
/// the verification, and a contradiction (200s, an encoding lie)
/// falls back to the worker-side proof / the single-stream
/// restart, exactly as a false probe verdict would be handled.
/// Any HEAD that is missing, unusable or ambiguous defers to the
/// full probe, retries and encoding proof included — the File
/// Info dialog's probe (probe / probe_with_auth) is unchanged
/// and always runs the full path.
pub fn probe_for_download(url: &str) -> Result<Probe, String> {
    if ftp::is_ftp_url(url) {
        // the FTP probe is already the full probe (SIZE + a real
        // REST check on a disposable connection) — there is no
        // cheaper tentative variant to prefer here
        return ftp::probe_ftp(url, &Auth::default());
    }
    probe_for_download_with(&probe_agent_for(url), url, &Auth::default(), &CookieJar::new())
}

fn probe_for_download_with(
    agent: &ureq::Agent,
    url: &str,
    auth: &Auth,
    jar: &Arc<CookieJar>,
) -> Result<Probe, String> {
    if let Ok(resp) = jar_call(
        agent,
        jar,
        auth,
        url,
        "HEAD",
        url,
        &[("Accept-Encoding", "identity")],
    ) {
        let total = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok());
        let header_claim = resp
            .header("Accept-Ranges")
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);
        if let Some(total) = total {
            if total > 0 && resp.status() == 200 && header_claim && response_encoding(&resp).is_none() {
                return Ok(Probe {
                    total,
                    range_ok: true,
                    unknown_size: false,
                    etag: resp.header("ETag").map(|x| x.to_string()),
                    last_modified: resp.header("Last-Modified").map(|x| x.to_string()),
                    suggested_name: extract_suggested_name(&resp, url),
                    final_url: resp.get_url().to_string(),
                    ignore_encoding_header: false,
                    cookies: jar.clone(),
                    range_unverified: true,
                });
            }
        }
    }
    probe_with(agent, url, auth, jar)
}

/// Probing must not hang a download start for minutes: a dedicated agent
/// with a short timeout, separate from the transfer agent. The
/// connect phase gets its own cap — ureq otherwise lets an
/// unroutable host sit in the connect phase until the overall
/// deadline, and DNS stalls are not covered by the read/write
/// timeouts at all (M5).
fn probe_agent_for(url: &str) -> ureq::Agent {
    // redirects(0) for the same reason as build_agent_for (F1):
    // probe requests follow redirects manually through jar_call
    apply_proxy_for(ureq::AgentBuilder::new()
        .redirect_auth_headers(ureq::RedirectAuthHeaders::Never)
        .redirects(0), url)
        .timeout(Duration::from_secs(15))
        .timeout_connect(Duration::from_secs(10))
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
    // an inverted range (end before start) is not a range at all:
    // trusting it underflows every length computed from it downstream
    // (finding #14)
    if end_num < start_num {
        return None;
    }
    let total_num: Option<u64> = total.trim().parse().ok();
    Some((start_num, end_num, total_num))
}

/// The lying-header proof (DIAG Report 1 fix): a response claims a
/// Content-Encoding the request refused, yet its body may still be
/// identity — some servers stamp the header on every answer while
/// sending the file untouched, and vetoing the header alone demoted
/// those files from 8 connections to 1 (or failed the probe
/// outright). The proof for a 1-byte range answer already in hand:
/// its declared length must be EXACTLY 1 (a real encoding of one
/// byte is never one byte — a gzip stream alone is ~21), and its
/// byte must equal the first byte a plain identity fetch returns
/// for offset 0, whose own declared length must match the known
/// total when there is one (an encoded whole-file body reports the
/// ENCODED length). Anything short of the full proof is not a lie
/// we trust: the caller keeps the veto / demotion, so true
/// encoders still fail safe.
fn prove_encoding_lie(
    agent: &ureq::Agent,
    url: &str,
    auth: &Auth,
    origin_url: &str,
    jar: &CookieJar,
    verify_byte: u8,
    known_total: Option<u64>,
) -> bool {
    let Ok(plain) = jar_call(
        agent,
        jar,
        auth,
        origin_url,
        "GET",
        url,
        &[("Accept-Encoding", "identity")],
    ) else {
        return false;
    };
    if let Some(t) = known_total {
        if let Some(cl) = plain
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
        {
            if cl != t {
                return false;
            }
        }
    }
    let mut byte = [0u8; 1];
    plain
        .into_reader()
        .read_exact(&mut byte)
        .map(|_| byte[0] == verify_byte)
        .unwrap_or(false)
}

/// Confirm with a real 1-byte request that the server honours Range —
/// some servers advertise `Accept-Ranges: bytes` but answer 200 to actual
/// range requests; trusting the header alone bricks those downloads.
/// Returns (range_ok, ignore_encoding_header): the second is set
/// when the answer carried a Content-Encoding header but the
/// lying-header proof passed — the job keeps its connections and
/// its transfer paths skip the header veto for this job only.
fn verify_range(
    agent: &ureq::Agent,
    url: &str,
    total: u64,
    header_claim: bool,
    auth: &Auth,
    origin_url: &str,
    jar: &CookieJar,
) -> (bool, bool) {
    // `url` is the HEAD's post-redirect address: if that is another
    // host than the one the credentials belong to, they are scoped
    // out (finding #13)
    match jar_call(
        agent,
        jar,
        auth,
        origin_url,
        "GET",
        url,
        &[("Range", "bytes=0-0"), ("Accept-Encoding", "identity")],
    ) {
        Ok(resp) => {
            if resp.status() != 206 {
                return (false, false);
            }
            // the answer must be exactly the byte we asked for, from a
            // file of exactly the probed size — a server that streams
            // from a different offset is not range-capable for our use
            let cr_ok = resp
                .header("Content-Range")
                .and_then(parse_content_range)
                .map(|(s, e, t)| s == 0 && e == 0 && (t.is_none() || t == Some(total)))
                .unwrap_or(false);
            if !cr_ok {
                return (false, false);
            }
            if response_encoding(&resp).is_none() {
                return (true, false);
            }
            // a Content-Encoding header on the verify answer: demote
            // (the old behaviour) unless the header is proven a lie.
            // A real encoder's 1-byte answer is never 1 byte long.
            let cl_ok = resp
                .header("Content-Length")
                .and_then(|v| v.parse::<u64>().ok())
                .map(|cl| cl == 1)
                .unwrap_or(false);
            if !cl_ok {
                return (false, false);
            }
            let mut byte = [0u8; 1];
            let read_ok = resp
                .into_reader()
                .read_exact(&mut byte)
                .is_ok();
            if read_ok
                && prove_encoding_lie(agent, url, auth, origin_url, jar, byte[0], Some(total))
            {
                (true, true)
            } else {
                (false, false)
            }
        }
        // a definitive HTTP refusal settles it (the claim is false); only
        // a transport failure leaves the question open — trust the header
        Err(ureq::Error::Status(..)) => (false, false),
        Err(_) => (header_claim, false),
    }
}

/// Why a single probe attempt failed. The distinction drives the
/// retry policy in `probe_with`: a transport failure means the
/// server never actually answered (a hiccup — EAGAIN, a reset, a
/// TLS cut), so another attempt can succeed; a definitive answer
/// (an HTTP status like 404/403, a range reply that is malformed
/// or from the wrong offset) will not change by asking again, so
/// retrying it would only delay the error the user must see.
enum ProbeFail {
    Transient(String),
    Definitive(String),
}

/// Attempts per probe: the initial try plus two retries, with a
/// short backoff (300 ms, then 900 ms) — at most 1.2 s of added
/// delay, and only on the path where attempts are actually failing
/// transiently. Success and definitive answers return immediately,
/// exactly as the single-attempt probe did.
const PROBE_ATTEMPTS: usize = 3;

fn probe_with(
    agent: &ureq::Agent,
    url: &str,
    auth: &Auth,
    jar: &Arc<CookieJar>,
) -> Result<Probe, String> {
    let started = Instant::now();
    let mut attempt = 0usize;
    loop {
        match probe_once(agent, url, auth, jar) {
            Ok(p) => return Ok(p),
            Err(ProbeFail::Definitive(m)) => return Err(m),
            Err(ProbeFail::Transient(m)) => {
                attempt += 1;
                // a hard total budget across attempts (M5): one
                // attempt can chain several requests (HEAD, range
                // check, range GET), each individually timed out,
                // and an unroutable host used to stretch three
                // attempts past three minutes of "Probing
                // connection…". Past the budget the failure is the
                // answer — the caller lands the entry in its error
                // state instead of waiting on a dead host.
                if attempt >= PROBE_ATTEMPTS
                    || started.elapsed() > Duration::from_secs(30)
                {
                    return Err(m);
                }
                std::thread::sleep(Duration::from_millis(if attempt == 1 { 300 } else { 900 }));
            }
        }
    }
}

fn probe_once(
    agent: &ureq::Agent,
    url: &str,
    auth: &Auth,
    jar: &Arc<CookieJar>,
) -> Result<Probe, ProbeFail> {
    let mut head_suggested: Option<String> = None;
    if let Ok(resp) = jar_call(
        agent,
        jar,
        auth,
        url,
        "HEAD",
        url,
        &[("Accept-Encoding", "identity")],
    ) {
        let resolved = resp.get_url().to_string();
        let etag = resp.header("ETag").map(|x| x.to_string());
        let last_modified = resp.header("Last-Modified").map(|x| x.to_string());
        let suggested_name = extract_suggested_name(&resp, url);
        if let Some(total) = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
        {
            // a HEAD that answers with an encoding despite the
            // identity request reports the ENCODED length: never
            // trust it — fall through to the range probe, which
            // checks its own response the same way (finding #3)
            // only a 200 HEAD's length is a file size: a terminal
            // 3xx (redirect loop, hop budget spent) or an error
            // status wearing a Content-Length is not — fall through
            // to the range probe, which judges its own answer
            if total > 0 && resp.status() == 200 && response_encoding(&resp).is_none() {
                let header_claim = resp
                    .header("Accept-Ranges")
                    .map(|v| v.eq_ignore_ascii_case("bytes"))
                    .unwrap_or(false);
                let (range_ok, ignore_encoding_header) =
                    verify_range(agent, &resolved, total, header_claim, auth, url, jar);
                return Ok(Probe {
                    total,
                    range_ok,
                    unknown_size: false,
                    etag,
                    last_modified,
                    suggested_name,
                    final_url: resolved,
                    ignore_encoding_header,
                    cookies: jar.clone(),
                    range_unverified: false,
                });
            }
        }
        head_suggested = suggested_name;
    }
    let resp = match jar_call(
        agent,
        jar,
        auth,
        url,
        "GET",
        url,
        &[("Range", "bytes=0-0"), ("Accept-Encoding", "identity")],
    ) {
        Ok(r) => r,
        Err(ureq::Error::Status(416, _)) => {
            // A 416 to `bytes=0-0` proves only that THIS range was
            // refused — it was once read as "the file is empty", and
            // the caller then truncated the target and reported a
            // 0-byte Finished (finding #2). Ask once more with a
            // plain GET: a genuinely empty file answers 200 with a
            // zero length (the handling below reports it as such), a
            // real file answers with its size, and a server that
            // refuses the plain request too is a clean error — the
            // target is never touched on a guess.
            match jar_call(
                agent,
                jar,
                auth,
                url,
                "GET",
                url,
                &[("Accept-Encoding", "identity")],
            ) {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!(
                        "probe failed: server refused the range probe (416) and the plain request: {e}"
                    );
                    return Err(match e {
                        ureq::Error::Status(..) => ProbeFail::Definitive(msg),
                        ureq::Error::Transport(_) => ProbeFail::Transient(msg),
                    });
                }
            }
        }
        Err(e) => {
            // an HTTP status is the server's definitive answer;
            // anything else is transport trouble the server never
            // saw — the retry policy treats them differently
            let msg = format!("probe failed: {e}");
            return Err(match e {
                ureq::Error::Status(..) => ProbeFail::Definitive(msg),
                ureq::Error::Transport(_) => ProbeFail::Transient(msg),
            });
        }
    };
    // A Content-Encoding header on the probe answer is a
    // Definitive failure — UNLESS the lying-header proof passes for
    // it (DIAG fix): a 206 answer with an exact Content-Range, a
    // declared length of exactly 1 and a byte matching a plain
    // identity fetch is an identity body wearing a false header, so
    // the probe succeeds with range support and the job is marked
    // to skip the header veto. Any other encoded answer (including
    // a 200, whose body the proof cannot isolate to one byte) stays
    // Definitive. All header facts are captured BEFORE the proof
    // consumes the response body.
    let probe_enc = response_encoding(&resp);
    let probe_status = resp.status();
    let probe_cr = resp.header("Content-Range").and_then(parse_content_range);
    let probe_cl = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());
    let probe_etag = resp.header("ETag").map(|s| s.to_string());
    let probe_lm = resp.header("Last-Modified").map(|s| s.to_string());
    let resolved = resp.get_url().to_string();
    let suggested_name = extract_suggested_name(&resp, url).or(head_suggested);
    if let Some(enc) = &probe_enc {
        let mut proven = false;
        if probe_status == 206 {
            if let Some((s, e, t)) = probe_cr {
                if s == 0 && e == 0 && probe_cl == Some(1) {
                    // read the single byte, then run the plain-fetch
                    // comparison against the resolved address
                    let mut byte = [0u8; 1];
                    if resp.into_reader().read_exact(&mut byte).is_ok()
                        && prove_encoding_lie(agent, &resolved, auth, url, jar, byte[0], t)
                    {
                        proven = true;
                    }
                    if proven {
                        return Ok(Probe {
                            total: t.unwrap_or(0),
                            range_ok: t.is_some(),
                            unknown_size: t.is_none(),
                            etag: probe_etag,
                            last_modified: probe_lm,
                            suggested_name,
                            final_url: resolved,
                            ignore_encoding_header: true,
                            cookies: jar.clone(),
                            range_unverified: false,
                        });
                    }
                }
            }
        }
        let _ = enc;
        return Err(ProbeFail::Definitive(format!(
            "probe: server answered with Content-Encoding: {enc} despite an identity request"
        )));
    }
    let resp_status_is_206 = probe_status == 206;
    if resp_status_is_206 {
        let (s, _e, total_opt) = resp
            .header("Content-Range")
            .and_then(parse_content_range)
            .ok_or_else(|| {
                ProbeFail::Definitive("probe: no usable Content-Range".to_string())
            })?;
        if s != 0 {
            return Err(ProbeFail::Definitive(
                "probe: server answered a range request from the wrong offset".to_string(),
            ));
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
            ignore_encoding_header: false,
            cookies: jar.clone(),
            range_unverified: false,
        });
    }
    // Anything that is not a 200 here is not a file body at all:
    // a terminal 3xx (jar_call spent its hop budget on a redirect
    // loop and hands back the last redirect) used to have its
    // Content-Length read as the file size — a 302 with
    // `Content-Length: 0` probed as a legitimately empty file and
    // the start then created that empty file and reported
    // Finished (WS-BUGS B5c). HTTP error statuses never reach
    // here (ureq returns them as Err); a leftover 3xx is a
    // definitive answer, not a size.
    if probe_status != 200 {
        return Err(ProbeFail::Definitive(format!(
            "probe: server answered status {probe_status} (redirects not followed to a file)"
        )));
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
                ignore_encoding_header: false,
                cookies: jar.clone(),
                range_unverified: false,
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
        ignore_encoding_header: false,
        cookies: jar.clone(),
        range_unverified: false,
    })
}

/// The worker-side lying-header proof (F5): a tentative start
/// never ran the probe's proof, so the first worker to meet a
/// Content-Encoding header runs it here — fetch the 1-byte range
/// answer, then let `prove_encoding_lie` compare it against a
/// plain identity fetch. On success the job's veto is lifted for
/// every connection, exactly as a probe verdict would have done.
fn worker_proves_encoding_lie(
    agent: &ureq::Agent,
    shared: &Shared,
    url: &str,
    total: u64,
) -> bool {
    let origin_url = plock(&shared.state).url.clone();
    let Ok(resp) = jar_call(
        agent,
        &shared.jar,
        &shared.auth,
        &origin_url,
        "GET",
        url,
        &[("Range", "bytes=0-0"), ("Accept-Encoding", "identity")],
    ) else {
        return false;
    };
    if resp.status() != 206 {
        return false;
    }
    let cl_ok = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|cl| cl == 1)
        .unwrap_or(false);
    if !cl_ok {
        return false;
    }
    let mut byte = [0u8; 1];
    if resp.into_reader().read_exact(&mut byte).is_err() {
        return false;
    }
    let proven = prove_encoding_lie(
        agent,
        url,
        &shared.auth,
        &origin_url,
        &shared.jar,
        byte[0],
        Some(total),
    );
    if proven {
        shared.ignore_encoding.store(true, Ordering::Relaxed);
    }
    proven
}

/// F6 accounting: a range request answered 200 after the job
/// started. Three in a row (a validated 206 resets the count)
/// mean the server stopped honouring Range — signed-URL hosts do
/// exactly this when the signature rots — so the job restarts its
/// transfer in single-stream mode, once, instead of retrying
/// ranges until the stall policy fails it.
const RANGE_LOSS_LIMIT: usize = 3;

fn note_range_loss(shared: &Shared) {
    if shared.range_200_count.fetch_add(1, Ordering::Relaxed) + 1 >= RANGE_LOSS_LIMIT
        && !shared.fallback_used.swap(true, Ordering::Relaxed)
    {
        shared.single_fallback.store(true, Ordering::Relaxed);
    }
}

/// WS-A (audit-232 S5): the HTTP statuses that mean the ADDRESS
/// died — an expired signature or revoked token (403/410) — as
/// opposed to the file being gone (404). One range's address
/// death must not nuke a download whose other ranges are still
/// mid-body, so these classify as Retry at the connection level
/// and are reported to the job layer through Progress::auth_dead,
/// which owns the re-probe-and-resume heal.
pub fn status_is_auth_dead(status: u16) -> bool {
    matches!(status, 403 | 410)
}

/// WS-A: does an engine error string carry an auth-death status?
/// The engine's own error formats ("http: …: status code 410",
/// "server answered 410 …") are the only inputs; callers outside
/// the engine use this instead of matching raw text themselves.
pub fn error_is_auth_dead(e: &str) -> bool {
    e.contains("status code 403")
        || e.contains("status code 410")
        || e.contains("answered 403")
        || e.contains("answered 410")
}

fn note_auth_dead(shared: &Shared) {
    shared.auth_dead.store(true, Ordering::Relaxed);
}

enum BlockError {
    Retry(String),
    /// a definitive refusal (404): retrying cannot change the
    /// answer, so the download fails at once instead of burning the
    /// stall window. (410 left this class with WS-A: it is the
    /// signed-URL expiry shape, recoverable by re-probing the
    /// original address — see status_is_auth_dead.)
    Permanent(String),
    Paused,
    Cancelled,
    /// this connection fell below the per-connection progress
    /// floor (F2): it is recycled — the worker drops it and
    /// re-requests the sub-run from the current offset on a new
    /// connection. Not a failure.
    Recycle,
    /// the run reached a work-stealing split boundary: this worker
    /// stops here on purpose, mid-response, so its claim's
    /// unstarted tail can be handed to idle peers. Not an error —
    /// carries the block the claim now ends at
    Yielded(usize),
    /// WS-D D1: the run stopped exactly at a tail byte-split
    /// offset inside its current block: this worker's half of the
    /// block is fully written, and the block's tail range is
    /// already published for a helper. Not an error — carries the
    /// split block's index
    YieldedByte(usize),
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
    let origin_url = plock(&shared.state).url.clone();
    let range_header = format!("bytes={req_start}-{end}");
    let mut headers: Vec<(&str, &str)> = vec![("Accept-Encoding", "identity")];
    if !is_gvideo {
        headers.push(("Range", &range_header));
    }
    // G1: the request is being sent — the dialog's Info column
    // shows "Send GET…" until the headers are in and the body
    // starts arriving ("Receiving data…")
    if let Some(p) = shared.conn_phase.get(worker_id) {
        p.store(1, Ordering::Relaxed);
    }
    if let Some(b) = shared.conn_block.get(worker_id) {
        b.store(*cur_block, Ordering::Relaxed);
    }
    let resp = jar_call(
        agent,
        &shared.jar,
        &shared.auth,
        &origin_url,
        "GET",
        &req_url,
        &headers,
    )
    .map_err(|e| {
        if matches!(e, ureq::Error::Status(403, _) | ureq::Error::Status(410, _)) {
            // WS-A: this connection's address died. Recorded on
            // the job (the job layer heals by re-probing the
            // original URL); the connection itself just retries —
            // other ranges may still be mid-body at full speed.
            note_auth_dead(shared);
        }
        if matches!(e, ureq::Error::Status(403, _) | ureq::Error::Status(429, _)) {
            // F3: a throttle lands on THIS connection only — its
            // timestamp feeds the governor's broad-throttling
            // test, and the connection itself cools down here
            // before the worker's retry backoff even starts. The
            // pool as a whole no longer pays for one sick range.
            plock(&shared.throttle_times).push(Instant::now());
            std::thread::sleep(Duration::from_millis(900));
        }
        if matches!(e, ureq::Error::Status(404, _)) {
            BlockError::Permanent(format!("http: {e}"))
        } else {
            BlockError::Retry(format!("http: {e}"))
        }
    })?;
    if !shared.ignore_encoding.load(Ordering::Relaxed) {
        if let Some(enc) = response_encoding(&resp) {
            // encoded bytes written at file offsets would assemble a
            // corrupt "finished" file; the server was asked for identity
            // and this will not change between retries (finding #3).
            // Skipped only when the job's bodies were proven identity
            // despite the header (the DIAG lying-header proof) — the
            // bytes written are still exactly the bytes received.
            // A TENTATIVE start (F5) never ran that proof in the
            // probe: run it here, on the first encoded answer —
            // a proven lie lifts the veto for the whole job and
            // THIS identity body is used; anything else keeps the
            // veto, exactly as the probe verdict would have.
            if !worker_proves_encoding_lie(agent, shared, &req_url, total) {
                return Err(BlockError::Permanent(format!(
                    "server answered with Content-Encoding: {enc} despite an identity request"
                )));
            }
        }
    }
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
        // a validated range answer clears the range-loss count
        // (F6): only PERSISTENT 200s declare the loss
        shared.range_200_count.store(0, Ordering::Relaxed);
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
    } else if status == 404 {
        return Err(BlockError::Permanent(format!(
            "server answered {status} to a range request"
        )));
    } else if status_is_auth_dead(status) {
        // WS-A: same rule as the transport-error arm above — a
        // 403/410 to THIS range is that range's dead address, not
        // the download's death sentence
        note_auth_dead(shared);
        return Err(BlockError::Retry(format!(
            "server answered {status} to a range request"
        )));
    } else {
        if status == 200 {
            note_range_loss(shared);
        }
        return Err(BlockError::Retry(format!(
            "server answered {status} to a range request"
        )));
    }

    // G1: headers are in, the body is next — "Receiving data…"
    if let Some(p) = shared.conn_phase.get(worker_id) {
        p.store(2, Ordering::Relaxed);
    }
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 512 * 1024];
    // work-stealing cap: once armed (see the check in the loop),
    // this run stops at a block boundary ahead of the first
    // unwritten block and the worker hands the rest of its claim
    // to the pool. The boundary is never behind the write cursor,
    // so no byte already requested-or-written changes owner: this
    // connection simply stops reading there, and the tail blocks —
    // which this run has not written a single byte of — are
    // fetched fresh by whoever claims them
    let mut cap_block: Option<usize> = None;
    // WS-D D1: the tail byte-split cap for THIS block, when one is
    // armed — this run stops at that byte offset and yields the
    // block's tail to a helper (see the arming check in the loop).
    // A split registered by an earlier call for the same block
    // (this run was recycled / paused and re-entered) is resumed
    // from the registry, never re-decided: the helper may already
    // be fetching the published range, so the cap must hold.
    let mut tail_cap: Option<u64> = {
        let splits = plock(&shared.tail_splits);
        match splits.get(*cur_block) {
            Some(Some(ts)) if !ts.owner_done => Some(ts.split),
            _ => None,
        }
    };
    // F2 per-connection progress watermark — the single-stream
    // rule (STALL_PROGRESS_BYTES / STALL_LIMIT) applied to THIS
    // connection: the pool-wide clock is refreshed by any chunk
    // of any size, so one connection dribbling a byte every
    // couple of seconds (inside every read timeout) used to hold
    // the whole download at ~0 B/s forever. Here, a connection
    // that has not delivered STALL_PROGRESS_BYTES of new bytes
    // within STALL_LIMIT is recycled by the worker.
    let mut wm_pos = *pos;
    let mut wm_clock = Instant::now();
    // Decay-window recycling (audit232): the F2 watermark above
    // only fires when a connection delivers <256 KiB in a full
    // 30 s — a connection that decays from a fast burst to a
    // steady 32–64 KiB/s clamp (the googlevideo shape) crosses
    // 256 KiB every 4–8 s and was NEVER recycled, yet a fresh
    // request earns a fresh burst. Track this run's delivery in
    // 10 s windows: the first window is the connection's own
    // baseline; a later window under half the baseline (and
    // under 1 MiB absolute, so genuinely fast or steady
    // connections never churn) recycles the connection from the
    // current offset — written bytes are never re-fetched.
    const DECAY_WINDOW: Duration = Duration::from_secs(10);
    let mut decay_pos = *pos;
    let mut decay_clock = Instant::now();
    let mut decay_baseline: Option<u64> = None;
    loop {
        if shared.single_fallback.load(Ordering::Relaxed) {
            // F6: the job switched to single-stream mode — this
            // run stops here; worker 0 takes over the transfer,
            // every other worker stands down (Cancelled settles
            // nothing by itself: the control flags stay clear)
            return Err(if worker_id == 0 {
                BlockError::Recycle
            } else {
                BlockError::Cancelled
            });
        }
        if shared.superseded.load(Ordering::Relaxed) {
            // the supervisor settled without this worker (C-2): it
            // must never write again — the file may already belong
            // to a superseding download
            return Err(BlockError::Cancelled);
        }
        if shared.control.cancelled.load(Ordering::Relaxed) {
            return Err(BlockError::Cancelled);
        }
        if shared.control.paused.load(Ordering::Relaxed) {
            return Err(BlockError::Paused);
        }
        let mut remaining = resp_end + 1 - *pos;
        if let Some(b) = cap_block {
            let cap_byte = ((b as u64) * block_size).min(total);
            remaining = remaining.min(cap_byte.saturating_sub(*pos));
        }
        if let Some(split) = tail_cap {
            remaining = remaining.min(split.saturating_sub(*pos));
        }
        if remaining == 0 {
            break;
        }
        // Pacing reserves BEFORE the read (M-13): the grant is
        // charged against both limiters' timelines and the worker
        // waits for its slot first, so the limit holds from byte 0
        // instead of letting every connection's first read escape
        // at wire speed.
        let want = (buf.len() as u64).min(remaining) as usize;
        let grant = pace_grant_size(shared, want);
        if !pace_before_read(shared, grant as u64) {
            if shared.control.paused.load(Ordering::Relaxed) {
                return Err(BlockError::Paused);
            }
            return Err(BlockError::Cancelled);
        }
        let n = reader
            .read(&mut buf[..grant])
            .map_err(|e| {
                pace_refund_both(shared, grant as u64);
                BlockError::Retry(format!("read: {e}"))
            })?;
        if (n as u64) < grant as u64 {
            pace_refund_both(shared, grant as u64 - n as u64);
        }
        if n == 0 {
            break;
        }
        // A halt that landed while this read was in flight must
        // freeze the byte count HERE: the chunk just read is
        // DISCARDED, never written. *pos does not advance, no
        // block is marked, the saved journal never claims these
        // bytes — a later resume simply re-fetches them by range.
        // (The loop-top check alone let exactly one in-flight
        // chunk land on disk after every pause / quota halt —
        // battery19 t1 "pause holds" and t6 "quota pauses".)
        if shared.superseded.load(Ordering::Relaxed) {
            return Err(BlockError::Cancelled);
        }
        if shared.control.cancelled.load(Ordering::Relaxed) {
            return Err(BlockError::Cancelled);
        }
        if shared.control.paused.load(Ordering::Relaxed) {
            return Err(BlockError::Paused);
        }
        // the classification is write_block_error's: disk-full
        // fails the download at once (QA-220 m2), anything else
        // retries the block
        write_at(file, &buf[..n], *pos).map_err(|e| write_block_error(&e))?;
        *pos += n as u64;
        shared.fetched_session.fetch_add(n as u64, Ordering::Relaxed);
        {
            // pool-wide progress clock (throttled: a quarter second
            // of precision is all the stall policy needs)
            let mut lp = plock(&shared.last_progress);
            if lp.elapsed() > Duration::from_millis(250) {
                *lp = Instant::now();
            }
        }
        if let Some(cb) = shared.conn_bytes.get(worker_id) {
            cb.fetch_add(n as u64, Ordering::Relaxed);
        }
        shared.since_save.fetch_add(n as u64, Ordering::Relaxed);
        shared.bytes_since_sync.fetch_add(n as u64, Ordering::Relaxed);
        // F2: this connection's own watermark — crossing 256 KiB
        // of delivered bytes resets its clock; a full STALL_LIMIT
        // below that floor recycles the connection (the worker
        // drops it and re-requests the sub-run from the current
        // offset: bytes already written stay written and are
        // never re-fetched)
        if !note_stall_progress(*pos, &mut wm_pos, &mut wm_clock)
            && wm_clock.elapsed() > STALL_LIMIT
        {
            return Err(BlockError::Recycle);
        }
        // decay-window check (see the state declared above)
        if decay_clock.elapsed() >= DECAY_WINDOW {
            let window_bytes = (*pos).saturating_sub(decay_pos);
            match decay_baseline {
                None => decay_baseline = Some(window_bytes),
                Some(base) => {
                    if window_bytes < base / 2 && window_bytes < 1024 * 1024 {
                        return Err(BlockError::Recycle);
                    }
                }
            }
            decay_pos = *pos;
            decay_clock = Instant::now();
        }

        // Mark any completed blocks in this contiguous run immediately
        while *cur_block < b_end_excl {
            let b_limit = ((*cur_block as u64 + 1) * block_size).min(total);
            if *pos >= b_limit {
                if shared.superseded.load(Ordering::Relaxed) {
                    // detached zombie (C-2): marks are writes too —
                    // a superseded worker changes nothing, ever
                    return Err(BlockError::Cancelled);
                }
                {
                    let mut st = plock(&shared.state);
                    if st.done[*cur_block] == 0 {
                        st.done[*cur_block] = 1;
                        // B-4: a sealed block carries no partial claim
                        if let Some(p) = st.partial.get_mut(*cur_block) {
                            *p = 0;
                        }
                        shared.done_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
                shared.block_state[*cur_block].store(1, Ordering::Relaxed);
                *cur_block += 1;
                if let Some(b) = shared.conn_block.get(worker_id) {
                    b.store(*cur_block, Ordering::Relaxed);
                }
                if *cur_block < b_end_excl {
                    shared.block_state[*cur_block].store(2, Ordering::Relaxed);
                }
            } else {
                break;
            }
        }
        // B-4: record the in-flight block's written prefix in
        // the journal state — the bytes are already in the file
        // at their offsets (write_at above), so this count is
        // all a restart needs to resume mid-block. Updated per
        // chunk alongside the sealed marks, so it rides the same
        // save_state throttling below: a crash loses at most the
        // same window the sealed marks already lose.
        if *cur_block < b_end_excl {
            let blk_start = (*cur_block as u64) * block_size;
            let in_block = (*pos).saturating_sub(blk_start).min(block_size);
            let mut st = plock(&shared.state);
            if let Some(p) = st.partial.get_mut(*cur_block) {
                *p = in_block;
            }
        }
        // persist completions continuously (save_state throttles
        // itself to 1 s / 4 MiB and try-locks): a crash or power loss
        // must lose at most the in-flight block, never a whole claim
        // run of up to 256 blocks whose marks existed only in RAM
        save_state(shared, false, Some(file));
        if *cur_block < b_end_excl {
            let b_base = (*cur_block as u64) * block_size;
            shared.partial[worker_id].store((*pos).saturating_sub(b_base), Ordering::Relaxed);
        } else {
            shared.partial[worker_id].store(0, Ordering::Relaxed);
        }
        // Arm the work-stealing cap (Finding 1): a peer is idle
        // while this run still has a splittable tail of unstarted
        // blocks. The first unwritten block is the block cursor,
        // or one past it when the cursor block already holds bytes
        // — the split point sits SPLIT_KEEP_BLOCKS beyond it, so
        // the boundary this run stops at is always ahead of every
        // byte written so far.
        if cap_block.is_none() && split_allowed(shared) {
            let first_unwritten = if *pos > (*cur_block as u64) * block_size {
                *cur_block + 1
            } else {
                *cur_block
            };
            cap_block = shed_point(shared, first_unwritten, b_end_excl);
        }
        // Arm the tail byte-split (WS-D D1): the end-game shape
        // the block split above cannot reach — this run's current
        // block is in flight and its unstarted tail is too small
        // to split, yet peers are idle. The owner executes the
        // split itself, here at a chunk boundary: it publishes the
        // helper range and caps its own reads at the split, so the
        // two ranges are disjoint by construction (see
        // tail_split_point). One split per block, ever: a block
        // already in the registry is resumed via tail_cap above.
        // The block must be THIS run's: when the marking loop
        // seals the run's final block it leaves the cursor on the
        // NEXT block — a stranger's block, owned by another claim —
        // and splitting that one would hand its tail to a helper
        // while its real owner writes it whole (a duplicated
        // range, caught by the regression test's fetched==size
        // audit). The block-split arming above is safe here only
        // because its arithmetic refuses an empty tail.
        // Conservative shape, second half: the owner must not
        // be nearly done with the block (splitting then saves
        // less than a fresh connection costs), and it must be a
        // proven straggler against this pool's own peak rate —
        // at uniform speeds a split is a measured loss, so the
        // gate is what confines the mechanism to the shape it
        // wins (see tail_owner_is_straggler).
        let block_start = (*cur_block as u64) * block_size;
        if tail_cap.is_none()
            && *cur_block < b_end_excl
            && *pos - block_start <= block_size * 3 / 4
            && (tail_split_due_survivor(shared)
                || (tail_split_due(shared) && tail_owner_is_straggler(shared, worker_id)))
        {
            let block_end = ((*cur_block as u64 + 1) * block_size).min(total);
            let registered = plock(&shared.tail_splits)
                .get(*cur_block)
                .map(|o| o.is_some())
                .unwrap_or(true);
            if !registered {
                if let Some(split) = tail_split_point(*pos, block_end) {
                    {
                        let mut splits = plock(&shared.tail_splits);
                        if let Some(slot) = splits.get_mut(*cur_block) {
                            *slot = Some(TailSplit {
                                split,
                                owner_done: false,
                                helper_done: false,
                            });
                        }
                    }
                    plock(&shared.byte_requeue).push((split, block_end));
                    *plock(&shared.last_split) = Some(Instant::now());
                    tail_cap = Some(split);
                }
            }
        }
    }
    if let Some(split) = cap_block {
        let cap_byte = ((split as u64) * block_size).min(total);
        if *pos == cap_byte && *pos < resp_end + 1 {
            // stopped exactly at the armed boundary, with response
            // bytes still unread: a deliberate yield, not a short run
            return Err(BlockError::Yielded(split));
        }
    }
    if let Some(split) = tail_cap {
        if *pos == split && *pos < resp_end + 1 {
            // stopped exactly at the tail-split offset, with this
            // response's tail bytes unread: the helper range owns
            // them now (see tail_split_point) — a deliberate yield
            return Err(BlockError::YieldedByte(*cur_block));
        }
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

/// WS-D D1: fetch one published tail byte-range [range_start,
/// range_end) — the helper half of a split block. Mirrors
/// `download_block_run`'s wire discipline (jar, auth scoping,
/// range validation, pacing, halt checks, per-connection stall
/// watermark) for a range that lives inside a single block:
/// every byte it writes belongs to that block and to no other
/// range, and `pos` advances only over bytes actually written,
/// so a retry or recycle resumes the range exactly where it
/// stopped — never re-fetching a written byte.
#[allow(clippy::too_many_arguments)]
fn download_byte_range(
    agent: &ureq::Agent,
    url: &str,
    file: &File,
    range_start: u64,
    range_end: u64,
    pos: &mut u64,
    shared: &Shared,
    worker_id: usize,
) -> Result<(), BlockError> {
    if *pos >= range_end {
        return Ok(());
    }
    let (block_size, total) = {
        let st = plock(&shared.state);
        (st.block_size, st.total)
    };
    if block_size == 0 {
        return Ok(());
    }
    let block = (range_start / block_size) as usize;
    let _active_guard = ActiveWorkerGuard::new(&shared.active_workers);
    let req_start = *pos;
    let end = range_end - 1; // inclusive, as on the wire
    let origin_url = plock(&shared.state).url.clone();
    let range_header = format!("bytes={req_start}-{end}");
    let headers: Vec<(&str, &str)> = vec![
        ("Accept-Encoding", "identity"),
        ("Range", &range_header),
    ];
    if let Some(p) = shared.conn_phase.get(worker_id) {
        p.store(1, Ordering::Relaxed);
    }
    if let Some(b) = shared.conn_block.get(worker_id) {
        b.store(block, Ordering::Relaxed);
    }
    let resp = jar_call(
        agent,
        &shared.jar,
        &shared.auth,
        &origin_url,
        "GET",
        url,
        &headers,
    )
    .map_err(|e| {
        if matches!(e, ureq::Error::Status(403, _) | ureq::Error::Status(429, _)) {
            plock(&shared.throttle_times).push(Instant::now());
            std::thread::sleep(Duration::from_millis(900));
        }
        if matches!(e, ureq::Error::Status(404, _) | ureq::Error::Status(410, _)) {
            BlockError::Permanent(format!("http: {e}"))
        } else {
            BlockError::Retry(format!("http: {e}"))
        }
    })?;
    if !shared.ignore_encoding.load(Ordering::Relaxed) {
        if let Some(enc) = response_encoding(&resp) {
            if !worker_proves_encoding_lie(agent, shared, url, total) {
                return Err(BlockError::Permanent(format!(
                    "server answered with Content-Encoding: {enc} despite an identity request"
                )));
            }
        }
    }
    let status = resp.status();
    let resp_end = if status == 206 {
        shared.range_200_count.store(0, Ordering::Relaxed);
        let parsed = resp.header("Content-Range").and_then(parse_content_range);
        match parsed {
            Some((s, e, t)) if s == req_start && e <= end && (t.is_none() || t == Some(total)) => e,
            _ => {
                return Err(BlockError::Retry(
                    "server answered a byte-range request with the wrong range".to_string(),
                ));
            }
        }
    } else if status == 404 || status == 410 {
        return Err(BlockError::Permanent(format!(
            "server answered {status} to a byte-range request"
        )));
    } else {
        if status == 200 {
            note_range_loss(shared);
        }
        return Err(BlockError::Retry(format!(
            "server answered {status} to a byte-range request"
        )));
    };
    if let Some(p) = shared.conn_phase.get(worker_id) {
        p.store(2, Ordering::Relaxed);
    }
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 512 * 1024];
    let mut wm_pos = *pos;
    let mut wm_clock = Instant::now();
    loop {
        if shared.single_fallback.load(Ordering::Relaxed)
            || shared.superseded.load(Ordering::Relaxed)
            || shared.control.cancelled.load(Ordering::Relaxed)
        {
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
        let grant = pace_grant_size(shared, want);
        if !pace_before_read(shared, grant as u64) {
            if shared.control.paused.load(Ordering::Relaxed) {
                return Err(BlockError::Paused);
            }
            return Err(BlockError::Cancelled);
        }
        let n = reader.read(&mut buf[..grant]).map_err(|e| {
            pace_refund_both(shared, grant as u64);
            BlockError::Retry(format!("read: {e}"))
        })?;
        if (n as u64) < grant as u64 {
            pace_refund_both(shared, grant as u64 - n as u64);
        }
        if n == 0 {
            break;
        }
        if shared.superseded.load(Ordering::Relaxed)
            || shared.control.cancelled.load(Ordering::Relaxed)
        {
            return Err(BlockError::Cancelled);
        }
        if shared.control.paused.load(Ordering::Relaxed) {
            return Err(BlockError::Paused);
        }
        write_at(file, &buf[..n], *pos).map_err(|e| write_block_error(&e))?;
        *pos += n as u64;
        shared.fetched_session.fetch_add(n as u64, Ordering::Relaxed);
        {
            let mut lp = plock(&shared.last_progress);
            if lp.elapsed() > Duration::from_millis(250) {
                *lp = Instant::now();
            }
        }
        if let Some(cb) = shared.conn_bytes.get(worker_id) {
            cb.fetch_add(n as u64, Ordering::Relaxed);
        }
        shared.since_save.fetch_add(n as u64, Ordering::Relaxed);
        shared.bytes_since_sync.fetch_add(n as u64, Ordering::Relaxed);
        if let Some(p) = shared.partial.get(worker_id) {
            p.store((*pos).saturating_sub(range_start), Ordering::Relaxed);
        }
        if !note_stall_progress(*pos, &mut wm_pos, &mut wm_clock)
            && wm_clock.elapsed() > STALL_LIMIT
        {
            return Err(BlockError::Recycle);
        }
    }
    if *pos != resp_end + 1 {
        return Err(BlockError::Retry(format!(
            "short byte range: got {} bytes, expected to reach {}",
            *pos,
            resp_end + 1
        )));
    }
    Ok(())
}

/// WS-D D1: run one published tail byte-range to completion on
/// this (idle) worker, with the same failure policy as a block
/// sub-run: partial responses and recycles continue the range
/// from its write cursor, repeated failures hand the UNWRITTEN
/// remainder back to `byte_requeue` for another helper, and only
/// the pool-wide stall policy or a Permanent refusal fails the
/// download. Returns false when the worker itself must exit
/// (cancelled, or the download failed through this range).
fn run_byte_helper(
    id: usize,
    agent: &ureq::Agent,
    url: &str,
    file: &Arc<File>,
    shared: &Arc<Shared>,
    range_start: u64,
    range_end: u64,
) -> bool {
    let block = {
        let st = plock(&shared.state);
        if st.block_size == 0 {
            return true;
        }
        (range_start / st.block_size) as usize
    };
    let mut pos = range_start;
    let mut attempt = 0;
    loop {
        if shared.superseded.load(Ordering::Relaxed)
            || shared.failed.load(Ordering::Relaxed)
            || shared.control.cancelled.load(Ordering::Relaxed)
        {
            return false;
        }
        let before = pos;
        let res = download_byte_range(agent, url, file, range_start, range_end, &mut pos, shared, id);
        // an empty 206 answer is not progress (the F-15 wedge
        // rule, applied to byte ranges as to block runs)
        let res = match res {
            Ok(()) if pos == before && pos < range_end => Err(BlockError::Retry(
                "server answered a byte-range request with an empty range".to_string(),
            )),
            other => other,
        };
        match res {
            Ok(()) => {
                if pos >= range_end {
                    shared.partial[id].store(0, Ordering::Relaxed);
                    note_tail_helper_done(shared, block);
                    save_state(shared, false, Some(file));
                    return true;
                }
                // partial response: progress, not a failure —
                // loop and fetch the range's remainder
                attempt = 0;
            }
            Err(BlockError::Recycle) => continue,
            Err(BlockError::Paused) => {
                if !wait_while_paused(shared) {
                    shared.partial[id].store(0, Ordering::Relaxed);
                    return false;
                }
            }
            Err(BlockError::Cancelled) => {
                shared.partial[id].store(0, Ordering::Relaxed);
                return false;
            }
            Err(BlockError::Permanent(e)) => {
                shared.partial[id].store(0, Ordering::Relaxed);
                let mut err = plock(&shared.error);
                *err = Some(format!("block {block}: {e}"));
                shared.failed.store(true, Ordering::Relaxed);
                return false;
            }
            Err(BlockError::Retry(e)) => {
                if plock(&shared.last_progress).elapsed() > STALL_LIMIT {
                    shared.partial[id].store(0, Ordering::Relaxed);
                    let mut err = plock(&shared.error);
                    *err = Some(format!(
                        "no progress for {}s (last error: block {block}: {e})",
                        STALL_LIMIT.as_secs()
                    ));
                    shared.failed.store(true, Ordering::Relaxed);
                    return false;
                }
                attempt += 1;
                if attempt >= 5 {
                    shared.partial[id].store(0, Ordering::Relaxed);
                    if pos < range_end {
                        plock(&shared.byte_requeue).push((pos, range_end));
                    }
                    return true;
                }
                let backoff = (300u64 << (attempt - 1)).min(5_000);
                std::thread::sleep(Duration::from_millis(backoff));
            }
            Err(BlockError::Yielded(_)) | Err(BlockError::YieldedByte(_)) => return true,
        }
    }
}

fn wait_while_paused(shared: &Shared) -> bool {
    // returns false when cancelled while waiting
    while shared.control.paused.load(Ordering::Relaxed) {
        if shared.control.cancelled.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let resumed = !shared.control.cancelled.load(Ordering::Relaxed);
    if resumed {
        // the stall policy measures from the resume, not from the
        // last byte before the pause — a paused download is waiting
        // on the user, not on the network
        *plock(&shared.last_progress) = Instant::now();
    }
    resumed
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
        let st = plock(&shared.state);
        (st.block_size, st.total)
    };
    loop {
        // G1: between claims this connection is waiting, holding
        // no block — the phase/block surface says so until the
        // next run's request goes out
        if let Some(p) = shared.conn_phase.get(id) {
            p.store(0, Ordering::Relaxed);
        }
        if let Some(b) = shared.conn_block.get(id) {
            b.store(usize::MAX, Ordering::Relaxed);
        }
        shared.partial[id].store(0, Ordering::Relaxed);
        if shared.superseded.load(Ordering::Relaxed)
            || shared.failed.load(Ordering::Relaxed)
            || shared.control.cancelled.load(Ordering::Relaxed)
            || shared.done_count.load(Ordering::Relaxed) >= total_blocks
        {
            return;
        }
        if shared.control.paused.load(Ordering::Relaxed) {
            if !wait_while_paused(&shared) {
                return;
            }
            continue;
        }
        if route_fallback(id, &agent, &url, &file, &shared) {
            return;
        }
        let active = shared.active_limit.load(Ordering::Relaxed).max(1);
        if id >= active {
            // gated by the adaptive connection limit. This worker
            // waits — it must not leave while work remains (see the
            // claim wait below) — and the stall policy is evaluated
            // here too, so a pool whose eligible workers have all
            // stopped moving bytes still settles as an error.
            if route_fallback(id, &agent, &url, &file, &shared) {
                return;
            }
            if fail_if_stalled(&shared, total_blocks) {
                return;
            }
            std::thread::sleep(Duration::from_millis(80));
            continue;
        }
        // Claim work. A worker must NEVER leave merely because
        // nothing is claimable at this instant: a range another
        // worker is struggling with can come back into the pool at
        // any moment, and the old code's `return` here is how a
        // download ended up owned by nobody — every claim-eligible
        // worker gone, the unfinished ranges visible to no one, zero
        // connections, no error, and a supervisor waiting forever on
        // the remaining gated/waiting workers (F-15: a resumed
        // 800 MB download froze at 67% for minutes, state file
        // bit-identical, until a fresh start revived it). Wait for
        // work instead; only a terminal condition ends a worker.
        let (claim_start, mut claim_end) = {
            // while nothing is claimable this worker counts as
            // waiting: that count is the signal busy workers split
            // their unstarted tails for (Finding 1)
            let mut waiter: Option<WaiterGuard> = None;
            loop {
                // WS-D D1: a published tail byte-range outranks
                // waiting for a block claim — it holds the
                // end-game's last unfinished bytes, and this
                // worker is idle by definition here. Helping is
                // work, not waiting, so the waiter registration
                // drops while the helper runs.
                if let Some((hs, he)) = plock(&shared.byte_requeue).pop() {
                    waiter = None;
                    if !run_byte_helper(id, &agent, &url, &file, &shared, hs, he) {
                        return;
                    }
                    continue;
                }
                if let Some(range) = try_claim(&shared, total_blocks) {
                    break range;
                }
                if waiter.is_none() {
                    waiter = Some(WaiterGuard::new(&shared.waiting_workers));
                }
                if shared.failed.load(Ordering::Relaxed)
                    || shared.control.cancelled.load(Ordering::Relaxed)
                    || shared.done_count.load(Ordering::Relaxed) >= total_blocks
                {
                    return;
                }
                if shared.control.paused.load(Ordering::Relaxed) {
                    if !wait_while_paused(&shared) {
                        return;
                    }
                    continue;
                }
                if route_fallback(id, &agent, &url, &file, &shared) {
                    return;
                }
                if fail_if_stalled(&shared, total_blocks) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };

        // Process any undone sub-runs inside [claim_start, claim_end)
        let mut cursor = claim_start;
        while cursor < claim_end {
            // WS-D D1: between sub-runs is a natural break — serve
            // a queued tail byte-range before starting the next
            // sub-run. The claim-wait loop alone made a thin,
            // transient helper pool: in the uniform-cap end-game
            // most workers sit inside long sub-run retry cycles
            // when a split arms, and a range queued behind them
            // was claimed seconds late, serially, erasing the
            // split's win (rig: 124 s helped vs 111 s unhelped at
            // 100 MiB / 16 conns / 64 KiB/s). A worker here is
            // between blocks, so helping costs its claim nothing
            // it was using.
            if let Some((hs, he)) = plock(&shared.byte_requeue).pop() {
                if !run_byte_helper(id, &agent, &url, &file, &shared, hs, he) {
                    return;
                }
                continue;
            }
            if shared.single_fallback.load(Ordering::Relaxed) {
                break; // F6: the loop head routes the fallback
            }
            // Between sub-runs the same split check as inside a run:
            // a fragmented claim's later sub-runs are unstarted work
            // an idle peer could be doing. The owner executes the
            // split itself — it is the only writer of its claim
            // boundary, so no cross-worker negotiation exists: the
            // released tail simply re-enters the pool as a requeue
            // range, under the same bitmap truth as any hand-back.
            if split_allowed(&shared) {
                if let Some(split) = shed_point(&shared, cursor, claim_end) {
                    plock(&shared.requeue).push((split, claim_end));
                    claim_end = split;
                    *plock(&shared.last_split) = Some(Instant::now());
                    if cursor >= claim_end {
                        break;
                    }
                }
            }
            let (b_first, b_last, resume_off) = {
                let st = plock(&shared.state);
                while cursor < claim_end && st.done[cursor] == 1 {
                    cursor += 1;
                }
                if cursor >= claim_end {
                    break;
                }
                let first = cursor;
                while cursor < claim_end && st.done[cursor] == 0 {
                    // B-4: a group may START on a block with a
                    // journaled partial prefix (it resumes inside
                    // it below), but a partial block in the
                    // group's MIDDLE ends the group before it —
                    // the run advances linearly and would
                    // otherwise plough over that block's prefix
                    // from its start, re-fetching it (harmless
                    // bytes, wasted work) instead of resuming it
                    // as its own group's head
                    if cursor > first
                        && st.partial.get(cursor).copied().unwrap_or(0) > 0
                    {
                        break;
                    }
                    cursor += 1;
                }
                let resume_off = st.partial.get(first).copied().unwrap_or(0);
                (first, cursor, resume_off)
            };

            let start = b_first as u64 * block_size;
            let end = ((b_last as u64) * block_size - 1).min(total - 1);
            let mut cur_block = b_first;
            // B-4: resume inside the first block at its journaled
            // prefix — those bytes are already in the file
            let mut cur_pos = start + resume_off;
            let mut attempt = 0;
            shared.block_state[cur_block].store(2, Ordering::Relaxed);
            if let Some(b) = shared.conn_block.get(id) {
                b.store(cur_block, Ordering::Relaxed);
            }

            loop {
                if shared.single_fallback.load(Ordering::Relaxed) {
                    break; // F6: the loop head routes the fallback
                }
                // A run that "succeeds" without moving the cursor —
                // a server answering a range request with an empty
                // 206 range does exactly this — is not progress:
                // treating it as Ok re-issues the identical request
                // in a tight loop forever (no bytes, no error, no
                // terminal state — the F-15 wedge, reproduced in the
                // empty-range regression test). It goes through the
                // failure accounting instead.
                let before = (cur_block, cur_pos);
                let run_result = download_block_run(
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
                );
                let run_result = match run_result {
                    Ok(()) if (cur_block, cur_pos) == before => Err(BlockError::Retry(
                        "server answered a range request with an empty range".to_string(),
                    )),
                    other => other,
                };
                match run_result {
                    Ok(()) => {
                        // a successful (possibly partial) run clears
                        // the consecutive-failure count
                        attempt = 0;
                        shared.partial[id].store(0, Ordering::Relaxed);
                        save_state(&shared, false, Some(&*file));
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
                    Err(BlockError::Yielded(split)) => {
                        // The run stopped at its armed split boundary:
                        // hand this claim's whole unstarted tail —
                        // [split, claim_end), which includes the rest
                        // of this sub-run — to the pool, and end the
                        // claim here. The owner executes the split
                        // itself, so the bitmap stays the single truth
                        // and no block is ever owned by two workers.
                        // Not a failure: no attempt counted, no error
                        // recorded, the stall policy untouched.
                        shared.partial[id].store(0, Ordering::Relaxed);
                        if cur_block < b_last {
                            shared.block_state[cur_block].store(0, Ordering::Relaxed);
                        }
                        if split < claim_end {
                            plock(&shared.requeue).push((split, claim_end));
                            claim_end = split;
                            *plock(&shared.last_split) = Some(Instant::now());
                        }
                        break;
                    }
                    Err(BlockError::YieldedByte(block)) => {
                        // WS-D D1: this run stopped at its tail
                        // byte-split offset — its half of `block`
                        // is fully written, and the block's tail
                        // range is already published for a helper.
                        // Flag the owner half (the block seals
                        // when the helper half lands, or already
                        // has), hand this sub-run's remaining whole
                        // blocks back to the pool, and end the
                        // claim here. The split block itself is
                        // owned by the byte-range bookkeeping now —
                        // it must NOT re-enter the block pool, or a
                        // second worker would re-fetch its head.
                        shared.partial[id].store(0, Ordering::Relaxed);
                        if block < b_last {
                            shared.block_state[block].store(2, Ordering::Relaxed);
                        }
                        note_tail_owner_done(&shared, block);
                        if block + 1 < b_last {
                            plock(&shared.requeue).push((block + 1, b_last));
                        }
                        break;
                    }
                    Err(BlockError::Recycle) => {
                        // F2: this connection fell below the
                        // progress floor — drop it and re-request
                        // the SAME sub-run from the CURRENT offset
                        // (cur_pos is preserved: no byte already
                        // written is lost or re-fetched, which is
                        // what lets a server that dribbles EVERY
                        // connection still be finished — each new
                        // connection's fast burst lands further
                        // along the block). The worker keeps the
                        // claim; idle peers can still split its
                        // unstarted blocks. Not a failure: no
                        // attempt counted, no error recorded.
                        continue;
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
                    Err(BlockError::Permanent(e)) => {
                        // a definitive refusal: fail at once, the
                        // stall window cannot change the answer
                        shared.partial[id].store(0, Ordering::Relaxed);
                        let mut err = plock(&shared.error);
                        *err = Some(format!("block {cur_block}: {e}"));
                        shared.failed.store(true, Ordering::Relaxed);
                        if cur_block < b_last {
                            shared.block_state[cur_block].store(0, Ordering::Relaxed);
                        }
                        return;
                    }
                    Err(BlockError::Retry(e)) => {
                        // Failure policy: one sick sub-run must not
                        // kill the download. The pool as a whole
                        // fails only when NO worker has moved a byte
                        // for STALL_LIMIT; short of that, a sub-run
                        // that keeps failing hands its unfinished
                        // tail back to the pool and this worker moves
                        // on to other work.
                        if plock(&shared.last_progress).elapsed() > STALL_LIMIT {
                            shared.partial[id].store(0, Ordering::Relaxed);
                            let mut err = plock(&shared.error);
                            *err = Some(format!(
                                "no progress for {}s (last error: block {cur_block}: {e})",
                                STALL_LIMIT.as_secs()
                            ));
                            shared.failed.store(true, Ordering::Relaxed);
                            if cur_block < b_last {
                                shared.block_state[cur_block].store(0, Ordering::Relaxed);
                            }
                            return;
                        }
                        attempt += 1;
                        if attempt >= 5 {
                            shared.partial[id].store(0, Ordering::Relaxed);
                            if cur_block < b_last {
                                shared.block_state[cur_block].store(0, Ordering::Relaxed);
                                plock(&shared.requeue).push((cur_block, b_last));
                            }
                            break;
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

/// F6 routing, callable from every worker wait point (loop
/// head, connection-limit gate, claim wait): when range loss
/// has been declared, worker 0 runs the single-stream restart
/// and every other worker simply stands down. Returns true when
/// the caller must return — the fallback owns the job now.
fn route_fallback(
    id: usize,
    agent: &ureq::Agent,
    url: &str,
    file: &Arc<File>,
    shared: &Arc<Shared>,
) -> bool {
    if !shared.single_fallback.load(Ordering::Relaxed) {
        return false;
    }
    if id == 0 {
        single_fallback_download(agent, url, file, shared);
    }
    true
}

/// The F6 single-stream restart, run by worker 0 inside the
/// ranged job's own Shared when range support is lost
/// mid-download: one plain GET streamed over the file from byte
/// 0. Writing in place is safe here precisely because this is
/// the SAME file the ranges were fetching — every byte the
/// stream lands is that file's byte at that offset, the sealed
/// blocks the journal holds stay true throughout, and only a
/// verified-complete stream marks the job done. (The demoted
/// resume path in start_single_stream spools instead because
/// there the file identity itself is in question.) Pacing,
/// pause/cancel and the 256 KiB stall watermark all apply
/// exactly as on the dedicated single-stream path.
fn single_fallback_download(
    agent: &ureq::Agent,
    url: &str,
    file: &Arc<File>,
    shared: &Arc<Shared>,
) {
    let (total, total_blocks) = {
        let st = plock(&shared.state);
        (st.total, st.done.len())
    };
    let origin_url = plock(&shared.state).url.clone();
    let attempt = || -> Result<bool, String> {
        // Ok(true) = the file is complete; Ok(false) = a cancel
        // landed mid-stream and the supervisor settles it
        let resp = jar_call(
            agent,
            &shared.jar,
            &shared.auth,
            &origin_url,
            "GET",
            url,
            &[("Accept-Encoding", "identity")],
        )
        .map_err(|e| {
            let msg = format!("http: {e}");
            if error_is_auth_dead(&msg) {
                note_auth_dead(shared);
            }
            msg
        })?;
        if resp.status() != 200 {
            return Err(format!(
                "server answered {} to the single-stream restart",
                resp.status()
            ));
        }
        if !shared.ignore_encoding.load(Ordering::Relaxed) {
            if let Some(enc) = response_encoding(&resp) {
                return Err(format!(
                    "server answered with Content-Encoding: {enc} despite an identity request"
                ));
            }
        }
        if let Some(p) = shared.conn_phase.first() {
            p.store(2, Ordering::Relaxed);
        }
        let mut reader = resp.into_reader();
        let mut buf = vec![0u8; 512 * 1024];
        let mut pos = 0u64;
        let mut stall_watermark = 0u64;
        let mut stall_clock = Instant::now();
        loop {
            if shared.control.cancelled.load(Ordering::Relaxed) {
                return Ok(false);
            }
            if shared.control.paused.load(Ordering::Relaxed) {
                if !wait_while_paused(shared) {
                    return Ok(false);
                }
                stall_watermark = pos;
                stall_clock = Instant::now();
            }
            if stall_clock.elapsed() > STALL_LIMIT {
                return Err(format!(
                    "no progress for {}s — the server stopped sending data",
                    STALL_LIMIT.as_secs()
                ));
            }
            let grant = pace_grant_size(shared, buf.len());
            if !pace_before_read(shared, grant as u64) {
                continue; // a halt landed; the checks above settle it
            }
            let n = reader.read(&mut buf[..grant]).map_err(|e| {
                pace_refund_both(shared, grant as u64);
                format!("read: {e}")
            })?;
            if (n as u64) < grant as u64 {
                pace_refund_both(shared, grant as u64 - n as u64);
            }
            if n == 0 {
                break;
            }
            if shared.control.cancelled.load(Ordering::Relaxed) {
                return Ok(false);
            }
            write_at(file, &buf[..n], pos).map_err(|e| plain_write_error(&e))?;
            pos += n as u64;
            if note_stall_progress(pos, &mut stall_watermark, &mut stall_clock) {
                *plock(&shared.last_progress) = stall_clock;
            }
            if pos > total {
                return Err(format!(
                    "server sent more data than the probed size ({total} bytes)"
                ));
            }
            shared.fetched_session.fetch_add(n as u64, Ordering::Relaxed);
            if let Some(cb) = shared.conn_bytes.first() {
                cb.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
        if pos != total {
            return Err(format!(
                "short download: got {pos} of {total} bytes"
            ));
        }
        Ok(true)
    };
    match attempt() {
        Ok(true) => {
            {
                let mut st = plock(&shared.state);
                for b in st.done.iter_mut() {
                    *b = 1;
                }
            }
            for b in &shared.block_state {
                b.store(1, Ordering::Relaxed);
            }
            shared
                .done_count
                .store(total_blocks, Ordering::Relaxed);
            save_state(shared, true, Some(file));
        }
        Ok(false) => {}
        Err(e) => {
            let mut err = plock(&shared.error);
            *err = Some(e);
            shared.failed.store(true, Ordering::Relaxed);
        }
    }
}

/// The governor's decision for one tick, as a pure function
/// (F3). The pool shrinks ONLY on broad throttling: at least
/// half the pool's connections throttled inside the recent
/// window. A lone 403/429 — an expired signed range, one
/// rate-limited CDN edge — used to halve every connection in
/// the job; now it costs only its own connection a cooldown
/// (the worker's throttle path), and the pool halves solely
/// when the throttling is demonstrably pool-wide.
#[derive(Debug, PartialEq, Eq)]
enum GovDecision {
    Shrink,
    Grow,
    Hold,
}

fn governor_decision(
    recent_throttles: usize,
    pool_size: usize,
    limit: usize,
    max_conn: usize,
    rate: f64,
) -> GovDecision {
    if recent_throttles > 0 && recent_throttles * 2 >= pool_size.max(1) && limit > 1 {
        return GovDecision::Shrink;
    }
    if limit < max_conn && rate > 0.0 {
        return GovDecision::Grow;
    }
    GovDecision::Hold
}

/// How far back a throttle event still counts as part of the
/// current episode for the governor's broad-throttling test.
const THROTTLE_WINDOW: Duration = Duration::from_secs(5);

fn governor(shared: Arc<Shared>, max_conn: usize, total_blocks: usize) {
    let mut last_bytes = 0u64;
    let mut last_check = Instant::now();
    let mut last_conn_bytes: Vec<u64> = vec![0; max_conn];
    loop {
        std::thread::sleep(Duration::from_millis(1500));
        if shared.done_count.load(Ordering::Relaxed) >= total_blocks
            || shared.failed.load(Ordering::Relaxed)
            || shared.control.cancelled.load(Ordering::Relaxed)
        {
            return;
        }
        let now_bytes = shared.fetched_session.load(Ordering::Relaxed);
        let tick_secs = last_check.elapsed().as_secs_f64();
        let rate = (now_bytes - last_bytes) as f64 / tick_secs;
        last_bytes = now_bytes;
        last_check = Instant::now();
        // WS-D D1: sample each connection's delivery rate over
        // this tick and remember the pool's peak — the tail
        // byte-split's straggler gate compares a block owner's
        // current rate against what this pool has demonstrably
        // been able to do.
        if tick_secs > 0.0 {
            for (i, cb) in shared.conn_bytes.iter().enumerate() {
                let now = cb.load(Ordering::Relaxed);
                let prev = last_conn_bytes.get(i).copied().unwrap_or(0);
                let r = (now.saturating_sub(prev) as f64 / tick_secs) as u64;
                if let Some(slot) = shared.conn_rate_bps.get(i) {
                    slot.store(r, Ordering::Relaxed);
                }
                if r > shared.peak_conn_rate_bps.load(Ordering::Relaxed) {
                    shared.peak_conn_rate_bps.store(r, Ordering::Relaxed);
                }
                if let Some(slot) = last_conn_bytes.get_mut(i) {
                    *slot = now;
                }
            }
        }
        let limit = shared.active_limit.load(Ordering::Relaxed);
        let recent_throttles = {
            let mut times = plock(&shared.throttle_times);
            times.retain(|t| t.elapsed() <= THROTTLE_WINDOW);
            times.len()
        };
        match governor_decision(
            recent_throttles,
            limit,
            limit,
            shared.conn_cap.load(Ordering::Relaxed).max(1),
            rate,
        ) {
            GovDecision::Shrink => {
                shared
                    .active_limit
                    .store((limit / 2).max(1), Ordering::Relaxed);
                // the episode is spent: one broad burst halves
                // the pool once, not once per governor tick
                plock(&shared.throttle_times).clear();
            }
            GovDecision::Grow => {
                shared.active_limit.store(
                    (limit * 2).min(shared.conn_cap.load(Ordering::Relaxed).max(1)),
                    Ordering::Relaxed,
                );
            }
            GovDecision::Hold => {}
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
        let status = *plock(&self.shared.status);
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
            sealed_done_bytes: if status == Status::Finished {
                self.total
            } else if self.resumable {
                plock(&self.shared.state).done_bytes()
            } else {
                0
            },
            done_bytes: if status == Status::Finished && self.total > 0 {
                self.total
            } else if self.resumable {
                // completed blocks + what each worker has already written
                // toward its current block: smooth, and never ahead of the
                // bytes actually on disk
                let persisted = plock(&self.shared.state).done_bytes();
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
                let mut hist = plock(&self.shared.rate_hist);
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
            error: plock(&self.shared.error).clone(),
            conn_bytes: self
                .shared
                .conn_bytes
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .collect(),
            conn_states: self
                .shared
                .conn_phase
                .iter()
                .map(|p| p.load(Ordering::Relaxed))
                .collect(),
            conn_partial: self
                .shared
                .partial
                .iter()
                .map(|p| p.load(Ordering::Relaxed))
                .collect(),
            conn_block: self
                .shared
                .conn_block
                .iter()
                .map(|b| {
                    let v = b.load(Ordering::Relaxed);
                    if v == usize::MAX {
                        -1
                    } else {
                        v as i64
                    }
                })
                .collect(),
            auth_dead: self.shared.auth_dead.load(Ordering::Relaxed),
        }
    }

    /// Set this download's own speed limit (bytes/sec, 0 = none) —
    /// IDM's per-download Speed Limiter tab. Applies immediately to a
    /// running download and composes with the global limit.
    pub fn set_limit(&self, bytes_per_sec: u64) {
        let old = self.shared.own_limit.swap(bytes_per_sec, Ordering::Relaxed);
        // a changed limit starts a fresh pacing clock, exactly like
        // the global setter — keeping the old clock made a raised
        // limit wait out slots reserved at the previous rate. The
        // generation bump makes waits parked under the old rate
        // re-reserve under the new one within one slice (M-14),
        // instead of ignoring the change for ≥15 s.
        if bytes_per_sec != old {
            *plock(&self.shared.own_limit_state) = None;
            // record the PRE-BUMP generation as the oldest-live
            // watermark BEFORE bumping (D7-v2, mirrored in
            // set_global_limit): charges from any older generation
            // are stale; a charge from the watermark generation
            // itself is live and earns no re-base clamp
            self.shared.own_limit_oldest_live.store(
                self.shared.own_limit_gen.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            self.shared.own_limit_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// WS-D D2: raise (or lower) how many of this download's
    /// spawned workers may be active at once, live. Workers park
    /// at the connection gate when their id is at or above the
    /// active limit and re-check every 80 ms, so raising the
    /// limit puts parked workers on the transfer within a tick —
    /// no restart, no lost progress. The governor's grow ceiling
    /// moves with it, so the governor will not shrink the pool
    /// back toward the old share on its next tick (its throttle
    /// shrink still applies, as for any download). Clamped to
    /// [1, spawned workers]: a download can never use connections
    /// it did not spawn. Pair cross-stealing calls this on the
    /// surviving stream when its sibling finishes.
    pub fn set_connection_limit(&self, n: usize) {
        let n = n.clamp(1, self.shared.worker_count.max(1));
        self.shared.conn_cap.store(n, Ordering::Relaxed);
        self.shared.active_limit.store(n, Ordering::Relaxed);
    }

    /// WS-D D2: mark this download as a video+audio pair's
    /// surviving stream (its sibling just finished). Relaxes its
    /// tail byte-split gate — see tail_split_due_survivor. Called
    /// together with set_connection_limit by the pair's poll.
    pub fn mark_pair_survivor(&self) {
        self.shared.pair_survivor.store(true, Ordering::Relaxed);
    }

    /// The most recent resume-state (journal) save failure for this
    /// download, if any — the surface for finding #8a's recorded
    /// errors (the app persists this alongside the entry so a
    /// download with no working journal is visible, not silent).
    pub fn last_journal_error(&self) -> Option<String> {
        plock(&self.shared.last_journal_error).clone()
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            *plock(&self.shared.status),
            Status::Finished | Status::Error | Status::Cancelled
        )
    }

    /// Terminal AND the engine's final resume-state save has
    /// landed on disk. `is_terminal` flips the moment the
    /// supervisor settles the status — BEFORE its file sync and
    /// final save — so a removal that waits only for terminal can
    /// sweep the journal away and watch the supervisor rewrite it
    /// a moment later (R4 item 2, both removal paths).
    pub fn is_settled(&self) -> bool {
        self.is_terminal() && self.shared.settled.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> Status {
        *plock(&self.shared.status)
    }
}

fn set_status(shared: &Shared, s: Status) {
    *plock(&shared.status) = s;
}



/// Start a download in the background. Progress is polled via `Download::progress`
/// (the CLI polls and prints; the Tauri backend polls and emits events).
/// Byte-compare one range of the partial file against a fresh
/// range GET of the same bytes. Used when a resume state has no
/// usable validators (see start_impl): any failure — transport,
/// a non-206 answer, a short read, a single differing byte —
/// returns false and the caller starts fresh rather than splicing
/// unverified data into the file.
fn verify_block_bytes(
    agent: &ureq::Agent,
    url: &str,
    auth: &Auth,
    origin_url: &str,
    jar: &CookieJar,
    out: &Path,
    start: u64,
    end: u64,
    ignore_encoding: bool,
) -> bool {
    use std::io::{Seek, SeekFrom};
    let want = (end - start) as usize;
    if want == 0 {
        return true;
    }
    if ftp::is_ftp_url(url) {
        return ftp::verify_block_ftp(url, auth, out, start, end);
    }
    let range_header = format!("bytes={start}-{}", end - 1);
    let Ok(resp) = jar_call(
        agent,
        jar,
        auth,
        origin_url,
        "GET",
        url,
        &[("Accept-Encoding", "identity"), ("Range", &range_header)],
    ) else {
        return false;
    };
    if resp.status() != 206 || (!ignore_encoding && response_encoding(&resp).is_some()) {
        return false;
    }
    // the answer must be the range that was asked for — a server
    // streaming from another offset would "verify" the wrong bytes
    // (parse_content_range also rejects an inverted end < start)
    match resp.header("Content-Range").and_then(parse_content_range) {
        Some((s, e, _)) if s == start && e == end - 1 => {}
        _ => return false,
    }
    let mut remote = Vec::with_capacity(want);
    if resp
        .into_reader()
        .take((want + 1) as u64)
        .read_to_end(&mut remote)
        .is_err()
        || remote.len() != want
    {
        return false;
    }
    let Ok(mut f) = File::open(out) else {
        return false;
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut local = vec![0u8; want];
    f.read_exact(&mut local).is_ok() && local == remote
}

/// Process-wide registry of output paths a live download is
/// writing (finding #10): two downloads started on the same path —
/// a double-clicked catch, a queue race — used to interleave writes
/// into the same file and the same journal, corrupting both while
/// each reported progress. Registration fails fast instead; the
/// guard is held by the download's supervisor thread and released
/// when it settles, on every exit path.
static IN_FLIGHT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The canonical identity of an output path (W1 F3): the file
/// itself when it exists (a symlinked FILE resolves to its
/// target), else its canonicalised parent directory plus the file
/// name (a symlinked DIRECTORY spelling — `linkdir/alias/f.bin`
/// where alias → the real dir — resolves to the real location, so
/// two spellings of one file share one guard key and one lock
/// file). When nothing on disk can be canonicalised yet, fall
/// back to lexical normalisation against the cwd, as before.
fn canonical_key(out: &Path) -> PathBuf {
    let abs = if out.is_absolute() {
        out.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(out))
            .unwrap_or_else(|_| out.to_path_buf())
    };
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return c;
    }
    if let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) {
        if let Ok(cp) = std::fs::canonicalize(parent) {
            return cp.join(name);
        }
    }
    let mut parts: Vec<std::path::Component> = Vec::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}

/// Unix: an OS advisory exclusive lock (flock) on a per-output
/// lock file, held for the download's life — the cross-process
/// half of the same-path guard (W1 F3b: two CLI processes spliced
/// one file while both reported DONE). Declared directly against
/// libc's flock so no new dependency is needed; closing the file
/// (guard drop, or process exit for any reason including SIGKILL)
/// releases it, so a crashed owner can never lock a path forever.
#[cfg(unix)]
mod os_file_lock {
    use std::fs::File;
    use std::os::raw::c_int;
    use std::os::unix::io::AsRawFd;

    extern "C" {
        fn flock(fd: c_int, operation: c_int) -> c_int;
    }
    const LOCK_EX: c_int = 2;
    const LOCK_NB: c_int = 4;

    pub fn try_acquire(file: &File) -> bool {
        unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) == 0 }
    }
}

/// Race-safe removal of ONE lock file at `path`; returns true
/// when the file was removed. This is the single implementation
/// of the removal protocol, shared by the janitorial sweep and
/// by `InFlightGuard::drop` (release-time cleanup): the file is
/// removed only when its flock can be acquired (no live download
/// holds it) AND the opened handle still names the same inode/dev
/// as a fresh metadata lookup of the path (the path was not
/// replaced between open and delete). Dropping the handle then
/// releases the flock on the now unlinked inode, which no future
/// contender can open. Every error is a silent skip: removal is
/// janitorial and must never fail a download.
#[cfg(unix)]
fn sweep_lock_file(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    // never create: a path that vanished already is gone, and
    // creating it just to delete it would be litter of our own.
    let f = match OpenOptions::new().write(true).truncate(false).open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    if !os_file_lock::try_acquire(&f) {
        // a live download holds this lock — leave it alone.
        return false;
    }
    // identity re-check: the inode we flocked must still be the
    // inode the path names, or the path was replaced under us and
    // deleting it could remove a fresh lock.
    let opened = match f.metadata() {
        Ok(m) => m,
        Err(_) => return false,
    };
    let fresh = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if opened.ino() != fresh.ino() || opened.dev() != fresh.dev() {
        return false;
    }
    std::fs::remove_file(path).is_ok()
    // dropping `f` here releases the flock.
}

/// Janitorial sweep of stale `.ndm.lock` litter in `dir`
/// (non-recursive), returning how many files were removed.
///
/// `InFlightGuard::drop` removes a download's own lock file at
/// release time (see the Drop impl), so on every clean path —
/// finish, removal, app exit — the file is already gone. This
/// sweep is the backstop for the paths Drop cannot reach: a
/// process killed mid-download (SIGKILL, power loss) leaves the
/// file behind while its flock dies with the process, and
/// litter written by older versions predates release-time
/// removal. A lock file is removed only through
/// `sweep_lock_file`'s race-safe check (flock-acquirable +
/// inode identity), so a live download's lock is never touched.
#[cfg(unix)]
pub fn sweep_stale_locks(dir: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt as _;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut swept = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let bytes = name.as_bytes();
        if bytes.len() < 9 || &bytes[bytes.len() - 9..] != b".ndm.lock" {
            continue;
        }
        if sweep_lock_file(&entry.path()) {
            swept += 1;
        }
    }
    swept
}

/// Non-Unix counterpart: conservative no-op. There the Drop impl
/// already unlinks the lock file, so litter can only come from a
/// crash — and on those platforms the exclusively-created file IS
/// the lock (the `claim_os_lock` fallback arm has no flock to
/// probe and no pid-liveness check to prove an existing file
/// stale), so deleting one from a sweep could remove a live
/// owner's lock. Leave crash litter rather than split a lock.
#[cfg(not(unix))]
pub fn sweep_stale_locks(_dir: &Path) -> usize {
    0
}

struct InFlightGuard {
    key: PathBuf,
    /// the held-open lock file (Unix: flocked; other platforms:
    /// created exclusively and deleted on drop)
    lock_file: Option<File>,
    lock_path: PathBuf,
}

impl InFlightGuard {
    fn register(out: &Path) -> Result<Self, String> {
        let key = canonical_key(out);
        let mut os_str = key.clone().into_os_string();
        os_str.push(".ndm.lock");
        let lock_path = PathBuf::from(os_str);
        // self-clean this folder's stale lock litter before
        // claiming (N2): every start sweeps the stale `.ndm.lock`
        // files left behind by processes that died mid-download
        // (Drop removes the file on every clean release — this is
        // the backstop for the paths Drop never runs on). Running BEFORE the claim is what makes sweeping
        // this download's own stale lock safe: it is removed
        // unlocked and then recreated fresh by the claim below,
        // while a live contender's lock is never touched (the
        // sweep can only delete locks it can flock). One read_dir
        // per start is the whole cost.
        if let Some(parent) = lock_path.parent() {
            let _ = sweep_stale_locks(parent);
        }
        {
            let mut registry = plock(&IN_FLIGHT);
            if registry.contains(&key) {
                return Err(format!(
                    "another download is already writing {}",
                    out.display()
                ));
            }
            registry.push(key.clone());
        }
        // cross-process half: claim the OS lock for this output.
        // Any failure rolls the in-process registration back, so a
        // rejected start leaves no trace in either registry.
        let claim = Self::claim_os_lock(&lock_path);
        match claim {
            Ok(f) => Ok(Self {
                key,
                lock_file: Some(f),
                lock_path,
            }),
            Err(e) => {
                plock(&IN_FLIGHT).retain(|p| p != &key);
                Err(e)
            }
        }
    }

    #[cfg(unix)]
    fn claim_os_lock(lock_path: &Path) -> Result<File, String> {
        // the lock file lives next to the output; its parent is
        // the canonical parent dir, which exists whenever the
        // download itself can be created. If it cannot be opened
        // at all (a read-only folder the download would fail in
        // anyway), fall back to the in-process guard alone rather
        // than inventing a new failure mode for a working path.
        let f = match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
        {
            Ok(f) => f,
            Err(_) => {
                return File::open("/dev/null").map_err(|e| e.to_string());
            }
        };
        if os_file_lock::try_acquire(&f) {
            Ok(f)
        } else {
            Err("another download is already writing this file".to_string())
        }
    }

    #[cfg(not(unix))]
    fn claim_os_lock(lock_path: &Path) -> Result<File, String> {
        // fallback where flock does not exist: an exclusively
        // created lock file carrying the owner's pid. An existing
        // file means another live process owns the path (a stale
        // file from a crashed owner blocks until removed — the
        // conservative direction: never splice two writers).
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                let _ = writeln!(f, "{}", std::process::id());
                Ok(f)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(
                "another download is already writing this file".to_string(),
            ),
            // a folder that cannot hold the lock file cannot hold
            // the download either; degrade to the process-local
            // guard instead of failing a start that used to work.
            // The placeholder is the null device, exactly the role
            // /dev/null plays in the Unix arm: opening the lock
            // path itself here could only succeed when the file
            // already existed (the AlreadyExists arm above owns
            // that case), so every genuine failure — a missing
            // parent (os error 3), access denied — became a hard
            // start error instead of the documented degradation.
            Err(_) => File::open("NUL").map_err(|e| e.to_string()),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        plock(&IN_FLIGHT).retain(|p| p != &self.key);
        // Unix: release the flock first (closing the held file),
        // then remove the lock file NOW — at release, not at the
        // next launch — through the sweep's race-safe check
        // (`sweep_lock_file`: fresh open, flock-acquirable, inode
        // identity). The fd must close first because the check's
        // own open would otherwise conflict with our still-held
        // flock. The old never-unlink rule guarded against
        // deleting a file a contender holds open on the same
        // inode; unconditional unlink would still be wrong, but
        // unlinking only while holding the flock ourselves,
        // after the identity re-check, is exactly the protocol
        // the sweep already proves. If another download claimed
        // the path in the gap, its live flock makes the check
        // skip and its file stays. A process killed mid-download
        // never runs Drop — the startup/register sweeps remain
        // the backstop for that litter. Other platforms: the
        // exclusive-create file is the lock, so it goes with the
        // guard, as before.
        #[cfg(unix)]
        {
            self.lock_file = None;
            let _ = sweep_lock_file(&self.lock_path);
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.lock_path);
        }
        let _ = &self.lock_file;
        let _ = &self.lock_path;
    }
}

pub fn start(
    url: &str,
    out: &Path,
    max_conn: usize,
    block_size: u64,
) -> Result<Arc<Download>, String> {
    start_impl(url, out, max_conn, max_conn, block_size, &Auth::default(), None)
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
    start_impl(url, out, max_conn, max_conn, block_size, &auth, None)
}

/// WS-D D2: start with MORE workers spawned than may be active
/// at once. The excess workers park at the connection gate until
/// Download::set_connection_limit raises the active count — a
/// video+audio pair starts each side with the full pool spawned
/// but only its pair share active, so when one side finishes the
/// survivor inherits the whole pool live instead of finishing
/// the larger stream on its original share while the finished
/// side's connections idle (audit-232 S4).
pub fn start_with_auth_limited(
    url: &str,
    out: &Path,
    spawn_conn: usize,
    active_conn: usize,
    block_size: u64,
    auth: Auth,
) -> Result<Arc<Download>, String> {
    start_impl(url, out, spawn_conn, active_conn, block_size, &auth, None)
}

/// Start with a probe the caller has JUST made for this exact URL
/// (Finding 4: every start used to probe twice — once by the caller
/// to describe the file, once again inside `start_impl` — two HEADs
/// and two verify GETs per start, seconds of latency on slow DNS).
/// Only a fresh probe of the same URL with no state change in
/// between may be passed; callers that probed earlier (a dialog the
/// user sat on) must pass `None` and let the start re-probe.
pub fn start_with_auth_preprobed(
    url: &str,
    out: &Path,
    max_conn: usize,
    block_size: u64,
    auth: Auth,
    probed: Option<Probe>,
) -> Result<Arc<Download>, String> {
    start_impl(url, out, max_conn, max_conn, block_size, &auth, probed)
}

fn start_impl(
    url: &str,
    out: &Path,
    max_conn: usize,
    active_conn: usize,
    block_size: u64,
    auth: &Auth,
    pre_probed: Option<Probe>,
) -> Result<Arc<Download>, String> {
    if block_size == 0 {
        return Err("block size must be positive".to_string());
    }
    // an absurd block size is clamped, not trusted (overflow bait)
    let block_size = block_size.min(64 * 1024 * 1024);
    let max_conn = max_conn.clamp(1, 64);
    // WS-D D2: how many of the spawned workers may be ACTIVE at
    // once (the rest park at the gate). Equals max_conn except
    // for a video+audio pair member, which starts at its pair
    // share so its sibling has a pool of its own; the survivor's
    // share is raised live via Download::set_connection_limit
    // when the sibling finishes.
    let active_conn = active_conn.clamp(1, max_conn);
    // one live writer per output path (finding #10): register
    // before any probing or file I/O so a collision fails fast,
    // and hand the guard to whichever thread settles this download
    let in_flight = InFlightGuard::register(out)?;
    let agent = build_agent_for(url);
    // F1: the job's cookie jar — inherited from the caller's
    // probe when there is one (its redirect chain may have set
    // the very cookie the file is gated behind), otherwise the
    // start probe below fills it and the workers share it
    let jar = pre_probed
        .as_ref()
        .map(|p| p.cookies.clone())
        .unwrap_or_default();
    let probed = match pre_probed {
        Some(p) => p,
        // F5: a start that was not already probed by the caller
        // runs the download probe (HEAD + tentative verdict),
        // not the dialog's full HEAD + verify-GET probe — but
        // ONLY when nothing is at stake: a tentative verdict is
        // acted on by preallocating and writing the target, so a
        // start over an EXISTING file or journal (a resume, a
        // pre-existing target a failed probe must leave
        // byte-identical) still pays for the full probe whose
        // verify GET can demote or veto before any byte moves.
        None => {
            if ftp::is_ftp_url(url) {
                ftp::probe_ftp(url, auth)?
            } else if out.exists() || state_path_for(out).exists() {
                probe_with(&probe_agent_for(url), url, auth, &jar)?
            } else {
                probe_for_download_with(&probe_agent_for(url), url, auth, &jar)?
            }
        }
    };
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
        if ftp::is_ftp_url(url) {
            return ftp::start_ftp_single_stream(url, out, probed, control, state_path, auth, in_flight);
        }
        return start_single_stream(url, out, probed, control, state_path, auth, in_flight);
    }

    // the journal records an FTP target's redacted address
    // (credentials never reach the .ndm.json), so the compare
    // runs against the same redacted form (ftp::journal_url)
    let journal = ftp::journal_url(url);
    let mut existing_state = std::fs::read(&state_path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<State>(&raw).ok())
        .filter(|s| {
            s.url == journal
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
    // B-4: partial prefixes are trusted exactly as far as they
    // are normalized — right length, within their block, zero
    // for sealed blocks. A hostile or torn journal loses only
    // its partial claims, never its sealed map.
    if let Some(s) = &mut existing_state {
        s.normalize_partial();
    }

    // A state whose validators cannot be compared (the server sends
    // no ETag/Last-Modified, or the save predates them) passes the
    // filter on size alone — and a same-size replacement file would
    // be spliced with the old blocks and reported Finished. When
    // neither validator pair is usable, prove the splice first:
    // byte-compare a sample of the completed blocks against fresh
    // range GETs — the first, the last and two spread between them
    // (all of them when there are fewer). Checking only the first
    // block, as this once did, lets a replacement that differs
    // anywhere later splice through. Anything but an exact match
    // on every sampled block discards the state and starts over.
    // F4: beyond the sample, the journal's done map is TRUSTED —
    // done ranges are never re-fetched on a resume — and the
    // sample itself (up to 8 sequential full-block GETs before
    // this change: up to 8 RTT and 8 blocks of pure start delay)
    // is now at most 4 blocks fetched in PARALLEL, one round trip.
    if let Some(s) = &existing_state {
        let validators_usable = (s.etag.is_some() && probed.etag.is_some())
            || (s.last_modified.is_some() && probed.last_modified.is_some());
        if !validators_usable {
            let done_idx: Vec<usize> = s
                .done
                .iter()
                .enumerate()
                .filter(|(_, &b)| b == 1)
                .map(|(i, _)| i)
                .collect();
            // B-4: partial prefixes get the same scepticism as
            // sealed blocks — a prefix that cannot be proven is
            // not skipped over, the whole state is discarded
            let partial_idx: Vec<usize> = s
                .done
                .iter()
                .enumerate()
                .filter(|(i, &b)| b == 0 && s.partial.get(*i).copied().unwrap_or(0) > 0)
                .map(|(i, _)| i)
                .collect();
            if !done_idx.is_empty() || !partial_idx.is_empty() {
                let last = done_idx.len().saturating_sub(1);
                let mut picks: Vec<usize> = Vec::new();
                if !done_idx.is_empty() {
                    for &at in &[0usize, last / 3, (2 * last) / 3, last] {
                        let idx = done_idx[at];
                        if !picks.contains(&idx) {
                            picks.push(idx);
                        }
                    }
                }
                // up to two partial prefixes (first + last),
                // verified over exactly their claimed length
                let mut partial_picks: Vec<(u64, u64)> = Vec::new();
                if !partial_idx.is_empty() {
                    let mut cand = vec![partial_idx[0]];
                    let tail = partial_idx[partial_idx.len() - 1];
                    if tail != partial_idx[0] {
                        cand.push(tail);
                    }
                    for b in cand {
                        let start = b as u64 * s.block_size;
                        partial_picks.push((start, start + s.partial[b]));
                    }
                }
                let bs = s.block_size;
                let total = s.total;
                let agent_ref = &agent;
                let jar_ref = &jar;
                let final_url_ref = &probed.final_url;
                let ignore_enc = probed.ignore_encoding_header;
                let all_match = std::thread::scope(|scope| {
                    let mut handles: Vec<_> = picks
                        .iter()
                        .map(|&b| {
                            let start = b as u64 * bs;
                            let end = ((b as u64 + 1) * bs).min(total);
                            scope.spawn(move || {
                                verify_block_bytes(
                                    agent_ref,
                                    final_url_ref,
                                    auth,
                                    url,
                                    jar_ref,
                                    out,
                                    start,
                                    end,
                                    ignore_enc,
                                )
                            })
                        })
                        .collect();
                    // B-4: the sampled partial prefixes verify
                    // over exactly their journaled length
                    handles.extend(partial_picks.iter().map(|&(start, end)| {
                        scope.spawn(move || {
                            verify_block_bytes(
                                agent_ref,
                                final_url_ref,
                                auth,
                                url,
                                jar_ref,
                                out,
                                start,
                                end,
                                ignore_enc,
                            )
                        })
                    }));
                    // a panicked verifier is a failed verification:
                    // the safe direction is the fresh start
                    handles
                        .into_iter()
                        .all(|h| h.join().unwrap_or(false))
                });
                if !all_match {
                    existing_state = None;
                }
            }
        }
    }

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
            url: journal.clone(),
            total: probed.total,
            block_size,
            etag: probed.etag.clone(),
            last_modified: probed.last_modified.clone(),
            done: vec![0u8; total_blocks],
            partial: vec![0u64; total_blocks],
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
        file.set_len(probed.total).map_err(|e| match disk_full_sentence(&e) {
            Some(s) => format!("{s} (cannot preallocate file: {e})"),
            None => format!("cannot preallocate file: {e}"),
        })?;
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
        active_limit: AtomicUsize::new(active_conn.min(total_blocks).max(1)),
        conn_cap: AtomicUsize::new(active_conn),
        worker_count: max_conn.max(1),
        pair_survivor: AtomicBool::new(false),
        active_workers: AtomicUsize::new(0),
        throttle_times: Mutex::new(Vec::new()),
        range_200_count: AtomicUsize::new(0),
        single_fallback: AtomicBool::new(false),
        fallback_used: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        auth_dead: AtomicBool::new(false),
        error: Mutex::new(None),
        control: control.clone(),
        block_state,
        status: Mutex::new(Status::Downloading),
        partial: (0..max_conn).map(|_| AtomicU64::new(0)).collect(),
        conn_bytes: (0..max_conn).map(|_| AtomicU64::new(0)).collect(),
        conn_rate_bps: (0..max_conn).map(|_| AtomicU64::new(0)).collect(),
        peak_conn_rate_bps: AtomicU64::new(0),
        rate_hist: Mutex::new(std::collections::VecDeque::new()),
        own_limit: AtomicU64::new(0),
        own_limit_state: Mutex::new(None),
        last_progress: Mutex::new(Instant::now()),
        requeue: Mutex::new(Vec::new()),
        byte_requeue: Mutex::new(Vec::new()),
        tail_splits: Mutex::new((0..total_blocks).map(|_| None).collect()),
        waiting_workers: AtomicUsize::new(0),
        last_split: Mutex::new(None),
        last_journal_error: Mutex::new(None),
        settled: AtomicBool::new(false),
        superseded: AtomicBool::new(false),
        conn_phase: (0..max_conn).map(|_| AtomicU8::new(0)).collect(),
        conn_block: (0..max_conn).map(|_| AtomicUsize::new(usize::MAX)).collect(),
        own_limit_gen: AtomicU64::new(0),
        own_limit_oldest_live: AtomicU64::new(0),
        ignore_encoding: AtomicBool::new(probed.ignore_encoding_header),
        last_data_sync: Mutex::new(Instant::now()),
        bytes_since_sync: AtomicU64::new(0),
        auth: auth.clone(),
        jar,
    });

    let download = Arc::new(Download {
        control,
        shared: shared.clone(),
        total: probed.total,
        resumable: true,
        started: Instant::now(),
    });

    // FTP jobs run the parallel FTP worker loop against the same
    // Shared (src/ftp.rs); HTTP jobs run the ureq worker below.
    // The governor, supervisor and status tracker after this are
    // shared verbatim. Parsed before any thread spawns so a parse
    // failure (impossible after a successful probe) cannot strand
    // the governor.
    let ftp_target = if ftp::is_ftp_url(url) {
        Some(ftp::parse_target(url, auth)?)
    } else {
        None
    };
    {
        let s = shared.clone();
        std::thread::spawn(move || governor(s, max_conn, total_blocks));
    }
    let mut handles = Vec::new();
    for id in 0..max_conn.max(1) {
        let s = shared.clone();
        let f = file.clone();
        if let Some(t) = ftp_target.clone() {
            handles.push(std::thread::spawn(move || {
                ftp::ftp_worker(id, t, f, s, total_blocks)
            }));
            continue;
        }
        let u = probed.final_url.clone();
        let a = agent.clone();
        handles.push(std::thread::spawn(move || worker(id, a, u, f, s, total_blocks)));
    }
    // supervisor: waits for workers, then settles the final status
    {
        let s = shared.clone();
        std::thread::spawn(move || {
            // Bounded settlement (W1 F13 / C-2): joining the workers
            // unconditionally wedged the whole download when ONE
            // worker parked on a server-held connection — the
            // transfer agent's read timeout never surfaced for that
            // call, so the join never returned, the final journal
            // save never landed, `settled` never fired and the
            // InFlightGuard leaked (the path stayed undownloadable
            // until process exit). The supervisor now polls the
            // handles: while the pool is healthy it waits as
            // before, but once a terminal intent exists (failure
            // declared by the stall policy — which the supervisor
            // also evaluates itself — a cancel, or completion) it
            // grants the workers SETTLE_GRACE to wind down, then
            // settles WITHOUT the stragglers. Detached workers are
            // made harmless first: `superseded` is checked before
            // every write, block mark and journal save, so a
            // late-waking zombie can never write into a file this
            // settlement (or a superseding download) now owns.
            let mut intent_since: Option<Instant> = None;
            loop {
                if handles.iter().all(|h| h.is_finished()) {
                    for h in handles {
                        let _ = h.join();
                    }
                    break;
                }
                let intent = s.failed.load(Ordering::Relaxed)
                    || s.control.cancelled.load(Ordering::Relaxed)
                    || s.done_count.load(Ordering::Relaxed) >= total_blocks;
                if intent {
                    let since = *intent_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= SETTLE_GRACE {
                        s.superseded.store(true, Ordering::SeqCst);
                        break;
                    }
                } else {
                    intent_since = None;
                    // the pool may be parked with nobody left to
                    // evaluate the failure policy (the F13 shape:
                    // every live worker stuck inside one call) —
                    // the supervisor evaluates it itself
                    fail_if_stalled(&s, total_blocks);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // Settle from the atomics alone. For the ERROR /
            // CANCELLED / PAUSED outcomes the terminal status is
            // published FIRST: the sync and the state save below can
            // be slow (an 800 MB file on a busy disk) or can fail,
            // and they must never delay — or, if this thread dies,
            // prevent — the terminal state the rest of the app is
            // waiting on (F-15: the entry sat non-terminal forever).
            // The FINISHED outcome is the one exception, because it
            // also DELETES the state file: publishing Finished before
            // that removal lands lets an observer see "finished"
            // while the state file is still on disk. On Linux the
            // gap is invisible; on Windows, where flush + rename +
            // delete is slower and a just-closed file can be briefly
            // locked, the gap is wide enough to observe (a lib test
            // asserting the file is gone right after Finished failed
            // on windows-latest exactly this way). So the finished
            // path finalizes its files first and publishes last —
            // every call in it ignores its own errors, so the status
            // cannot be wedged by them either.
            let finished = !s.failed.load(Ordering::Relaxed)
                && s.done_count.load(Ordering::Relaxed) >= total_blocks;
            if finished {
                // byte-complete is Finished even if a cancel landed in
                // the same instant — the file on disk IS the full file
                for b in &s.block_state {
                    b.store(1, Ordering::Relaxed);
                }
                let _ = file.sync_all();
                save_state(&s, true, Some(&*file));
                remove_file_bounded(&s.state_path);
                set_status(&s, Status::Finished);
            } else {
                if s.failed.load(Ordering::Relaxed) {
                    set_status(&s, Status::Error);
                } else if s.control.cancelled.load(Ordering::Relaxed) {
                    set_status(&s, Status::Cancelled);
                } else if s.control.paused.load(Ordering::Relaxed) {
                    set_status(&s, Status::Paused);
                } else {
                    set_status(&s, Status::Error);
                    let mut err = plock(&s.error);
                    if err.is_none() {
                        *err = Some("download stopped before completion".to_string());
                    }
                }
                let _ = file.sync_all();
                save_state(&s, true, Some(&*file));
            }
            // Release the path BEFORE publishing settled: the
            // contract is_settled() waiters rely on (core's
            // restart/removal gates) is "settled ⇒ this download
            // will never write again AND its output path is
            // free". The old order stored settled first, leaving
            // a hairline where a waiter could start on the path
            // and be rejected by this guard — invisible locally,
            // hit by the B-4 rig test on a loaded CI runner
            // (CI #75). The last write this download will ever
            // make has landed above, so removal paths waiting on
            // is_settled may still sweep the state file without
            // racing this thread (R4).
            drop(in_flight);
            s.settled.store(true, Ordering::SeqCst);
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
            let mut st = plock(&s.status);
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
    // a finished download leaves no resume journal behind (W1 F7):
    // a stale journal claiming sealed blocks for a file that just
    // finished empty is litter today and a landmine for any future
    // path that trusts it without the exact size guard
    remove_file_bounded(&state_path);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            url: String::new(),
            total: 0,
            block_size: 1,
            etag: None,
            last_modified: None,
            done: vec![1u8; 1],
            partial: vec![0u64; 1],
        }),
        state_path,
        next_block: AtomicUsize::new(0),
        done_count: AtomicUsize::new(1),
        fetched_session: AtomicU64::new(0),
        since_save: AtomicU64::new(0),
        last_save: Mutex::new(Instant::now()),
        active_limit: AtomicUsize::new(1),
        conn_cap: AtomicUsize::new(1),
        worker_count: 1,
        pair_survivor: AtomicBool::new(false),
        active_workers: AtomicUsize::new(0),
        throttle_times: Mutex::new(Vec::new()),
        range_200_count: AtomicUsize::new(0),
        single_fallback: AtomicBool::new(false),
        fallback_used: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        auth_dead: AtomicBool::new(false),
        error: Mutex::new(None),
        control: control.clone(),
        block_state: vec![AtomicU8::new(1)],
        status: Mutex::new(Status::Finished),
        partial: vec![AtomicU64::new(0)],
        conn_bytes: vec![AtomicU64::new(0)],
        conn_rate_bps: vec![AtomicU64::new(0)],
        peak_conn_rate_bps: AtomicU64::new(0),
        rate_hist: Mutex::new(std::collections::VecDeque::new()),
        own_limit: AtomicU64::new(0),
        own_limit_state: Mutex::new(None),
        last_progress: Mutex::new(Instant::now()),
        requeue: Mutex::new(Vec::new()),
        byte_requeue: Mutex::new(Vec::new()),
        tail_splits: Mutex::new(vec![None]),
        waiting_workers: AtomicUsize::new(0),
        last_split: Mutex::new(None),
        last_journal_error: Mutex::new(None),
        settled: AtomicBool::new(true),
        superseded: AtomicBool::new(false),
        conn_phase: vec![AtomicU8::new(0)],
        conn_block: vec![AtomicUsize::new(usize::MAX)],
        own_limit_gen: AtomicU64::new(0),
        own_limit_oldest_live: AtomicU64::new(0),
        ignore_encoding: AtomicBool::new(false),
        last_data_sync: Mutex::new(Instant::now()),
        bytes_since_sync: AtomicU64::new(0),
        auth: Auth::default(),
        jar: CookieJar::new(),
    });
    Arc::new(Download {
        control,
        shared,
        total: 0,
        resumable: false,
        started: Instant::now(),
    })
}

/// Whether a single-stream attempt's error is definitive — the
/// server has given an answer retrying cannot change (the
/// Content-Encoding veto, a 404/410) — mirroring the ranged path's
/// BlockError::Permanent classification (W1 F14).
fn single_error_is_permanent(e: &str) -> bool {
    e.contains("Content-Encoding")
        || e.contains("status code 404")
        || e.contains("status code 410")
}

/// Newly written bytes that reset the single-stream stall clock:
/// smaller trickles do not count as progress, or a server sending
/// a byte just inside every read timeout would run forever.
const STALL_PROGRESS_BYTES: u64 = 256 * 1024;

/// Record single-stream progress against the stall clock: the
/// clock resets only once STALL_PROGRESS_BYTES of new bytes have
/// been written since the watermark. Returns true when it reset.
fn note_stall_progress(pos: u64, watermark: &mut u64, clock: &mut Instant) -> bool {
    if pos.saturating_sub(*watermark) < STALL_PROGRESS_BYTES {
        return false;
    }
    *watermark = pos;
    *clock = Instant::now();
    true
}

fn start_single_stream(
    url: &str,
    out: &Path,
    probed: Probe,
    control: Arc<Control>,
    state_path: PathBuf,
    auth: &Auth,
    in_flight: InFlightGuard,
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
            partial: Vec::new(),
        }),
        state_path,
        next_block: AtomicUsize::new(0),
        done_count: AtomicUsize::new(0),
        fetched_session: AtomicU64::new(0),
        since_save: AtomicU64::new(0),
        last_save: Mutex::new(Instant::now()),
        active_limit: AtomicUsize::new(1),
        conn_cap: AtomicUsize::new(1),
        worker_count: 1,
        pair_survivor: AtomicBool::new(false),
        active_workers: AtomicUsize::new(1),
        throttle_times: Mutex::new(Vec::new()),
        range_200_count: AtomicUsize::new(0),
        single_fallback: AtomicBool::new(false),
        fallback_used: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        auth_dead: AtomicBool::new(false),
        error: Mutex::new(None),
        control: control.clone(),
        block_state: vec![AtomicU8::new(0)],
        status: Mutex::new(Status::Downloading),
        partial: vec![AtomicU64::new(0)],
        conn_bytes: vec![AtomicU64::new(0)],
        conn_rate_bps: vec![AtomicU64::new(0)],
        peak_conn_rate_bps: AtomicU64::new(0),
        rate_hist: Mutex::new(std::collections::VecDeque::new()),
        own_limit: AtomicU64::new(0),
        own_limit_state: Mutex::new(None),
        last_progress: Mutex::new(Instant::now()),
        requeue: Mutex::new(Vec::new()),
        byte_requeue: Mutex::new(Vec::new()),
        tail_splits: Mutex::new(vec![None]),
        waiting_workers: AtomicUsize::new(0),
        last_split: Mutex::new(None),
        last_journal_error: Mutex::new(None),
        settled: AtomicBool::new(false),
        superseded: AtomicBool::new(false),
        conn_phase: vec![AtomicU8::new(0)],
        conn_block: vec![AtomicUsize::new(usize::MAX)],
        own_limit_gen: AtomicU64::new(0),
        own_limit_oldest_live: AtomicU64::new(0),
        ignore_encoding: AtomicBool::new(probed.ignore_encoding_header),
        last_data_sync: Mutex::new(Instant::now()),
        bytes_since_sync: AtomicU64::new(0),
        auth: auth.clone(),
        jar: probed.cookies.clone(),
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
        // M-1 (W1 F1): when a resume journal exists for this path,
        // the demotion to single-stream must NOT File::create over
        // the preallocated partial — a plain body that then failed
        // (truncated, reset, over-long) destroyed every sealed
        // block while the journal still claimed them. Spool the
        // replacement body to a temp file instead and only rename
        // it over the partial once it is verified complete; on any
        // failure the partial and its journal stay exactly as they
        // were, and a later start can still resume them.
        let spool = shared.state_path.exists() && out.exists();
        let spool_path: PathBuf = {
            let mut s = out.clone().into_os_string();
            s.push(".ndm.spool");
            PathBuf::from(s)
        };
        let attempt_once = || -> Result<(), String> {
            shared.active_workers.store(1, Ordering::Relaxed);
            if let Some(p) = shared.conn_phase.first() {
                p.store(1, Ordering::Relaxed);
            }
            let resp = jar_call(
                &agent,
                &shared.jar,
                &auth,
                &url,
                "GET",
                &url,
                &[("Accept-Encoding", "identity")],
            )
            .map_err(|e| {
                // WS-A: a single-stream job's 403/410 is the same
                // dead-address signal the ranged path records
                let msg = format!("http: {e}");
                if error_is_auth_dead(&msg) {
                    note_auth_dead(&shared);
                }
                msg
            })?;
            if !shared.ignore_encoding.load(Ordering::Relaxed) {
                if let Some(enc) = response_encoding(&resp) {
                    return Err(format!(
                        "server answered with Content-Encoding: {enc} despite an identity request"
                    ));
                }
            }
            if let Some(p) = shared.conn_phase.first() {
                p.store(2, Ordering::Relaxed);
            }
            let mut reader = resp.into_reader();
            let write_path: &Path = if spool { &spool_path } else { &out };
            let f = File::create(write_path).map_err(|e| match disk_full_sentence(&e) {
                Some(s) => format!("{s} (create: {e})"),
                None => format!("create: {e}"),
            })?;
            let mut buf = vec![0u8; 512 * 1024];
            let mut pos = 0u64;
            // bytes read from the stream but not yet written: a
            // single stream cannot DISCARD a read chunk the way the
            // ranged engine can (the stream cannot re-serve it —
            // dropping it would leave a hole in the file), so a
            // chunk caught by a halt is HELD in memory instead and
            // written only after the resume, and dropped only on
            // cancel (whose file is discarded anyway). The on-disk
            // byte count freezes the moment the halt lands, with
            // stream and file still in sync (battery19 t1/t6).
            let mut held = 0usize;
            let mut stall_watermark = 0u64;
            let mut stall_clock = Instant::now();
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
                    // a pause waits on the user, not the network:
                    // the stall clock restarts from the resume
                    stall_watermark = pos;
                    stall_clock = Instant::now();
                }
                if stall_clock.elapsed() > STALL_LIMIT {
                    return Err(format!(
                        "no progress for {}s — the server stopped sending data",
                        STALL_LIMIT.as_secs()
                    ));
                }
                if held == 0 {
                    // pacing reserves BEFORE the read, exactly as
                    // on the ranged path (M-13)
                    let grant = pace_grant_size(&shared, buf.len());
                    if !pace_before_read(&shared, grant as u64) {
                        continue; // a halt landed; the checks above settle it
                    }
                    let n = reader
                        .read(&mut buf[..grant])
                        .map_err(|e| {
                            pace_refund_both(&shared, grant as u64);
                            format!("read: {e}")
                        })?;
                    if (n as u64) < grant as u64 {
                        pace_refund_both(&shared, grant as u64 - n as u64);
                    }
                    if n == 0 {
                        break;
                    }
                    held = n;
                    // the halt landed while this read was in
                    // flight: hold the chunk and let the flag
                    // checks above settle it before any write
                    if shared.control.cancelled.load(Ordering::Relaxed)
                        || shared.control.paused.load(Ordering::Relaxed)
                    {
                        continue;
                    }
                }
                let n = held;
                held = 0;
                write_at(&f, &buf[..n], pos).map_err(|e| plain_write_error(&e))?;
                pos += n as u64;
                if note_stall_progress(pos, &mut stall_watermark, &mut stall_clock) {
                    *plock(&shared.last_progress) = stall_clock;
                }
                // the server is sending MORE than the probed size (a
                // HEAD/GET disagreement): stop now instead of writing
                // the whole over-long body and reporting it backwards
                if expected_total > 0 && pos > expected_total {
                    if spool {
                        let _ = std::fs::remove_file(&spool_path);
                    }
                    return Err(format!(
                        "server sent more data than the probed size ({expected_total} bytes)"
                    ));
                }
                shared.fetched_session.fetch_add(n as u64, Ordering::Relaxed);
                if let Some(cb) = shared.conn_bytes.first() {
                    cb.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            // a clean early EOF must not pose as a finished file
            if expected_total > 0 && pos != expected_total {
                if spool {
                    let _ = std::fs::remove_file(&spool_path);
                }
                return Err(format!(
                    "short download: got {pos} of {expected_total} bytes"
                ));
            }
            if spool {
                // the replacement body is verified complete: only
                // NOW may it replace the partial (M-1). The handle
                // closes first — renaming over an open file fails
                // on Windows.
                drop(f);
                if let Err(e) = std::fs::rename(&spool_path, &out) {
                    let _ = std::fs::remove_file(&spool_path);
                    return Err(format!("could not replace the partial file: {e}"));
                }
            }
            Ok(())
        };
        // A non-range download cannot resume, so a dropped
        // connection used to be an instant hard error even at 99%.
        // Retry the whole transfer (restarting from byte 0) up to 5
        // attempts with 1/2/4/8 s backoff; pause waits inside the
        // attempt, cancel breaks out at once.
        let mut attempt = 0u32;
        let result: Result<(), String> = loop {
            attempt += 1;
            // a restart re-fetches from byte 0: session counters
            // must not double-count the earlier attempt's bytes
            shared.fetched_session.store(0, Ordering::Relaxed);
            if let Some(cb) = shared.conn_bytes.first() {
                cb.store(0, Ordering::Relaxed);
            }
            match attempt_once() {
                Ok(()) => break Ok(()),
                Err(e) => {
                    if spool {
                        let _ = std::fs::remove_file(&spool_path);
                    }
                    if shared.control.cancelled.load(Ordering::Relaxed) {
                        set_status(&shared, Status::Cancelled);
                        break Ok(());
                    }
                    // m-5 (W1 F14): definitive errors — the
                    // Content-Encoding veto, 404/410 — fail fast
                    // like the ranged path's Permanent class, not
                    // after 5 retries and ~15 s of backoff spent
                    // re-asking a question with a settled answer
                    if single_error_is_permanent(&e) || attempt >= 5 {
                        break Err(e);
                    }
                    let deadline = Instant::now() + Duration::from_secs(1u64 << (attempt - 1));
                    while Instant::now() < deadline {
                        if shared.control.cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        };
        match result {
            Ok(()) => {
                shared.active_workers.store(0, Ordering::Relaxed);
                if shared.control.cancelled.load(Ordering::Relaxed) {
                    if spool {
                        let _ = std::fs::remove_file(&spool_path);
                    }
                } else {
                    shared.done_count.store(1, Ordering::Relaxed);
                    if let Some(b) = shared.block_state.first() {
                        b.store(1, Ordering::Relaxed);
                    }
                    // a finished download leaves no resume journal
                    // behind — on this path a journal exists only
                    // when the download was a demoted resume (M-1),
                    // and its blocks describe the file the spool
                    // just replaced, not this one
                    remove_file_bounded(&shared.state_path);
                    set_status(&shared, Status::Finished);
                }
            }
            Err(e) => {
                shared.active_workers.store(0, Ordering::Relaxed);
                let mut err = plock(&shared.error);
                *err = Some(e);
                shared.failed.store(true, Ordering::Relaxed);
                set_status(&shared, Status::Error);
            }
        }
        // this path never writes a resume-state file, but the
        // settled contract is uniform: the guard drops FIRST,
        // so settled ⇒ path released (see the supervisor's
        // settle; CI #75), and removal paths may proceed (R4)
        drop(in_flight);
        shared.settled.store(true, Ordering::SeqCst);
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
    // H-2: the journal records an FTP target under its REDACTED
    // address (journal_url — credentials never touch disk), so
    // the comparison must normalize the caller's raw URL the
    // same way; comparing raw-to-redacted missed every
    // credentialed-FTP journal and reported resumable downloads
    // as unresumable after a restart. Non-FTP URLs pass through
    // journal_url unchanged.
    if s.url != ftp::journal_url(url) || !out.exists() {
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
        // B-4: bytes on disk include unsealed blocks' written
        // prefixes (partial_bytes clamps every claim to its
        // block, so an unnormalized journal cannot inflate this)
        done_bytes: s.done_bytes() + s.partial_bytes(),
        blocks: s.done.clone(),
    })
}

/// Re-point a resume journal at a new address for the same file —
/// the engine half of Refresh Download Address. The journal FILE is
/// keyed to the output path, but the `State` inside records the URL
/// it was created for, and both consumers (`peek_state` above and
/// the resume filter in `start_impl`) refuse a journal whose URL
/// differs from the one being started. Rewriting that one field —
/// and nothing else — is what lets a refreshed entry keep its
/// sealed blocks: the next start runs the journal through the
/// ordinary resume verification against the NEW address (total,
/// ETag / Last-Modified, sampled sealed-block bytes) and discards
/// it, restarting from zero, on any mismatch. This function makes
/// no sameness judgement of its own.
///
/// Returns false — journal untouched — when there is no journal,
/// when it does not belong to `old_url`, or when it fails the same
/// poison guards `peek_state` applies: a journal too broken to
/// render is too broken to re-point.
pub fn rebind_state_url(out: &Path, old_url: &str, new_url: &str) -> bool {
    let path = state_path_for(out);
    let raw = match std::fs::read(&path) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let mut s: State = match serde_json::from_slice(&raw) {
        Ok(s) => s,
        Err(_) => return false,
    };
    // H-2: both sides are normalized through journal_url — the
    // journal holds the redacted FTP address, and the rewritten
    // journal must hold the NEW address in that same redacted
    // form, or the resume filter and peek_state would refuse it
    // on the very next start. Non-FTP URLs are unchanged by it.
    if s.url != ftp::journal_url(old_url)
        || s.block_size == 0
        || s.block_size > 64 * 1024 * 1024
        || s.done.len() != s.total.div_ceil(s.block_size) as usize
        || s.done.iter().any(|&b| b > 1)
        || !out.exists()
        || out.metadata().map(|m| m.len()).unwrap_or(0) != s.total
    {
        return false;
    }
    s.url = ftp::journal_url(new_url);
    // write-then-rename, exactly as save_state does it — the
    // on-disk journal is never torn. This write is one-shot and
    // user-initiated (not the periodic hot path), so it takes the
    // durable form: journal synced, renamed, directory synced.
    let tmp = path.with_extension("json.tmp");
    let written: io::Result<()> = (|| {
        {
            let f = File::create(&tmp)?;
            serde_json::to_writer(&f, &s).map_err(io::Error::from)?;
            f.sync_all()?;
        } // handle closed before the rename (Windows sharing rules)
        std::fs::rename(&tmp, &path)?;
        if let Some(dir) = path.parent() {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    })();
    match written {
        Ok(()) => true,
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            false
        }
    }
}

fn legacy_state_path_for(out: &Path) -> PathBuf {
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

pub fn state_path_for(out: &Path) -> PathBuf {
    let legacy = legacy_state_path_for(out);
    // a journal an earlier version already wrote keeps its name:
    // resume compatibility is checked before any renaming applies
    if legacy.exists() {
        return legacy;
    }
    let legacy_name = legacy
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if legacy_name.len() <= 255 {
        return legacy;
    }
    // Finding #8b: "<name>.ndm.json" pushes output names of ~247+
    // bytes past the 255-byte filesystem limit, so those downloads
    // could never journal at all (every save failed, silently).
    // Derive a bounded name instead: as much of the original stem
    // as fits, plus a short stable hash of the full journal name so
    // two different long names that share a prefix still get
    // different journals. ~82 Bengali characters (3 bytes each)
    // land here.
    let mut hasher = Sha256::new();
    hasher.update(legacy_name.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let suffix = format!(".{}.ndm.json", &hash[..12]);
    // 4 bytes of the 255 stay in reserve: save_state's scratch name
    // replaces the ".json" tail with ".json.tmp" (+4), and it must
    // fit the same limit or the save fails exactly like before
    let budget = (255usize - 4).saturating_sub(suffix.len());
    let mut end = legacy_name.len().min(budget);
    while end > 0 && !legacy_name.is_char_boundary(end) {
        end -= 1;
    }
    out.with_file_name(format!("{}{}", &legacy_name[..end], suffix))
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
    apply_proxy_for(ureq::AgentBuilder::new()
        .redirect_auth_headers(ureq::RedirectAuthHeaders::Never), url)
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

/// Remove `<!-- ... -->` comments: links inside them are not live
/// links, and a commented-out example must not be crawled.
fn strip_html_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The document's `<base href>`, resolved against the page URL, if
/// it declares one; otherwise the page URL itself. Relative links
/// resolve against this — exactly as a browser resolves them.
fn document_base(html: &str, page_url: &str) -> String {
    let cleaned = strip_html_comments(html);
    let lower = cleaned.to_ascii_lowercase();
    if let Some(pos) = lower.find("<base") {
        let end = lower[pos..]
            .find('>')
            .map(|e| pos + e)
            .unwrap_or(cleaned.len());
        if let Some(href) = find_attr(&cleaned[pos..end], "href") {
            let href = decode_entities(href.trim());
            if let Some(abs) = resolve_url(page_url, &href) {
                return abs;
            }
        }
    }
    page_url.to_string()
}

/// Extract (raw link, label) pairs from page HTML.
fn extract_links(html: &str) -> Vec<(String, String)> {
    let cleaned = strip_html_comments(html);
    let html = cleaned.as_str();
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
    // a query-only reference keeps the page's own path and replaces
    // only its query (?page=2 on /list keeps /list, not its folder)
    if let Some(q) = href.strip_prefix('?') {
        let base_noq = base.split(['?', '#']).next().unwrap_or(base);
        return Some(format!("{base_noq}?{q}"));
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

/// Extensions a query-string filename may legitimately carry —
/// mirrors the extension's FILE_EXTENSIONS list, so a name the
/// browser side would accept is accepted here too.
const QUERY_NAME_EXTS: &[&str] = &[
    "zip", "rar", "7z", "tar", "gz", "gzip", "bz2", "xz", "zst", "tgz", "tbz2", "txz", "cab",
    "ace", "arj", "lzh", "sit", "sitx", "exe", "msi", "msix", "msixbundle", "deb", "rpm",
    "dmg", "pkg", "iso", "img", "apk", "bin", "run", "appimage", "flatpakref", "msu", "pdf",
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "pps", "rtf", "epub", "mobi", "tif", "tiff",
    "mp3", "mp4", "mkv", "avi", "mov", "webm", "flac", "aac", "ogg", "ogv", "m4a", "m4b",
    "opus", "wav", "wma", "flv", "wmv", "3gp", "m4v", "ts", "mpg", "mpeg", "mpa", "mpe",
    "asf", "rm", "rmvb", "aif", "torrent", "jar", "war", "whl", "vmdk", "qcow2", "vdi",
];

/// The filename a URL carries in its query string — download
/// endpoints like `/dl?file=report.pdf` whose path tail is a bare
/// handler name. Mirrors the extension's `filenameFromUrl`: a
/// parameter whose key mentions file/name/attachment/download and
/// whose value ends in a known file extension. Returns the cleaned
/// name, or None when the query carries nothing usable.
pub fn query_file_name(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next().unwrap_or("");
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let key = percent_decode(&key.replace('+', " ")).to_ascii_lowercase();
        if !(key.contains("file")
            || key.contains("name")
            || key.contains("attachment")
            || key.contains("download"))
        {
            continue;
        }
        let decoded = percent_decode(&value.replace('+', " "));
        let tail = decoded
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .trim();
        let Some(dot) = tail.rfind('.') else { continue };
        if dot == 0 {
            continue;
        }
        let ext = tail[dot + 1..].to_ascii_lowercase();
        if !QUERY_NAME_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let cleaned = sanitize_suggested_name(tail);
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    None
}

/// Cap a file name at the 255-byte filesystem limit, truncating the
/// stem on a UTF-8 boundary and keeping the extension — the same
/// treatment the video path gives its titles (F-6). A name already
/// within the limit is returned untouched.
pub fn cap_file_name(name: &str) -> String {
    if name.len() <= 255 {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let budget = 255usize.saturating_sub(ext.len());
    let mut end = stem.len().min(budget);
    while end > 0 && !stem.is_char_boundary(end) {
        end -= 1;
    }
    let capped = format!("{}{}", &stem[..end], ext);
    if capped.len() <= 255 {
        return capped;
    }
    // the "extension" alone exceeds the limit (a hostile or
    // pathological name like `a.<300 x's>`): the stem budget
    // saturated at 0 and the extension passed through whole, so
    // the create still failed with ENAMETOOLONG downstream.
    // Hard-cap the whole name on a char boundary — a truncated
    // extension beats an uncreatable file.
    let mut end = 255;
    while end > 0 && !capped.is_char_boundary(end) {
        end -= 1;
    }
    capped[..end].to_string()
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
    // F-5: a URL-derived name gets the SAME full treatment as a
    // Content-Disposition name — control characters stripped, the
    // remaining Windows-hostile characters (`:` `*` `?`) replaced,
    // leading/trailing dots and spaces trimmed, DOS device names
    // defused — not the weaker local cleanup above (which stays as
    // a first pass for the extra shell metacharacters it covers).
    let sanitized = sanitize_suggested_name(&cleaned);
    let name = if sanitized.is_empty() {
        "index".to_string()
    } else {
        sanitized
    };
    // A bare handler path (/dl, /getfile, /) carries no usable
    // name — but the query string often carries the real one
    // (/dl?file=report.pdf). A path name WITH an extension always
    // wins; the query is only consulted when the path has none.
    if !name.contains('.') {
        if let Some(q) = query_file_name(url) {
            return q;
        }
    }
    name
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
        if response_encoding(&resp).is_none() {
            if let Some(len) = resp
                .header("Content-Length")
                .and_then(|v| v.parse::<u64>().ok())
            {
                if len > 0 {
                    return Some(len);
                }
            }
        }
    }
    // fall back to a 1-byte range request and read the total from Content-Range
    if let Ok(resp) = agent.get(url).set("Range", "bytes=0-0").set("Accept-Encoding", "identity").call() {
        if response_encoding(&resp).is_some() {
            return None;
        }
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
    if let Some(enc) = response_encoding(&resp) {
        return Err(format!(
            "the page was served with Content-Encoding: {enc} despite an identity request"
        ));
    }
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

    let base = document_base(&html, &final_url);
    let mut seen = std::collections::HashSet::new();
    let mut items: Vec<GrabItem> = Vec::new();
    for (raw, label) in extract_links(&html) {
        let Some(resolved) = resolve_url(&base, &raw) else {
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
/// Judgment bound on the whole crawl: the per-page caps alone
/// allow ~60 pages × 25 s, so the crawl as a whole stops at
/// 600 s and returns what it gathered, as the probe phase does.
const GRAB_CRAWL_LIMIT: Duration = Duration::from_secs(600);

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

/// The outcome of fetching one crawled URL: an HTML page to scan,
/// or a file the URL itself serves. Extensionless download
/// endpoints (/download?id=4831, /getfile) are the standard shape
/// for served files, and classifying purely by URL path — before
/// any response exists — fetched the binary, scanned it for links
/// and silently discarded the main file the user came for.
enum PageFetch {
    Html { final_url: String, html: String },
    File {
        final_url: String,
        /// extension derived from the response's Content-Type, for
        /// the project's filters when the URL itself carries none
        ctype_ext: String,
        /// Content-Disposition filename (sanitised), when offered
        name: Option<String>,
    },
}

/// Map a response Content-Type to the extension the project's
/// include/exclude filters should judge it by.
fn content_type_ext(ctype: &str) -> String {
    match ctype {
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/pdf" => "pdf",
        "application/octet-stream" => "bin",
        "application/x-msdownload" => "exe",
        "application/vnd.android.package-archive" => "apk",
        "application/x-iso9660-image" => "iso",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/x-tar" => "tar",
        "application/gzip" | "application/x-gzip" => "gz",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "video/mp4" => "mp4",
        "video/x-matroska" => "mkv",
        "video/webm" => "webm",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "",
    }
    .to_string()
}

fn fetch_page(agent: &ureq::Agent, url: &str) -> Option<PageFetch> {
    let resp = agent.get(url).set("Accept-Encoding", "identity").call().ok()?;
    if response_encoding(&resp).is_some() {
        return None;
    }
    let final_url = resp.get_url().to_string();
    let ctype = resp
        .header("Content-Type")
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let attachment = resp
        .header("Content-Disposition")
        .map(|d| {
            d.trim_start()
                .to_ascii_lowercase()
                .starts_with("attachment")
        })
        .unwrap_or(false);
    let is_html =
        ctype.is_empty() || ctype == "text/html" || ctype == "application/xhtml+xml";
    if attachment || !is_html {
        let name = extract_suggested_name(&resp, url);
        return Some(PageFetch::File {
            final_url,
            ctype_ext: content_type_ext(&ctype),
            name,
        });
    }
    let mut body = Vec::new();
    {
        use std::io::Read as _;
        resp.into_reader()
            .take(8 * 1024 * 1024)
            .read_to_end(&mut body)
            .ok()?;
    }
    Some(PageFetch::Html {
        final_url,
        html: String::from_utf8_lossy(&body).to_string(),
    })
}

/// Apply the project's file filters and collect one file — shared
/// by discovered links and by crawled URLs that turn out to serve
/// a file themselves, so both paths judge a file identically.
#[allow(clippy::too_many_arguments)]
fn grab_collect(
    items: &mut Vec<GrabItem>,
    seen_files: &mut std::collections::HashSet<String>,
    opts: &GrabOptions,
    include: &[String],
    exclude: &[String],
    start_host: &str,
    url: &str,
    label: String,
    ext: &str,
    name: String,
) {
    if opts.same_site_files_only && url_host(url) != start_host {
        return;
    }
    if !include.is_empty() && !include.iter().any(|e| e == ext) {
        return;
    }
    if exclude.iter().any(|e| e == ext) {
        return;
    }
    if seen_files.insert(url.to_string()) {
        items.push(GrabItem {
            name,
            url: url.to_string(),
            label,
            size: None,
        });
    }
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
    // BFS frontier with DUAL level counters: levels spent inside
    // the start site and levels spent outside it are counted
    // separately along each path — a one-hop detour through an
    // external page must not consume the same-site budget
    let mut queue: std::collections::VecDeque<(String, u32, u32)> =
        std::collections::VecDeque::new();
    queue.push_back((start_url.to_string(), 0, 0));
    visited.insert(start_url.to_string());
    let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut items: Vec<GrabItem> = Vec::new();
    let mut title = String::new();
    let mut title_taken = false;
    let mut pages_visited = 0usize;
    let crawl_deadline = Instant::now() + GRAB_CRAWL_LIMIT;

    while let Some((page_url, d_same, d_other)) = queue.pop_front() {
        if pages_visited >= MAX_GRAB_PAGES || Instant::now() >= crawl_deadline {
            break;
        }
        let (final_url, html) = match fetch_page(&agent, &page_url) {
            Some(PageFetch::Html { final_url, html }) => (final_url, html),
            Some(PageFetch::File {
                final_url,
                ctype_ext,
                name,
            }) => {
                // the crawled URL itself serves a file: collect it
                // under the same filters as a discovered link
                pages_visited += 1;
                let url_ext_now = url_ext(&final_url);
                let ext = if url_ext_now.is_empty() {
                    ctype_ext.as_str()
                } else {
                    url_ext_now.as_str()
                };
                let name = name.unwrap_or_else(|| file_name_of(&final_url));
                grab_collect(
                    &mut items,
                    &mut seen_files,
                    opts,
                    &include,
                    &exclude,
                    &start_host,
                    &final_url,
                    String::new(),
                    ext,
                    name,
                );
                if items.len() >= MAX_SITE_ITEMS {
                    break;
                }
                continue;
            }
            None => continue,
        };
        pages_visited += 1;
        if !title_taken {
            title_taken = true;
            let lower = html.to_ascii_lowercase();
            title = (|| {
                let start = lower.find("<title")?;
                let gt = lower[start..].find('>')? + start + 1;
                let end = lower[gt..].find("</title>")? + gt;
                Some(plain_text(&decode_entities(&html[gt..end])))
            })()
            .unwrap_or_default();
        }
        let base = document_base(&html, &final_url);
        for (raw, label) in extract_links(&html) {
            let Some(resolved) = resolve_url(&base, &raw) else {
                continue;
            };
            let same_site = url_host(&resolved) == start_host;
            if is_page_link(&resolved) {
                // exploration budget: same-site and other-site links
                // have their own level allowance, like IDM Step 3,
                // and each crossing spends only its own axis
                let (used, limit, child) = if same_site {
                    (d_same, depth_same, (d_same + 1, d_other))
                } else {
                    (d_other, depth_other, (d_same, d_other + 1))
                };
                if used >= limit {
                    continue;
                }
                if opts.no_parent && same_site && !url_path(&resolved).starts_with(&start_dir) {
                    continue;
                }
                if visited.insert(resolved.clone()) {
                    queue.push_back((resolved, child.0, child.1));
                }
            } else {
                let ext = url_ext(&resolved);
                let name = file_name_of(&resolved);
                grab_collect(
                    &mut items,
                    &mut seen_files,
                    opts,
                    &include,
                    &exclude,
                    &start_host,
                    &resolved,
                    label,
                    &ext,
                    name,
                );
                if items.len() >= MAX_SITE_ITEMS {
                    break;
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
    fn query_string_filename_wins_when_path_has_none() {
        // F-EXT-11: /dl?file=report.pdf must save as report.pdf
        assert_eq!(file_name_of("http://h/dl?file=report.pdf"), "report.pdf");
        assert_eq!(
            query_file_name("http://h/get?filename=Music%20Pack.zip&x=1").as_deref(),
            Some("Music Pack.zip")
        );
        assert_eq!(
            query_file_name("http://h/d?attachment=/srv/files/a.torrent").as_deref(),
            Some("a.torrent")
        );
        // a path name with an extension always wins over the query
        assert_eq!(file_name_of("http://h/files/real.zip?file=other.pdf"), "real.zip");
        // values without a known file extension are not filenames,
        // and unrelated keys are ignored entirely
        assert_eq!(query_file_name("http://h/dl?file=page.html"), None);
        assert_eq!(query_file_name("http://h/dl?token=report.pdf"), None);
        assert_eq!(file_name_of("http://h/dl?token=1"), "dl");
    }

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

    /// A GitHub-release-shaped redirect pair on a loopback
    /// port: the friendly release path 302s to a signed asset
    /// URL whose path tail is a bare UUID — the shape that used
    /// to land in the download list AS the file name (2.3.1
    /// bug 6b, the updater's own downloads included).
    fn signed_asset_site() -> u16 {
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
                    let path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    const UUID: &str = "4c62eef7-1111-4222-8333-444444444444";
                    if path.starts_with("/releases/download/") {
                        let loc = format!(
                            "http://127.0.0.1:{port}/asset/{UUID}?response-content-disposition=attachment%3B%20filename%3DNDM_2.3.0_amd64.deb"
                        );
                        let _ = s.write_all(
                            format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
                        );
                    } else if path.starts_with("/plain/") {
                        let loc = format!("http://127.0.0.1:{port}/asset/{UUID}");
                        let _ = s.write_all(
                            format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
                        );
                    } else if path.starts_with("/asset/") {
                        let _ = s.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        );
                        let _ = s.write_all(b"data");
                    } else {
                        let _ = s.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                });
            }
        });
        port
    }

    #[test]
    fn suggested_name_survives_signed_asset_redirects() {
        let port = signed_asset_site();
        // with the disposition echoed in the signed URL's query
        let p = probe(&format!(
            "http://127.0.0.1:{port}/releases/download/v2.3.0/NDM_2.3.0_amd64.deb"
        ))
        .expect("probe must succeed");
        assert_eq!(
            p.suggested_name.as_deref(),
            Some("NDM_2.3.0_amd64.deb"),
            "the list must show the real asset name, not the signed URL's UUID tail"
        );
        // without it: the ORIGINAL URL's tail is the only
        // plausible name — the UUID tail must not win
        let p = probe(&format!("http://127.0.0.1:{port}/plain/realname.zip"))
            .expect("probe must succeed");
        assert_eq!(p.suggested_name.as_deref(), Some("realname.zip"));
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

    // --- F-15: a resumed / partially-failing download must always
    // reach a terminal state — never silent eternity ---

    enum TailMode {
        /// ranges starting in the tail fail with 500 for the first
        /// `fail_first` such requests, then serve normally
        FailTimes(std::sync::atomic::AtomicUsize, usize),
        /// ranges starting in the tail are answered with an empty
        /// 206 range (Content-Range end < start, zero bytes)
        Empty,
    }

    /// A loopback range-capable file server for download tests.
    fn range_server(body: Vec<u8>, tail_from: u64, mode: TailMode) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = std::sync::Arc::new(body);
        let mode = std::sync::Arc::new(mode);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                let mode = mode.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    let Some((rs, re)) = range else {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        let _ = s.write_all(&body);
                        return;
                    };
                    if rs >= tail_from {
                        match &*mode {
                            TailMode::FailTimes(counter, n) => {
                                if counter
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    < *n
                                {
                                    let _ = s.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                                    return;
                                }
                            }
                            TailMode::Empty => {
                                let _ = s.write_all(
                                    format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{}/{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", rs - 1)
                                        .as_bytes(),
                                );
                                return;
                            }
                        }
                    }
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        )
                        .as_bytes(),
                    );
                    let _ = s.write_all(slice);
                });
            }
        });
        port
    }

    fn pattern_body(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
    }

    fn wait_terminal(dl: &Download, deadline: Duration) -> Option<Status> {
        let t0 = Instant::now();
        loop {
            if dl.is_terminal() {
                return Some(dl.status());
            }
            if t0.elapsed() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn download_survives_pause_resume_and_flaky_tail() {
        let body = pattern_body(4 * 1024 * 1024);
        let port = range_server(
            body.clone(),
            3 * 1024 * 1024,
            TailMode::FailTimes(std::sync::atomic::AtomicUsize::new(0), 8),
        );
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let out = std::env::temp_dir().join(format!("ndm-f15-flaky-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        // run to ~40%, pause, resume — the F-15 sequence
        let t0 = Instant::now();
        while dl.progress().done_bytes < (body.len() as u64) * 2 / 5 {
            assert!(t0.elapsed() < Duration::from_secs(30), "never reached 40%");
            std::thread::sleep(Duration::from_millis(25));
        }
        dl.control.pause();
        std::thread::sleep(Duration::from_millis(400));
        dl.control.resume();
        let st = wait_terminal(&dl, Duration::from_secs(90));
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl.status());
        assert_eq!(std::fs::read(&out).unwrap(), body);
        assert!(!state_path_for(&out).exists(), "state file must be removed");
        let _ = std::fs::remove_file(&out);
    }

    /// WS-A rig (audit-232 S5): one block's range requests are
    /// answered `dead_status` (403/410) for the first seconds —
    /// a signed URL rotting for ONE range — then serve normally.
    /// Every other range serves at full speed the whole time.
    fn single_range_death_server(
        body: Vec<u8>,
        dead_status: u16,
        dead_start: u64,
        dead_for: Duration,
    ) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = std::sync::Arc::new(body);
        let until = Instant::now() + dead_for;
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"wsa1\"\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    if let Some((rs, _)) = range {
                        if rs == dead_start && Instant::now() < until {
                            let _ = s.write_all(
                                format!("HTTP/1.1 {dead_status} Gone\r\nContent-Length: 7\r\nConnection: close\r\n\r\nexpired")
                                    .as_bytes(),
                            );
                            return;
                        }
                    }
                    let (rs, re) = match range {
                        Some(r) => r,
                        None => (0, total - 1),
                    };
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    if range.is_some() {
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                slice.len()
                            )
                            .as_bytes(),
                        );
                    } else {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                    }
                    let _ = s.write_all(slice);
                });
            }
        });
        port
    }

    /// WS-A (audit-232 S5, requirement 1): a 403/410 answered to
    /// ONE range while the other ranges flow must not be a
    /// download-killing Permanent error. The 410 arm failed
    /// before the fix: the whole download errored at once with
    /// most connections still mid-body. (The 403 arm already
    /// retried per-connection and is kept as a regression guard.)
    #[test]
    fn single_range_auth_death_does_not_kill_the_download() {
        for dead_status in [410u16, 403] {
            let body = pattern_body(2 * 1024 * 1024);
            // block 1 (512 KiB in, at a 512 KiB block size) dies
            // for 6 s; everything else serves normally
            let port = single_range_death_server(
                body.clone(),
                dead_status,
                512 * 1024,
                Duration::from_secs(6),
            );
            let url = format!("http://127.0.0.1:{port}/file.bin");
            let out = std::env::temp_dir()
                .join(format!("ndm-wsa-single-{dead_status}-{}.bin", std::process::id()));
            let _ = std::fs::remove_file(&out);
            let _ = std::fs::remove_file(state_path_for(&out));
            let dl = start(&url, &out, 4, 512 * 1024).unwrap();
            let st = wait_terminal(&dl, Duration::from_secs(60));
            assert_eq!(
                st,
                Some(Status::Finished),
                "status {dead_status} on one range killed the download: {:?} err={:?}",
                dl.status(),
                dl.progress().error
            );
            assert_eq!(std::fs::read(&out).unwrap(), body, "status {dead_status}");
            let _ = std::fs::remove_file(&out);
        }
    }

    /// WS-A rig, second shape: EVERY range request is answered
    /// `dead_status` once `die_after` bytes have been served —
    /// the audit's expiring-token server at reduced scale.
    fn total_death_server(body: Vec<u8>, dead_status: u16, die_after: u64) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = std::sync::Arc::new(body);
        let served = std::sync::Arc::new(AtomicU64::new(0));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                let served = served.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"wsa1\"\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    if served.load(Ordering::Relaxed) >= die_after {
                        let _ = s.write_all(
                            format!("HTTP/1.1 {dead_status} Gone\r\nContent-Length: 7\r\nConnection: close\r\n\r\nexpired")
                                .as_bytes(),
                        );
                        return;
                    }
                    let (rs, re) = match range {
                        Some(r) => r,
                        None => (0, total - 1),
                    };
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    served.fetch_add(slice.len() as u64, Ordering::Relaxed);
                    if range.is_some() {
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                slice.len()
                            )
                            .as_bytes(),
                        );
                    } else {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                    }
                    let _ = s.write_all(slice);
                });
            }
        });
        port
    }

    /// WS-A (audit-232 S5): when the whole address dies after
    /// bytes flowed, the engine ends in Error (the heal belongs
    /// to the job layer) — but it must say WHY through the typed
    /// auth_dead signal, with the flowed bytes still counted.
    /// Before the fix the 410 arm failed instantly as Permanent
    /// with no signal at all.
    #[test]
    fn total_auth_death_reports_the_typed_signal() {
        for dead_status in [410u16, 403] {
            let body = pattern_body(4 * 1024 * 1024);
            let port = total_death_server(body.clone(), dead_status, 1024 * 1024);
            let url = format!("http://127.0.0.1:{port}/file.bin");
            let out = std::env::temp_dir()
                .join(format!("ndm-wsa-total-{dead_status}-{}.bin", std::process::id()));
            let _ = std::fs::remove_file(&out);
            let _ = std::fs::remove_file(state_path_for(&out));
            let dl = start(&url, &out, 4, 256 * 1024).unwrap();
            let st = wait_terminal(&dl, Duration::from_secs(90));
            assert_eq!(st, Some(Status::Error), "status {dead_status}");
            let p = dl.progress();
            assert!(
                p.auth_dead,
                "status {dead_status}: the typed auth-death signal must be set (err={:?})",
                p.error
            );
            assert!(
                p.done_bytes > 0,
                "status {dead_status}: bytes flowed before the death"
            );
            let _ = std::fs::remove_file(&out);
            let _ = std::fs::remove_file(state_path_for(&out));
        }
    }

    /// A range server that drips the first `slow_pieces` body pieces
    /// (64 KiB each) with `drip` of delay before each, then serves at
    /// full speed — so a pause lands deterministically while a chunk
    /// read is in flight.
    fn trickle_range_server(
        body: Vec<u8>,
        drip: Duration,
        slow_pieces: usize,
    ) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = std::sync::Arc::new(body);
        let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                let sent = sent.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    let (rs, re) = match range {
                        Some(r) => r,
                        None => (0, total - 1),
                    };
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        )
                        .as_bytes(),
                    );
                    for piece in slice.chunks(64 * 1024) {
                        if sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            < slow_pieces
                        {
                            std::thread::sleep(drip);
                        }
                        if s.write_all(piece).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    #[test]
    fn pause_freezes_bytes_and_resume_refetches_the_discarded_chunk() {
        // battery19 t1/t6: once a pause (or quota halt, same flag)
        // lands, the on-disk byte count must FREEZE — a chunk whose
        // read was in flight is discarded, not written, and resume
        // re-fetches it by range (final content must be exact).
        let body = pattern_body(4 * 1024 * 1024);
        let port = trickle_range_server(body.clone(), Duration::from_millis(1200), 4);
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let out = std::env::temp_dir().join(format!("ndm-freeze-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 1, 256 * 1024).unwrap();
        // first piece lands (~1.2 s in); the next piece's read is
        // then in flight for another drip interval
        let t0 = Instant::now();
        while dl.progress().done_bytes == 0 {
            assert!(t0.elapsed() < Duration::from_secs(30), "no bytes ever landed");
            std::thread::sleep(Duration::from_millis(25));
        }
        dl.control.pause();
        // sample for 3 s: after a short settle the count must be
        // bit-frozen — pre-fix, the in-flight piece was written at
        // the next drip boundary (~1.2 s after the pause)
        let mut frozen: Option<u64> = None;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(3000) {
            let done = dl.progress().done_bytes;
            if t0.elapsed() >= Duration::from_millis(800) {
                match frozen {
                    None => frozen = Some(done),
                    Some(f) => assert_eq!(
                        done, f,
                        "byte count crept after the pause took effect"
                    ),
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let frozen = frozen.expect("sampling window produced no frozen sample");
        assert!(frozen < body.len() as u64, "download finished despite the pause");
        dl.control.resume();
        let st = wait_terminal(&dl, Duration::from_secs(90));
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl.status());
        assert_eq!(
            std::fs::read(&out).unwrap(),
            body,
            "the discarded chunk was not re-fetched exactly"
        );
        assert!(!state_path_for(&out).exists(), "state file must be removed");
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn empty_range_responses_end_in_error_not_eternity() {
        let body = pattern_body(2 * 1024 * 1024);
        let port = range_server(body.clone(), 1024 * 1024, TailMode::Empty);
        let url = format!("http://127.0.0.1:{port}/half.bin");
        let out = std::env::temp_dir().join(format!("ndm-f15-empty-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let dl = start(&url, &out, 3, 256 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(120));
        assert_eq!(
            st,
            Some(Status::Error),
            "an empty-range server must produce a terminal error, got {:?}",
            dl.status()
        );
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
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

    // ---------- WS-BUGS B5(a): hostile server-suggested names ----------

    #[test]
    fn sanitize_hostile_names_stay_one_safe_file_name() {
        // path separators and traversal collapse to the tail —
        // the result can never escape the target folder
        assert_eq!(sanitize_suggested_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_suggested_name("..\\..\\win.ini"), "win.ini");
        assert_eq!(sanitize_suggested_name("/abs/path/file.zip"), "file.zip");
        assert_eq!(sanitize_suggested_name("a/b\\c.zip"), "c.zip");
        // trailing dots and spaces (illegal on Windows) are trimmed
        assert_eq!(sanitize_suggested_name("file.txt "), "file.txt");
        assert_eq!(sanitize_suggested_name("file."), "file");
        assert_eq!(sanitize_suggested_name("name...  "), "name");
        // unicode survives untouched
        assert_eq!(
            sanitize_suggested_name("রিপোর্ট ২০২৬.pdf"),
            "রিপোর্ট ২০২৬.pdf"
        );
        // a name of nothing but dots/spaces has no usable form
        assert_eq!(sanitize_suggested_name("..."), "");
        assert_eq!(sanitize_suggested_name("   "), "");
        // DOS device names: defused on Windows builds (where they
        // are uncreatable), byte-identical elsewhere (Linux creates
        // them fine) — the invariant everywhere is: no separator,
        // no traversal, non-empty
        for raw in ["CON", "con.txt", "PRN", "nul.mp3", "COM1", "lpt9.dat"] {
            let got = sanitize_suggested_name(raw);
            assert!(!got.is_empty(), "{raw} must keep a usable name");
            assert!(!got.contains(['/', '\\']), "{raw} -> {got}");
            if cfg!(target_os = "windows") {
                let stem = got.split('.').next().unwrap_or("").to_ascii_uppercase();
                assert!(
                    !["CON", "PRN", "AUX", "NUL", "COM1", "LPT9"].contains(&stem.as_str()),
                    "windows device name not defused: {raw} -> {got}"
                );
            }
        }
        // 300+ characters are capped to the filesystem limit
        let long = format!("{}.zip", "x".repeat(400));
        let got = sanitize_suggested_name(&long);
        assert!(got.len() <= 255, "capped name is {} bytes", got.len());
        assert!(got.ends_with(".zip"), "extension survives the cap: {got}");
    }

    #[test]
    fn cap_file_name_never_exceeds_255_bytes() {
        // ordinary long stem: stem truncates, extension survives
        let n = cap_file_name(&format!("{}.bin", "a".repeat(300)));
        assert!(n.len() <= 255 && n.ends_with(".bin"), "{n}");
        // pathological: the EXTENSION alone exceeds the limit —
        // the stem budget saturates at 0 and the old code returned
        // the whole >255-byte extension untouched, so the create
        // still failed with ENAMETOOLONG downstream
        let n = cap_file_name(&format!("a.{}", "x".repeat(300)));
        assert!(n.len() <= 255, "extension-only overflow: {} bytes", n.len());
        // a dotfile-shaped name with a huge tail after the dot
        let n = cap_file_name(&format!(".{}", "y".repeat(400)));
        assert!(n.len() <= 255, "dotfile overflow: {} bytes", n.len());
        // unicode stems truncate on a char boundary (no panic, valid)
        let n = cap_file_name(&format!("{}.pdf", "é".repeat(200)));
        assert!(n.len() <= 255 && n.ends_with(".pdf"), "{n}");
        // names within the limit are untouched
        assert_eq!(cap_file_name("short.txt"), "short.txt");
    }

    // ---------- WS-BUGS B5(c): redirect loop ----------

    /// A server that 302s every request back to itself, carrying
    /// `Content-Length: 0` — the shape that used to read as a
    /// legitimately empty file.
    fn redirect_loop_server() -> u16 {
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
                    let path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    let loc = format!("http://127.0.0.1:{port}{path}");
                    let _ = s.write_all(
                        format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
                    );
                });
            }
        });
        port
    }

    // ---------- WS-BUGS B5(b): the total changes between probe and workers ----------

    /// A range server that answers the probe's 1-byte verify
    /// request TRUTHFULLY but reports `lie_total` in the
    /// Content-Range of every real worker range — the size lie
    /// that only appears mid-download.
    fn lying_total_server(body: Vec<u8>, lie_total: u64) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = std::sync::Arc::new(body);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    let Some((rs, re)) = range else {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        let _ = s.write_all(&body);
                        return;
                    };
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    let reported = if rs == 0 && re == 0 { total } else { lie_total };
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{reported}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        )
                        .as_bytes(),
                    );
                    let _ = s.write_all(slice);
                });
            }
        });
        port
    }

    fn size_lie_outcome(lie_total: u64, tag: &str) {
        let body = pattern_body(600_000);
        let port = lying_total_server(body.clone(), lie_total);
        let url = format!("http://127.0.0.1:{port}/lie.bin");
        let out = std::env::temp_dir()
            .join(format!("ndm-b5-lie-{tag}-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(120));
        assert_eq!(
            st,
            Some(Status::Error),
            "a mid-download total lie must end in a clean error, got {:?}",
            dl.status()
        );
        let err = dl.progress().error.unwrap_or_default();
        assert!(
            err.contains("wrong range") || err.contains("no progress"),
            "the error surface must name the lie: {err}"
        );
        assert!(
            std::fs::read(&out).map(|b| b != body).unwrap_or(true),
            "the lied-about file must never assemble as the real content"
        );
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }

    #[test]
    fn content_range_total_growing_mid_download_fails_clean() {
        size_lie_outcome(1_200_000, "grow");
    }

    #[test]
    fn content_range_total_shrinking_mid_download_fails_clean() {
        size_lie_outcome(300_000, "shrink");
    }

    // ---------- WS-BUGS B5(d): encoding proof + chunked regression ----------

    /// Every response is LABELLED gzip; every body is identity.
    fn gzip_labelled_server(body: Vec<u8>) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = std::sync::Arc::new(body);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    if let Some((rs, re)) = range {
                        let end = re.min(total - 1);
                        let slice = &body[rs as usize..=end as usize];
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\n",
                                slice.len()
                            )
                            .as_bytes(),
                        );
                        let _ = s.write_all(slice);
                    } else {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        let _ = s.write_all(&body);
                    }
                });
            }
        });
        port
    }

    #[test]
    fn gzip_labelled_identity_body_still_downloads_exact() {
        // the lying-header proof (finding #3 / DIAG): bodies are
        // identity despite the header, so the job keeps its range
        // path and finishes byte-exact instead of demoting/failing
        let body = pattern_body(700_000);
        let port = gzip_labelled_server(body.clone());
        let url = format!("http://127.0.0.1:{port}/labelled.bin");
        let p = probe(&url).expect("the proof must accept the labelled probe");
        assert!(p.range_ok, "a proven header lie must keep range support");
        assert!(p.ignore_encoding_header);
        let out = std::env::temp_dir()
            .join(format!("ndm-b5-gziplabel-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(60));
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl.status());
        assert!(dl.progress().resumable, "the ranged path must be kept");
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let _ = std::fs::remove_file(&out);
    }

    /// Range answers arrive chunked (no Content-Length at all).
    fn chunked_range_server(body: Vec<u8>) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = std::sync::Arc::new(body);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    if let Some((rs, re)) = range {
                        let end = re.min(total - 1);
                        let slice = &body[rs as usize..=end as usize];
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        );
                        for piece in slice.chunks(8192) {
                            let _ = s.write_all(
                                format!("{:x}\r\n", piece.len()).as_bytes(),
                            );
                            let _ = s.write_all(piece);
                            let _ = s.write_all(b"\r\n");
                        }
                        let _ = s.write_all(b"0\r\n\r\n");
                    } else {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        let _ = s.write_all(&body);
                    }
                });
            }
        });
        port
    }

    #[test]
    fn chunked_range_answers_download_exact() {
        let body = pattern_body(500_000);
        let port = chunked_range_server(body.clone());
        let url = format!("http://127.0.0.1:{port}/chunked.bin");
        let out = std::env::temp_dir()
            .join(format!("ndm-b5-chunked-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 3, 256 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(60));
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl.status());
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let _ = std::fs::remove_file(&out);
    }

    // ---------- WS-BUGS B5(e): zero-byte and 1-byte files ----------

    fn tiny_server() -> u16 {
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
                    let mut has_range = false;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                if h.to_ascii_lowercase().starts_with("range:") {
                                    has_range = true;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    if path.starts_with("/empty") {
                        let _ = s.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        );
                        return;
                    }
                    // /one — a single byte, range-capable
                    if method == "HEAD" {
                        let _ = s.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        );
                    } else if has_range {
                        let _ = s.write_all(
                            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/1\r\nContent-Length: 1\r\nConnection: close\r\n\r\n",
                        );
                        let _ = s.write_all(&[0xABu8]);
                    } else {
                        let _ = s.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n",
                        );
                        let _ = s.write_all(&[0xABu8]);
                    }
                });
            }
        });
        port
    }

    #[test]
    fn zero_byte_and_one_byte_files_finish_exact() {
        let port = tiny_server();
        let dir = std::env::temp_dir();
        let out0 = dir.join(format!("ndm-b5-zero-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out0);
        let _ = std::fs::remove_file(state_path_for(&out0));
        let dl = start(&format!("http://127.0.0.1:{port}/empty"), &out0, 2, 256 * 1024)
            .expect("a zero-byte start must succeed");
        assert_eq!(dl.status(), Status::Finished);
        assert_eq!(std::fs::metadata(&out0).unwrap().len(), 0);

        let out1 = dir.join(format!("ndm-b5-one-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out1);
        let _ = std::fs::remove_file(state_path_for(&out1));
        let dl = start(&format!("http://127.0.0.1:{port}/one"), &out1, 2, 256 * 1024)
            .expect("a one-byte start must succeed");
        let st = wait_terminal(&dl, Duration::from_secs(30));
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl.status());
        assert_eq!(std::fs::read(&out1).unwrap(), vec![0xABu8]);
        let _ = std::fs::remove_file(&out0);
        let _ = std::fs::remove_file(&out1);
    }

    // ---------- WS-BUGS B5(f): truncated engine journal ----------

    #[test]
    fn truncated_journal_restarts_clean_and_finishes_exact() {
        let body = pattern_body(700_000);
        let port = range_server(
            body.clone(),
            u64::MAX,
            TailMode::FailTimes(std::sync::atomic::AtomicUsize::new(0), 0),
        );
        let url = format!("http://127.0.0.1:{port}/resume.bin");
        let out = std::env::temp_dir()
            .join(format!("ndm-b5-journal-{}.bin", std::process::id()));
        // a full-size partial of WRONG bytes, plus a sidecar cut
        // mid-JSON (the power-cut-mid-save shape)
        std::fs::write(&out, vec![0xEEu8; body.len()]).unwrap();
        let sidecar = state_path_for(&out);
        std::fs::write(
            &sidecar,
            format!(
                "{{\"url\":\"{url}\",\"total\":{},\"block_size\":262144,\"done\":[1,1,0",
                body.len()
            ),
        )
        .unwrap();
        // the corrupt journal is simply not a resume source…
        assert!(peek_state(&url, &out).is_none());
        // …and the start recovers by restarting from byte zero:
        // no wedge, no splice of the wrong bytes, exact finish
        let dl = start(&url, &out, 3, 256 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(60));
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl.status());
        assert_eq!(std::fs::read(&out).unwrap(), body);
        assert!(!sidecar.exists(), "state file must be removed at finish");
        let _ = std::fs::remove_file(&out);
    }

    // ---------- WS-BUGS B5(a): Content-Disposition shapes end to end ----------

    fn disposition_server() -> u16 {
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
                    let mut has_range = false;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                if h.to_ascii_lowercase().starts_with("range:") {
                                    has_range = true;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let cd = match path.as_str() {
                        "/a" => "attachment; filename*=UTF-8''%2e%2e%2f%2e%2e%2fsecret.txt",
                        "/b" => "attachment; filename=\"..\\..\\win.dll\"",
                        "/c" => "attachment; filename=\"report final .pdf \"",
                        _ => "attachment; filename=\"plain.bin\"",
                    };
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\nContent-Disposition: {cd}\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                    } else if has_range {
                        let _ = s.write_all(
                            format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/4\r\nContent-Length: 1\r\nContent-Disposition: {cd}\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        let _ = s.write_all(b"d");
                    } else {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: {cd}\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        let _ = s.write_all(b"data");
                    }
                });
            }
        });
        port
    }

    #[test]
    fn disposition_traversal_names_are_sanitized_at_probe() {
        let port = disposition_server();
        let p = probe(&format!("http://127.0.0.1:{port}/a")).unwrap();
        assert_eq!(p.suggested_name.as_deref(), Some("secret.txt"));
        let p = probe(&format!("http://127.0.0.1:{port}/b")).unwrap();
        assert_eq!(p.suggested_name.as_deref(), Some("win.dll"));
        let p = probe(&format!("http://127.0.0.1:{port}/c")).unwrap();
        assert_eq!(p.suggested_name.as_deref(), Some("report final .pdf"));
    }

    #[test]
    fn redirect_loop_is_an_error_not_an_empty_finished_file() {
        let port = redirect_loop_server();
        let url = format!("http://127.0.0.1:{port}/loop.bin");
        // the probe must refuse: following the loop gives up after
        // the hop budget and what remains is a bare 302 — never a
        // verdict that the file is 0 bytes long
        let err = probe(&url).expect_err("a redirect loop must fail the probe");
        assert!(!err.is_empty());
        // and a start must fail with it — pre-fix it created an
        // empty file and reported Finished
        let out = std::env::temp_dir()
            .join(format!("ndm-b5-loop-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let r = start(&url, &out, 2, 256 * 1024);
        assert!(r.is_err(), "redirect loop start must fail, got a handle");
        assert!(!out.exists(), "no empty 'finished' file may be created");
    }

    // ---------- 2.0.4: video pair split (Finding 2) ----------

    #[test]
    fn video_pair_connections_weights_the_video_stream() {
        assert_eq!(video_pair_connections(1), (1, 1));
        assert_eq!(video_pair_connections(2), (1, 1));
        assert_eq!(video_pair_connections(3), (2, 1));
        assert_eq!(video_pair_connections(4), (2, 2));
        assert_eq!(video_pair_connections(5), (3, 2));
        assert_eq!(video_pair_connections(8), (6, 2));
        assert_eq!(video_pair_connections(16), (14, 2));
        assert_eq!(video_pair_connections(32), (30, 2));
        for mc in 1..=32usize {
            let (v, a) = video_pair_connections(mc);
            assert!(v >= 1 && a >= 1, "mc={mc}: neither stream may starve");
            assert!(v >= a, "mc={mc}: video never gets less than audio");
            if mc >= 2 {
                assert_eq!(v + a, mc, "mc={mc}: the pair uses the full count");
            }
        }
        // a zero setting must not produce a zero share either
        assert_eq!(video_pair_connections(0), (1, 1));
    }

    // ---------- 2.0.4: probe retry (Finding 4) ----------

    /// A probe target that hangs up on the first `drop_first`
    /// accepted connections without sending a single byte (a pure
    /// transport failure), then serves as a normal range server.
    /// Returns the port and the live connection counter, so tests
    /// can prove how many attempts the probe actually made.
    fn probe_flaky_server(body: Vec<u8>, drop_first: usize) -> (u16, Arc<AtomicUsize>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = Arc::new(body);
        let conns = Arc::new(AtomicUsize::new(0));
        let counter = conns.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                if counter.fetch_add(1, Ordering::Relaxed) < drop_first {
                    drop(s); // hang up: no HTTP answer ever arrives
                    continue;
                }
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    let (rs, re) = range.unwrap_or((0, total - 1));
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        )
                        .as_bytes(),
                    );
                    let _ = s.write_all(slice);
                });
            }
        });
        (port, conns)
    }

    #[test]
    fn probe_retries_transient_transport_failures() {
        // the first four connections die without an answer (two full
        // probe passes: HEAD + GET each); the third pass must land —
        // before the retry, the first pass's failure WAS the result
        // and the download start died on the spot (Finding 4)
        let body = pattern_body(2 * 1024 * 1024);
        let (port, conns) = probe_flaky_server(body.clone(), 4);
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let p = probe(&url).expect("probe must survive transient failures");
        assert_eq!(p.total, body.len() as u64);
        assert!(p.range_ok);
        assert!(
            conns.load(Ordering::Relaxed) >= 5,
            "expected the retries to make fresh connections, saw {}",
            conns.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn probe_does_not_retry_a_definitive_answer() {
        // a server that answers 404 to everything: the probe must
        // fail after ONE pass (HEAD + GET = 2 connections) — a
        // definitive answer retried is just a slower error
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let conns = Arc::new(AtomicUsize::new(0));
        let counter = conns.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                counter.fetch_add(1, Ordering::Relaxed);
                std::thread::spawn(move || {
                    // read the request first: answering into an
                    // unread socket can RST the reply away and turn
                    // a clean 404 into a transport error client-side
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) if line == "\r\n" || line == "\n" => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    let _ = s.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                });
            }
        });
        let url = format!("http://127.0.0.1:{port}/gone.bin");
        let err = probe(&url).expect_err("a 404 probe must fail");
        assert!(err.contains("404"), "unexpected error text: {err}");
        assert_eq!(
            conns.load(Ordering::Relaxed),
            2,
            "a definitive answer must not be retried"
        );
    }

    // ---------- 2.0.4: work-stealing (Finding 1) ----------

    #[test]
    fn split_point_keeps_a_working_share_and_never_splits_a_nearly_done_run() {
        // tail = end - first_unwritten; a split needs tail >=
        // KEEP(2) + MIN_STEAL(2) = 4 unstarted blocks
        assert_eq!(split_point(0, 3), None);
        assert_eq!(split_point(10, 13), None);
        assert_eq!(split_point(5, 5), None);
        assert_eq!(split_point(9, 4), None, "inverted range must not split (or panic)");
        assert_eq!(split_point(0, 4), Some(2), "releases exactly the minimum");
        assert_eq!(split_point(10, 14), Some(12));
        assert_eq!(split_point(0, 200), Some(2), "the owner keeps only its working share");
        for end in 0..300usize {
            if let Some(sp) = split_point(7, end) {
                assert!(sp > 7 && sp < end);
                assert_eq!(sp - 7, SPLIT_KEEP_BLOCKS);
                assert!(end - sp >= SPLIT_MIN_STEAL);
            }
        }
    }

    /// The Finding 1 scenario as an in-process server: a GET whose
    /// range starts at byte 0 and spans more than 1 MiB trickles at
    /// `slow_bps`; every other response goes at full loopback speed.
    /// (The byte-0 run is whichever worker claimed it first — the
    /// slow lane is a property of the range, not of a worker.)
    fn het_range_server(body: Vec<u8>, slow_bps: u64) -> u16 {
        het_range_server_ex(body, slow_bps, 0, Duration::ZERO)
    }

    /// `tail_hold`: responses to non-zero range starts below
    /// `tail_hold_below` are held for `tail_hold` before their
    /// first body byte. Zero disables the hold entirely.
    fn het_range_server_ex(
        body: Vec<u8>,
        slow_bps: u64,
        tail_hold_below: u64,
        tail_hold: Duration,
    ) -> u16 {
        het_range_server_impl(body, slow_bps, tail_hold_below, tail_hold, false)
    }

    /// Control-lane variant: EVERY response drips at `slow_bps`,
    /// so no redistribution strategy can gain anything — the
    /// finish is total bytes / (connections × slow_bps) whatever
    /// the engine does. Tests use it as a same-process un-stolen
    /// control: its wall time is dominated by the same scripted
    /// sleeps that dominate the shape under test, so a ratio
    /// against it survives the CI scheduling that breaks absolute
    /// wall-clock thresholds.
    fn het_range_server_slow_all(body: Vec<u8>, slow_bps: u64) -> u16 {
        het_range_server_impl(body, slow_bps, 0, Duration::ZERO, true)
    }

    fn het_range_server_impl(
        body: Vec<u8>,
        slow_bps: u64,
        tail_hold_below: u64,
        tail_hold: Duration,
        slow_all: bool,
    ) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = Arc::new(body);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    let (rs, re) = range.unwrap_or((0, total - 1));
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        )
                        .as_bytes(),
                    );
                    let slow = if slow_all {
                        true
                    } else {
                        rs == 0 && slice.len() > 1024 * 1024
                    };
                    if tail_hold_below > 0 && rs > 0 && rs < tail_hold_below {
                        std::thread::sleep(tail_hold);
                    }
                    for piece in slice.chunks(64 * 1024) {
                        if s.write_all(piece).is_err() {
                            return;
                        }
                        if slow {
                            std::thread::sleep(Duration::from_secs_f64(
                                piece.len() as f64 / slow_bps as f64,
                            ));
                        }
                    }
                });
            }
        });
        port
    }

    /// A range server that caps EACH connection at ~512 KB/s
    /// (32 KiB every 62 ms): the per-connection-throttle shape
    /// of bug 10's audit. Returns (port, live connection count).
    fn per_conn_cap_server(
        body: Vec<u8>,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count2 = count.clone();
        let body = std::sync::Arc::new(body);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let count = count2.clone();
                let body = body.clone();
                std::thread::spawn(move || {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range:") {
                                    let spec = v.trim().trim_start_matches("bytes=");
                                    let mut it = spec.splitn(2, '-');
                                    let a: u64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                                    let b: u64 = it
                                        .next()
                                        .and_then(|x| x.trim().parse().ok())
                                        .unwrap_or(body.len() as u64 - 1);
                                    range = Some((a, b));
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    let (a, b) = range.unwrap_or((0, total - 1));
                    let b = b.min(total - 1);
                    let slice = &body[a as usize..=b as usize];
                    if line.starts_with("HEAD") {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n").as_bytes(),
                        );
                        return;
                    }
                    let hdr = if range.is_some() {
                        format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len())
                    } else {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len())
                    };
                    if s.write_all(hdr.as_bytes()).is_err() {
                        return;
                    }
                    for piece in slice.chunks(32 * 1024) {
                        if s.write_all(piece).is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(62));
                    }
                });
            }
        });
        (port, count)
    }

    #[test]
    fn disk_full_write_errors_are_permanent_not_retried() {
        // QA-220 m2 (2.3.1): the classification itself — an
        // ENOSPC-class write error must come out Permanent (the
        // download fails at once) with the plain sentence; any
        // other write error stays a retryable block error. The
        // raw code is per-platform (ENOSPC 28 / ERROR_DISK_FULL
        // 112): hardcoding 28 made this test fail on Windows,
        // where 28 is a different error entirely (CI #67).
        let enospc = io::Error::from_raw_os_error(if cfg!(windows) { 112 } else { 28 });
        match write_block_error(&enospc) {
            BlockError::Permanent(msg) => {
                assert!(msg.contains("The disk is full"), "{msg}")
            }
            BlockError::Retry(msg) => {
                panic!("ENOSPC must be Permanent, got Retry: {msg}")
            }
            _ => panic!("ENOSPC must be Permanent"),
        }
        let eio = io::Error::from_raw_os_error(5);
        assert!(
            matches!(write_block_error(&eio), BlockError::Retry(_)),
            "ordinary write errors stay retryable"
        );
    }

    #[test]
    fn disk_full_mid_download_fails_fast() {
        // The m2 shape end to end: a 3 MiB tmpfs and an 8 MiB
        // download — preallocation (sparse set_len) succeeds,
        // the writes start failing ENOSPC mid-download. The old
        // code looped on those blocks for minutes; the download
        // must now error within seconds, naming the disk.
        // Needs mount privileges (root); skips where it cannot
        // mount rather than failing for the wrong reason.
        let dir = std::env::temp_dir().join(format!("ndm-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mounted = std::process::Command::new("mount")
            .args(["-t", "tmpfs", "-o", "size=3M", "tmpfs"])
            .arg(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !mounted {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let body = pattern_body(8 * 1024 * 1024);
        let port = het_range_server(body, 0);
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let out = dir.join("big.bin");
        let t0 = Instant::now();
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(30));
        let elapsed = t0.elapsed();
        let err = dl.progress().error.unwrap_or_default();
        let _ = std::process::Command::new("umount").arg(&dir).status();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(st, Some(Status::Error), "status: {:?}", dl.status());
        assert!(
            elapsed < Duration::from_secs(20),
            "disk-full must surface in seconds, not minutes (took {elapsed:?})"
        );
        assert!(
            err.contains("The disk is full"),
            "the error must name the disk: {err}"
        );
    }

    #[test]
    fn multi_connection_aggregates_per_connection_caps() {
        // 2.3.1 (bug 10 audit): against a server that caps every
        // single connection, the engine must (a) open its
        // connection set promptly at start — not trickle them
        // in — and (b) aggregate the caps: N connections finish
        // in a small fraction of the one-connection time, bytes
        // exact.
        let body = pattern_body(3 * 1024 * 1024);
        let (port, conns) = per_conn_cap_server(body.clone());
        let url = format!("http://127.0.0.1:{port}/iso.bin");
        let dir = std::env::temp_dir();

        // one connection: ~3 MiB at ~512 KB/s ≈ 6 s
        let out1 = dir.join(format!("ndm-cap1-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out1);
        let _ = std::fs::remove_file(state_path_for(&out1));
        let t0 = Instant::now();
        let dl = start(&url, &out1, 1, 256 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(60));
        let single = t0.elapsed();
        assert_eq!(st, Some(Status::Finished));
        assert_eq!(std::fs::read(&out1).unwrap(), body);

        // eight connections: the set opens promptly and the
        // aggregate rate is a multiple of the single cap
        let before = conns.load(std::sync::atomic::Ordering::SeqCst);
        let out8 = dir.join(format!("ndm-cap8-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out8);
        let _ = std::fs::remove_file(state_path_for(&out8));
        let t0 = Instant::now();
        let dl = start(&url, &out8, 8, 256 * 1024).unwrap();
        let mut prompt_at: Option<Duration> = None;
        while t0.elapsed() < Duration::from_secs(3) {
            if conns.load(std::sync::atomic::Ordering::SeqCst) >= before + 6 {
                prompt_at = Some(t0.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let st = wait_terminal(&dl, Duration::from_secs(60));
        let multi = t0.elapsed();
        assert_eq!(st, Some(Status::Finished));
        assert_eq!(std::fs::read(&out8).unwrap(), body);
        assert!(
            prompt_at.is_some_and(|d| d < Duration::from_millis(1500)),
            "the connection set must open promptly (saw {prompt_at:?})"
        );
        assert!(
            multi * 2 < single,
            "8 connections ({multi:?}) must at least halve the 1-connection time ({single:?})"
        );
        let _ = std::fs::remove_file(&out1);
        let _ = std::fs::remove_file(&out8);
    }

    /// Regression server for decay-window recycling: a server
    /// that bursts the first 1 MiB of every
    /// response at full speed and then clamps that response to
    /// 32 KiB/s (the googlevideo shape). Returns (port, conns).
    fn burst_clamp_server(
        body: Vec<u8>,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count2 = count.clone();
        let body = std::sync::Arc::new(body);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let count = count2.clone();
                let body = body.clone();
                std::thread::spawn(move || {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range:") {
                                    let spec = v.trim().trim_start_matches("bytes=");
                                    let mut it = spec.splitn(2, '-');
                                    let a: u64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                                    let b: u64 = it
                                        .next()
                                        .and_then(|x| x.trim().parse().ok())
                                        .unwrap_or(body.len() as u64 - 1);
                                    range = Some((a, b));
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    let (a, b) = range.unwrap_or((0, total - 1));
                    let b = b.min(total - 1);
                    let slice = &body[a as usize..=b as usize];
                    if line.starts_with("HEAD") {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n").as_bytes(),
                        );
                        return;
                    }
                    let hdr = if range.is_some() {
                        format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len())
                    } else {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len())
                    };
                    if s.write_all(hdr.as_bytes()).is_err() {
                        return;
                    }
                    // burst: first 1 MiB as fast as the socket
                    // takes it; the rest at 32 KiB/s
                    const BURST: usize = 1024 * 1024;
                    const CAP_BPS: u64 = 32 * 1024;
                    let burst_end = BURST.min(slice.len());
                    if s.write_all(&slice[..burst_end]).is_err() {
                        return;
                    }
                    let t0 = Instant::now();
                    let mut sent = 0usize;
                    for piece in slice[burst_end..].chunks(16 * 1024) {
                        if s.write_all(piece).is_err() {
                            return;
                        }
                        sent += piece.len();
                        let target = Duration::from_secs_f64(sent as f64 / CAP_BPS as f64);
                        if let Some(wait) = target.checked_sub(t0.elapsed()) {
                            std::thread::sleep(wait);
                        }
                    }
                });
            }
        });
        (port, count)
    }

    #[test]
    fn burst_then_clamp_connections_get_recycled() {
        // 16 MiB, 1 MiB blocks, 4 connections → 4 MiB claims.
        // Without decay recycling each claim is 1 MiB of burst +
        // 3 MiB at 32 KiB/s ≈ 94 s, and the F2 watermark (256 KiB
        // per 30 s) never fires — the connection crosses 256 KiB
        // every 8 s. (Measured pre-fix: 95.8 s.) With decay
        // recycling a fresh request earns a fresh burst roughly
        // every two windows, cutting the run to ~41 s (measured)
        // and visibly multiplying the request count.
        let body = pattern_body(20 * 1024 * 1024);
        let (port, conns) = burst_clamp_server(body.clone());
        let url = format!("http://127.0.0.1:{port}/burst.bin");
        let out = std::env::temp_dir().join(format!("ndm-burst-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let t0 = Instant::now();
        let dl = start(&url, &out, 4, 1024 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(150));
        let elapsed = t0.elapsed();
        assert_eq!(st, Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let n_conns = conns.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            elapsed < Duration::from_secs(75),
            "burst-then-clamp download took {elapsed:?} — decayed connections were not recycled (conns={n_conns})"
        );
        assert!(
            n_conns >= 10,
            "expected recycling to multiply requests, saw only {n_conns} connections"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn slow_run_tail_is_stolen_by_idle_workers() {
        // 48 blocks, 4 workers, 12-block claims. The byte-0 claim
        // crawls at 1 MiB/s: un-stolen, the finish would take ~12 s
        // (that claim alone) while the other workers idle after
        // their fast claims. With stealing, the slow worker keeps
        // only its split share and the idle workers take the tail.
        // (The claim is deliberately long: the split window is the
        // first blocks of the slow run, and a short claim lets a
        // loaded test machine close that window before any peer
        // goes idle — the engine is not what races there.)
        // A second race sits AFTER the split, and this one the
        // claim length cannot fix: the owner hands its tail to the
        // pool and loops straight back into try_claim, while the
        // parked peers only poll for work every 100 ms. On a fast
        // loopback the owner can re-claim its own tail slice by
        // slice (each a ~10 ms request) inside a single poll gap,
        // ending with exactly its original claim while stealing
        // demonstrably worked (finish ~3 s, not ~12 s) — the CI
        // failure this test once flaked on. The server therefore
        // holds every tail response for 300 ms: three times the
        // poll period, so during the owner's first held slice each
        // parked peer is guaranteed a claim turn, and the pool's
        // waiting-aware slicing then bounds the owner's total take
        // (kept share + first slice) far below a full claim while
        // every peer necessarily ends above one. The hold changes
        // only timing — bytes, ranges and counts are untouched.
        // The finish-time verdict is a RATIO against a control
        // run in this same process, not an absolute threshold:
        // the old `elapsed < 6 s` compared a locally measured
        // ~3 s against a fixed bound and flaked in CI at
        // 6.02 s, where every thread (engine, server, poll
        // loops) stretches together. The control downloads the
        // same body through the same 4 workers from a server
        // whose EVERY lane drips at the slow rate — redistribution
        // conserves time there by construction, so its finish is
        // the un-stolen shape (48 MiB / (4 × 1 MiB/s) ≈ 12 s)
        // measured under the same machine load and dominated by
        // the same scripted sleeps. A broken steal finishes the
        // main run at ≈ the control (ratio ≈ 1) and fails; the
        // working steal lands near 0.25. The structural share
        // assertions below remain the primary proof; this ratio
        // is the wall-clock guard.
        let body = pattern_body(48 * 1024 * 1024);
        let ctl_port = het_range_server_slow_all(body.clone(), 1024 * 1024);
        let ctl_url = format!("http://127.0.0.1:{ctl_port}/big.bin");
        let ctl_out =
            std::env::temp_dir().join(format!("ndm-steal-ctl-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&ctl_out);
        let _ = std::fs::remove_file(state_path_for(&ctl_out));
        let ct0 = Instant::now();
        let ctl = start(&ctl_url, &ctl_out, 4, 1024 * 1024).unwrap();
        let ctl_st = wait_terminal(&ctl, Duration::from_secs(120));
        let ctl_elapsed = ct0.elapsed();
        assert_eq!(ctl_st, Some(Status::Finished), "control status: {:?}", ctl.status());
        assert_eq!(
            std::fs::read(&ctl_out).unwrap(),
            body,
            "control content must be byte-identical"
        );
        assert!(
            ctl_elapsed >= Duration::from_secs(8),
            "control finished in {ctl_elapsed:?} — the all-slow lane did not drip at its scripted rate, so a ratio against it would be meaningless"
        );
        let _ = std::fs::remove_file(&ctl_out);
        let _ = std::fs::remove_file(state_path_for(&ctl_out));
        let port = het_range_server_ex(
            body.clone(),
            1024 * 1024,
            12 * 1024 * 1024,
            Duration::from_millis(300),
        );
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let out = std::env::temp_dir().join(format!("ndm-steal-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let t0 = Instant::now();
        let dl = start(&url, &out, 4, 1024 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(60));
        let elapsed = t0.elapsed();
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl.status());
        assert_eq!(std::fs::read(&out).unwrap(), body, "content must be byte-identical");
        let p = dl.progress();
        // coverage exactness: a well-behaved server + no errors
        // means every byte is fetched exactly once — a range two
        // workers both wrote would inflate this count
        assert_eq!(
            p.fetched_session,
            body.len() as u64,
            "fetched must equal the file size exactly"
        );
        assert_eq!(
            p.conn_bytes.iter().sum::<u64>(),
            body.len() as u64,
            "per-connection bytes must add up to the file, no more"
        );
        // redistribution really happened — proven structurally, not
        // by who ended slow: fresh claims are exactly 12 MiB each
        // and this server never fails a run, so the ONLY way any
        // worker ends above a full claim is by taking split-off
        // work, and the only way one ends below it is by yielding.
        // (A yielded worker may legitimately re-claim pool shares
        // afterwards, so no fixed small share is asserted for it.)
        let min_share = p.conn_bytes.iter().min().copied().unwrap_or(0);
        let max_share = p.conn_bytes.iter().max().copied().unwrap_or(0);
        assert!(
            min_share < 12 * 1024 * 1024 && max_share > 12 * 1024 * 1024,
            "no redistribution in conn_bytes: {:?}",
            p.conn_bytes
        );
        assert!(
            elapsed * 4 <= ctl_elapsed * 3,
            "finish {elapsed:?} must beat 3/4 of the same-process all-slow control {ctl_elapsed:?} (the un-stolen shape)"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn tail_split_point_splits_only_a_worthwhile_remainder() {
        // WS-D D1 arithmetic: a sub-256 KiB remainder is never
        // split (a second connection cannot pay for itself), each
        // side keeps at least 64 KiB, and the split lands mid-
        // remainder — always strictly inside (pos, block_end), so
        // the owner's range and the helper's range are disjoint
        // and together cover the block exactly
        let mib = 1024 * 1024;
        assert_eq!(tail_split_point(0, 128 * 1024), None);
        assert_eq!(tail_split_point(900 * 1024, mib), None, "124 KiB left: too small");
        assert_eq!(tail_split_point(mib, mib), None);
        assert_eq!(tail_split_point(mib, 0), None, "inverted range must not split (or panic)");
        let s = tail_split_point(0, mib).expect("a full block splits");
        assert!(s > 0 && s < mib);
        assert_eq!(tail_split_point(64 * 1024, mib), Some(64 * 1024 + (mib - 64 * 1024) / 2));
        for pos in (0..mib).step_by(37 * 1024) {
            if let Some(sp) = tail_split_point(pos, mib) {
                assert!(sp > pos && sp < mib, "split {sp} must sit inside ({pos}, {mib})");
                assert!(sp - pos >= 64 * 1024 && mib - sp >= 64 * 1024);
            }
        }
    }

    #[test]
    fn limited_start_parks_excess_workers_until_limit_raised() {
        // WS-D D2 engine half: a start with fewer active than
        // spawned connections parks the excess at the gate — only
        // worker 0 may move bytes — and set_connection_limit
        // raises the active count live on the same download.
        let body = pattern_body(4 * 1024 * 1024);
        let (port, _conns) = per_conn_cap_server(body.clone());
        let url = format!("http://127.0.0.1:{port}/lim.bin");
        let out = std::env::temp_dir().join(format!("ndm-limited-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start_with_auth_limited(&url, &out, 4, 1, 1024 * 1024, Auth::default()).unwrap();
        // ~512 KiB/s on the single active connection: after 1.5 s
        // roughly one block has moved, all of it on worker 0
        std::thread::sleep(Duration::from_millis(1500));
        let p = dl.progress();
        assert!(p.done_bytes > 0, "no progress on the active worker");
        assert_eq!(
            p.conn_bytes.iter().filter(|&&b| b > 0).count(),
            1,
            "parked workers must not move bytes: {:?}",
            p.conn_bytes
        );
        assert_eq!(p.conn_bytes[0], p.fetched_session);
        // raising past the spawn count clamps instead of panicking
        dl.set_connection_limit(99);
        let st = wait_terminal(&dl, Duration::from_secs(60));
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl.status());
        assert_eq!(std::fs::read(&out).unwrap(), body);
        assert_eq!(dl.progress().fetched_session, body.len() as u64);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn in_flight_tail_block_is_byte_split_for_idle_workers() {
        // WS-D D1 (audit-232 S6): the end-game shape in miniature.
        // 8 blocks, 4 connections, 2-block claims. The byte-0 claim
        // crawls at 256 KiB/s (the het server's slow lane); the
        // other claims are wire-fast, so within a second six blocks
        // are sealed, three workers sit idle, and the finish hangs
        // on the slow run's two blocks. Block stealing CANNOT help
        // here: the run's unstarted tail (one block) is below the
        // split minimum by design. Pre-fix the slow run alone set
        // the finish: 2 MiB at 256 KiB/s ≈ 8 s. With the tail
        // byte-split, an idle worker takes the in-flight block's
        // tail at a byte offset while the owner finishes its head,
        // and the second block goes back to the pool whole.
        // The finish verdict is a ratio against a same-process
        // all-slow control (every lane at 256 KiB/s, where a byte
        // split conserves time by construction: 8 MiB / (4 ×
        // 256 KiB/s) ≈ 8 s) — not an absolute bound. See
        // slow_run_tail_is_stolen_by_idle_workers for why fixed
        // wall-clock thresholds flake under CI scheduling. The
        // unaligned-share assertion below is the structural proof
        // a byte split happened; this ratio is the time guard,
        // and an un-split finish (≈ the control) fails it.
        let body = pattern_body(8 * 1024 * 1024);
        let ctl_port = het_range_server_slow_all(body.clone(), 256 * 1024);
        let ctl_url = format!("http://127.0.0.1:{ctl_port}/big.bin");
        let ctl_out =
            std::env::temp_dir().join(format!("ndm-tailsplit-ctl-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&ctl_out);
        let _ = std::fs::remove_file(state_path_for(&ctl_out));
        let ct0 = Instant::now();
        let ctl = start(&ctl_url, &ctl_out, 4, 1024 * 1024).unwrap();
        let ctl_st = wait_terminal(&ctl, Duration::from_secs(120));
        let ctl_elapsed = ct0.elapsed();
        assert_eq!(ctl_st, Some(Status::Finished), "control status: {:?}", ctl.status());
        assert_eq!(
            std::fs::read(&ctl_out).unwrap(),
            body,
            "control content must be byte-identical"
        );
        assert!(
            ctl_elapsed >= Duration::from_secs(5),
            "control finished in {ctl_elapsed:?} — the all-slow lane did not drip at its scripted rate, so a ratio against it would be meaningless"
        );
        let _ = std::fs::remove_file(&ctl_out);
        let _ = std::fs::remove_file(state_path_for(&ctl_out));
        let port = het_range_server(body.clone(), 256 * 1024);
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let out = std::env::temp_dir().join(format!("ndm-tailsplit-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let t0 = Instant::now();
        let dl = start(&url, &out, 4, 1024 * 1024).unwrap();
        let st = wait_terminal(&dl, Duration::from_secs(60));
        let elapsed = t0.elapsed();
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl.status());
        assert_eq!(
            std::fs::read(&out).unwrap(),
            body,
            "content must be byte-identical"
        );
        let p = dl.progress();
        // duplication audit: every byte was written exactly once —
        // the owner's head range and the helper's tail range are
        // disjoint by construction, and any overlap would inflate
        // the fetched count past the file size
        assert_eq!(
            p.fetched_session,
            body.len() as u64,
            "fetched must equal the file size exactly — a duplicated range inflates it"
        );
        assert_eq!(
            p.conn_bytes.iter().sum::<u64>(),
            body.len() as u64,
            "per-connection bytes must add up to the file, no more"
        );
        // structural proof a byte-offset fetch happened: without a
        // tail split every connection's total is a whole number of
        // 1 MiB blocks (every run is whole blocks); a split leaves
        // at least the owner and the helper with a non-block-
        // aligned share
        let unaligned = p
            .conn_bytes
            .iter()
            .filter(|&&b| b % (1024 * 1024) != 0)
            .count();
        assert!(
            unaligned >= 2,
            "no byte-split shares in conn_bytes: {:?}",
            p.conn_bytes
        );
        assert!(
            elapsed * 4 <= ctl_elapsed * 3,
            "finish {elapsed:?} must beat 3/4 of the same-process all-slow control {ctl_elapsed:?} (the un-split shape)"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn steal_then_cancel_and_resume_stays_byte_identical() {
        // The journal-consistency gate for stealing: a download
        // whose claims were split mid-flight is cut dead, then a
        // fresh start resumes from the saved journal. The resumed
        // session must fetch precisely the blocks the journal had
        // not credited — none twice, none skipped — and the final
        // file must be byte-identical.
        let body = pattern_body(24 * 1024 * 1024);
        let port = het_range_server(body.clone(), 1024 * 1024);
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let out = std::env::temp_dir().join(format!("ndm-steal-resume-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 4, 1024 * 1024).unwrap();
        // let several blocks land (the fast lanes finish theirs in
        // well under a second), then cut the download dead
        let t0 = Instant::now();
        while dl.progress().done_bytes < 5 * 1024 * 1024 {
            assert!(t0.elapsed() < Duration::from_secs(30), "no early progress");
            std::thread::sleep(Duration::from_millis(25));
        }
        dl.control.cancel();
        let t0 = Instant::now();
        while !dl.is_settled() {
            assert!(t0.elapsed() < Duration::from_secs(30), "engine never settled");
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(dl);
        // the journal is the contract for the resume: count what it
        // credits, then hold the resumed session to exactly the rest
        let raw = std::fs::read(state_path_for(&out)).expect("journal must survive the cut");
        let journal: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let done_blocks = journal["done"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| **b == 1)
            .count() as u64;
        assert!(
            done_blocks >= 1 && done_blocks < 24,
            "journal credits {done_blocks} blocks — the cut did not land mid-download"
        );
        // B-4: the journal also credits partial prefixes of
        // unsealed blocks — the resumed session must fetch
        // exactly what NEITHER credit covers
        let partial_bytes: u64 = journal["partial"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_u64()).sum())
            .unwrap_or(0);
        let t1 = Instant::now();
        let dl2 = start(&url, &out, 4, 1024 * 1024).unwrap();
        let st = wait_terminal(&dl2, Duration::from_secs(60));
        let elapsed2 = t1.elapsed();
        assert_eq!(st, Some(Status::Finished), "status: {:?}", dl2.status());
        assert_eq!(
            std::fs::read(&out).unwrap(),
            body,
            "resumed content must be byte-identical"
        );
        assert_eq!(
            dl2.progress().fetched_session,
            (24 - done_blocks) * 1024 * 1024 - partial_bytes,
            "resumed session must fetch exactly the uncredited bytes \
             (unsealed blocks minus their journaled partial prefixes)"
        );
        assert!(
            elapsed2 < Duration::from_secs(10),
            "resumed finish {elapsed2:?} — stealing must still work after a resume"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// Change 4 (2.0.4, probe-once): a start handed the dialog's
    /// already-obtained probe must put NO probe traffic on the wire
    /// — the server log shows worker range GETs only, no HEAD and
    /// no 1-byte verify GET — while a plain start probes as before.
    /// This is the wire-level proof behind the app's probe handoff.
    #[test]
    fn preprobed_start_issues_no_probe_requests() {
        use std::io::{BufRead, BufReader, Write};
        // deterministic payload
        let mut body = vec![0u8; 300_000];
        for (i, b) in body.iter_mut().enumerate() {
            *b = ((i * 31 + 7) % 251) as u8;
        }
        let body = Arc::new(body);
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        {
            let body = body.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut s) = conn else { continue };
                    let body = body.clone();
                    let log = log.clone();
                    std::thread::spawn(move || {
                        let mut rd = BufReader::new(match s.try_clone() {
                            Ok(c) => c,
                            Err(_) => return,
                        });
                        loop {
                            let mut line = String::new();
                            match rd.read_line(&mut line) {
                                Ok(0) | Err(_) => return,
                                _ => {}
                            }
                            let line = line.trim_end().to_string();
                            // consume headers
                            let mut range: Option<(u64, u64)> = None;
                            loop {
                                let mut h = String::new();
                                match rd.read_line(&mut h) {
                                    Ok(0) | Err(_) => return,
                                    _ => {}
                                }
                                let h = h.trim_end();
                                if h.is_empty() {
                                    break;
                                }
                                if let Some(v) =
                                    h.to_ascii_lowercase().strip_prefix("range: bytes=")
                                {
                                    let mut it = v.splitn(2, '-');
                                    if let (Some(a), Some(b)) = (it.next(), it.next()) {
                                        if let (Ok(a), Ok(b)) =
                                            (a.parse::<u64>(), b.parse::<u64>())
                                        {
                                            range = Some((a, b));
                                        }
                                    }
                                }
                            }
                            log.lock().unwrap().push(format!("{line} range={range:?}"));
                            let total = body.len() as u64;
                            let resp = if line.starts_with("HEAD") {
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                                )
                                .into_bytes()
                            } else if let Some((a, b)) = range {
                                let b = b.min(total - 1);
                                let slice = &body[a as usize..=b as usize];
                                let mut r = format!(
                                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{b}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    slice.len()
                                )
                                .into_bytes();
                                r.extend_from_slice(slice);
                                r
                            } else {
                                let mut r = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                                )
                                .into_bytes();
                                r.extend_from_slice(&body);
                                r
                            };
                            if s.write_all(&resp).is_err() {
                                return;
                            }
                        }
                    });
                }
            });
        }
        let url = format!("http://{addr}/file.bin");
        let auth = Auth::default();
        // the dialog's probe: exactly one probe round on the wire
        let probe = probe_with_auth(&url, &auth).unwrap();
        {
            let log = log.lock().unwrap();
            assert_eq!(
                log.iter().filter(|l| l.starts_with("HEAD")).count(),
                1,
                "dialog probe = one HEAD: {log:?}"
            );
            assert_eq!(
                log.iter()
                    .filter(|l| l.starts_with("GET") && l.contains("file.bin"))
                    .count(),
                1,
                "dialog probe = one verify GET: {log:?}"
            );
        }
        // the handed start: workers only, NO second probe round
        log.lock().unwrap().clear();
        let dir = std::env::temp_dir().join(format!("ndm-preprobe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("a.bin");
        let dl = start_with_auth_preprobed(
            &url,
            &out,
            4,
            64 * 1024,
            auth.clone(),
            Some(probe.clone()),
        )
        .unwrap();
        assert_eq!(
            wait_terminal(&dl, Duration::from_secs(30)),
            Some(Status::Finished)
        );
        assert_eq!(std::fs::read(&out).unwrap(), *body, "content identical");
        {
            let log = log.lock().unwrap();
            assert!(
                !log.iter().any(|l| l.starts_with("HEAD")),
                "handed start must not re-probe (HEAD seen): {log:?}"
            );
            assert!(
                log.iter().all(|l| l.starts_with("GET")),
                "handed start issues worker GETs only: {log:?}"
            );
            assert!(
                !log.iter().any(|l| l.contains("range=Some((0, 0))")),
                "handed start must not re-run the 1-byte verify GET: {log:?}"
            );
            assert!(!log.is_empty(), "workers did fetch");
        }
        // control: a plain start probes again, as before
        log.lock().unwrap().clear();
        let out2 = dir.join("b.bin");
        let dl2 = start_with_auth(&url, &out2, 4, 64 * 1024, auth).unwrap();
        assert_eq!(
            wait_terminal(&dl2, Duration::from_secs(30)),
            Some(Status::Finished)
        );
        {
            let log = log.lock().unwrap();
            assert_eq!(
                log.iter().filter(|l| l.starts_with("HEAD")).count(),
                1,
                "plain start still probes: {log:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}


// ---------- 2.0.7 deep-hunt batch (engine): findings #2, #3, #7,
// #8, #9, #10, #13, #14 + engine minors ----------

#[cfg(test)]
mod audit207_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    fn pattern_body(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
    }

    fn wait_terminal(dl: &Download, deadline: Duration) -> Option<Status> {
        let t0 = Instant::now();
        loop {
            if dl.is_terminal() {
                return Some(dl.status());
            }
            if t0.elapsed() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_settled(dl: &Download, deadline: Duration) {
        let t0 = Instant::now();
        while !dl.is_settled() {
            assert!(t0.elapsed() < deadline, "download never settled");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    struct Req {
        method: String,
        path: String,
        range: Option<String>,
        headers: Vec<(String, String)>,
    }

    impl Req {
        fn header(&self, name: &str) -> Option<String> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        }
    }

    /// A loopback server whose response is fully decided by the
    /// handler closure: it receives the parsed request and returns
    /// the complete raw HTTP response.
    fn test_server<F>(handler: F) -> u16
    where
        F: Fn(&Req) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let handler = handler.clone();
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
                    let mut headers = Vec::new();
                    let mut range = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                if let Some((k, v)) = h.split_once(':') {
                                    let v = v.trim().to_string();
                                    if k.trim().eq_ignore_ascii_case("range") {
                                        range = Some(v.clone());
                                    }
                                    headers.push((k.trim().to_string(), v));
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let req = Req { method, path, range, headers };
                    let resp = handler(&req);
                    let _ = s.write_all(&resp);
                });
            }
        });
        port
    }

    fn http_resp(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n").into_bytes();
        for (k, v) in headers {
            out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        out.extend_from_slice(b"Connection: close\r\n\r\n");
        out.extend_from_slice(body);
        out
    }

    /// A proper range server; every GET answer is delayed by `delay`
    /// so tests can act while a download is genuinely in flight.
    fn delayed_range_server(body: Vec<u8>, delay: Duration) -> u16 {
        let body = Arc::new(body);
        test_server(move |req| {
            let total = body.len() as u64;
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            std::thread::sleep(delay);
            if let Some(r) = &req.range {
                let r = r.trim().strip_prefix("bytes=").unwrap_or(r);
                let (a, b) = r.split_once('-').unwrap_or(("0", ""));
                let a: u64 = a.parse().unwrap_or(0);
                let b: u64 = if b.is_empty() { total - 1 } else { b.parse().unwrap_or(total - 1) };
                let end = b.min(total - 1);
                let slice = &body[a as usize..=end as usize];
                return http_resp(
                    "206 Partial Content",
                    &[
                        ("Content-Range", format!("bytes {a}-{end}/{total}")),
                        ("Content-Length", slice.len().to_string()),
                    ],
                    slice,
                );
            }
            http_resp("200 OK", &[("Content-Length", total.to_string())], &body)
        })
    }

    // --- #14: inverted Content-Range is not a range ---

    #[test]
    fn content_range_with_end_before_start_is_rejected() {
        assert_eq!(parse_content_range("bytes 500-499/1000"), None);
        assert_eq!(parse_content_range("bytes 5-3/100"), None);
        assert_eq!(
            parse_content_range("bytes 499-500/1000"),
            Some((499, 500, Some(1000)))
        );
    }

    // --- #2: 416 must never become a 0-byte Finished ---

    #[test]
    fn range_416_refusal_is_a_clean_error_and_the_target_is_untouched() {
        let port = test_server(|req| {
            if req.method == "HEAD" {
                return http_resp("405 Method Not Allowed", &[("Content-Length", "0".to_string())], &[]);
            }
            http_resp(
                "416 Range Not Satisfiable",
                &[("Content-Range", "bytes */0".to_string()), ("Content-Length", "0".to_string())],
                &[],
            )
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let err = probe(&url).expect_err("a server refusing every request must fail the probe");
        assert!(err.contains("416"), "unexpected error: {err}");

        let out = std::env::temp_dir().join(format!("ndm-207-416-{}.bin", std::process::id()));
        std::fs::write(&out, b"PRE-EXISTING CONTENT").unwrap();
        let r = start(&url, &out, 2, 256 * 1024);
        assert!(r.is_err(), "start must fail cleanly, got a download handle");
        assert_eq!(
            std::fs::read(&out).unwrap(),
            b"PRE-EXISTING CONTENT",
            "the target must never be truncated on a 416 guess"
        );
        assert!(!state_path_for(&out).exists());
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn range_416_with_a_plain_get_fallback_downloads_the_real_file() {
        let body = pattern_body(200_000);
        let body2 = body.clone();
        let port = test_server(move |req| {
            if req.method == "HEAD" {
                return http_resp("405 Method Not Allowed", &[("Content-Length", "0".to_string())], &[]);
            }
            if req.range.is_some() {
                return http_resp(
                    "416 Range Not Satisfiable",
                    &[("Content-Range", "bytes */200000".to_string()), ("Content-Length", "0".to_string())],
                    &[],
                );
            }
            http_resp("200 OK", &[("Content-Length", body2.len().to_string())], &body2)
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let p = probe(&url).expect("plain-GET fallback must probe the real file");
        assert_eq!(p.total, 200_000);
        assert!(!p.range_ok);

        let out = std::env::temp_dir().join(format!("ndm-207-416fb-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        wait_settled(&dl, Duration::from_secs(10));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let _ = std::fs::remove_file(&out);
    }

    // --- #3: Content-Encoding must never be saved as the file ---

    #[test]
    fn gzip_probe_answers_are_rejected() {
        let port = test_server(|req| {
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", "1234".to_string()),
                        ("Content-Encoding", "gzip".to_string()),
                    ],
                    &[],
                );
            }
            http_resp(
                "206 Partial Content",
                &[
                    ("Content-Range", "bytes 0-0/200000".to_string()),
                    ("Content-Length", "1".to_string()),
                    ("Content-Encoding", "gzip".to_string()),
                ],
                &[42],
            )
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let err = probe(&url).expect_err("an encoded probe answer must fail");
        assert!(err.contains("Content-Encoding"), "unexpected error: {err}");
    }

    #[test]
    fn gzip_block_answers_fail_the_download_cleanly() {
        let body = pattern_body(1024 * 1024);
        let body2 = body.clone();
        let port = test_server(move |req| {
            let total = body2.len() as u64;
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            let r = req.range.clone().unwrap_or_default();
            if r == "bytes=0-0" {
                return http_resp(
                    "206 Partial Content",
                    &[
                        ("Content-Range", format!("bytes 0-0/{total}")),
                        ("Content-Length", "1".to_string()),
                    ],
                    &body2[0..1],
                );
            }
            // every real block: a valid-looking range answer whose
            // body is labelled gzip — the engine must refuse it, not
            // write it at the block's offset
            let r = r.trim().strip_prefix("bytes=").unwrap_or(&r).to_string();
            let (a, b) = r.split_once('-').unwrap_or(("0", "0"));
            let a: u64 = a.parse().unwrap_or(0);
            let b: u64 = b.parse().unwrap_or(0);
            let garbage = vec![0x1fu8; (b - a + 1) as usize];
            http_resp(
                "206 Partial Content",
                &[
                    ("Content-Range", format!("bytes {a}-{b}/{total}")),
                    ("Content-Length", garbage.len().to_string()),
                    ("Content-Encoding", "gzip".to_string()),
                ],
                &garbage,
            )
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = std::env::temp_dir().join(format!("ndm-207-gzip-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        assert_eq!(
            wait_terminal(&dl, Duration::from_secs(60)),
            Some(Status::Error),
            "encoded blocks must end in a clean error, got {:?}",
            dl.status()
        );
        let p = dl.progress();
        assert!(
            p.error.as_deref().unwrap_or("").contains("Content-Encoding"),
            "error must name the cause: {:?}",
            p.error
        );
        wait_settled(&dl, Duration::from_secs(10));
        drop(dl);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }

    // --- #13: credentials never cross hosts ---

    #[test]
    fn cross_host_redirect_and_resolve_drop_cookie_and_authorization() {
        let body = pattern_body(4096);
        let body2 = body.clone();
        let seen: Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let port = test_server(move |req| {
            seen2.lock().unwrap().push((
                req.path.clone(),
                req.header("Cookie"),
                req.header("Authorization"),
            ));
            let total = body2.len() as u64;
            if req.path == "/start" {
                // same server, OTHER host name: 127.0.0.1 -> localhost
                let host = req.header("Host").unwrap_or_default();
                let port = host.rsplit(':').next().unwrap_or("").to_string();
                return http_resp(
                    "302 Found",
                    &[("Location", format!("http://localhost:{port}/landing"))],
                    &[],
                );
            }
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            if let Some(r) = &req.range {
                let r = r.trim().strip_prefix("bytes=").unwrap_or(r);
                let (a, b) = r.split_once('-').unwrap_or(("0", ""));
                let a: u64 = a.parse().unwrap_or(0);
                let b: u64 = if b.is_empty() { total - 1 } else { b.parse().unwrap_or(total - 1) };
                let end = b.min(total - 1);
                let slice = &body2[a as usize..=end as usize];
                return http_resp(
                    "206 Partial Content",
                    &[
                        ("Content-Range", format!("bytes {a}-{end}/{total}")),
                        ("Content-Length", slice.len().to_string()),
                    ],
                    slice,
                );
            }
            http_resp("200 OK", &[("Content-Length", total.to_string())], &body2)
        });
        let url = format!("http://127.0.0.1:{port}/start");
        let auth = Auth {
            cookie: Some("session=secret".to_string()),
            referer: Some("http://127.0.0.1/page".to_string()),
            authorization: Some("Basic abc".to_string()),
            user_agent: None,
        };
        let _ = probe_with_auth(&url, &auth);
        let seen = seen.lock().unwrap();
        let start_reqs: Vec<_> = seen.iter().filter(|(p, _, _)| p == "/start").collect();
        assert!(!start_reqs.is_empty(), "the origin must have been asked");
        assert!(
            start_reqs.iter().all(|(_, c, a)| c.as_deref() == Some("session=secret")
                && a.as_deref() == Some("Basic abc")),
            "the origin host must receive its credentials: {start_reqs:?}"
        );
        let landing: Vec<_> = seen.iter().filter(|(p, _, _)| p == "/landing").collect();
        assert!(!landing.is_empty(), "the redirect must have been followed");
        assert!(
            landing.iter().all(|(_, c, a)| c.is_none() && a.is_none()),
            "no credential may reach the other host: {landing:?}"
        );
    }

    // --- #8b: bounded journal names ---

    #[test]
    fn state_path_for_bounds_long_names_and_keeps_legacy() {
        let dir = std::env::temp_dir().join(format!("ndm-207-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 82 Bengali characters = 246 bytes, + ".bin": the legacy
        // journal name would be 259 bytes — over the 255 limit
        let name = format!("{}.bin", "অ".repeat(82));
        let out = dir.join(&name);
        let sp = state_path_for(&out);
        let journal_name = sp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            journal_name.len() <= 255,
            "journal name must fit the filesystem limit: {} bytes",
            journal_name.len()
        );
        assert_eq!(sp, state_path_for(&out), "bounded name must be stable");
        let other = dir.join(format!("{}.bin", "আ".repeat(82)));
        assert_ne!(sp, state_path_for(&other), "different long names need different journals");
        // names within the limit keep the legacy journal name
        // untouched (the legacy file of a >255-byte name can never
        // exist — that impossibility is the bug being fixed)
        let short = dir.join("short.bin");
        assert_eq!(state_path_for(&short), legacy_state_path_for(&short));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_bengali_name_journals_and_resumes() {
        let body = pattern_body(2 * 1024 * 1024);
        let port = delayed_range_server(body.clone(), Duration::from_millis(150));
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let dir = std::env::temp_dir().join(format!("ndm-207-bn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join(format!("{}.bin", "অ".repeat(82)));
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        let t0 = Instant::now();
        while dl.progress().sealed_done_bytes == 0 {
            assert!(t0.elapsed() < Duration::from_secs(30), "no block ever sealed");
            std::thread::sleep(Duration::from_millis(25));
        }
        dl.control.cancel();
        wait_settled(&dl, Duration::from_secs(30));
        let sealed_at_cut = dl.progress().sealed_done_bytes;
        drop(dl);
        let journal = state_path_for(&out);
        assert!(journal.exists(), "the long name must still journal");
        let raw = std::fs::read(&journal).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let credited = v["done"].as_array().unwrap().iter().filter(|b| **b == 1).count() as u64;
        assert!(credited >= 1, "journal must credit the sealed blocks");
        assert_eq!(
            sealed_at_cut,
            credited * 256 * 1024,
            "sealed_done_bytes must equal exactly what the journal credits"
        );
        let dl2 = start(&url, &out, 2, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl2, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        assert!(!journal.exists(), "finished download removes its journal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- #8a: journal save failures are recorded ---

    #[test]
    fn journal_save_failure_is_recorded_on_the_download() {
        let control = Arc::new(Control {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        let dl = finished_empty_download(
            control,
            PathBuf::from("/tmp/ndm-207-no-such-dir/state.ndm.json"),
        );
        assert!(dl.last_journal_error().is_none());
        save_state(&dl.shared, true, None);
        let err = dl.last_journal_error().expect("the failed save must be recorded");
        assert!(err.contains("resume state"), "unexpected record: {err}");
    }

    // --- #9: pacing waits are sliced, halt- and limit-aware ---

    #[test]
    fn pace_wait_obeys_halt_and_a_cleared_limit() {
        let control = Arc::new(Control {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        let dl = finished_empty_download(control, PathBuf::from("/tmp/ndm-207-pace.ndm.json"));
        // a cleared limit ends the wait at once
        let t0 = Instant::now();
        let r = pace_wait(&dl.shared, Instant::now() + Duration::from_secs(30), 1000, || 0, || 0, 0);
        assert!(matches!(r, PaceWait::Cleared), "cleared limit must end the wait");
        assert!(t0.elapsed() < Duration::from_millis(500), "cleared limit must end the wait");
        // a pause ends the wait at once
        dl.control.pause();
        let t0 = Instant::now();
        let r = pace_wait(&dl.shared, Instant::now() + Duration::from_secs(30), 1000, || 1000, || 0, 0);
        assert!(matches!(r, PaceWait::Halted), "pause must interrupt the wait");
        assert!(t0.elapsed() < Duration::from_millis(500), "pause must interrupt the wait");
        dl.control.resume();
        // an ordinary short wait still waits
        let t0 = Instant::now();
        let r = pace_wait(&dl.shared, Instant::now() + Duration::from_millis(200), 1000, || 1000, || 0, 0);
        assert!(matches!(r, PaceWait::Reached), "the slot must be reached by waiting");
        assert!(t0.elapsed() >= Duration::from_millis(150), "the slot must be honoured");
    }

    // --- MINOR: per-download set_limit resets the pacing clock ---

    #[test]
    fn set_limit_change_resets_the_pacing_clock() {
        let control = Arc::new(Control {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        let dl = finished_empty_download(control, PathBuf::from("/tmp/ndm-207-limit.ndm.json"));
        dl.shared.own_limit.store(500, Ordering::Relaxed);
        *plock(&dl.shared.own_limit_state) = Some(Instant::now());
        dl.set_limit(500);
        assert!(
            plock(&dl.shared.own_limit_state).is_some(),
            "an unchanged limit keeps its clock"
        );
        dl.set_limit(900);
        assert!(
            plock(&dl.shared.own_limit_state).is_none(),
            "a changed limit must reset the pacing clock"
        );
    }

    // --- #10: one live writer per output path ---

    #[test]
    fn in_flight_registry_rejects_duplicates_and_releases() {
        let dir = std::env::temp_dir().join(format!("ndm-207-reg-{}", std::process::id()));
        let out = dir.join("f.bin");
        let g = InFlightGuard::register(&out).unwrap();
        assert!(InFlightGuard::register(&out).is_err());
        // the same file via a different spelling is the same path
        let alt = dir.join("sub/../f.bin");
        assert!(InFlightGuard::register(&alt).is_err());
        drop(g);
        let _g2 = InFlightGuard::register(&out).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn guard_drop_removes_the_lock_file_at_release() {
        let dir = std::env::temp_dir().join(format!("ndm-207-drop-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("f.bin");
        let lock = dir.join("f.bin.ndm.lock");
        let g = InFlightGuard::register(&out).unwrap();
        assert!(lock.exists(), "the live guard's lock file must exist");
        drop(g);
        assert!(
            !lock.exists(),
            "release must remove the lock file then and there, not at the next launch"
        );
        // a later download of the same path re-creates the lock
        // and its release removes it again — the lifecycle is
        // unchanged apart from the cleanup timing.
        let g2 = InFlightGuard::register(&out).unwrap();
        assert!(lock.exists(), "the next guard must re-create its lock file");
        drop(g2);
        assert!(!lock.exists(), "the second release must remove it too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- N2: stale `.ndm.lock` sweep ---

    #[cfg(unix)]
    #[test]
    fn sweep_stale_locks_removes_an_unlocked_lock_file() {
        let dir = std::env::temp_dir().join(format!("ndm-207-sweep-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("x.bin.ndm.lock");
        std::fs::write(&lock, b"").unwrap();
        assert_eq!(sweep_stale_locks(&dir), 1);
        assert!(!lock.exists(), "an unlocked stale lock file must be swept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_stale_locks_keeps_a_lock_held_by_a_live_guard() {
        let dir = std::env::temp_dir().join(format!("ndm-207-sweep-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("f.bin");
        let g = InFlightGuard::register(&out).unwrap();
        let lock = dir.join("f.bin.ndm.lock");
        assert!(lock.exists(), "the live guard's lock file must exist");
        assert_eq!(sweep_stale_locks(&dir), 0);
        assert!(lock.exists(), "a flocked lock file must survive the sweep");
        drop(g);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_stale_locks_leaves_other_files_untouched() {
        let dir = std::env::temp_dir().join(format!("ndm-207-sweep-c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let keep = [
            dir.join("keep.bin"),
            dir.join("keep.ndm.json"),
            dir.join("other.lock"),
            dir.join("x.ndm.lock.bak"),
        ];
        for p in &keep {
            std::fs::write(p, b"data").unwrap();
        }
        assert_eq!(sweep_stale_locks(&dir), 0);
        for p in &keep {
            assert!(p.exists(), "non-lock file {} must be untouched", p.display());
            assert_eq!(std::fs::read(p).unwrap(), b"data");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_start_on_the_same_path_fails_fast() {
        let body = pattern_body(8 * 1024 * 1024);
        let port = delayed_range_server(body.clone(), Duration::from_millis(200));
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = std::env::temp_dir().join(format!("ndm-207-dupe-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        let err = start(&url, &out, 4, 256 * 1024)
            .err()
            .expect("a second live start on the same path must fail");
        assert!(err.contains("already writing"), "unexpected error: {err}");
        dl.control.cancel();
        wait_settled(&dl, Duration::from_secs(30));
        drop(dl);
        // after the first download settles, the path is free again
        let dl2 = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl2, Duration::from_secs(90)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let _ = std::fs::remove_file(&out);
    }

    // --- #7: sealed_done_bytes on a finished download ---

    #[test]
    fn sealed_done_bytes_reaches_total_on_finish() {
        let dl = finished_empty_download(
            Arc::new(Control {
                paused: AtomicBool::new(false),
                cancelled: AtomicBool::new(false),
            }),
            PathBuf::from("/tmp/ndm-207-sealed.ndm.json"),
        );
        let p = dl.progress();
        assert_eq!(p.sealed_done_bytes, p.total);
    }
}

// ---------- 2.0.8 fix batch (engine worker): C-2, M-1, M-2,
// M-13/M-14, F15 sync policy, DIAG lying-header, m-5, m-1, G1 ----------

#[cfg(test)]
mod audit208_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    fn pattern_body(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
    }

    fn wait_terminal(dl: &Download, deadline: Duration) -> Option<Status> {
        let t0 = Instant::now();
        loop {
            if dl.is_terminal() {
                return Some(dl.status());
            }
            if t0.elapsed() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_settled(dl: &Download, deadline: Duration) {
        let t0 = Instant::now();
        while !dl.is_settled() {
            assert!(t0.elapsed() < deadline, "download never settled");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    struct Req {
        method: String,
        range: Option<String>,
    }

    fn test_server<F>(handler: F) -> u16
    where
        F: Fn(&Req) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let handler = handler.clone();
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
                    let _path = parts.next().unwrap_or("/").to_string();
                    let mut range = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                if let Some((k, v)) = h.split_once(':') {
                                    let v = v.trim().to_string();
                                    if k.trim().eq_ignore_ascii_case("range") {
                                        range = Some(v.clone());
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let req = Req { method, range };
                    let resp = handler(&req);
                    let _ = s.write_all(&resp);
                });
            }
        });
        port
    }

    fn http_resp(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n").into_bytes();
        for (k, v) in headers {
            out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        out.extend_from_slice(b"Connection: close\r\n\r\n");
        out.extend_from_slice(body);
        out
    }

    /// A proper range server; every GET is delayed by `delay`.
    fn delayed_range_server(body: Vec<u8>, delay: Duration) -> u16 {
        let body = Arc::new(body);
        test_server(move |req| {
            let total = body.len() as u64;
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            std::thread::sleep(delay);
            if let Some(r) = &req.range {
                let r = r.trim().strip_prefix("bytes=").unwrap_or(r);
                let (a, b) = r.split_once('-').unwrap_or(("0", ""));
                let a: u64 = a.parse().unwrap_or(0);
                let b: u64 = if b.is_empty() { total - 1 } else { b.parse().unwrap_or(total - 1) };
                let end = b.min(total - 1);
                let slice = &body[a as usize..=end as usize];
                return http_resp(
                    "206 Partial Content",
                    &[
                        ("Content-Range", format!("bytes {a}-{end}/{total}")),
                        ("Content-Length", slice.len().to_string()),
                    ],
                    slice,
                );
            }
            http_resp("200 OK", &[("Content-Length", total.to_string())], &body)
        })
    }

    fn tmp_out(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ndm-208-{tag}-{}.bin", std::process::id()))
    }

    // --- item 5: journal sync policy (pure) ---

    #[test]
    fn journal_sync_policy_bounds_the_fsync_cadence() {
        // forced/terminal saves are always durable
        assert!(sync_is_durable(true, 0, Duration::ZERO));
        // a periodic save below both thresholds syncs nothing
        assert!(!sync_is_durable(false, 0, Duration::ZERO));
        assert!(!sync_is_durable(
            false,
            SYNC_EVERY_BYTES - 1,
            SYNC_EVERY - Duration::from_millis(1)
        ));
        // either threshold crossing makes the save durable
        assert!(sync_is_durable(false, SYNC_EVERY_BYTES, Duration::ZERO));
        assert!(sync_is_durable(false, 0, SYNC_EVERY));
    }

    // --- item 4: pacer math (pure-ish, own-limit timeline) ---

    fn shared_with_own_limit(rate: u64) -> Arc<Download> {
        let dl = finished_empty_download(
            Arc::new(Control {
                paused: AtomicBool::new(false),
                cancelled: AtomicBool::new(false),
            }),
            PathBuf::from("/tmp/ndm-208-pace.ndm.json"),
        );
        dl.shared.own_limit.store(rate, Ordering::Relaxed);
        dl
    }

    #[test]
    fn pace_reserve_grants_one_slice_at_once_then_paces() {
        // 1 MiB/s, 100 KiB reservations: the first is granted
        // immediately (its slot starts now), the rest are spaced
        // ~0.1 s apart by the shared timeline — the limit holds
        // from byte 0 instead of the first reads escaping free
        let dl = shared_with_own_limit(1_000_000);
        let t0 = Instant::now();
        assert!(pace_reserve(
            &dl.shared,
            &dl.shared.own_limit_state,
            100_000,
            || dl.shared.own_limit.load(Ordering::Relaxed),
            || dl.shared.own_limit_gen.load(Ordering::Relaxed),
            || dl.shared.own_limit_oldest_live.load(Ordering::Relaxed),
        ));
        assert!(
            t0.elapsed() < Duration::from_millis(60),
            "first slice must be granted at once, took {:?}",
            t0.elapsed()
        );
        for _ in 0..4 {
            assert!(pace_reserve(
                &dl.shared,
                &dl.shared.own_limit_state,
                100_000,
                || dl.shared.own_limit.load(Ordering::Relaxed),
                || dl.shared.own_limit_gen.load(Ordering::Relaxed),
                || dl.shared.own_limit_oldest_live.load(Ordering::Relaxed),
            ));
        }
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(300),
            "5 paced grants at 1 MiB/s must take ~0.4 s, took {elapsed:?} (burst!)"
        );
        assert!(elapsed < Duration::from_secs(3), "grants over-paced: {elapsed:?}");
    }

    #[test]
    fn pace_reserve_limit_change_takes_effect_promptly() {
        // a reservation parked for ~30 s at 1 KB/s must be released
        // within ~2 s of the limit being cleared (M-14), and a
        // reservation parked at a low rate must re-reserve promptly
        // when the limit is raised — never sleep out the old slot.
        // (The FIRST reservation on a fresh timeline grants at
        // once; it is the SECOND that parks behind it — so each
        // half pre-charges once before parking the waiter.)
        let dl = shared_with_own_limit(1000);
        assert!(pace_reserve(
            &dl.shared,
            &dl.shared.own_limit_state,
            PACE_SLICE,
            || dl.shared.own_limit.load(Ordering::Relaxed),
            || dl.shared.own_limit_gen.load(Ordering::Relaxed),
            || dl.shared.own_limit_oldest_live.load(Ordering::Relaxed),
        ));
        let dl2 = dl.clone();
        let waiter = std::thread::spawn(move || {
            let t0 = Instant::now();
            let ok = pace_reserve(
                &dl2.shared,
                &dl2.shared.own_limit_state,
                PACE_SLICE,
                || dl2.shared.own_limit.load(Ordering::Relaxed),
                || dl2.shared.own_limit_gen.load(Ordering::Relaxed),
                || dl2.shared.own_limit_oldest_live.load(Ordering::Relaxed),
            );
            (ok, t0.elapsed())
        });
        std::thread::sleep(Duration::from_millis(300));
        dl.set_limit(0); // cleared entirely
        let (ok, elapsed) = waiter.join().unwrap();
        assert!(ok, "a cleared limit still grants the read");
        assert!(
            elapsed < Duration::from_secs(2),
            "clearing the limit took {elapsed:?} to reach a parked reservation"
        );

        let dl = shared_with_own_limit(1000);
        assert!(pace_reserve(
            &dl.shared,
            &dl.shared.own_limit_state,
            PACE_SLICE,
            || dl.shared.own_limit.load(Ordering::Relaxed),
            || dl.shared.own_limit_gen.load(Ordering::Relaxed),
            || dl.shared.own_limit_oldest_live.load(Ordering::Relaxed),
        ));
        let dl2 = dl.clone();
        let waiter = std::thread::spawn(move || {
            let t0 = Instant::now();
            let ok = pace_reserve(
                &dl2.shared,
                &dl2.shared.own_limit_state,
                PACE_SLICE,
                || dl2.shared.own_limit.load(Ordering::Relaxed),
                || dl2.shared.own_limit_gen.load(Ordering::Relaxed),
                || dl2.shared.own_limit_oldest_live.load(Ordering::Relaxed),
            );
            (ok, t0.elapsed())
        });
        std::thread::sleep(Duration::from_millis(300));
        dl.set_limit(10_000_000); // raised 10000x
        let (ok, elapsed) = waiter.join().unwrap();
        assert!(ok);
        assert!(
            elapsed < Duration::from_secs(2),
            "raising the limit took {elapsed:?} to reach a parked reservation"
        );
    }

    #[test]
    fn pace_reserve_rebase_clamps_stacked_debt() {
        // D7: a waiter parked at a low rate whose limit changes
        // mid-flight re-reserves as a RE-BASE — it must not queue
        // behind finish-line debt stacked onto the fresh timeline
        // in the same instant (here: a fake 60 s of peer debt,
        // landed before the waiter's re-reserve in every realistic
        // interleaving — the waiter needs a 50 ms slice boundary
        // to even notice the change). Without the clamp its slot
        // is that 60 s; with it, at most one slice-cost out.
        // D7-v2 setup note: the stale-generation gate only clamps
        // a charge that PREDATES the oldest-live watermark, and a
        // single bump records the waiter's own generation as that
        // watermark (live, not stale) — so this test now bumps
        // TWICE back-to-back below, within the waiter's one
        // notice slice, which is what makes its gen-0 charge
        // stale. The asserted semantics are unchanged.
        let dl = shared_with_own_limit(1000);
        assert!(pace_reserve(
            &dl.shared,
            &dl.shared.own_limit_state,
            PACE_SLICE,
            || dl.shared.own_limit.load(Ordering::Relaxed),
            || dl.shared.own_limit_gen.load(Ordering::Relaxed),
            || dl.shared.own_limit_oldest_live.load(Ordering::Relaxed),
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        let dl2 = dl.clone();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let ok = pace_reserve(
                &dl2.shared,
                &dl2.shared.own_limit_state,
                PACE_SLICE,
                || dl2.shared.own_limit.load(Ordering::Relaxed),
                || dl2.shared.own_limit_gen.load(Ordering::Relaxed),
                || dl2.shared.own_limit_oldest_live.load(Ordering::Relaxed),
            );
            let _ = tx.send((ok, t0.elapsed()));
        });
        std::thread::sleep(Duration::from_millis(300));
        dl.set_limit(1_000_000); // changed: timeline reset, gen bumped, watermark = gen 0
        dl.set_limit(999_999); // bumped again: watermark = gen 1 — the gen-0 charge is now stale
        *plock(&dl.shared.own_limit_state) =
            Some(Instant::now() + Duration::from_secs(60));
        let got = rx.recv_timeout(Duration::from_secs(3));
        // the clamped re-reserve rides within the stacked debt —
        // the finish line itself must not have moved backward
        let finish = (*plock(&dl.shared.own_limit_state))
            .expect("the stacked timeline must still be armed");
        assert!(
            finish > Instant::now() + Duration::from_secs(55),
            "re-based reservation moved the finish line backward: {finish:?}"
        );
        match got {
            Ok((ok, elapsed)) => {
                assert!(ok);
                assert!(
                    elapsed < Duration::from_secs(3),
                    "re-based reservation queued behind stacked debt: {elapsed:?}"
                );
            }
            Err(_) => {
                dl.set_limit(0); // release the parked waiter
                panic!("re-based reservation stayed parked behind stacked debt");
            }
        }
    }

    #[test]
    fn pace_reserve_rebase_refused_for_live_generation() {
        // D7-v2 gate, live side: a waiter whose charge was made
        // under the CURRENT (live) generation gets NO clamp when
        // a bump interrupts it. The single bump below records the
        // waiter's own generation (gen 0) as the oldest-live
        // watermark, and a charge made under the watermark
        // generation itself is live — its re-reserve queues
        // behind stacked debt exactly like a steady-state
        // reservation. (Under the v1 changed-flag this same
        // waiter clamped and was granted within milliseconds.)
        let dl = shared_with_own_limit(1000);
        assert!(pace_reserve(
            &dl.shared,
            &dl.shared.own_limit_state,
            PACE_SLICE,
            || dl.shared.own_limit.load(Ordering::Relaxed),
            || dl.shared.own_limit_gen.load(Ordering::Relaxed),
            || dl.shared.own_limit_oldest_live.load(Ordering::Relaxed),
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        let dl2 = dl.clone();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let ok = pace_reserve(
                &dl2.shared,
                &dl2.shared.own_limit_state,
                PACE_SLICE,
                || dl2.shared.own_limit.load(Ordering::Relaxed),
                || dl2.shared.own_limit_gen.load(Ordering::Relaxed),
                || dl2.shared.own_limit_oldest_live.load(Ordering::Relaxed),
            );
            let _ = tx.send((ok, t0.elapsed()));
        });
        std::thread::sleep(Duration::from_millis(300));
        dl.set_limit(1_000_000); // ONE bump: gen 1, watermark = gen 0 (the waiter's charge gen)
        *plock(&dl.shared.own_limit_state) =
            Some(Instant::now() + Duration::from_secs(60));
        // no grant may arrive while the stacked debt stands
        if let Ok((ok, elapsed)) = rx.recv_timeout(Duration::from_secs(3)) {
            dl.set_limit(0);
            panic!(
                "live-generation re-reserve was clamped (ok={ok}, granted in {elapsed:?})"
            );
        }
        // the refused re-reserve queued behind the debt — the
        // finish line itself must not have moved backward either
        let finish = (*plock(&dl.shared.own_limit_state))
            .expect("the stacked timeline must still be armed");
        assert!(
            finish > Instant::now() + Duration::from_secs(55),
            "finish line moved backward: {finish:?}"
        );
        dl.set_limit(0); // release the parked waiter
        let (ok, _) = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the released waiter must finish");
        assert!(ok);
    }

    #[test]
    fn pace_reserve_rebase_stops_after_first_stale_generation() {
        // D7-v2 gate, aggregate bound: two successive bump epochs
        // grant a waiter AT MOST ONE clamp in total. Epoch 1 is a
        // rapid double bump that makes the waiter's gen-0 charge
        // stale — the clamp fires and the waiter is granted at
        // once despite 60 s of stacked debt (that grant is its
        // one slice of credit). Epoch 2 is a single bump
        // interrupting the waiter's NEXT charge, made under the
        // then-live generation (gen 2) — the gate refuses, the
        // re-reserve queues behind fresh stacked debt, and no
        // second credit is granted: the waiter's total advance
        // over the unclamped schedule stays at the one slice.
        let dl = shared_with_own_limit(1000);
        assert!(pace_reserve(
            &dl.shared,
            &dl.shared.own_limit_state,
            PACE_SLICE,
            || dl.shared.own_limit.load(Ordering::Relaxed),
            || dl.shared.own_limit_gen.load(Ordering::Relaxed),
            || dl.shared.own_limit_oldest_live.load(Ordering::Relaxed),
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        let dl2 = dl.clone();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let t0 = Instant::now();
                let ok = pace_reserve(
                    &dl2.shared,
                    &dl2.shared.own_limit_state,
                    PACE_SLICE,
                    || dl2.shared.own_limit.load(Ordering::Relaxed),
                    || dl2.shared.own_limit_gen.load(Ordering::Relaxed),
                    || dl2.shared.own_limit_oldest_live.load(Ordering::Relaxed),
                );
                if tx.send((ok, t0.elapsed())).is_err() {
                    return;
                }
            }
        });
        std::thread::sleep(Duration::from_millis(300));
        // epoch 1: rapid double bump — the gen-0 charge goes stale
        dl.set_limit(1_000_000); // gen 1, watermark = gen 0
        dl.set_limit(999_999); // gen 2, watermark = gen 1 — gen 0 predates it
        *plock(&dl.shared.own_limit_state) =
            Some(Instant::now() + Duration::from_secs(60));
        let (ok, elapsed) = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the stale waiter's clamped grant must land");
        assert!(ok);
        assert!(
            elapsed < Duration::from_secs(3),
            "the one stale clamp must free the waiter at once, took {elapsed:?}"
        );
        // the waiter's second call has now charged under the live
        // generation (gen 2); give it a slice boundary to park
        std::thread::sleep(Duration::from_millis(300));
        // epoch 2: a single bump — the watermark becomes gen 2,
        // the live charge's own generation: the gate must refuse
        dl.set_limit(2_000_000);
        *plock(&dl.shared.own_limit_state) =
            Some(Instant::now() + Duration::from_secs(60));
        if let Ok((ok, elapsed)) = rx.recv_timeout(Duration::from_secs(3)) {
            dl.set_limit(0);
            panic!(
                "second epoch clamped again (ok={ok}, granted in {elapsed:?}) — credit granted twice"
            );
        }
        let finish = (*plock(&dl.shared.own_limit_state))
            .expect("the stacked timeline must still be armed");
        assert!(
            finish > Instant::now() + Duration::from_secs(55),
            "finish line moved backward: {finish:?}"
        );
        dl.set_limit(0); // release the parked waiter
        let (ok, _) = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the released waiter must finish");
        assert!(ok);
    }

    // --- item 3: canonical key + symlink guard ---

    #[test]
    fn canonical_key_resolves_symlinked_directories() {
        let base = std::env::temp_dir().join(format!("ndm-208-canon-{}", std::process::id()));
        let real = base.join("real");
        let linkdir = base.join("linkdir");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&linkdir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, linkdir.join("alias")).unwrap();
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        #[cfg(unix)]
        {
            let a = canonical_key(&real.join("f.bin"));
            let b = canonical_key(&linkdir.join("alias").join("f.bin"));
            assert_eq!(a, b, "symlink spelling must share the guard key");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn second_start_via_symlink_spelling_fails_fast() {
        let body = pattern_body(8 * 1024 * 1024);
        let port = delayed_range_server(body.clone(), Duration::from_millis(200));
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let base = std::env::temp_dir().join(format!("ndm-208-sym-{}", std::process::id()));
        let real = base.join("real");
        let linkdir = base.join("linkdir");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&linkdir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, linkdir.join("alias")).unwrap();
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let out_real = real.join("f.bin");
        let dl = start(&url, &out_real, 4, 256 * 1024).unwrap();
        #[cfg(unix)]
        {
            let out_link = linkdir.join("alias").join("f.bin");
            let err = start(&url, &out_link, 4, 256 * 1024)
                .err()
                .expect("a second start via a symlink spelling must fail");
            assert!(err.contains("already writing"), "unexpected error: {err}");
        }
        dl.control.cancel();
        wait_settled(&dl, Duration::from_secs(30));
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- item 3: cross-process guard (child does the attempt) ---

    #[test]
    fn inflight_cross_process_child() {
        // runs only inside the spawned child (env-guarded); in the
        // main run this is a no-op so it cannot recurse
        let Ok(url) = std::env::var("NDM208_XPROC_URL") else {
            return;
        };
        let out = std::env::var("NDM208_XPROC_OUT").unwrap();
        let r = start(&url, Path::new(&out), 2, 256 * 1024);
        assert!(
            r.is_err(),
            "a second PROCESS starting on the same path must be rejected"
        );
        let err = r.err().unwrap();
        assert!(err.contains("already writing"), "unexpected error: {err}");
    }

    #[test]
    fn inflight_guard_holds_across_processes() {
        if std::env::var("NDM208_XPROC_URL").is_ok() {
            return; // we ARE the child; the sibling test did the work
        }
        let body = pattern_body(8 * 1024 * 1024);
        let port = delayed_range_server(body.clone(), Duration::from_millis(250));
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("xproc");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        let exe = std::env::current_exe().unwrap();
        let status = std::process::Command::new(exe)
            .args(["inflight_cross_process_child", "--nocapture"])
            .env("NDM208_XPROC_URL", &url)
            .env("NDM208_XPROC_OUT", out.to_string_lossy().into_owned())
            .status()
            .expect("child test binary must run");
        assert!(status.success(), "child process guard check failed: {status}");
        dl.control.cancel();
        wait_settled(&dl, Duration::from_secs(30));
        drop(dl);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }

    // --- item 1 (C-2): a worker parked on a held connection ---

    /// Range server whose handler for ranges starting at or after
    /// `hang_from` accepts the request and then never answers and
    /// never closes (the handler thread sleeps with the socket
    /// open — the F13 shape).
    fn hang_tail_server(body: Vec<u8>, hang_from: u64) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = Arc::new(body);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    let (rs, re) = range.unwrap_or((0, total - 1));
                    if re >= hang_from && !(rs == 0 && re == 0) {
                        // parked: any request whose range REACHES the
                        // hanging tail (a multi-block run including it
                        // counts) gets no response, socket held open
                        std::thread::sleep(Duration::from_secs(600));
                        return;
                    }
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        )
                        .as_bytes(),
                    );
                    let _ = s.write_all(slice);
                });
            }
        });
        port
    }

    #[test]
    fn parked_worker_cannot_wedge_settlement() {
        // 8 blocks; only the LAST block's range hangs. Every other
        // block lands, then one worker parks inside its request.
        // Pre-fix: the supervisor's join never returned — settled
        // never fired, the journal's final save never landed and
        // the path stayed locked for the process's life.
        let body = pattern_body(2 * 1024 * 1024);
        let block = 256 * 1024u64;
        let port = hang_tail_server(body.clone(), 7 * block);
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("hang");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let t0 = Instant::now();
        let dl = start(&url, &out, 4, block).unwrap();
        // terminal via the stall policy (~30 s), settled within
        // the grace that follows — bounded, never eternal
        let st = wait_terminal(&dl, Duration::from_secs(90));
        assert_eq!(st, Some(Status::Error), "status: {:?}", dl.status());
        wait_settled(&dl, Duration::from_secs(30));
        assert!(
            t0.elapsed() < Duration::from_secs(100),
            "settlement took {:?} — not bounded",
            t0.elapsed()
        );
        let p = dl.progress();
        assert!(
            p.error.as_deref().unwrap_or("").contains("no progress"),
            "error must name the stall: {:?}",
            p.error
        );
        // the final journal save landed: the journal exists and
        // credits exactly the blocks that sealed before the hang
        let journal = state_path_for(&out);
        assert!(journal.exists(), "the final save must land even when a worker is parked");
        let raw = std::fs::read(&journal).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let credited = v["done"].as_array().unwrap().iter().filter(|b| **b == 1).count();
        assert!(credited >= 1 && credited < 8, "journal credits {credited}/8 blocks");
        drop(dl);
        // the guard was released with the settlement: the same
        // path accepts a fresh start immediately
        let dl2 = start(&url, &out, 2, block).expect("guard must release on settle");
        dl2.control.cancel();
        wait_settled(&dl2, Duration::from_secs(60));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }

    // --- item 2 (M-1): 416 demotion must not destroy sealed progress ---

    fn write_journal(out: &Path, url: &str, total: u64, block: u64, done: &[u8]) {
        let v = serde_json::json!({
            "url": url, "total": total, "block_size": block,
            "etag": null, "last_modified": null, "done": done,
        });
        std::fs::write(state_path_for(out), serde_json::to_vec(&v).unwrap()).unwrap();
    }

    #[test]
    fn resume_against_416_server_preserves_partial_and_journal() {
        // the F1 shape: 12 blocks were sealed into a preallocated
        // partial; the server now 416s every range and its plain
        // GET truncates at 1000 bytes. Pre-fix, the single-stream
        // demotion File::create-truncated the partial, then failed
        // short — sealed progress destroyed, journal orphaned.
        let total = 1024 * 1024u64;
        let block = 256 * 1024u64;
        let original = pattern_body(total as usize);
        let original2 = original.clone();
        let port = test_server(move |req| {
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            if req.range.is_some() {
                return http_resp(
                    "416 Range Not Satisfiable",
                    &[
                        ("Content-Range", format!("bytes */{total}")),
                        ("Content-Length", "0".to_string()),
                    ],
                    &[],
                );
            }
            // plain GET: correct length header, truncated body
            http_resp(
                "200 OK",
                &[("Content-Length", total.to_string())],
                &original2[..1000],
            )
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("r416");
        std::fs::write(&out, &original).unwrap();
        write_journal(&out, &url, total, block, &[1, 1, 0, 0]);
        let dl = start(&url, &out, 2, block).unwrap();
        assert_eq!(
            wait_terminal(&dl, Duration::from_secs(90)),
            Some(Status::Error),
            "the truncated plain body must fail, got {:?}",
            dl.status()
        );
        wait_settled(&dl, Duration::from_secs(10));
        drop(dl);
        assert_eq!(
            std::fs::read(&out).unwrap(),
            original,
            "the partial must survive a failed demotion byte-for-byte"
        );
        let journal = state_path_for(&out);
        assert!(journal.exists(), "the journal must survive too");
        let raw = std::fs::read(&journal).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["done"], serde_json::json!([1, 1, 0, 0]));
        let spool = {
            let mut s = out.clone().into_os_string();
            s.push(".ndm.spool");
            PathBuf::from(s)
        };
        assert!(!spool.exists(), "the spool must be cleaned up after a failed attempt");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&journal);
    }

    #[test]
    fn demoted_single_stream_success_replaces_and_clears_journal() {
        // same demotion, but the plain GET is healthy: the spool
        // replaces the partial, the file is byte-exact and the
        // stale journal (which described the OLD file) is removed
        let total = 512 * 1024u64;
        let block = 256 * 1024u64;
        let body = pattern_body(total as usize);
        let body2 = body.clone();
        let port = test_server(move |req| {
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            if req.range.is_some() {
                return http_resp(
                    "416 Range Not Satisfiable",
                    &[
                        ("Content-Range", format!("bytes */{total}")),
                        ("Content-Length", "0".to_string()),
                    ],
                    &[],
                );
            }
            http_resp("200 OK", &[("Content-Length", total.to_string())], &body2)
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("r416ok");
        std::fs::write(&out, vec![0u8; total as usize]).unwrap();
        write_journal(&out, &url, total, block, &[1, 0]);
        let dl = start(&url, &out, 2, block).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        wait_settled(&dl, Duration::from_secs(10));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        assert!(
            !state_path_for(&out).exists(),
            "a finished download must not leave the stale journal behind"
        );
        let _ = std::fs::remove_file(&out);
    }

    // --- item 7 (m-5): single-stream definitive errors fail fast ---

    #[test]
    fn single_stream_encoding_veto_fails_fast_without_retries() {
        // gzipplain shape: ranges are refused by a 200 full body,
        // the plain GET is encoding-labelled garbage. Pre-fix the
        // single-stream loop retried the proven-permanent veto 5x
        // (~15 s of backoff); now it must fail on the FIRST plain
        // GET, and a pre-existing target stays untouched (the veto
        // precedes any create).
        let total = 200_000u64;
        let plain_gets = Arc::new(AtomicUsize::new(0));
        let counter = plain_gets.clone();
        let port = test_server(move |req| {
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            if req.range.is_some() {
                // a 200 to the verify GET: range_ok = false, demote
                return http_resp(
                    "200 OK",
                    &[("Content-Length", total.to_string())],
                    &vec![7u8; 100],
                );
            }
            counter.fetch_add(1, Ordering::Relaxed);
            http_resp(
                "200 OK",
                &[
                    ("Content-Length", "50".to_string()),
                    ("Content-Encoding", "gzip".to_string()),
                ],
                &vec![0x1fu8; 50],
            )
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("ssfast");
        std::fs::write(&out, b"PRE-EXISTING").unwrap();
        let _ = std::fs::remove_file(state_path_for(&out));
        let t0 = Instant::now();
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(30)), Some(Status::Error));
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "a permanent error must fail fast, took {:?}",
            t0.elapsed()
        );
        assert_eq!(
            plain_gets.load(Ordering::Relaxed),
            1,
            "exactly one plain GET — no retries of a definitive answer"
        );
        assert_eq!(std::fs::read(&out).unwrap(), b"PRE-EXISTING");
        wait_settled(&dl, Duration::from_secs(10));
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn single_error_classification_matches_the_ranged_path() {
        assert!(single_error_is_permanent(
            "server answered with Content-Encoding: gzip despite an identity request"
        ));
        assert!(single_error_is_permanent("http: http://x/f: status code 404"));
        assert!(single_error_is_permanent("http: http://x/f: status code 410"));
        assert!(!single_error_is_permanent("read: response body closed before all bytes were read"));
        assert!(!single_error_is_permanent("short download: got 3 of 9 bytes"));
    }

    #[test]
    fn single_stall_clock_resets_only_per_256kib_written() {
        let old = Instant::now() - Duration::from_secs(60);
        let mut watermark = 0u64;
        let mut clock = old;
        // a trickle below the threshold leaves the old clock alone
        assert!(!note_stall_progress(
            STALL_PROGRESS_BYTES - 1,
            &mut watermark,
            &mut clock
        ));
        assert_eq!(watermark, 0);
        assert_eq!(clock, old);
        // reaching the threshold resets both watermark and clock
        assert!(note_stall_progress(
            STALL_PROGRESS_BYTES,
            &mut watermark,
            &mut clock
        ));
        assert_eq!(watermark, STALL_PROGRESS_BYTES);
        assert!(clock.elapsed() <= STALL_LIMIT);
        // the stall error itself is retryable, not permanent
        assert!(!single_error_is_permanent(
            "no progress for 30s — the server stopped sending data"
        ));
    }

    // --- item 6 (DIAG c): the lying Content-Encoding header ---

    /// A range server that stamps Content-Encoding: gzip on EVERY
    /// response while sending identity bodies (the lying header).
    fn lying_header_server(body: Vec<u8>, head_ok: bool) -> u16 {
        let body = Arc::new(body);
        test_server(move |req| {
            let total = body.len() as u64;
            if req.method == "HEAD" {
                if !head_ok {
                    return http_resp("405 Method Not Allowed", &[("Content-Length", "0".to_string())], &[]);
                }
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            if let Some(r) = &req.range {
                let r = r.trim().strip_prefix("bytes=").unwrap_or(r);
                let (a, b) = r.split_once('-').unwrap_or(("0", ""));
                let a: u64 = a.parse().unwrap_or(0);
                let b: u64 = if b.is_empty() { total - 1 } else { b.parse().unwrap_or(total - 1) };
                let end = b.min(total - 1);
                let slice = &body[a as usize..=end as usize];
                return http_resp(
                    "206 Partial Content",
                    &[
                        ("Content-Range", format!("bytes {a}-{end}/{total}")),
                        ("Content-Length", slice.len().to_string()),
                        ("Content-Encoding", "gzip".to_string()),
                    ],
                    slice,
                );
            }
            http_resp(
                "200 OK",
                &[
                    ("Content-Length", total.to_string()),
                    ("Content-Encoding", "gzip".to_string()),
                ],
                &body,
            )
        })
    }

    #[test]
    fn lying_encoding_header_keeps_range_support_via_head() {
        let body = pattern_body(1024 * 1024);
        let port = lying_header_server(body.clone(), true);
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let p = probe(&url).expect("the lying-header proof must pass");
        assert!(p.range_ok, "a proven liar must keep range support");
        assert!(p.ignore_encoding_header);
        let out = tmp_out("liehead");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body, "content must be byte-exact");
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn lying_encoding_header_keeps_range_support_via_probe_get() {
        // HEAD refused: the probe's own range GET carries the
        // lying header — pre-fix this failed the probe Definitively
        let body = pattern_body(1024 * 1024);
        let port = lying_header_server(body.clone(), false);
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let p = probe(&url).expect("the probe GET proof must pass");
        assert!(p.range_ok);
        assert!(p.ignore_encoding_header);
        let out = tmp_out("lieget");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn true_encoder_still_fails_safe_and_fast() {
        // a TRUE encoder: the verify answer's body is a 21-byte
        // encoded blob (never 1 byte), the plain GET is encoded
        // garbage too. The proof must fail, the demotion must hit
        // the single-stream veto, and that veto is Permanent — the
        // pre-existing target stays byte-identical.
        let total = 200_000u64;
        let port = test_server(move |req| {
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            if req.range.is_some() {
                return http_resp(
                    "206 Partial Content",
                    &[
                        ("Content-Range", format!("bytes 0-0/{total}")),
                        ("Content-Length", "21".to_string()),
                        ("Content-Encoding", "gzip".to_string()),
                    ],
                    &vec![0x1fu8; 21],
                );
            }
            http_resp(
                "200 OK",
                &[
                    ("Content-Length", "21".to_string()),
                    ("Content-Encoding", "gzip".to_string()),
                ],
                &vec![0x1fu8; 21],
            )
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let p = probe(&url).expect("probe falls back, does not fail");
        assert!(!p.range_ok, "a true encoder must not keep range support");
        assert!(!p.ignore_encoding_header);
        let out = tmp_out("trueenc");
        std::fs::write(&out, b"PRE-EXISTING").unwrap();
        let t0 = Instant::now();
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(30)), Some(Status::Error));
        assert!(t0.elapsed() < Duration::from_secs(10), "must fail fast");
        assert_eq!(std::fs::read(&out).unwrap(), b"PRE-EXISTING");
        wait_settled(&dl, Duration::from_secs(10));
        let _ = std::fs::remove_file(&out);
    }

    // --- item 8 (m-1 / F7): finishing empty deletes the journal ---

    #[test]
    fn finishing_as_empty_deletes_the_old_journal() {
        let total = 1024 * 1024u64;
        let port = test_server(move |req| {
            if req.method == "HEAD" {
                return http_resp("405 Method Not Allowed", &[("Content-Length", "0".to_string())], &[]);
            }
            if req.range.is_some() {
                return http_resp(
                    "416 Range Not Satisfiable",
                    &[
                        ("Content-Range", format!("bytes */{total}")),
                        ("Content-Length", "0".to_string()),
                    ],
                    &[],
                );
            }
            http_resp("200 OK", &[("Content-Length", "0".to_string())], &[])
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("emptyfin");
        std::fs::write(&out, vec![0u8; total as usize]).unwrap();
        write_journal(&out, &url, total, 256 * 1024, &[1, 1, 0, 0]);
        let journal = state_path_for(&out);
        assert!(journal.exists());
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(30)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap().len(), 0);
        assert!(!journal.exists(), "the stale journal must be deleted on an empty finish");
        let _ = std::fs::remove_file(&out);
    }

    /// A range server that drips each body in 64 KiB pieces with
    /// `drip` between them, so a download stays observably in the
    /// receiving phase instead of flashing past it.
    fn drip_range_server(body: Vec<u8>, drip: Duration) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = Arc::new(body);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    let (rs, re) = range.unwrap_or((0, total - 1));
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        )
                        .as_bytes(),
                    );
                    for piece in slice.chunks(64 * 1024) {
                        std::thread::sleep(drip);
                        if s.write_all(piece).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    // --- item 9 (G1): per-connection surface on a live download ---

    #[test]
    fn progress_exposes_conn_phase_partial_and_block() {
        let body = pattern_body(4 * 1024 * 1024);
        let port = drip_range_server(body.clone(), Duration::from_millis(30));
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("g1");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        // sample while live: the G1 vectors exist, are sized to
        // the connection count, and at least one connection is
        // seen receiving (phase 2) on a real block (>= 0)
        let mut saw_receiving = false;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(20) {
            let p = dl.progress();
            assert_eq!(p.conn_states.len(), 4);
            assert_eq!(p.conn_partial.len(), 4);
            assert_eq!(p.conn_block.len(), 4);
            assert!(p.conn_states.iter().all(|&s| s <= 2));
            if p.conn_states.iter().any(|&s| s == 2)
                && p.conn_block.iter().any(|&b| b >= 0)
            {
                saw_receiving = true;
                break;
            }
            if dl.is_terminal() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(saw_receiving, "no connection was ever seen receiving on a block");
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let _ = std::fs::remove_file(&out);
    }

    // --- item 4: limiter on the wire (dedicated child process) ---

    #[test]
    fn limiter_wire_child() {
        // runs only inside the spawned child (env-guarded): the
        // GLOBAL limiter is process-wide, so the wire measurement
        // must not share a process with the rest of the suite
        if std::env::var("NDM208_LIMIT_CHILD").is_err() {
            return;
        }
        let body = pattern_body(2 * 1024 * 1024);
        let port = delayed_range_server(body.clone(), Duration::ZERO);
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("limwire");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        set_global_limit(200_000);
        let t0 = Instant::now();
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        let mut samples: Vec<(f64, u64)> = Vec::new();
        let mut halved_at: Option<f64> = None;
        loop {
            let t = t0.elapsed().as_secs_f64();
            samples.push((t, dl.progress().fetched_session));
            if halved_at.is_none() && t >= 3.0 {
                set_global_limit(100_000); // halve mid-flight
                halved_at = Some(t);
            }
            if halved_at.map(|h| t - h >= 2.5).unwrap_or(false) {
                break;
            }
            assert!(t < 20.0, "measurement window overran");
            std::thread::sleep(Duration::from_millis(200));
        }
        let at = |t: f64| -> u64 {
            samples
                .iter()
                .min_by(|a, b| (a.0 - t).abs().partial_cmp(&(b.0 - t).abs()).unwrap())
                .map(|s| s.1)
                .unwrap_or(0)
        };
        // first second: within ~2x of the 200 KB/s limit (pre-fix,
        // every connection's first 512 KiB escaped uncharged and
        // the whole 2 MiB landed inside the first second)
        let first_sec = at(1.0);
        assert!(
            first_sec <= 420_000,
            "first-second bytes {first_sec} blow past the 200 KB/s limit"
        );
        // steady state (second 2): within a slice of the limit
        let second = at(2.0) - at(1.0);
        assert!(
            (140_000..=280_000).contains(&second),
            "steady-state second moved {second} bytes at a 200 KB/s limit"
        );
        // the halving is effective within 2 s, with no catch-up
        // burst: by the second after the change the flow runs at
        // the NEW rate (~100 KB in that second, one slice of
        // carryover tolerated) — the old pacer ignored a change
        // for ≥15 s and would still be moving ~200 KB there
        let h = halved_at.unwrap();
        let after_total = at(h + 2.0) - at(h);
        assert!(
            after_total <= 480_000,
            "2 s after halving to 100 KB/s, {after_total} bytes moved (burst)"
        );
        let second_after = at(h + 2.0) - at(h + 1.0);
        assert!(
            (30_000..=180_000).contains(&second_after),
            "the second after halving moved {second_after} bytes — the new rate did not take hold"
        );
        set_global_limit(0);
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }

    #[test]
    fn limiter_holds_from_byte_zero_and_follows_changes_on_the_wire() {
        if std::env::var("NDM208_LIMIT_CHILD").is_ok() {
            return; // we ARE the child; the sibling test did the work
        }
        let exe = std::env::current_exe().unwrap();
        let status = std::process::Command::new(exe)
            .args(["limiter_wire_child", "--nocapture"])
            .env("NDM208_LIMIT_CHILD", "1")
            .status()
            .expect("child test binary must run");
        assert!(status.success(), "limiter wire check failed in the child: {status}");
    }

    // --- Refresh Download Address, IDM-true: a refreshed entry
    // keeps its sealed progress when the new address serves the
    // same file, and restarts — honestly — when it does not. The
    // refresh itself is `rebind_state_url` (the journal's recorded
    // URL flips); every keep/discard decision below is made by the
    // ordinary start-path verification, exactly as for any resume.

    /// Lay down the on-disk remains of a paused download: a
    /// full-size partial whose sealed blocks hold the real bytes
    /// (the rest zero fill) plus its resume journal under `url`.
    fn refresh_partial(out: &Path, url: &str, body: &[u8], block: u64, sealed: &[u8]) {
        let mut partial = vec![0u8; body.len()];
        for (i, &s) in sealed.iter().enumerate() {
            if s == 1 {
                let a = i * block as usize;
                let b = ((i + 1) * block as usize).min(body.len());
                partial[a..b].copy_from_slice(&body[a..b]);
            }
        }
        std::fs::write(out, &partial).unwrap();
        write_journal(out, url, body.len() as u64, block, sealed);
    }

    fn write_journal_validators(
        out: &Path,
        url: &str,
        total: u64,
        block: u64,
        done: &[u8],
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) {
        let v = serde_json::json!({
            "url": url, "total": total, "block_size": block,
            "etag": etag, "last_modified": last_modified, "done": done,
        });
        std::fs::write(state_path_for(out), serde_json::to_vec(&v).unwrap()).unwrap();
    }

    /// Range-capable handler serving `body`, optionally with an
    /// ETag validator on every response (HEAD, 206 and 200 alike,
    /// as real servers send it).
    fn range_handler(body: Vec<u8>, etag: Option<String>) -> impl Fn(&Req) -> Vec<u8> + Send + Sync {
        move |req| {
            let total = body.len() as u64;
            let mut head: Vec<(&str, String)> = vec![
                ("Content-Length", total.to_string()),
                ("Accept-Ranges", "bytes".to_string()),
            ];
            if let Some(e) = &etag {
                head.push(("ETag", e.clone()));
            }
            if req.method == "HEAD" {
                return http_resp("200 OK", &head, &[]);
            }
            if let Some(r) = &req.range {
                let r = r.trim().strip_prefix("bytes=").unwrap_or(r);
                let (a, b) = r.split_once('-').unwrap_or(("0", ""));
                let a: u64 = a.parse().unwrap_or(0);
                let b: u64 = if b.is_empty() {
                    total - 1
                } else {
                    b.parse().unwrap_or(total - 1)
                };
                let end = b.min(total - 1);
                let slice = &body[a as usize..=end as usize];
                let mut h: Vec<(&str, String)> = vec![
                    ("Content-Range", format!("bytes {a}-{end}/{total}")),
                    ("Content-Length", slice.len().to_string()),
                ];
                if let Some(e) = &etag {
                    h.push(("ETag", e.clone()));
                }
                return http_resp("206 Partial Content", &h, slice);
            }
            http_resp("200 OK", &head, &body)
        }
    }

    #[test]
    fn credentialed_ftp_journal_peeks_and_rebinds_by_raw_url() {
        // H-2 regression: an FTP journal stores the target's
        // REDACTED address (journal_url — credentials never touch
        // disk), while callers hold the RAW credentialed URL.
        // peek/rebind compared raw-to-redacted and missed every
        // such journal: after a restart the entry read as
        // unresumable, and a Refresh Address discarded its sealed
        // progress outright.
        let body = pattern_body(1024 * 1024);
        let block = 256 * 1024u64;
        let out = tmp_out("ftprebind");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let raw1 = "ftp://nill:s3cret@127.0.0.1:2121/dir/old.bin";
        let raw2 = "ftp://nill:s3cret@127.0.0.1:2122/dir/fresh.bin";
        std::fs::write(&out, &body).unwrap();
        // the journal exactly as the engine writes it: redacted
        write_journal(&out, &ftp::journal_url(raw1), body.len() as u64, block, &[1, 1, 0, 0]);
        let pk = peek_state(raw1, &out).expect("the raw URL must find its redacted journal");
        assert_eq!(pk.total, body.len() as u64);
        assert_eq!(pk.done_bytes, 2 * block);
        // the flip: raw old → raw new, stored redacted
        assert!(rebind_state_url(&out, raw1, raw2));
        assert!(peek_state(raw1, &out).is_none());
        assert!(peek_state(raw2, &out).is_some(), "the refreshed address must keep the journal");
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state_path_for(&out)).unwrap()).unwrap();
        let stored_url = stored["url"].as_str().unwrap();
        assert_eq!(stored_url, ftp::journal_url(raw2));
        assert!(!stored_url.contains("s3cret"), "journal leaked the password: {stored_url}");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }

    #[test]
    fn rebind_state_url_flips_only_a_valid_matching_journal() {        let body = pattern_body(1024 * 1024);
        let block = 256 * 1024u64;
        let out = tmp_out("rebind");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let url1 = "http://127.0.0.1:1/old.bin";
        let url2 = "http://127.0.0.1:2/fresh.bin";
        // no journal at all
        assert!(!rebind_state_url(&out, url1, url2));
        refresh_partial(&out, url1, &body, block, &[1, 1, 0, 0]);
        // a journal belonging to a different URL is not ours to flip
        assert!(!rebind_state_url(&out, "http://127.0.0.1:9/other.bin", url2));
        assert!(peek_state(url1, &out).is_some(), "journal must be untouched");
        // the flip: the old address loses the journal, the new one
        // gains it whole — total, sealed bytes and block map intact
        assert!(rebind_state_url(&out, url1, url2));
        assert!(peek_state(url1, &out).is_none());
        let pk = peek_state(url2, &out).expect("rebound journal must peek under the new URL");
        assert_eq!(pk.total, body.len() as u64);
        assert_eq!(pk.done_bytes, 2 * block);
        assert_eq!(pk.blocks, vec![1u8, 1, 0, 0]);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }

    #[test]
    fn rebind_state_url_refuses_poison_and_size_lies() {
        let block = 256 * 1024u64;
        let out = tmp_out("rebindbad");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let url1 = "http://127.0.0.1:1/old.bin";
        let url2 = "http://127.0.0.1:2/fresh.bin";
        // block map that disagrees with its own block size
        std::fs::write(&out, vec![0u8; 1024 * 1024]).unwrap();
        write_journal(&out, url1, 1024 * 1024, block, &[1, 1]);
        assert!(!rebind_state_url(&out, url1, url2));
        assert!(peek_state(url1, &out).is_none(), "poison stays untrusted either way");
        // partial file no longer the journal's full size
        write_journal(&out, url1, 1024 * 1024, block, &[1, 1, 0, 0]);
        std::fs::write(&out, vec![0u8; 500]).unwrap();
        assert!(!rebind_state_url(&out, url1, url2));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }

    #[test]
    fn refresh_same_file_resumes_from_sealed_bytes() {
        let body = pattern_body(1024 * 1024);
        let block = 256 * 1024u64;
        // one server, two addresses: the fresh URL serves the SAME
        // file (the expired-link case the feature exists for)
        let port = delayed_range_server(body.clone(), Duration::from_millis(300));
        let url1 = format!("http://127.0.0.1:{port}/old.bin");
        let url2 = format!("http://127.0.0.1:{port}/fresh.bin");
        let out = tmp_out("refkeep");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        refresh_partial(&out, &url1, &body, block, &[1, 1, 0, 0]);
        assert!(
            peek_state(&url2, &out).is_none(),
            "before the refresh the new address owns nothing"
        );
        assert!(rebind_state_url(&out, &url1, &url2));
        let dl = start(&url2, &out, 2, block).unwrap();
        // the start credited the kept blocks instead of zeroing
        // them: with every GET delayed 300 ms no new block can
        // have sealed yet
        assert_eq!(dl.progress().sealed_done_bytes, 2 * block);
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        wait_settled(&dl, Duration::from_secs(10));
        // only the missing remainder was fetched this session (one
        // block of slack for a work-stealing re-read; a restart
        // would have fetched all four)
        assert!(
            dl.progress().fetched_session <= 3 * block,
            "resume fetched {} bytes for a {}-byte remainder",
            dl.progress().fetched_session,
            2 * block
        );
        assert_eq!(std::fs::read(&out).unwrap(), body);
        assert!(!state_path_for(&out).exists(), "finished download removes its journal");
        drop(dl);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn refresh_different_total_restarts_from_zero() {
        let body_a = pattern_body(1024 * 1024);
        let body_b = pattern_body(1536 * 1024);
        let block = 256 * 1024u64;
        let url1 = "http://127.0.0.1:1/old.bin".to_string();
        let port = delayed_range_server(body_b.clone(), Duration::from_millis(300));
        let url2 = format!("http://127.0.0.1:{port}/fresh.bin");
        let out = tmp_out("refsize");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        refresh_partial(&out, &url1, &body_a, block, &[1, 1, 0, 0]);
        assert!(rebind_state_url(&out, &url1, &url2));
        // the kept journal still PEEKS (peek does not probe): the
        // discard decision belongs to the start's verification
        assert_eq!(peek_state(&url2, &out).map(|p| p.done_bytes), Some(2 * block));
        let dl = start(&url2, &out, 2, block).unwrap();
        assert_eq!(dl.progress().total, body_b.len() as u64);
        assert_eq!(
            dl.progress().sealed_done_bytes, 0,
            "a different total must discard the kept journal"
        );
        assert_eq!(wait_terminal(&dl, Duration::from_secs(90)), Some(Status::Finished));
        wait_settled(&dl, Duration::from_secs(10));
        assert_eq!(std::fs::read(&out).unwrap(), body_b);
        drop(dl);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn refresh_same_size_different_content_restarts_via_block_check() {
        let body_a = pattern_body(1024 * 1024);
        let body_c: Vec<u8> = body_a.iter().map(|b| b ^ 0xFF).collect();
        let block = 256 * 1024u64;
        let url1 = "http://127.0.0.1:1/old.bin".to_string();
        // no validators on this server: the start must byte-compare
        // sampled sealed blocks against the new content to decide
        let port = delayed_range_server(body_c.clone(), Duration::from_millis(300));
        let url2 = format!("http://127.0.0.1:{port}/fresh.bin");
        let out = tmp_out("refcontent");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        refresh_partial(&out, &url1, &body_a, block, &[1, 1, 1, 0]);
        assert!(rebind_state_url(&out, &url1, &url2));
        let dl = start(&url2, &out, 2, block).unwrap();
        assert_eq!(
            dl.progress().sealed_done_bytes, 0,
            "different content under the same size must discard the kept journal"
        );
        assert_eq!(wait_terminal(&dl, Duration::from_secs(90)), Some(Status::Finished));
        wait_settled(&dl, Duration::from_secs(10));
        assert_eq!(
            std::fs::read(&out).unwrap(),
            body_c,
            "the file must be the new content whole — never a splice"
        );
        drop(dl);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn refresh_validator_mismatch_restarts_and_match_resumes() {
        let body = pattern_body(1024 * 1024);
        let block = 256 * 1024u64;
        let url1 = "http://127.0.0.1:1/old.bin";
        // --- mismatch: sealed under ETag "v1", the fresh address
        // answers "v2" for the same bytes — the start's filter
        // refuses the journal outright, no sampling involved
        let port = test_server(range_handler(body.clone(), Some("\"v2\"".to_string())));
        let url2 = format!("http://127.0.0.1:{port}/fresh.bin");
        let out = tmp_out("refetag");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        refresh_partial(&out, url1, &body, block, &[1, 1, 0, 0]);
        write_journal_validators(&out, url1, body.len() as u64, block, &[1, 1, 0, 0], Some("\"v1\""), None);
        assert!(rebind_state_url(&out, url1, &url2));
        let dl = start(&url2, &out, 2, block).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        wait_settled(&dl, Duration::from_secs(10));
        assert!(
            dl.progress().fetched_session >= 3 * block + block / 2,
            "a changed validator must re-fetch the file (fetched {})",
            dl.progress().fetched_session
        );
        assert_eq!(std::fs::read(&out).unwrap(), body);
        drop(dl);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        // --- match: the fresh address answers the SAME ETag —
        // the journal is trusted on the validators alone and the
        // sealed half is kept
        let port = test_server(range_handler(body.clone(), Some("\"v1\"".to_string())));
        let url2 = format!("http://127.0.0.1:{port}/fresh.bin");
        let out = tmp_out("refetagok");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        refresh_partial(&out, url1, &body, block, &[1, 1, 0, 0]);
        write_journal_validators(&out, url1, body.len() as u64, block, &[1, 1, 0, 0], Some("\"v1\""), None);
        assert!(rebind_state_url(&out, url1, &url2));
        let dl = start(&url2, &out, 2, block).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        wait_settled(&dl, Duration::from_secs(10));
        assert!(
            dl.progress().fetched_session <= 3 * block,
            "a matching validator must keep the sealed half (fetched {})",
            dl.progress().fetched_session
        );
        assert_eq!(std::fs::read(&out).unwrap(), body);
        drop(dl);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn refresh_onto_no_range_server_replaces_cleanly() {
        let body_a = pattern_body(1024 * 1024);
        let body_d: Vec<u8> = body_a.iter().map(|b| b ^ 0xFF).collect();
        let block = 256 * 1024u64;
        let total = body_d.len();
        let served = body_d.clone();
        // no Accept-Ranges anywhere and Range is ignored: the
        // canonical no-range server — the start demotes to the
        // single-stream spool path (M-1)
        let port = test_server(move |req| {
            if req.method == "HEAD" {
                return http_resp("200 OK", &[("Content-Length", total.to_string())], &[]);
            }
            http_resp("200 OK", &[("Content-Length", total.to_string())], &served)
        });
        let url2 = format!("http://127.0.0.1:{port}/fresh.bin");
        let out = tmp_out("refnorng");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        refresh_partial(&out, "http://127.0.0.1:1/old.bin", &body_a, block, &[1, 1, 0, 0]);
        assert!(rebind_state_url(&out, "http://127.0.0.1:1/old.bin", &url2));
        let dl = start(&url2, &out, 2, block).unwrap();
        assert!(!dl.progress().resumable, "a no-range start must be the single-stream path");
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        wait_settled(&dl, Duration::from_secs(10));
        assert_eq!(
            std::fs::read(&out).unwrap(),
            body_d,
            "the replacement must be whole and exact — the kept partial must not bleed into it"
        );
        assert!(
            !state_path_for(&out).exists(),
            "the stale journal goes with the finish"
        );
        let spool = {
            let mut s = out.clone().into_os_string();
            s.push(".ndm.spool");
            PathBuf::from(s)
        };
        assert!(!spool.exists(), "the spool must be renamed away, not left behind");
        drop(dl);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn refresh_onto_failing_no_range_server_keeps_partial_and_journal() {
        let body_a = pattern_body(1024 * 1024);
        let block = 256 * 1024u64;
        let total = body_a.len() as u64;
        let served = body_a.clone();
        let port = test_server(move |req| {
            if req.method == "HEAD" {
                return http_resp("200 OK", &[("Content-Length", total.to_string())], &[]);
            }
            // correct length promised, body cut at 1000 bytes
            http_resp("200 OK", &[("Content-Length", total.to_string())], &served[..1000])
        });
        let url2 = format!("http://127.0.0.1:{port}/fresh.bin");
        let out = tmp_out("refnorngfail");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        refresh_partial(&out, "http://127.0.0.1:1/old.bin", &body_a, block, &[1, 1, 0, 0]);
        let partial_before = std::fs::read(&out).unwrap();
        assert!(rebind_state_url(&out, "http://127.0.0.1:1/old.bin", &url2));
        let dl = start(&url2, &out, 2, block).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(120)), Some(Status::Error));
        wait_settled(&dl, Duration::from_secs(10));
        drop(dl);
        assert_eq!(
            std::fs::read(&out).unwrap(),
            partial_before,
            "a failed replacement must leave the kept partial byte-for-byte"
        );
        let pk = peek_state(&url2, &out).expect("the kept journal must survive a failed demotion");
        assert_eq!(pk.done_bytes, 2 * block);
        assert_eq!(pk.blocks, vec![1u8, 1, 0, 0]);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }
}

// ---------- 2.2.0 workstream A (engine performance): F1 cookie
// jar, F2 per-connection watermark, F3 governor backoff, F4
// resume fast-path, F5 download probe, F6 range-loss restart,
// F7 journal batching, F8 disk-full wording ----------

#[cfg(test)]
mod perf220_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    fn pattern_body(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
    }

    fn wait_terminal(dl: &Download, deadline: Duration) -> Option<Status> {
        let t0 = Instant::now();
        loop {
            if dl.is_terminal() {
                return Some(dl.status());
            }
            if t0.elapsed() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn tmp_out(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ndm-220-{}-{}.bin", tag, std::process::id()))
    }

    struct Req {
        method: String,
        path: String,
        range: Option<String>,
        headers: Vec<(String, String)>,
    }

    impl Req {
        fn header(&self, name: &str) -> Option<String> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        }
    }

    /// audit207-style handler server that also logs every
    /// request it receives (method, path, range, headers).
    fn logged_server<F>(handler: F) -> (u16, Arc<Mutex<Vec<Req>>>)
    where
        F: Fn(&Req) -> Vec<u8> + Send + Sync + 'static,
    {
        logged_server_on("127.0.0.1", handler)
    }

    fn logged_server_on<F>(bind: &str, handler: F) -> (u16, Arc<Mutex<Vec<Req>>>)
    where
        F: Fn(&Req) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(format!("{bind}:0")).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let handler = handler.clone();
                let log = log2.clone();
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
                    let mut headers = Vec::new();
                    let mut range = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                if let Some((k, v)) = h.split_once(':') {
                                    let v = v.trim().to_string();
                                    if k.trim().eq_ignore_ascii_case("range") {
                                        range = Some(v.clone());
                                    }
                                    headers.push((k.trim().to_string(), v));
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let req = Req { method, path, range, headers };
                    let resp = handler(&req);
                    log.lock().unwrap().push(req);
                    let _ = s.write_all(&resp);
                });
            }
        });
        (port, log)
    }

    fn http_resp(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n").into_bytes();
        for (k, v) in headers {
            out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        out.extend_from_slice(b"Connection: close\r\n\r\n");
        out.extend_from_slice(body);
        out
    }

    /// A plain range-capable file serve for one request.
    fn serve_range(body: &[u8], req: &Req) -> Vec<u8> {
        let total = body.len() as u64;
        if req.method == "HEAD" {
            return http_resp(
                "200 OK",
                &[
                    ("Content-Length", total.to_string()),
                    ("Accept-Ranges", "bytes".to_string()),
                ],
                &[],
            );
        }
        if let Some(r) = &req.range {
            let r = r.trim().strip_prefix("bytes=").unwrap_or(r);
            let (a, b) = r.split_once('-').unwrap_or(("0", ""));
            let a: u64 = a.parse().unwrap_or(0);
            let b: u64 = if b.is_empty() { total - 1 } else { b.parse().unwrap_or(total - 1) };
            let end = b.min(total - 1);
            let slice = &body[a as usize..=end as usize];
            return http_resp(
                "206 Partial Content",
                &[
                    ("Content-Range", format!("bytes {a}-{end}/{total}")),
                    ("Content-Length", slice.len().to_string()),
                ],
                slice,
            );
        }
        http_resp("200 OK", &[("Content-Length", total.to_string())], body)
    }

    // --- F1: cookie parsing, scoping and merge rules ---

    #[test]
    fn set_cookie_parsing_rules() {
        let (n, v, d, p, x) = parse_set_cookie("gate=1; Path=/").unwrap();
        assert_eq!((n.as_str(), v.as_str(), d, p.as_str(), x), ("gate", "1", None, "/", false));
        let (_, _, d, _, _) = parse_set_cookie("a=b; Domain=.Example.COM; Path=/dl").unwrap();
        assert_eq!(d.as_deref(), Some("example.com"));
        let (_, _, _, _, x) = parse_set_cookie("a=b; Max-Age=0").unwrap();
        assert!(x, "Max-Age=0 is a delete instruction");
        assert!(parse_set_cookie("noequals").is_none());
        assert!(parse_set_cookie("=v").is_none());
    }

    #[test]
    fn jar_scoping_is_host_only_unless_domain_names_the_parent() {
        let jar = CookieJar::default();
        let push = |host: &str, domain: Option<&str>, path: &str, name: &str, value: &str| {
            jar.entries.lock().unwrap().push(JarCookie {
                host: host.to_string(),
                domain: domain.map(|d| d.to_string()),
                path: path.to_string(),
                name: name.to_string(),
                value: value.to_string(),
            });
        };
        push("example.com", None, "/", "host_only", "1");
        push("sub.example.com", Some("example.com"), "/", "wide", "2");
        push("example.com", None, "/dl", "pathscoped", "3");

        let h = jar.header_for("http://example.com/f").unwrap();
        assert!(h.contains("host_only=1"), "{h}");
        assert!(jar.header_for("http://other.com/f").is_none());
        let h = jar.header_for("http://sub.example.com/f").unwrap();
        assert!(!h.contains("host_only"), "host-only cookie widened: {h}");
        assert!(h.contains("wide=2"), "{h}");
        let h = jar.header_for("http://example.com/f").unwrap();
        assert!(h.contains("wide=2"), "{h}");
        let h = jar.header_for("http://example.com/other").unwrap();
        assert!(!h.contains("pathscoped"), "{h}");
        let h = jar.header_for("http://example.com/dl/x").unwrap();
        assert!(h.contains("pathscoped=3"), "{h}");
    }

    #[test]
    fn jar_merge_lets_the_static_cookie_win_name_clashes() {
        let jar = CookieJar::default();
        for (name, value) in [("gate", "old"), ("extra", "1")] {
            jar.entries.lock().unwrap().push(JarCookie {
                host: "example.com".to_string(),
                domain: None,
                path: "/".to_string(),
                name: name.to_string(),
                value: value.to_string(),
            });
        }
        let merged = merged_cookie_header(&jar, "http://example.com/f", Some("gate=new")).unwrap();
        assert!(merged.contains("gate=new"), "{merged}");
        assert!(!merged.contains("gate=old"), "{merged}");
        assert!(merged.contains("extra=1"), "{merged}");
    }

    // --- F1: the cookie gate downloads end to end ---

    #[test]
    fn cookie_gated_host_downloads_byte_identical() {
        let body = pattern_body(600_000);
        let body2 = body.clone();
        let (port, log) = logged_server(move |req| {
            if req.path == "/cookieredir" {
                return http_resp(
                    "302 Found",
                    &[
                        ("Location", "/gated.bin".to_string()),
                        ("Set-Cookie", "gate=1; Path=/".to_string()),
                        ("Content-Length", "0".to_string()),
                    ],
                    &[],
                );
            }
            if req.path == "/gated.bin" {
                let ok = req.header("Cookie").map(|c| c.contains("gate=1")).unwrap_or(false);
                if !ok {
                    return http_resp(
                        "403 Forbidden",
                        &[("Content-Length", "9".to_string())],
                        b"forbidden",
                    );
                }
                return serve_range(&body2, req);
            }
            http_resp("404 Not Found", &[("Content-Length", "0".to_string())], &[])
        });
        let url = format!("http://127.0.0.1:{port}/cookieredir");
        let out = tmp_out("cookie");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(30)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let log = log.lock().unwrap();
        assert!(
            log.iter().any(|r| r.path == "/gated.bin"
                && r.method == "GET"
                && r.header("Cookie").map(|c| c.contains("gate=1")).unwrap_or(false)),
            "no worker request carried the gate cookie"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn jar_cookie_never_leaks_to_a_cross_host_redirect_target() {
        let body = pattern_body(300_000);
        let body2 = body.clone();
        let (port_b, log_b) = logged_server_on("127.0.0.2", move |req| serve_range(&body2, req));
        let (port_a, _log_a) = logged_server(move |req| {
            if req.path == "/redir" {
                return http_resp(
                    "302 Found",
                    &[
                        ("Location", format!("http://127.0.0.2:{port_b}/gated.bin")),
                        ("Set-Cookie", "gate=1; Path=/".to_string()),
                        ("Content-Length", "0".to_string()),
                    ],
                    &[],
                );
            }
            http_resp("404 Not Found", &[("Content-Length", "0".to_string())], &[])
        });
        let url = format!("http://127.0.0.1:{port_a}/redir");
        let out = tmp_out("xhost");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(30)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let log_b = log_b.lock().unwrap();
        assert!(!log_b.is_empty());
        for r in log_b.iter() {
            let c = r.header("Cookie").unwrap_or_default();
            assert!(!c.contains("gate=1"), "the gate cookie leaked cross-host in {} {}", r.method, r.path);
        }
        let _ = std::fs::remove_file(&out);
    }

    // --- F2: a dribbling connection is recycled, not wedging ---

    /// First range GET: 4 KiB fast, then 1 byte every 50 ms for
    /// ~35 s (inside every read timeout — the live wedge shape).
    /// Every later GET serves its slice at full speed, as a fresh
    /// connection to a healthy route would.
    fn dribble_once_server(body: Vec<u8>) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = Arc::new(body);
        let gets = Arc::new(AtomicUsize::new(0));
        let gets2 = gets.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                let gets = gets2.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    let (rs, re) = range.unwrap_or((0, total - 1));
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        )
                        .as_bytes(),
                    );
                    if gets.fetch_add(1, Ordering::Relaxed) == 0 {
                        if s.write_all(&slice[..4096.min(slice.len())]).is_err() {
                            return;
                        }
                        for i in 4096..slice.len().min(4096 + 700) {
                            std::thread::sleep(Duration::from_millis(50));
                            if s.write_all(&slice[i..i + 1]).is_err() {
                                return;
                            }
                        }
                    } else {
                        let _ = s.write_all(slice);
                    }
                });
            }
        });
        (port, gets)
    }

    #[test]
    fn dribbling_connection_is_recycled_and_the_download_completes() {
        let body = pattern_body(300_000);
        let (port, gets) = dribble_once_server(body.clone());
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("dribble");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 1, 256 * 1024).unwrap();
        assert_eq!(
            wait_terminal(&dl, Duration::from_secs(90)),
            Some(Status::Finished),
            "a dribbling connection must be recycled, not wedge the download"
        );
        assert_eq!(std::fs::read(&out).unwrap(), body);
        assert!(gets.load(Ordering::Relaxed) >= 2, "the tail was never re-requested on a fresh connection");
        let _ = std::fs::remove_file(&out);
    }

    // --- F3: governor decision logic ---

    #[test]
    fn governor_shrinks_only_on_broad_throttling() {
        assert_eq!(governor_decision(1, 8, 8, 8, 0.0), GovDecision::Hold);
        assert_eq!(governor_decision(4, 8, 8, 8, 0.0), GovDecision::Shrink);
        assert_eq!(governor_decision(2, 3, 3, 8, 0.0), GovDecision::Shrink);
        assert_eq!(governor_decision(1, 1, 1, 8, 0.0), GovDecision::Hold);
        assert_eq!(governor_decision(0, 8, 4, 8, 5.0), GovDecision::Grow);
        assert_eq!(governor_decision(1, 8, 4, 8, 5.0), GovDecision::Grow);
    }

    // --- F4: resume trusts the done map; sample is ≤4 ---

    #[test]
    fn validator_less_resume_samples_at_most_four_done_blocks() {
        let block = 256 * 1024u64;
        let body = pattern_body((8 * block) as usize);
        let body2 = body.clone();
        let (port, log) = logged_server(move |req| serve_range(&body2, req));
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("resume4");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        {
            let f = File::create(&out).unwrap();
            f.set_len(8 * block).unwrap();
            for b in 0..7u64 {
                let off = b * block;
                write_at(&f, &body[off as usize..(off + block) as usize], off).unwrap();
            }
        }
        let state = State {
            url: url.clone(),
            total: 8 * block,
            block_size: block,
            etag: None,
            last_modified: None,
            done: vec![1, 1, 1, 1, 1, 1, 1, 0],
            partial: Vec::new(),
        };
        std::fs::write(state_path_for(&out), serde_json::to_vec(&state).unwrap()).unwrap();
        let dl = start(&url, &out, 2, block).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(30)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let log = log.lock().unwrap();
        // full-BLOCK range GETs into the sealed region are the
        // resume re-fetch traffic this test bounds (the probe's
        // own 1-byte verify GET is not a done-range re-fetch)
        let sealed_gets: Vec<&Req> = log
            .iter()
            .filter(|r| r.method == "GET" && r.range.is_some())
            .filter(|r| {
                let rng = r.range.as_ref().unwrap();
                let rng = rng.trim().strip_prefix("bytes=").unwrap_or(rng);
                let (a, b) = rng.split_once('-').unwrap_or(("0", "0"));
                let a: u64 = a.parse().unwrap_or(0);
                let b: u64 = b.parse().unwrap_or(0);
                a < 7 * block && b + 1 - a == block
            })
            .collect();
        assert!(
            sealed_gets.len() <= 4,
            "resume re-fetched {} sealed ranges (sample must be ≤4)",
            sealed_gets.len()
        );
        for r in log.iter().filter(|r| r.method == "GET") {
            let rng = r.range.clone().unwrap_or_default();
            for b in [1u64, 3, 5] {
                assert_ne!(
                    rng,
                    format!("bytes={}-{}", b * block, (b + 1) * block - 1),
                    "unsampled sealed block {b} was re-fetched on resume"
                );
            }
        }
        let _ = std::fs::remove_file(&out);
    }

    // --- B-4 (audit-233): partial-block journaling ---

    /// A range server that records every GET range it SERVES
    /// (start, end inclusive) and, while `slow` is set, drips
    /// GET bodies in 4 KiB chunks — the byte accounting a
    /// partial-resume assertion needs.
    fn partial_server(
        body: Vec<u8>,
    ) -> (u16, Arc<Mutex<Vec<(u64, u64)>>>, Arc<AtomicBool>) {
        use std::io::BufRead;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = Arc::new(body);
        let served = Arc::new(Mutex::new(Vec::new()));
        let slow = Arc::new(AtomicBool::new(true));
        let served2 = served.clone();
        let slow2 = slow.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                let served = served2.clone();
                let slow = slow2.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let method = line.split_whitespace().next().unwrap_or("").to_string();
                    let mut range: Option<(u64, u64)> = None;
                    loop {
                        let mut h = String::new();
                        match reader.read_line(&mut h) {
                            Ok(0) => break,
                            Ok(_) if h == "\r\n" || h == "\n" => break,
                            Ok(_) => {
                                let hl = h.to_ascii_lowercase();
                                if let Some(v) = hl.strip_prefix("range: bytes=") {
                                    if let Some((a, b)) = v.trim().split_once('-') {
                                        range = Some((
                                            a.trim().parse().unwrap_or(0),
                                            b.trim().parse().unwrap_or(0),
                                        ));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let total = body.len() as u64;
                    if method == "HEAD" {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        return;
                    }
                    let (rs, re) = range.unwrap_or((0, total - 1));
                    let end = re.min(total - 1);
                    let slice = &body[rs as usize..=end as usize];
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {rs}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        )
                        .as_bytes(),
                    );
                    served.lock().unwrap().push((rs, end));
                    if slow.load(Ordering::Relaxed) {
                        for chunk in slice.chunks(4096) {
                            std::thread::sleep(Duration::from_millis(40));
                            if s.write_all(chunk).is_err() {
                                return;
                            }
                        }
                    } else {
                        let _ = s.write_all(slice);
                    }
                });
            }
        });
        (port, served, slow)
    }

    #[test]
    fn state_json_without_a_partial_field_loads_as_all_zero() {
        // backwards compatibility in the small: a pre-B-4
        // journal has no `partial` key at all
        let raw = br#"{"url":"http://x/f","total":10,"block_size":4,"etag":null,"last_modified":null,"done":[1,0,1]}"#;
        let s: State = serde_json::from_slice(raw).unwrap();
        assert!(s.partial.is_empty());
        assert_eq!(s.partial_bytes(), 0);
    }

    #[test]
    fn partial_block_prefix_is_journaled_and_never_refetched() {
        let block = 64 * 1024u64;
        let body = pattern_body((6 * block) as usize);
        let (port, served, slow) = partial_server(body.clone());
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("partial-b4");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));

        // phase 1: slow download, one connection; pause it
        // mid-block once the journal shows a sealed block AND a
        // live partial prefix
        let dl = start(&url, &out, 1, block).unwrap();
        let mut journaled: Option<State> = None;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Ok(raw) = std::fs::read(state_path_for(&out)) {
                if let Ok(s) = serde_json::from_slice::<State>(&raw) {
                    if s.done.iter().any(|&b| b == 1)
                        && s.partial.iter().any(|&p| p >= 16 * 1024)
                    {
                        journaled = Some(s);
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let s = journaled.expect("the journal must show a sealed block and a partial prefix");
        let _ = s;
        // the job is DROPPED mid-block (cancel settles it: the
        // supervisor force-saves the journal and releases the
        // path — the same on-disk shape a process exit leaves,
        // and the only settle a fresh start can follow)
        dl.control.cancel();
        assert_eq!(
            wait_terminal(&dl, Duration::from_secs(30)),
            Some(Status::Cancelled)
        );
        // phase 2 starts a fresh job on the SAME path, so this
        // phase must clear the same gate production uses before
        // a restart: is_settled (the sibling rig tests' loop),
        // not mere terminal status — a status can publish while
        // the supervisor is still saving the journal and holding
        // the path guard (CI #75 caught exactly that here).
        {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !dl.is_settled() {
                assert!(
                    Instant::now() < deadline,
                    "the cancelled job never settled"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        // the journal ON DISK after settle carries the prefix
        let s: State =
            serde_json::from_slice(&std::fs::read(state_path_for(&out)).unwrap()).unwrap();
        let k = s
            .partial
            .iter()
            .position(|&p| p > 0)
            .expect("a partial prefix is journaled");
        assert_eq!(s.done[k], 0, "the partial block is unsealed");
        let p = s.partial[k];
        assert!(p < block, "a prefix is smaller than its block");
        // and the claimed bytes really are in the file at offset
        let on_disk = std::fs::read(&out).unwrap();
        let ks = (k as u64 * block) as usize;
        assert_eq!(
            &on_disk[ks..ks + p as usize],
            &body[ks..ks + p as usize],
            "the journaled prefix matches the bytes on disk"
        );
        drop(dl);

        // phase 2: fast server, fresh job object, same files.
        // Byte accounting over block k: the prefix-verification
        // GET (validator-less resume samples it, exactly P
        // bytes) plus the resumed remainder (block - P) must
        // total EXACTLY one block — any re-fetch of the prefix
        // by a worker inflates it past that.
        served.lock().unwrap().clear();
        slow.store(false, Ordering::Relaxed);
        let dl = start(&url, &out, 1, block).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body, "the resumed file is byte-identical");
        let kstart = k as u64 * block;
        let kend = kstart + block - 1;
        let mut overlap: u64 = 0;
        for &(rs, re) in served.lock().unwrap().iter() {
            let lo = rs.max(kstart);
            let hi = re.min(kend);
            if hi >= lo {
                overlap += hi - lo + 1;
            }
        }
        assert_eq!(
            overlap, block,
            "block {k} was served {overlap} bytes across the resume — \
             exactly one block means the {p}-byte prefix was never re-fetched"
        );
        let _ = std::fs::remove_file(&out);
    }

    // --- F5: the download probe skips the verify GET ---

    #[test]
    fn download_probe_skips_the_verify_round_trip() {
        let body = pattern_body(300_000);
        let body2 = body.clone();
        let (port, log) = logged_server(move |req| serve_range(&body2, req));
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let p = probe_for_download(&url).unwrap();
        assert!(p.range_ok);
        assert!(p.range_unverified, "a clean HEAD claim must be taken tentatively by the download probe");
        {
            let log = log.lock().unwrap();
            assert_eq!(log.len(), 1, "the download probe must be ONE request");
            assert_eq!(log[0].method, "HEAD");
        }
        let out = tmp_out("f5");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(30)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let log = log.lock().unwrap();
        assert!(
            log.iter().all(|r| r.range.as_deref() != Some("bytes=0-0")),
            "a verify GET (bytes=0-0) was issued"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn tentative_start_proves_a_lying_encoding_header_in_the_worker() {
        // HEAD is clean, but every GET answer is stamped
        // Content-Encoding: gzip while carrying identity bytes:
        // the tentative start's first worker must run the proof
        // and keep its connections, as the probe verdict would
        let body = pattern_body(400_000);
        let body2 = body.clone();
        let (port, _log) = logged_server(move |req| {
            let total = body2.len() as u64;
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            let resp = serve_range(&body2, req);
            let head_end = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 2;
            let mut out = resp[..head_end].to_vec();
            out.extend_from_slice(b"Content-Encoding: gzip\r\n");
            out.extend_from_slice(&resp[head_end..]);
            out
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("f5lie");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 2, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(30)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let _ = std::fs::remove_file(&out);
    }

    // --- F6: persistent range loss restarts single-stream ---

    #[test]
    fn persistent_range_loss_restarts_in_single_stream_mode() {
        let body = pattern_body(700_000);
        let body2 = body.clone();
        let range_gets = Arc::new(AtomicUsize::new(0));
        let range_gets2 = range_gets.clone();
        let (port, log) = logged_server(move |req| {
            let total = body2.len() as u64;
            if req.method == "HEAD" {
                return http_resp(
                    "200 OK",
                    &[
                        ("Content-Length", total.to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                    ],
                    &[],
                );
            }
            if req.range.is_some() {
                if range_gets2.fetch_add(1, Ordering::Relaxed) == 0 {
                    return serve_range(&body2, req);
                }
                return http_resp("200 OK", &[("Content-Length", total.to_string())], &body2);
            }
            http_resp("200 OK", &[("Content-Length", total.to_string())], &body2)
        });
        let url = format!("http://127.0.0.1:{port}/f.bin");
        let out = tmp_out("f6");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(
            wait_terminal(&dl, Duration::from_secs(60)),
            Some(Status::Finished),
            "persistent 200s must restart the job single-stream, not stall-fail it"
        );
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let log = log.lock().unwrap();
        let plain_gets = log.iter().filter(|r| r.method == "GET" && r.range.is_none()).count();
        assert_eq!(plain_gets, 1, "expected exactly one single-stream restart GET");
        let _ = std::fs::remove_file(&out);
    }

    // --- F7: periodic save cadence ---

    #[test]
    fn periodic_save_never_fires_inside_the_minimum_interval() {
        assert!(!periodic_save_due(Duration::from_millis(100), 100 << 20));
        assert!(!periodic_save_due(Duration::from_millis(249), 4 << 20));
        assert!(periodic_save_due(Duration::from_millis(250), 4 << 20));
        assert!(!periodic_save_due(Duration::from_millis(300), 0));
        assert!(periodic_save_due(Duration::from_secs(1), 0));
    }

    // --- F8: disk-full wording ---

    #[cfg(unix)]
    #[test]
    fn disk_full_write_error_says_so_in_plain_words() {
        let e = io::Error::from_raw_os_error(28); // ENOSPC
        let s = plain_write_error(&e);
        assert!(s.contains("The disk is full"), "{s}");
        assert!(s.contains("write:"), "the historical prefix must survive: {s}");
        assert!(s.contains("os error 28"), "the raw error must survive: {s}");
        let other = io::Error::from_raw_os_error(2);
        let s = plain_write_error(&other);
        assert!(!s.contains("disk is full"), "{s}");
        assert!(s.starts_with("write: "), "{s}");
    }
}
