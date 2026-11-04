// FTP download support (WS-H) — a minimal blocking FTP client on
// std::net::TcpStream (no new dependencies) plus the engine-side
// workers that drive it.
//
// Shape: the HTTP engine's contracts are reused, not bent. Probe
// fills the same `Probe`; the ranged transfer claims blocks from
// the same pool (`try_claim`), writes through the same journal /
// state machinery (`save_state`, `State`), paces through the same
// GCRA limiters (`pace_before_read` & friends), and obeys the same
// pause / cancel / per-connection stall-watermark rules as the
// HTTP worker loop. Where the HTTP loop is ureq-shaped (request
// construction, response headers), the FTP loop here is a parallel
// implementation against the same `Shared` state.
//
// Credentials: the URL userinfo (or, failing that, the entry's
// Basic authorization — the same place HTTP logins land) is used
// for USER/PASS only. It is never written to the journal (the
// journal records the URL exactly as the HTTP path records it —
// see `State`), never logged, and never appears in an error
// string: every message names the redacted `display` address.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{
    file_name_of, note_range_loss, note_stall_progress, pace_before_read, pace_grant_size,
    pace_refund_both, plain_write_error, plock, remove_file_bounded, save_state, set_status,
    split_allowed, split_point, try_claim, wait_while_paused, ActiveWorkerGuard, Auth, BlockError,
    Control, CookieJar, Download, InFlightGuard, Probe, Shared, State, Status, WaiterGuard,
    STALL_LIMIT,
};

// ---------- URL handling ----------

pub(crate) fn is_ftp_url(url: &str) -> bool {
    // `get`, not a byte slice: a mangled paste whose byte 6 falls
    // inside a multi-byte character must answer false, not panic
    // the calling thread (M-1)
    url.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("ftp://"))
}

/// A parsed ftp:// address. `pass` lives only in this struct and
/// in the PASS command on the wire — `display` is the credential-
/// free form every error message uses.
#[derive(Clone, Debug)]
pub(crate) struct FtpTarget {
    host: String,
    port: u16,
    user: String,
    pass: String,
    /// remote path as sent in SIZE / RETR (no leading slash,
    /// percent-decoded)
    path: String,
    display: String,
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| *b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c)? << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if chunk.len() > 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

/// Credentials from the entry's Basic authorization header, when
/// the URL itself carries no userinfo — the FTP analogue of how
/// the HTTP path repeats the dialog's login on every request.
fn creds_from_auth(auth: &Auth) -> Option<(String, String)> {
    let header = auth.authorization.as_deref()?;
    let b64 = header.strip_prefix("Basic ").or_else(|| header.strip_prefix("basic "))?;
    let raw = String::from_utf8(base64_decode(b64.trim())?).ok()?;
    let (u, p) = raw.split_once(':')?;
    if u.is_empty() {
        return None;
    }
    Some((u.to_string(), p.to_string()))
}

pub(crate) fn parse_target(url: &str, auth: &Auth) -> Result<FtpTarget, String> {
    let rest = url
        .get(6..)
        .filter(|_| is_ftp_url(url))
        .ok_or_else(|| "not an ftp:// address".to_string())?;
    let (authority, raw_path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err("the FTP address does not name a server".to_string());
    }
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let (host, port) = parse_hostport(hostport)
        .ok_or_else(|| "the FTP address does not name a server".to_string())?;
    let (user, pass) = match userinfo {
        Some(info) => {
            let (u, p) = info.split_once(':').unwrap_or((info, ""));
            (
                crate::percent_decode(u),
                crate::percent_decode(p),
            )
        }
        None => creds_from_auth(auth)
            .unwrap_or_else(|| ("anonymous".to_string(), "anonymous@".to_string())),
    };
    let path = crate::percent_decode(raw_path)
        .trim_start_matches('/')
        .to_string();
    if path.is_empty() {
        return Err("the FTP address does not name a file".to_string());
    }
    // USER/PASS and the RETR path are written onto the control
    // connection as command lines: a decoded CR or LF in any of
    // them would break out of its own command and inject new
    // ones (a crafted ftp:// URL with %0d%0a in the userinfo or
    // path). No legitimate credential or path needs a line break.
    if [user.as_str(), pass.as_str(), path.as_str()]
        .iter()
        .any(|s| s.contains('\r') || s.contains('\n'))
    {
        return Err("the FTP address contains a line break".to_string());
    }
    let display = format!("ftp://{host}:{port}/{path}");
    Ok(FtpTarget {
        host,
        port,
        user,
        pass,
        path,
        display,
    })
}

/// The URL as the resume journal may record it. Credentials
/// travel in Auth and the entry record — never in the .ndm.json
/// beside the partial file, where a raw URL's userinfo would
/// leave the password in plaintext in the download folder.
/// Redacting through parse_target keeps the journal form
/// identical at write and at the resume filter's compare; a URL
/// parse_target refuses cannot be downloaded anyway, so it
/// passes through unchanged.
pub(crate) fn journal_url(url: &str) -> String {
    if is_ftp_url(url) {
        if let Ok(t) = parse_target(url, &Auth::default()) {
            return t.display;
        }
    }
    url.to_string()
}

fn parse_hostport(s: &str) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // [v6 literal]:port
        let (host, tail) = rest.split_once(']')?;
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(21);
        return Some((host.to_string(), port));
    }
    match s.rfind(':') {
        Some(i) if s[i + 1..].chars().all(|c| c.is_ascii_digit()) && !s[i + 1..].is_empty() => {
            Some((s[..i].to_string(), s[i + 1..].parse().ok()?))
        }
        _ => Some((s.to_string(), 21)),
    }
}

// ---------- the client ----------

/// A classified FTP failure: the worker loop's retry policy keys
/// off the same Permanent / Retry split as the HTTP path.
enum FtpErr {
    Retry(String),
    Permanent(String),
    /// REST was refused for an offset the transfer needs — the
    /// caller routes this into the single-stream fallback instead
    /// of the retry accounting
    RestRefused(String),
}

impl FtpErr {
    fn message(self) -> String {
        match self {
            FtpErr::Retry(m) | FtpErr::Permanent(m) | FtpErr::RestRefused(m) => m,
        }
    }
    fn into_block(self) -> BlockError {
        match self {
            FtpErr::Permanent(m) => BlockError::Permanent(m),
            FtpErr::Retry(m) | FtpErr::RestRefused(m) => BlockError::Retry(m),
        }
    }
}

struct FtpConn {
    reader: BufReader<TcpStream>,
    display: String,
    /// The control connection's actual peer — the data connection
    /// dials THIS address (with the PASV port), never a fresh DNS
    /// resolution of the host name: a name that resolves to a
    /// different address on the second lookup would send the data
    /// connection to a different machine than the one that was
    /// logged into (M-5)
    peer: SocketAddr,
}

impl FtpConn {
    fn connect(target: &FtpTarget, connect_timeout: Duration, read_timeout: Duration) -> Result<Self, FtpErr> {
        let addr = (target.host.as_str(), target.port)
            .to_socket_addrs()
            .map_err(|e| FtpErr::Retry(format!("could not resolve the FTP server for {}: {e}", target.display)))?
            .next()
            .ok_or_else(|| FtpErr::Retry(format!("could not resolve the FTP server for {}", target.display)))?;
        let stream = TcpStream::connect_timeout(&addr, connect_timeout).map_err(|e| {
            FtpErr::Retry(format!("could not connect to the FTP server at {}: {e}", target.display))
        })?;
        let _ = stream.set_read_timeout(Some(read_timeout));
        let _ = stream.set_write_timeout(Some(read_timeout));
        let _ = stream.set_nodelay(true);
        let peer = stream.peer_addr().unwrap_or(addr);
        let mut conn = FtpConn {
            reader: BufReader::new(stream),
            display: target.display.clone(),
            peer,
        };
        let (code, _) = conn.read_reply()?;
        if code != 220 {
            return Err(FtpErr::Retry(format!(
                "the FTP server at {} did not greet the connection (reply {code})",
                conn.display
            )));
        }
        conn.login(target)?;
        // SYST is informational only — tolerated, never required
        let _ = conn.cmd("SYST");
        Ok(conn)
    }

    fn login(&mut self, target: &FtpTarget) -> Result<(), FtpErr> {
        let (code, _) = self.cmd(&format!("USER {}", target.user))?;
        match code {
            230 => return Ok(()),
            331 => {}
            530 => {
                return Err(FtpErr::Permanent(format!(
                    "the FTP server at {} refused the login (530) — check the user name and password",
                    self.display
                )))
            }
            c => {
                return Err(FtpErr::Retry(format!(
                    "the FTP server at {} answered {c} to the login",
                    self.display
                )))
            }
        }
        let (code, _) = self.cmd(&format!("PASS {}", target.pass))?;
        match code {
            230 => Ok(()),
            530 => Err(FtpErr::Permanent(format!(
                "the FTP server at {} refused the login (530) — check the user name and password",
                self.display
            ))),
            c => Err(FtpErr::Retry(format!(
                "the FTP server at {} answered {c} to the password",
                self.display
            ))),
        }
    }

    /// Send one command (never logged anywhere — PASS included)
    /// and read its reply: (code, last line's text after the code).
    fn cmd(&mut self, command: &str) -> Result<(u16, String), FtpErr> {
        self.reader
            .get_mut()
            .write_all(format!("{command}\r\n").as_bytes())
            .map_err(|e| FtpErr::Retry(format!("could not write to the FTP server at {}: {e}", self.display)))?;
        self.read_reply()
    }

    /// W3-13: read ONE reply line with a hard length cap. The
    /// line count was already bounded (100) but a single line
    /// was not: read_line would buffer an endless line until the
    /// socket timeout. Reading through fill_buf keeps the cap
    /// real — the buffer never holds more than the cap plus one
    /// buffered chunk. Ok(None) = clean EOF before any byte.
    fn read_reply_line(&mut self) -> Result<Option<String>, FtpErr> {
        const MAX_REPLY_LINE: usize = 8 * 1024;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let buf = self
                .reader
                .fill_buf()
                .map_err(|e| FtpErr::Retry(format!("could not read from the FTP server at {}: {e}", self.display)))?;
            if buf.is_empty() {
                return Ok(if out.is_empty() { None } else { Some(String::from_utf8_lossy(&out).into_owned()) });
            }
            let room = MAX_REPLY_LINE + 1 - out.len().min(MAX_REPLY_LINE + 1);
            let (take, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => ((i + 1).min(room), true),
                None => (buf.len().min(room), false),
            };
            out.extend_from_slice(&buf[..take]);
            self.reader.consume(take);
            if out.len() > MAX_REPLY_LINE {
                return Err(FtpErr::Retry(format!(
                    "the FTP server at {} sent a reply line longer than {MAX_REPLY_LINE} bytes",
                    self.display
                )));
            }
            if done {
                return Ok(Some(String::from_utf8_lossy(&out).into_owned()));
            }
        }
    }

    fn read_reply(&mut self) -> Result<(u16, String), FtpErr> {
        // RFC 959 replies: "NNN text", or multi-line "NNN-..." up
        // to the closing "NNN text". Bounded so a hostile server
        // cannot stream banner lines forever.
        let mut code: Option<u16> = None;
        let text: String;
        for _ in 0..100 {
            let line = self.read_reply_line()?.unwrap_or_default();
            let n = line.len();
            if n == 0 {
                return Err(FtpErr::Retry(format!(
                    "the FTP server at {} closed the connection",
                    self.display
                )));
            }
            let line = line.trim_end_matches(['\r', '\n']);
            let this_code = if line.len() >= 3 && line.as_bytes()[..3].iter().all(|b| b.is_ascii_digit()) {
                line[..3].parse::<u16>().ok()
            } else {
                None
            };
            match code {
                None => {
                    let Some(c) = this_code else {
                        continue; // a banner line with no code: skip it
                    };
                    code = Some(c);
                    if line.as_bytes().get(3) == Some(&b'-') {
                        continue;
                    }
                    text = line.get(4..).unwrap_or("").to_string();
                    return Ok((c, text));
                }
                Some(c) => {
                    if this_code == Some(c) && line.as_bytes().get(3) != Some(&b'-') {
                        text = line.get(4..).unwrap_or("").to_string();
                        return Ok((c, text));
                    }
                }
            }
        }
        Err(FtpErr::Retry(format!(
            "the FTP server at {} sent a reply that never ended",
            self.display
        )))
    }

    fn binary(&mut self) -> Result<(), FtpErr> {
        let (code, _) = self.cmd("TYPE I")?;
        if code == 200 {
            Ok(())
        } else {
            Err(FtpErr::Retry(format!(
                "the FTP server at {} refused binary transfer mode (reply {code})",
                self.display
            )))
        }
    }

    fn size(&mut self, path: &str) -> Option<u64> {
        let (code, text) = self.cmd(&format!("SIZE {path}")).ok()?;
        if code == 213 {
            text.trim().parse().ok()
        } else {
            None
        }
    }

    fn mdtm(&mut self, path: &str) -> Option<String> {
        let (code, text) = self.cmd(&format!("MDTM {path}")).ok()?;
        if code == 213 {
            let t = text.trim();
            // a timestamp, not a byte count: 14 digits YYYYMMDDHHMMSS
            if t.len() == 14 && t.bytes().all(|b| b.is_ascii_digit()) {
                return Some(t.to_string());
            }
        }
        None
    }

    fn pasv(&mut self) -> Result<(String, u16), FtpErr> {
        let (code, text) = self.cmd("PASV")?;
        if code != 227 {
            return Err(FtpErr::Retry(format!(
                "the FTP server at {} refused passive mode (reply {code})",
                self.display
            )));
        }
        parse_pasv(&text).ok_or_else(|| {
            FtpErr::Retry(format!(
                "the FTP server at {} sent an unreadable passive-mode reply",
                self.display
            ))
        })
    }

    /// Open the data connection and start a RETR of `path` at
    /// `offset` (REST first when offset > 0). Returns the data
    /// stream positioned at the first wanted byte. The control
    /// connection stays OPEN in the caller for the whole transfer
    /// (servers may abort a transfer whose control connection
    /// closes) and drops with it when the run ends.
    fn retr(&mut self, target: &FtpTarget, offset: u64, read_timeout: Duration) -> Result<TcpStream, FtpErr> {
        let (host, port) = self.pasv()?;
        // A PASV reply pointing anywhere but the control peer is a
        // bounce-attack shape (and useless for NAT'd servers that
        // advertise their LAN address — those still serve the data
        // on the peer we know): always dial the control peer, only
        // the port comes from the reply. The peer ADDRESS comes
        // from the live control connection, not a fresh DNS lookup
        // of the host name (M-5).
        let _ = host;
        let addr = SocketAddr::new(self.peer.ip(), port);
        let data = TcpStream::connect_timeout(&addr, Duration::from_secs(15)).map_err(|e| {
            FtpErr::Retry(format!("could not open the FTP data connection at {}: {e}", self.display))
        })?;
        let _ = data.set_read_timeout(Some(read_timeout));
        let _ = data.set_nodelay(true);
        if offset > 0 {
            let (code, _) = self.cmd(&format!("REST {offset}"))?;
            if code != 350 {
                return Err(FtpErr::RestRefused(format!(
                    "the FTP server at {} refused to resume at byte {offset} (reply {code})",
                    self.display
                )));
            }
        }
        let (code, _) = self.cmd(&format!("RETR {}", target.path))?;
        match code {
            150 | 125 => Ok(data),
            550 => Err(FtpErr::Permanent(format!(
                "the FTP server at {} says the file is not available (550)",
                self.display
            ))),
            530 => Err(FtpErr::Permanent(format!(
                "the FTP server at {} refused the login (530) — check the user name and password",
                self.display
            ))),
            c => Err(FtpErr::Retry(format!(
                "the FTP server at {} answered {c} to the download request",
                self.display
            ))),
        }
    }

    fn quit(mut self) {
        let _ = self.cmd("QUIT");
    }
}

fn parse_pasv(text: &str) -> Option<(String, u16)> {
    let start = text.find('(')?;
    let end = text[start..].find(')')? + start;
    let nums: Vec<u16> = text[start + 1..end]
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    if nums.len() != 6 {
        return None;
    }
    Some((
        format!("{}.{}.{}.{}", nums[0], nums[1], nums[2], nums[3]),
        (nums[4] << 8) | nums[5],
    ))
}

// ---------- probe ----------

const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_READ_TIMEOUT: Duration = Duration::from_secs(15);
const TRANSFER_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TRANSFER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The FTP probe — the same contract as the HTTP probe: total
/// from SIZE, resumable only when a real REST is accepted (never
/// on a server's say-so alone), the validator from MDTM, the name
/// from the path. Transient transport failures retry on the HTTP
/// probe's cadence (300 ms, then 900 ms, 30 s budget); a
/// definitive answer (530, 550) returns at once.
pub(crate) fn probe_ftp(url: &str, auth: &Auth) -> Result<Probe, String> {
    let started = Instant::now();
    let mut attempt = 0usize;
    loop {
        match probe_ftp_once(url, auth) {
            Ok(p) => return Ok(p),
            Err(FtpErr::Permanent(m)) => return Err(m),
            Err(e) => {
                let m = e.message();
                attempt += 1;
                if attempt >= 3 || started.elapsed() > Duration::from_secs(30) {
                    return Err(m);
                }
                std::thread::sleep(Duration::from_millis(if attempt == 1 { 300 } else { 900 }));
            }
        }
    }
}

fn probe_ftp_once(url: &str, auth: &Auth) -> Result<Probe, FtpErr> {
    let target = parse_target(url, auth).map_err(FtpErr::Permanent)?;
    let mut conn = FtpConn::connect(&target, PROBE_CONNECT_TIMEOUT, PROBE_READ_TIMEOUT)?;
    conn.binary()?;
    // existence first: a 550 on SIZE for a missing file is a
    // definitive answer, not an "unknown size" — but many servers
    // refuse SIZE on files that RETR fine, so only the pair
    // (SIZE 550 AND MDTM 550) settles it
    let size = conn.size(&target.path);
    let mdtm = conn.mdtm(&target.path);
    if size.is_none() && mdtm.is_none() {
        // distinguish "no SIZE support" from "no such file" with
        // the cheapest available witness: REST works on a real
        // file even when SIZE does not
        let (code, _) = conn.cmd(&"REST 0".to_string())?;
        if code == 550 {
            return Err(FtpErr::Permanent(format!(
                "the FTP server at {} says the file is not available (550)",
                target.display
            )));
        }
    }
    let (total, unknown_size) = match size {
        Some(t) => (t, false),
        None => (0, true),
    };
    // resumable = SIZE gave a total AND a real REST inside the
    // file is accepted (the connection is disposable, so the
    // pending restart point costs nothing)
    let mut range_ok = false;
    if !unknown_size && total > 1 {
        let (code, _) = conn.cmd("REST 1")?;
        range_ok = code == 350;
    }
    conn.quit();
    let name = file_name_of(url);
    Ok(Probe {
        total,
        range_ok,
        unknown_size,
        etag: None,
        last_modified: mdtm,
        suggested_name: if name == "index" { None } else { Some(name) },
        final_url: url.to_string(),
        ignore_encoding_header: false,
        cookies: CookieJar::new(),
        range_unverified: false,
    })
}

/// Byte-compare one range of the partial file against a fresh
/// FTP fetch of the same bytes — the FTP arm of the engine's
/// resume verification (`verify_block_bytes` dispatches here for
/// ftp:// URLs). Any failure returns false and the caller starts
/// fresh rather than splicing unverified data.
pub(crate) fn verify_block_ftp(
    url: &str,
    auth: &Auth,
    out: &Path,
    start: u64,
    end: u64,
) -> bool {
    let want = (end - start) as usize;
    if want == 0 {
        return true;
    }
    let Ok(target) = parse_target(url, auth) else {
        return false;
    };
    let Ok(mut conn) = FtpConn::connect(&target, PROBE_CONNECT_TIMEOUT, PROBE_READ_TIMEOUT) else {
        return false;
    };
    if conn.binary().is_err() {
        return false;
    }
    let Ok(data) = conn.retr(&target, start, PROBE_READ_TIMEOUT) else {
        return false;
    };
    let mut remote = Vec::with_capacity(want);
    if data
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

// ---------- ranged transfer (the FTP worker pool) ----------

/// One block run over one fresh control connection — the FTP
/// analogue of `download_block_run`, with the same loop contract:
/// pacing before every read, a read chunk caught by a halt is
/// discarded unwritten, the per-connection 256 KiB / 30 s stall
/// watermark recycles a dribbling connection, blocks are marked
/// and journalled as they complete, and an armed work-stealing
/// cap can yield the run's unstarted tail to idle peers.
#[allow(clippy::too_many_arguments)]
fn ftp_download_block_run(
    target: &FtpTarget,
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
    // G1 surface: 1 = request being sent, 2 = receiving
    if let Some(p) = shared.conn_phase.get(worker_id) {
        p.store(1, Ordering::Relaxed);
    }
    if let Some(b) = shared.conn_block.get(worker_id) {
        b.store(*cur_block, Ordering::Relaxed);
    }
    let mut conn = match FtpConn::connect(target, TRANSFER_CONNECT_TIMEOUT, TRANSFER_READ_TIMEOUT) {
        Ok(c) => c,
        Err(e) => return Err(e.into_block()),
    };
    if let Err(e) = conn.binary() {
        return Err(e.into_block());
    }
    let retr_offset = *pos;
    let mut reader = match conn.retr(target, retr_offset, TRANSFER_READ_TIMEOUT) {
        Ok(d) => {
            // a RETR accepted at a resume offset is the FTP shape
            // of a validated 206: the range was honoured, so the
            // range-loss count clears — only PERSISTENT refusals
            // declare the loss (M-2, mirroring the HTTP path's F6
            // reset; without it, refusals scattered across a long
            // download's life accumulated into a phantom loss)
            if retr_offset > 0 {
                shared.range_200_count.store(0, Ordering::Relaxed);
            }
            d
        }
        Err(FtpErr::RestRefused(m)) => {
            // the server accepted REST at probe time and refuses
            // now — the range-loss shape: count it like the HTTP
            // path counts 200s, and at the limit the job restarts
            // single-stream (once) instead of retrying offsets
            note_range_loss(shared);
            if shared.single_fallback.load(Ordering::Relaxed) {
                return Err(if worker_id == 0 {
                    BlockError::Recycle
                } else {
                    BlockError::Cancelled
                });
            }
            return Err(BlockError::Retry(m));
        }
        Err(e) => return Err(e.into_block()),
    };
    if let Some(p) = shared.conn_phase.get(worker_id) {
        p.store(2, Ordering::Relaxed);
    }
    let resp_end = end;
    let mut buf = vec![0u8; 512 * 1024];
    let mut cap_block: Option<usize> = None;
    // per-connection stall watermark (F2 rule, shared helper)
    let mut wm_pos = *pos;
    let mut wm_clock = Instant::now();
    loop {
        if shared.single_fallback.load(Ordering::Relaxed) {
            return Err(if worker_id == 0 {
                BlockError::Recycle
            } else {
                BlockError::Cancelled
            });
        }
        if shared.superseded.load(Ordering::Relaxed) {
            return Err(BlockError::Cancelled);
        }
        if shared.control.cancelled.load(Ordering::Relaxed) {
            return Err(BlockError::Cancelled);
        }
        if shared.control.paused.load(Ordering::Relaxed) {
            return Err(BlockError::Paused);
        }
        let remaining = match cap_block {
            Some(b) => {
                let cap_byte = ((b as u64) * block_size).min(total);
                (resp_end + 1 - *pos).min(cap_byte.saturating_sub(*pos))
            }
            None => resp_end + 1 - *pos,
        };
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
        // a halt that landed while this read was in flight: the
        // chunk is discarded, never written (same rule as HTTP)
        if shared.superseded.load(Ordering::Relaxed) {
            return Err(BlockError::Cancelled);
        }
        if shared.control.cancelled.load(Ordering::Relaxed) {
            return Err(BlockError::Cancelled);
        }
        if shared.control.paused.load(Ordering::Relaxed) {
            return Err(BlockError::Paused);
        }
        crate::write_at(file, &buf[..n], *pos)
            .map_err(|e| BlockError::Retry(plain_write_error(&e)))?;
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
        if !note_stall_progress(*pos, &mut wm_pos, &mut wm_clock)
            && wm_clock.elapsed() > STALL_LIMIT
        {
            return Err(BlockError::Recycle);
        }
        while *cur_block < b_end_excl {
            let b_limit = ((*cur_block as u64 + 1) * block_size).min(total);
            if *pos >= b_limit {
                if shared.superseded.load(Ordering::Relaxed) {
                    return Err(BlockError::Cancelled);
                }
                {
                    let mut st = plock(&shared.state);
                    if st.done[*cur_block] == 0 {
                        st.done[*cur_block] = 1;
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
        save_state(shared, false, Some(file));
        if *cur_block < b_end_excl {
            let b_base = (*cur_block as u64) * block_size;
            shared.partial[worker_id].store((*pos).saturating_sub(b_base), Ordering::Relaxed);
        } else {
            shared.partial[worker_id].store(0, Ordering::Relaxed);
        }
        if cap_block.is_none() && split_allowed(shared) {
            let first_unwritten = if *pos > (*cur_block as u64) * block_size {
                *cur_block + 1
            } else {
                *cur_block
            };
            cap_block = split_point(first_unwritten, b_end_excl);
        }
    }
    if let Some(split) = cap_block {
        let cap_byte = ((split as u64) * block_size).min(total);
        if *pos == cap_byte && *pos < resp_end + 1 {
            return Err(BlockError::Yielded(split));
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

/// The FTP worker — a structural copy of the HTTP `worker` loop
/// (claim, gate, wait, hand-back and failure accounting are the
/// same discipline) driving FTP block runs instead of HTTP range
/// requests.
pub(crate) fn ftp_worker(
    id: usize,
    target: FtpTarget,
    file: Arc<File>,
    shared: Arc<Shared>,
    total_blocks: usize,
) {
    let (block_size, total) = {
        let st = plock(&shared.state);
        (st.block_size, st.total)
    };
    loop {
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
        if ftp_route_fallback(id, &target, &file, &shared) {
            return;
        }
        let active = shared.active_limit.load(Ordering::Relaxed).max(1);
        if id >= active {
            if ftp_route_fallback(id, &target, &file, &shared) {
                return;
            }
            if crate::fail_if_stalled(&shared, total_blocks) {
                return;
            }
            std::thread::sleep(Duration::from_millis(80));
            continue;
        }
        let (claim_start, mut claim_end) = {
            let mut waiter: Option<WaiterGuard> = None;
            loop {
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
                if ftp_route_fallback(id, &target, &file, &shared) {
                    return;
                }
                if crate::fail_if_stalled(&shared, total_blocks) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };

        let mut cursor = claim_start;
        while cursor < claim_end {
            if shared.single_fallback.load(Ordering::Relaxed) {
                break;
            }
            if split_allowed(&shared) {
                if let Some(split) = split_point(cursor, claim_end) {
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
                    // B-4 parity with the HTTP worker (M-4): a
                    // partial block in the group's MIDDLE ends the
                    // group before it — the run advances linearly
                    // and would otherwise plough over that block's
                    // journaled prefix from its start, re-fetching
                    // bytes the journal already vouches for
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
            // B-4 parity (M-4): resume inside the first block at
            // its journaled prefix — those bytes are already in
            // the file (the run's RETR starts at exactly this
            // offset), instead of re-fetching the whole block on
            // every restart as this path used to
            let mut cur_pos = start + resume_off;
            let mut attempt = 0;
            shared.block_state[cur_block].store(2, Ordering::Relaxed);
            if let Some(b) = shared.conn_block.get(id) {
                b.store(cur_block, Ordering::Relaxed);
            }

            loop {
                if shared.single_fallback.load(Ordering::Relaxed) {
                    break;
                }
                let before = (cur_block, cur_pos);
                let run_result = ftp_download_block_run(
                    &target,
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
                        "the FTP server sent an empty data stream for a block run".to_string(),
                    )),
                    other => other,
                };
                match run_result {
                    Ok(()) => {
                        attempt = 0;
                        shared.partial[id].store(0, Ordering::Relaxed);
                        save_state(&shared, false, Some(&*file));
                        if cur_block >= b_last {
                            break;
                        }
                    }
                    Err(BlockError::Yielded(split)) => {
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
                    Err(BlockError::YieldedByte(_)) => {
                        // FTP runs never arm a tail byte-split
                        // (that is the HTTP helper path's alone);
                        // if one ever surfaced here, re-running
                        // the same sub-run from its cursor is the
                        // safe response
                        continue;
                    }
                    Err(BlockError::Recycle) => {
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

/// When REST support is lost mid-download, worker 0 runs the
/// single-stream restart and every other worker stands down —
/// the same routing as the HTTP path's `route_fallback`.
fn ftp_route_fallback(
    id: usize,
    target: &FtpTarget,
    file: &Arc<File>,
    shared: &Arc<Shared>,
) -> bool {
    if !shared.single_fallback.load(Ordering::Relaxed) {
        return false;
    }
    if id == 0 {
        ftp_single_fallback_download(target, file, shared);
    }
    true
}

/// The single-stream restart inside a ranged FTP job whose REST
/// support vanished: one plain RETR streamed over the file from
/// byte 0, with pacing, pause/cancel and the 256 KiB stall
/// watermark exactly as on the HTTP fallback.
fn ftp_single_fallback_download(target: &FtpTarget, file: &Arc<File>, shared: &Arc<Shared>) {
    let (total, total_blocks) = {
        let st = plock(&shared.state);
        (st.total, st.done.len())
    };
    let attempt = || -> Result<bool, String> {
        let mut conn = FtpConn::connect(target, TRANSFER_CONNECT_TIMEOUT, TRANSFER_READ_TIMEOUT)
            .map_err(|e| e.message())?;
        conn.binary().map_err(|e| e.message())?;
        if let Some(p) = shared.conn_phase.first() {
            p.store(2, Ordering::Relaxed);
        }
        let mut reader = conn.retr(target, 0, TRANSFER_READ_TIMEOUT).map_err(|e| e.message())?;
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
                continue;
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
            crate::write_at(file, &buf[..n], pos).map_err(|e| plain_write_error(&e))?;
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
            return Err(format!("short download: got {pos} of {total} bytes"));
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
            shared.done_count.store(total_blocks, Ordering::Relaxed);
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

// ---------- single-stream transfer (no usable REST) ----------

/// Whether a single-stream FTP attempt's error is definitive —
/// the server has given an answer retrying cannot change (login
/// refused, file not available), mirroring the HTTP path's
/// `single_error_is_permanent`.
/// The FTP single-stream start — used when the probe found no
/// usable REST (or no SIZE): no resume, no acceleration, exactly
/// the HTTP single-stream contract. The same spool protection
/// applies (M-1): when a resume journal exists for this path,
/// the body spools to a temp file and only replaces the partial
/// once verified complete. Pacing, the held-chunk halt rule and
/// the 256 KiB stall watermark are the HTTP path's own.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_ftp_single_stream(
    url: &str,
    out: &Path,
    probed: Probe,
    control: Arc<Control>,
    state_path: PathBuf,
    auth: &Auth,
    in_flight: InFlightGuard,
) -> Result<Arc<Download>, String> {
    let target = parse_target(url, auth)?;
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            url: journal_url(url),
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
    let out = out.to_path_buf();
    std::thread::spawn(move || {
        // M-1 spool rule, identical to the HTTP path
        let spool = shared.state_path.exists() && out.exists();
        let spool_path: PathBuf = {
            let mut s = out.clone().into_os_string();
            s.push(".ndm.spool");
            PathBuf::from(s)
        };
        // M-3: attempts return the classified FtpErr itself, so
        // the retry policy keys off the STRUCTURE (a 530/550 the
        // server actually answered, classified at the source in
        // connect/login/retr) — never a substring scan of a
        // message string that embeds the file's own path, where a
        // file named "x (550).bin" made every transient failure
        // look permanent.
        let attempt_once = || -> Result<(), FtpErr> {
            shared.active_workers.store(1, Ordering::Relaxed);
            if let Some(p) = shared.conn_phase.first() {
                p.store(1, Ordering::Relaxed);
            }
            let mut conn = FtpConn::connect(&target, TRANSFER_CONNECT_TIMEOUT, TRANSFER_READ_TIMEOUT)?;
            conn.binary()?;
            if let Some(p) = shared.conn_phase.first() {
                p.store(2, Ordering::Relaxed);
            }
            let mut reader = conn.retr(&target, 0, TRANSFER_READ_TIMEOUT)?;
            let write_path: &Path = if spool { &spool_path } else { &out };
            let f = File::create(write_path).map_err(|e| match crate::disk_full_sentence(&e) {
                Some(s) => FtpErr::Retry(format!("{s} (create: {e})")),
                None => FtpErr::Retry(format!("create: {e}")),
            })?;
            let mut buf = vec![0u8; 512 * 1024];
            let mut pos = 0u64;
            // a chunk caught by a halt is HELD, not dropped — the
            // stream cannot re-serve it (same rule as HTTP single)
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
                    stall_watermark = pos;
                    stall_clock = Instant::now();
                }
                if stall_clock.elapsed() > STALL_LIMIT {
                    return Err(FtpErr::Retry(format!(
                        "no progress for {}s — the server stopped sending data",
                        STALL_LIMIT.as_secs()
                    )));
                }
                if held == 0 {
                    let grant = pace_grant_size(&shared, buf.len());
                    if !pace_before_read(&shared, grant as u64) {
                        continue;
                    }
                    let n = reader.read(&mut buf[..grant]).map_err(|e| {
                        pace_refund_both(&shared, grant as u64);
                        FtpErr::Retry(format!("read: {e}"))
                    })?;
                    if (n as u64) < grant as u64 {
                        pace_refund_both(&shared, grant as u64 - n as u64);
                    }
                    if n == 0 {
                        break;
                    }
                    held = n;
                    if shared.control.cancelled.load(Ordering::Relaxed)
                        || shared.control.paused.load(Ordering::Relaxed)
                    {
                        continue;
                    }
                }
                let n = held;
                held = 0;
                crate::write_at(&f, &buf[..n], pos).map_err(|e| FtpErr::Retry(plain_write_error(&e)))?;
                pos += n as u64;
                if note_stall_progress(pos, &mut stall_watermark, &mut stall_clock) {
                    *plock(&shared.last_progress) = stall_clock;
                }
                if expected_total > 0 && pos > expected_total {
                    if spool {
                        let _ = std::fs::remove_file(&spool_path);
                    }
                    return Err(FtpErr::Retry(format!(
                        "server sent more data than the probed size ({expected_total} bytes)"
                    )));
                }
                shared.fetched_session.fetch_add(n as u64, Ordering::Relaxed);
                if let Some(cb) = shared.conn_bytes.first() {
                    cb.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            if expected_total > 0 && pos != expected_total {
                if spool {
                    let _ = std::fs::remove_file(&spool_path);
                }
                return Err(FtpErr::Retry(format!(
                    "short download: got {pos} of {expected_total} bytes"
                )));
            }
            if expected_total == 0 {
                // H-1: with no probed size, the byte count cannot
                // witness completion — the server's post-transfer
                // control reply is the only witness there is. A
                // clean EOF on the data connection proves nothing
                // by itself: a server (or middlebox) that cuts a
                // transfer short closes the data connection in
                // exactly the same way, and only the control reply
                // tells the two apart — 226 confirms the whole
                // file arrived; 426 (or anything else, or no reply
                // at all) means it did not, so the attempt fails
                // and the state file stays for a later resume
                // instead of a truncated file being crowned
                // Finished and the journal deleted.
                match conn.read_reply() {
                    Ok((226, _)) => {}
                    Ok((code, _)) => {
                        if spool {
                            let _ = std::fs::remove_file(&spool_path);
                        }
                        return Err(FtpErr::Retry(format!(
                            "the FTP server at {} did not confirm the transfer completed (reply {code}) — the file may be incomplete",
                            target.display
                        )));
                    }
                    Err(e) => {
                        if spool {
                            let _ = std::fs::remove_file(&spool_path);
                        }
                        return Err(e);
                    }
                }
            }
            if spool {
                drop(f);
                if let Err(e) = std::fs::rename(&spool_path, &out) {
                    let _ = std::fs::remove_file(&spool_path);
                    return Err(FtpErr::Retry(format!("could not replace the partial file: {e}")));
                }
            }
            Ok(())
        };
        // no resume on this path: a dropped connection retries the
        // whole transfer, up to 5 attempts with 1/2/4/8 s backoff,
        // exactly like the HTTP single-stream path
        let mut attempt = 0u32;
        let result: Result<(), String> = loop {
            attempt += 1;
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
                    // M-3: permanence is the error's own class,
                    // decided where the reply code was seen — not
                    // a substring of the message text
                    let permanent = matches!(e, FtpErr::Permanent(_));
                    if permanent || attempt >= 5 {
                        break Err(e.message());
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
        // guard first, settled second — settled ⇒ path
        // released (the uniform contract; see lib.rs, CI #75)
        drop(in_flight);
        shared.settled.store(true, Ordering::SeqCst);
    });
    Ok(download)
}

// ---------- tests (engine-level, local fixture server) ----------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{probe, start, state_path_for, Download, Status};
    use std::process::{Child, Command};

    /// Every FTP test binds the same fixture port (8921), so the
    /// whole module runs serially.
    static SERIAL: Mutex<()> = Mutex::new(());
    const PORT: u16 = 8921;

    /// Lock SERIAL, recovering from poison: the guard carries no
    /// data (it only serializes the port), so one test's panic
    /// must not cascade-fail its siblings (CI #79: a fixture
    /// startup panic failed all six FTP tests, five of them
    /// with PoisonError before they even ran).
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pattern_body(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
    }

    struct Server {
        child: Child,
        dir: PathBuf,
        log: PathBuf,
    }

    impl Server {
        fn start(name: &str, files: &[(&str, Vec<u8>)], extra: &[&str]) -> Server {
            let dir = std::env::temp_dir().join(format!("ndm-ftp-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for (n, body) in files {
                std::fs::write(dir.join(n), body).unwrap();
            }
            let log = dir.join("server.log");
            let ready = dir.join("ready");
            let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ftp_server.py");
            let mut child = Command::new("python3")
                .args([
                    script,
                    "--root",
                    dir.to_str().unwrap(),
                    "--port",
                    &PORT.to_string(),
                    "--log",
                    log.to_str().unwrap(),
                    "--ready",
                    ready.to_str().unwrap(),
                ])
                .args(extra)
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("python3 fixture server");
            // 60 s, not 10: a cold Windows runner (process
            // creation + AV scan of a fresh python) ate the
            // whole 10 s once (CI #79) and the panic cascaded
            // through the SERIAL lock to all six FTP tests.
            // Fail fast — with the real cause — if the child
            // dies first, and never leak it holding the port.
            let t0 = Instant::now();
            while !ready.exists() {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("fixture server exited early: {status}");
                }
                if t0.elapsed() >= Duration::from_secs(60) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("fixture server never came up");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Server { child, dir, log }
        }
        fn log_text(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
        /// RETR evidence lines as (conn id, offset) pairs.
        fn retrs(&self) -> Vec<(u64, u64)> {
            self.log_text()
                .lines()
                .filter(|l| l.starts_with("RETR "))
                .filter_map(|l| {
                    let mut conn = None;
                    let mut off = None;
                    for tok in l.split_whitespace() {
                        if let Some(v) = tok.strip_prefix("conn=") {
                            conn = v.parse().ok();
                        }
                        if let Some(v) = tok.strip_prefix("offset=") {
                            off = v.parse().ok();
                        }
                    }
                    Some((conn?, off?))
                })
                .collect()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn tmp_out(name: &str) -> PathBuf {
        let out = std::env::temp_dir().join(format!("ndm-ftp-{name}-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
        out
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
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn reply_lines_are_length_capped() {
        // W3-13: a server streaming one endless line must fail as
        // a protocol Retry, not grow the line buffer unbounded
        let _serial = serial();
        for (payload, expect_ok) in [
            (b"220 ready\r\n".to_vec(), true),
            (vec![b'x'; 9000], false),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut sock, _) = listener.accept().unwrap();
                use std::io::Write as _;
                let _ = sock.write_all(&payload);
                std::thread::sleep(Duration::from_millis(300));
            });
            let stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            let peer = stream.peer_addr().unwrap();
            let mut conn = FtpConn {
                reader: BufReader::new(stream),
                display: "test".to_string(),
                peer,
            };
            let res = conn.read_reply();
            if expect_ok {
                match res {
                    Ok((code, _)) => assert_eq!(code, 220),
                    Err(e) => panic!("expected a 220 reply, got: {}", e.message()),
                }
            } else {
                let msg = res.unwrap_err().message();
                assert!(msg.contains("longer than"), "unexpected error: {msg}");
            }
            let _ = server.join();
        }
    }

    #[test]
    fn is_ftp_url_is_char_boundary_safe() {
        // M-1 regression: byte 6 inside a multi-byte character
        // used to panic the slicing thread outright
        assert!(!is_ftp_url("ftpxxé"));
        assert!(!is_ftp_url("ftp:/é"));
        assert!(is_ftp_url("ftp://127.0.0.1/f.bin"));
        assert!(is_ftp_url("FTP://127.0.0.1/f.bin"));
        assert!(!is_ftp_url("ftp:/f.bin"));
        assert!(!is_ftp_url("https://example.com/f.bin"));
        assert!(!is_ftp_url("ftp"));
    }

    #[test]
    fn parse_target_decodes_userinfo_and_redacts_display() {
        let t = parse_target(
            "ftp://nill:p%40ss%3Aword@127.0.0.1:2121/dir/f%20ile.bin",
            &Auth::default(),
        )
        .unwrap();
        assert_eq!(t.user, "nill");
        assert_eq!(t.pass, "p@ss:word");
        assert_eq!(t.port, 2121);
        assert_eq!(t.path, "dir/f ile.bin");
        assert!(!t.display.contains("p@ss"), "display leaked the password: {}", t.display);
        assert!(!t.display.contains("p%40ss"), "display leaked the password: {}", t.display);
        let anon = parse_target("ftp://127.0.0.1/f.bin", &Auth::default()).unwrap();
        assert_eq!(anon.user, "anonymous");
        assert_eq!(anon.port, 21);
    }

    #[test]
    fn parse_target_rejects_crlf_injection() {
        // %0d%0a in the password would inject a second command
        // into the PASS line; in the path, into RETR.
        assert!(parse_target(
            "ftp://user:pa%0d%0aDELE%20x@127.0.0.1/f.bin",
            &Auth::default()
        )
        .is_err());
        assert!(parse_target(
            "ftp://127.0.0.1/f%0aDELE%20other.bin",
            &Auth::default()
        )
        .is_err());
        assert!(parse_target(
            "ftp://us%0der@127.0.0.1/f.bin",
            &Auth::default()
        )
        .is_err());
    }

    #[test]
    fn journal_url_never_carries_the_password() {
        let j = journal_url("ftp://nill:s3cret@127.0.0.1:2121/dir/f.bin");
        assert!(!j.contains("s3cret"), "journal url leaked the password: {j}");
        assert!(!j.contains("nill"), "journal url leaked the user: {j}");
        assert!(j.contains("127.0.0.1"), "journal url lost the host: {j}");
        // non-FTP and credential-free URLs pass through untouched
        assert_eq!(journal_url("https://example.com/f.bin"), "https://example.com/f.bin");
        assert_eq!(journal_url("ftp://127.0.0.1/f.bin"), "ftp://127.0.0.1:21/f.bin");
    }

    /// A scripted in-process FTP server for the unknown-size
    /// single-stream path (H-1): SIZE and MDTM are refused, so
    /// the probe reports an unknown-size, non-resumable file;
    /// RETR serves `send` bytes over the data connection, closes
    /// it with a clean FIN, and only then answers `final_code`
    /// on the control connection — the exact shape in which a
    /// truncated transfer is indistinguishable from a complete
    /// one without reading that reply. Connections are handled
    /// on detached threads over an ephemeral port, so nothing
    /// here can collide with the fixture server's fixed port.
    fn spawn_fake_ftp(send: Vec<u8>, final_code: u16) -> u16 {
        use std::io::{BufRead, BufReader, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut sock) = stream else { continue };
                let send = send.clone();
                std::thread::spawn(move || {
                    let Ok(mut reader) = sock.try_clone().map(BufReader::new) else {
                        return;
                    };
                    let mut data_listener: Option<std::net::TcpListener> = None;
                    let _ = sock.write_all(b"220 fake ready\r\n");
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let cmd = line.trim().to_ascii_uppercase();
                        let reply: String = if cmd.starts_with("USER") {
                            "331 password required\r\n".to_string()
                        } else if cmd.starts_with("PASS") {
                            "230 logged in\r\n".to_string()
                        } else if cmd.starts_with("SYST") {
                            "215 UNIX\r\n".to_string()
                        } else if cmd.starts_with("TYPE") {
                            "200 binary mode\r\n".to_string()
                        } else if cmd.starts_with("SIZE") || cmd.starts_with("MDTM") {
                            "550 not available\r\n".to_string()
                        } else if cmd.starts_with("REST") {
                            "350 restart ok\r\n".to_string()
                        } else if cmd.starts_with("PASV") {
                            match std::net::TcpListener::bind("127.0.0.1:0") {
                                Ok(l) => {
                                    let p = l.local_addr().unwrap().port();
                                    data_listener = Some(l);
                                    format!(
                                        "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                                        p >> 8,
                                        p & 0xff
                                    )
                                }
                                Err(_) => "425 no data listener\r\n".to_string(),
                            }
                        } else if cmd.starts_with("RETR") {
                            let _ = sock.write_all(b"150 opening data connection\r\n");
                            if let Some(l) = data_listener.take() {
                                if let Ok((mut data, _)) = l.accept() {
                                    let _ = data.write_all(&send);
                                    // clean FIN — byte-for-byte the
                                    // close of a complete transfer
                                    drop(data);
                                }
                            }
                            format!("{final_code} closing\r\n")
                        } else if cmd.starts_with("QUIT") {
                            let _ = sock.write_all(b"221 bye\r\n");
                            break;
                        } else {
                            "502 not implemented\r\n".to_string()
                        };
                        if sock.write_all(reply.as_bytes()).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        port
    }

    #[test]
    fn unknown_size_ftp_finishes_only_on_a_226_witness() {
        let _serial = serial();
        let body = pattern_body(300_000);
        let port = spawn_fake_ftp(body.clone(), 226);
        let url = format!("ftp://127.0.0.1:{port}/file.bin");
        let out = tmp_out("witness-ok");
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        wait_settled(&dl, Duration::from_secs(10));
        assert!(
            !state_path_for(&out).exists(),
            "a witnessed finish removes the journal"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn unknown_size_ftp_abort_426_is_an_error_not_a_finish() {
        let _serial = serial();
        // the server sends only part of the body, closes the data
        // connection cleanly, then answers 426 — before the fix
        // this exact shape was crowned Finished and the journal
        // deleted, leaving a truncated file with no way back
        let body = pattern_body(300_000);
        let partial = body[..100_000].to_vec();
        let port = spawn_fake_ftp(partial.clone(), 426);
        let url = format!("ftp://127.0.0.1:{port}/file.bin");
        let out = tmp_out("witness-abort");
        // a previous attempt's leftovers: a partial file and its
        // journal — the pair the spool rule (and the journal-
        // kept promise) exists for
        let old_partial = body[..50_000].to_vec();
        std::fs::write(&out, &old_partial).unwrap();
        let state = serde_json::json!({
            "url": journal_url(&url),
            "total": 0,
            "block_size": 1,
            "etag": null,
            "last_modified": null,
            "done": [0],
            "partial": [],
        });
        std::fs::write(state_path_for(&out), state.to_string()).unwrap();
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(90)), Some(Status::Error));
        let err = dl.progress().error.clone().unwrap_or_default();
        assert!(
            err.contains("did not confirm the transfer completed"),
            "unexpected error: {err}"
        );
        wait_settled(&dl, Duration::from_secs(10));
        assert!(
            state_path_for(&out).exists(),
            "an unwitnessed transfer keeps its journal for resume"
        );
        assert_eq!(
            std::fs::read(&out).unwrap(),
            old_partial,
            "the unverified spool body must not replace the partial"
        );
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(state_path_for(&out));
    }

    #[test]
    fn ftp_anonymous_download_byte_identical() {
        let _serial = serial();
        let body = pattern_body(3 * 1024 * 1024);
        let _srv = Server::start("anon", &[("big.bin", body.clone())], &[]);
        let url = format!("ftp://127.0.0.1:{PORT}/big.bin");
        let p = probe(&url).expect("ftp probe");
        assert_eq!(p.total, body.len() as u64);
        assert!(p.range_ok, "fixture accepts REST, probe must see resumable");
        assert!(!p.unknown_size);
        assert_eq!(p.suggested_name.as_deref(), Some("big.bin"));
        let out = tmp_out("anon");
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        wait_settled(&dl, Duration::from_secs(10));
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn ftp_user_pass_download_byte_identical() {
        let _serial = serial();
        let body = pattern_body(2 * 1024 * 1024);
        let _srv = Server::start(
            "auth",
            &[("secret.bin", body.clone())],
            &["--user", "nill", "--pass", "pw-test-1"],
        );
        // a wrong password is a clean, credential-free error
        let bad = probe("ftp://nill:WRONGPW@127.0.0.1:8921/secret.bin").unwrap_err();
        assert!(bad.contains("530"), "unexpected error: {bad}");
        assert!(!bad.contains("WRONGPW"), "error leaked the password: {bad}");
        let url = format!("ftp://nill:pw-test-1@127.0.0.1:{PORT}/secret.bin");
        let out = tmp_out("auth");
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        wait_settled(&dl, Duration::from_secs(10));
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn ftp_resume_uses_rest_offsets_no_full_refetch() {
        let _serial = serial();
        let body = pattern_body(4 * 1024 * 1024);
        // throttled to 512 KiB/s per connection so the cancel
        // lands mid-file deterministically
        let srv = Server::start(
            "resume",
            &[("big.bin", body.clone())],
            &["--throttle", "524288"],
        );
        let url = format!("ftp://127.0.0.1:{PORT}/big.bin");
        let out = tmp_out("resume");
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        let t0 = Instant::now();
        while dl.progress().sealed_done_bytes < 1024 * 1024 {
            assert!(t0.elapsed() < Duration::from_secs(30), "never sealed 1 MiB");
            std::thread::sleep(Duration::from_millis(25));
        }
        dl.control.cancel();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(30)), Some(Status::Cancelled));
        wait_settled(&dl, Duration::from_secs(10));
        assert!(state_path_for(&out).exists(), "the resume journal must survive a cancel");
        let phase2_from = srv.log_text().len();
        drop(dl);
        // resume: same start call, the journal does the rest
        let dl2 = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl2, Duration::from_secs(90)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let fetched = dl2.progress().fetched_session;
        assert!(
            fetched < body.len() as u64,
            "resume re-fetched the whole file ({fetched} of {})",
            body.len()
        );
        let phase2 = &srv.log_text()[phase2_from..];
        let retrs: Vec<u64> = phase2
            .lines()
            .filter(|l| l.starts_with("RETR "))
            .filter_map(|l| {
                l.split_whitespace()
                    .find_map(|t| t.strip_prefix("offset="))
                    .and_then(|v| v.parse().ok())
            })
            .collect();
        assert!(!retrs.is_empty(), "no RETR in the resume phase:\n{phase2}");
        assert!(
            retrs.iter().all(|&o| o > 0),
            "resume fetched from offset 0 (full re-fetch): {retrs:?}"
        );
        wait_settled(&dl2, Duration::from_secs(10));
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn ftp_parallel_uses_multiple_control_connections() {
        let _serial = serial();
        let body = pattern_body(6 * 1024 * 1024);
        let srv = Server::start("parallel", &[("big.bin", body.clone())], &[]);
        let url = format!("ftp://127.0.0.1:{PORT}/big.bin");
        let out = tmp_out("parallel");
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let retrs = srv.retrs();
        let mut conns: Vec<u64> = retrs.iter().map(|(c, _)| *c).collect();
        conns.sort_unstable();
        conns.dedup();
        assert!(
            conns.len() >= 2,
            "parallel download used control connections {conns:?} (retrs: {retrs:?})"
        );
        wait_settled(&dl, Duration::from_secs(10));
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn ftp_rest_refused_falls_back_to_single_stream() {
        let _serial = serial();
        let body = pattern_body(2 * 1024 * 1024);
        let srv = Server::start("norest", &[("file.bin", body.clone())], &["--no-rest"]);
        let url = format!("ftp://127.0.0.1:{PORT}/file.bin");
        let p = probe(&url).expect("ftp probe");
        assert_eq!(p.total, body.len() as u64);
        assert!(!p.range_ok, "a REST-refusing server must probe as non-resumable");
        let out = tmp_out("norest");
        let dl = start(&url, &out, 4, 256 * 1024).unwrap();
        assert_eq!(wait_terminal(&dl, Duration::from_secs(60)), Some(Status::Finished));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        let retrs = srv.retrs();
        assert!(
            retrs.iter().any(|(_, o)| *o == 0),
            "single-stream fallback never fetched from offset 0: {retrs:?}"
        );
        wait_settled(&dl, Duration::from_secs(10));
        let _ = std::fs::remove_file(&out);
    }
}
