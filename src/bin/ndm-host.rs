// ndm-host — native messaging bridge between the NDM browser extension and
// the NDM app. The browser launches this binary and speaks the native
// messaging protocol: 4-byte little-endian length prefix + JSON, both ways.
//
// Supports:
// - `type: "ping"`     -> health check
// - `type: "probe"`    -> probes video metadata, thumbnail & per-quality MB
//                         sizes directly for the in-browser IDM popup
// - `type: "download"` -> queues `"file"`, `"video"`, or `"video_direct"`
//                         (pre-selected quality from the in-browser popup)
//                         and ensures `ndm-app` is running.
// - `type: "pause_entry" / "cancel_entry" / "resume_entry"`
//                      -> controls one existing entry via the queue
// - `type: "set_speed_limit"`
//                      -> sets/clears one entry's own speed limit
//                         (`limit_bps`, bytes/sec; null = no limit)
//                         via the queue — the progress card's
//                         Speed Limiter tab
// - `type: "progress"` -> reads the app's per-entry progress file
//                         (progress/<id>.json) for the panel's live card
// - `type: "captured_streams"` -> queues the browser-capture door's
//                         stream set (the streams the browser itself
//                         played, sent only at the user's click) for
//                         the app to store and open its picker on

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::PathBuf;

const HOST_DIR_NAME: &str = "com.ndm.downloadmanager";
const QUEUE_FILE_NAME: &str = "incoming.jsonl";
const MAX_MESSAGE_BYTES: u32 = 1 << 20;

fn app_data_dir() -> io::Result<PathBuf> {
    Ok(data_home()?.join(HOST_DIR_NAME))
}

fn queue_path() -> io::Result<PathBuf> {
    Ok(app_data_dir()?.join(QUEUE_FILE_NAME))
}

#[cfg(windows)]
fn data_home() -> io::Result<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA is not set"))
}

#[cfg(not(windows))]
fn data_home() -> io::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg));
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local").join("share"))
}

fn write_private_file(path: &std::path::Path, content: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Temp + rename (the save_state pattern in the engine): an
    // in-place truncate could leave a torn file for a concurrent
    // reader, and the pid in the temp name keeps two hosts from
    // sharing one temp.
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        {
            let mut f = opts.open(&tmp)?;
            f.write_all(content)?;
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn append_to_queue(mut obj: Value, cookies: &str) -> io::Result<()> {
    let path = queue_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if !cookies.is_empty() {
        obj["cookies"] = json!(cookies);
    }
    let line = obj.to_string() + "\n";
    // Several browser processes can hand over downloads at the same
    // moment; a bare append from each can interleave bytes and corrupt
    // a queue line (one line can be ~600 KB with cookies — far beyond
    // a single atomic write). Take a small lock file first; the spin
    // is generous (~15 s) because an unlocked append can corrupt
    // BOTH lines, while failing this hand-off only delays this one.
    let lock_path = path.with_extension("lock");
    let mut guard: Option<std::fs::File> = None;
    for _ in 0..300 {
        match acquire_queue_lock(&lock_path) {
            Some(f) => {
                guard = Some(f);
                break;
            }
            None => {
                // a DEAD owner's lock is reaped as soon as it is
                // recognised (see lock_is_stale), so a crashed
                // hand-off costs the next one well under a second —
                // only a LIVE owner's lock is waited on
                if lock_is_stale(&lock_path) {
                    // reaped through the fingerprint check, so a
                    // lock a fresh owner created in the meantime is
                    // never the file that gets removed
                    reap_stale_lock(&lock_path);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    if guard.is_none() && reap_stale_lock(&lock_path) {
        // one last recovery attempt after the spin
        guard = acquire_queue_lock(&lock_path);
    }
    if guard.is_none() {
        // No unlocked append: callers answer {ok:false} for this
        // error, so a hand-off is only ever reported queued when
        // its line was durably appended under the lock.
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "queue lock could not be acquired",
        ));
    }
    let result = (|| {
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(line.as_bytes())
    })();
    if let Some(f) = guard.take() {
        // the handle is dropped BEFORE the lock file is removed:
        // on Windows a file cannot be deleted while a handle to it
        // is open, so removing first left the lock behind and every
        // later hand-off paid the dead-lock price for it
        drop(f);
        let _ = std::fs::remove_file(&lock_path);
    }
    result
}

/// Create the queue lock, recording this process's pid inside it so
/// later hosts can tell a live owner from a dead one instantly
/// instead of guessing from the file's age.
fn acquire_queue_lock(lock_path: &std::path::Path) -> Option<std::fs::File> {
    use std::io::Write as _;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
        .ok()?;
    let _ = f.write_all(std::process::id().to_string().as_bytes());
    Some(f)
}

/// Whether this pid currently names a live process.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // /proc/<pid> exists for a ZOMBIE too — the entry only goes
    // away once the parent reaps it — so existence alone made a
    // dead hand-off's lock look owned by a live process and every
    // later hand-off burned its whole spin on it (W1 F12). Read
    // the stat state field instead, the same care app_is_running
    // below already takes: state 'Z' is dead, whatever /proc says.
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(_) => return false,
    };
    match stat.rsplit_once(')') {
        Some((_, rest)) => !rest.trim_start().starts_with('Z'),
        // a stat we cannot parse still proves the process exists;
        // stay conservative and call it alive
        None => true,
    }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    // Win32 probe, declared directly so the engine needs no extra
    // dependency for three kernel32 calls. OpenProcess failing
    // with ERROR_INVALID_PARAMETER names a pid that does not
    // exist: a DEFINITIVE dead, stale at once. Every other
    // outcome stays conservative — access denied means the
    // process exists but is not ours to inspect, and an exit
    // code of STILL_ACTIVE cannot be told apart from a process
    // that genuinely exited with that code — so ambiguity always
    // reports alive and a live owner's lock is never stolen.
    mod win {
        use std::ffi::c_void;
        pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        pub const STILL_ACTIVE: u32 = 259;
        pub const ERROR_INVALID_PARAMETER: u32 = 87;
        extern "system" {
            pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            pub fn GetExitCodeProcess(handle: *mut c_void, code: *mut u32) -> i32;
            pub fn CloseHandle(handle: *mut c_void) -> i32;
            pub fn GetLastError() -> u32;
        }
    }
    unsafe {
        let handle = win::OpenProcess(win::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return win::GetLastError() != win::ERROR_INVALID_PARAMETER;
        }
        let mut code: u32 = 0;
        let ok = win::GetExitCodeProcess(handle, &mut code);
        win::CloseHandle(handle);
        ok == 0 || code == win::STILL_ACTIVE
    }
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    // no cheap liveness probe on this platform: assume alive and
    // let the age backstop below reap true debris
    true
}

/// A queue lock is stale when its owner is gone. The owner records
/// its pid in the file (see acquire_queue_lock): a pid that no
/// longer names a live process is debris from a host killed
/// mid-hand-off and is reaped immediately — the old age-only rule
/// (30 s) made every later hand-off burn the full 15 s spin first.
/// Locks with no readable pid (written by an older host, or caught
/// in the instant between creation and the pid write) get a short
/// grace period, then the same treatment; the 30 s age backstop
/// covers a recorded pid that can no longer be probed.
fn lock_is_stale(lock_path: &std::path::Path) -> bool {
    let age = std::fs::metadata(lock_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    if let Ok(content) = std::fs::read_to_string(lock_path) {
        if let Ok(pid) = content.trim().parse::<u32>() {
            if pid != std::process::id() && !process_alive(pid) {
                return true;
            }
            return age.map(|d| d.as_secs() > 30).unwrap_or(false);
        }
    }
    age.map(|d| d >= std::time::Duration::from_millis(400))
        .unwrap_or(false)
}

/// The identity of one lock file at one instant: its exact content
/// (the owner pid) plus its inode, so a file that was removed and
/// re-created by a NEW owner in between never compares equal to
/// the one that was judged stale.
#[derive(PartialEq, Eq)]
struct LockFingerprint {
    content: Vec<u8>,
    #[cfg(unix)]
    ino: u64,
    len: u64,
}

fn lock_fingerprint(lock_path: &std::path::Path) -> Option<LockFingerprint> {
    let content = std::fs::read(lock_path).ok()?;
    let meta = std::fs::metadata(lock_path).ok()?;
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    };
    Some(LockFingerprint {
        content,
        #[cfg(unix)]
        ino,
        len: meta.len(),
    })
}

/// Remove the lock only if it is still byte-for-byte the file the
/// caller fingerprinted — the close-the-TOCTOU half of the stale
/// reap (W1 F12): `lock_is_stale` → `remove_file` was not atomic
/// with the next owner's `create_new`, so a host that judged a
/// lock stale just before a new owner re-created it could delete
/// the NEW owner's lock and append concurrently with it.
fn remove_lock_if_matches(lock_path: &std::path::Path, expected: &LockFingerprint) -> bool {
    if lock_fingerprint(lock_path).as_ref() != Some(expected) {
        return false;
    }
    std::fs::remove_file(lock_path).is_ok()
}

/// Reap the queue lock if — and only if — it is stale: fingerprint
/// it, judge it, then re-verify the fingerprint immediately before
/// removing, so the file removed is provably the file that was
/// judged stale and never a fresh owner's lock.
fn reap_stale_lock(lock_path: &std::path::Path) -> bool {
    let Some(fp) = lock_fingerprint(lock_path) else {
        return false;
    };
    if !lock_is_stale(lock_path) {
        return false;
    }
    remove_lock_if_matches(lock_path, &fp)
}

/// Apply the app's configured proxy to this process before running
/// probes. The settings live in settings.json in the same data dir
/// as the queue; the proxy is a process-local static in the engine,
/// set by the app for its own process only — without this, the host
/// probed direct while the app's downloads used the proxy, so
/// in-page dialogs failed for hosts that download perfectly fine.
fn apply_saved_proxy_settings() {
    let Ok(dir) = app_data_dir() else { return };
    let Ok(raw) = std::fs::read_to_string(dir.join("settings.json")) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    if let Some(p) = v.get("proxy").and_then(Value::as_str) {
        ndm::set_global_proxy(p);
    }
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(true);
    ndm::set_proxy_protocols(flag("proxy_http"), flag("proxy_https"), flag("proxy_ftp"));
}

const EXT_STAMP_FILE_NAME: &str = "ext_seen.json";
/// A stamp older than this is refreshed even when the version it
/// records is unchanged, so `at` stays a meaningful "the extension
/// was last seen talking" instead of the first sighting forever.
const EXT_STAMP_MAX_AGE_SECS: u64 = 24 * 60 * 60;

fn ext_stamp_path() -> io::Result<PathBuf> {
    Ok(app_data_dir()?.join(EXT_STAMP_FILE_NAME))
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record what a message claims the running extension's version is
/// (EXT-AUTOUPDATE-SPEC §1b): `ext_seen.json` in the data dir,
/// `{ "ext_version": <string|null>, "at": <unix seconds> }`. A ping
/// without the field records `null` — that positively means a
/// pre-handshake extension is alive, not an error. Write-if-changed:
/// the file is rewritten only when the version differs from the
/// stored one or the stored stamp is over 24 h old. The host stays
/// dumb — it never learns the bundled version; comparing is the
/// app's job. Failures are swallowed: a stamp that cannot be
/// written must never change the ping reply.
fn record_ext_seen(stamp_path: &std::path::Path, message: &Value, now_secs: u64) {
    let version: Option<String> = message
        .get("ext_version")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Ok(raw) = std::fs::read_to_string(stamp_path) {
        if let Ok(stored) = serde_json::from_str::<Value>(&raw) {
            let has_field = stored.get("ext_version").is_some();
            let stored_version = stored
                .get("ext_version")
                .and_then(Value::as_str)
                .map(str::to_string);
            let stored_at = stored.get("at").and_then(Value::as_u64).unwrap_or(0);
            if has_field
                && stored_version == version
                && now_secs.saturating_sub(stored_at) <= EXT_STAMP_MAX_AGE_SECS
            {
                return;
            }
        }
    }
    let body = json!({ "ext_version": version, "at": now_secs }).to_string();
    let _ = write_private_file(stamp_path, body.as_bytes());
}

/// Handles one parsed message; returns the reply object to frame back.
fn handle(message: &Value) -> Value {
    let stamp = ext_stamp_path().ok();
    handle_at(message, stamp.as_deref(), unix_now_secs())
}

/// `handle` with the extension-stamp path and clock injected, so
/// tests can observe the stamp without touching the real data dir
/// (the same injection pattern as the queue-lock tests).
fn handle_at(message: &Value, stamp_path: Option<&std::path::Path>, now_secs: u64) -> Value {
    // Any message carrying ext_version stamps it, so a download-only
    // session is seen too; a ping stamps even without the field
    // (null = a pre-handshake extension is alive, §1b). A message
    // with no field must not overwrite a good stamp with its absence.
    if let Some(path) = stamp_path {
        let is_ping = message.get("type").and_then(Value::as_str) == Some("ping");
        let carries = message.get("ext_version").and_then(Value::as_str).is_some();
        if is_ping || carries {
            record_ext_seen(path, message, now_secs);
        }
    }
    match message.get("type").and_then(Value::as_str) {
        Some("ping") => json!({ "ok": true, "pong": true }),
        Some("probe") => handle_probe(message),
        Some("probe_file") => handle_probe_file(message),
        Some("show_app") => handle_show_app(),
        Some("warm") => handle_warm(message),
        Some("download") => handle_download(message),
        Some("pause_entry") => handle_entry_control(message, "pause_entry"),
        Some("cancel_entry") => handle_entry_control(message, "cancel_entry"),
        Some("resume_entry") => handle_entry_control(message, "resume_entry"),
        Some("set_speed_limit") => handle_set_speed_limit(message),
        Some("progress") => handle_progress(message),
        Some("captured_streams") => handle_captured_streams(message),
        Some(other) => json!({ "ok": false, "error": format!("unknown type: {other}") }),
        None => json!({ "ok": false, "error": "missing type" }),
    }
}

/// Convert the extension's Netscape cookie export into a Cookie
/// header for ONE target URL. Only entries the browser would
/// actually send to that URL are included — the whole jar
/// unfiltered shipped unrelated cookies (including ones scoped to
/// other sites) to whatever host was being probed.
fn cookie_header_from_netscape(text: &str, url: &str) -> Option<String> {
    let after = url.split("://").nth(1).unwrap_or("");
    let (hostport, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, "/"),
    };
    let host = hostport
        .split(':')
        .next()
        .unwrap_or(hostport)
        .to_ascii_lowercase();
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let is_https = url.starts_with("https://");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        // Netscape fields: domain, flag, path, secure, expiry, name, value
        if f.len() >= 7 {
            let dom = f[0].trim_start_matches('.').to_ascii_lowercase();
            // SW2-3 (second sweep): honour the include-subdomains
            // flag — TRUE means the cookie was set for the whole
            // domain, FALSE means host-only. Ignoring it shipped
            // host-only cookies to every sibling subdomain.
            let include_subdomains = f[1].trim().eq_ignore_ascii_case("true");
            let domain_match = !dom.is_empty()
                && (host == dom
                    || (include_subdomains && host.ends_with(&format!(".{dom}"))));
            // SW2-3: RFC 6265 path-match — equal, or the cookie
            // path is a prefix that ends at a '/' boundary (the
            // cookie path itself ends with '/', or the next
            // request-path character is '/'). A raw starts_with
            // sent /account cookies to /accountevil on the same host.
            let cookie_path = if f[2].is_empty() { "/" } else { f[2] };
            let path_match = path == cookie_path
                || (path.starts_with(cookie_path)
                    && (cookie_path.ends_with('/')
                        || path[cookie_path.len()..].starts_with('/')));
            if !domain_match || !path_match {
                continue;
            }
            // a Secure cookie must never travel to an http:// target,
            // and an expired one (expiry field, seconds since the
            // epoch; 0 = session cookie) must not be sent at all —
            // the browser itself would withhold both
            if f[3].eq_ignore_ascii_case("true") && !is_https {
                continue;
            }
            if let Ok(expiry) = f[4].trim().parse::<i64>() {
                if expiry > 0 && expiry <= now {
                    continue;
                }
            }
            pairs.push(format!("{}={}", f[5], f[6]));
        }
    }
    if pairs.is_empty() {
        None
    } else {
        Some(pairs.join("; "))
    }
}

fn category_for_name(name: &str) -> &'static str {
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
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

fn default_file_name(url: &str) -> String {
    // An endpoint like /dl?file=report.pdf carries its real name in
    // the query string; the path tail ("dl") is a handler name, and
    // using it saved the file extensionless in the wrong category
    // (F-EXT-11). The engine's rule mirrors the extension's
    // filenameFromUrl, so both sides agree on the name.
    if let Some(q) = ndm::query_file_name(url) {
        return q;
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let tail = path.rsplit('/').next().unwrap_or("download.bin").trim();
    if tail.is_empty() {
        "download.bin".to_string()
    } else {
        tail.to_string()
    }
}

fn handle_probe_file(message: &Value) -> Value {
    let url = message.get("url").and_then(Value::as_str).unwrap_or("").trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return json!({ "ok": false, "error": "url must be a non-empty http(s) URL" });
    }
    let raw_cookies = message.get("cookies").and_then(Value::as_str).unwrap_or("");
    let cookies_str = if raw_cookies.len() <= 600_000 { raw_cookies } else { "" };
    let referer = message
        .get("pageUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let auth = ndm::Auth {
        cookie: cookie_header_from_netscape(cookies_str, url),
        referer,
        authorization: None,
    };
    apply_saved_proxy_settings();
    match ndm::probe_with_auth(url, &auth) {
        Ok(probe) => {
            let suggested = probe
                .suggested_name
                .unwrap_or_else(|| default_file_name(url));
            let category = category_for_name(&suggested);
            json!({
                "ok": true,
                "total": probe.total,
                "resumable": probe.range_ok,
                "unknown_size": probe.unknown_size,
                "suggested_name": suggested,
                "category": category,
            })
        }
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

fn handle_show_app() -> Value {
    let obj = json!({ "kind": "show_app", "url": "https://ndm.local/show" });
    let _ = append_to_queue(obj, "");
    ensure_app_running(false);
    json!({ "ok": true })
}

fn handle_probe(message: &Value) -> Value {
    let url = message.get("url").and_then(Value::as_str).unwrap_or("").trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return json!({ "ok": false, "error": "url must be a non-empty http(s) URL" });
    }
    apply_saved_proxy_settings();
    let raw_cookies = message.get("cookies").and_then(Value::as_str).unwrap_or("");
    let cookies_str = if raw_cookies.len() <= 600_000 { raw_cookies } else { "" };

    let base_dir = match app_data_dir() {
        Ok(d) => d,
        Err(e) => return json!({ "ok": false, "error": format!("data dir error: {e}") }),
    };
    let components_dir = base_dir.join("components");
    if ndm::video_probe::find_tool("yt-dlp", &components_dir).is_none() {
        ensure_app_running(true);
        // the app was just started so the engine can arrive in the
        // background; the user is never told a setup exists — this
        // reads as a transient read state, and any download sent
        // from it queues silently in the app until the engine lands
        return json!({
            "ok": false,
            "error": "Could not read this page yet — please wait a few seconds and click again."
        });
    }

    let mut temp_cookie_path: Option<PathBuf> = None;
    if !cookies_str.is_empty() {
        let p = base_dir
            .join("cookies")
            .join(format!("probe-{}.txt", std::process::id()));
        if write_private_file(&p, cookies_str.as_bytes()).is_ok() {
            temp_cookie_path = Some(p);
        }
    }

    let result = ndm::video_probe::probe(url, &components_dir, temp_cookie_path.as_deref());
    if let Some(p) = temp_cookie_path {
        let _ = std::fs::remove_file(p);
    }

    match result {
        Ok(info) => json!({ "ok": true, "info": info }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

fn new_entry_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{nanos}-{n}", std::process::id())
}

fn wait_for_ack_and_result(id: &str, timeout_secs: u64, queued_on_timeout: bool) -> Value {
    let Ok(base) = app_data_dir() else {
        return json!({ "ok": false, "error": "cannot resolve app data directory" });
    };
    let ack_file = base.join("acks").join(format!("{id}.ack"));
    let res_file = base.join("results").join(format!("{id}.json"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if ack_file.is_file() {
            let _ = std::fs::remove_file(&ack_file);
            // The app writes the result just before the ack; give a
            // torn/raced read a few retries before believing there is
            // no result. Reporting ok:true WITHOUT the result hid real
            // outcomes (e.g. a duplicate refusal) from the extension,
            // which then told the user the send was clean.
            for _ in 0..10 {
                if let Ok(raw) = std::fs::read_to_string(&res_file) {
                    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                        let _ = std::fs::remove_file(&res_file);
                        return v;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
            return json!({
                "ok": false,
                "id": id,
                "error": "NDM acknowledged the request but its result could not be read"
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    if queued_on_timeout {
        // the line IS in the app's queue file — the download was
        // accepted; only the acknowledgement never surfaced in
        // time. Reporting failure here made the extension claim the
        // send failed while the download appeared moments later
        // (and a user retry created a duplicate)
        return json!({ "ok": true, "queued": true, "id": id });
    }
    json!({
        "ok": false,
        "error": "NDM desktop app did not acknowledge the download in time"
    })
}

/// Queue background stream resolutions for a video page the flyout
/// is currently showing, so the quality click that follows starts
/// from the app's resolution cache instead of paying a full yt-dlp
/// resolve first. Fire-and-forget by design: the lines carry no id
/// (the app writes no ack/result for them), no entry is created,
/// and the reply only confirms the lines were queued.
///
/// m-3: warming is purely opportunistic — it exists to make a
/// click that is seconds away faster. It must never START the
/// desktop app: merely opening the flyout on a video page used to
/// launch ndm-app (hidden) on machines where the user had never
/// opened it. When the app is not already running, nothing is
/// queued (a warm line drained at some later launch would be
/// stale anyway) and the reply reports zero.
fn handle_warm(message: &Value) -> Value {
    let url = message.get("url").and_then(Value::as_str).unwrap_or("");
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return json!({ "ok": false, "error": "url must be a non-empty http(s) URL" });
    }
    if !app_is_running() {
        return json!({ "ok": true, "queued": 0 });
    }
    let raw_cookies = message.get("cookies").and_then(Value::as_str).unwrap_or("");
    let cookies = if raw_cookies.len() <= 600_000 { raw_cookies } else { "" };
    let Some(items) = message.get("items").and_then(Value::as_array) else {
        return json!({ "ok": false, "error": "items must be an array" });
    };
    let mut queued = 0usize;
    for item in items.iter().take(6) {
        let selector = item
            .get("selector")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(512)
            .collect::<String>();
        if selector.is_empty() || selector.contains(['\r', '\n']) {
            continue;
        }
        let audio = item.get("audio").and_then(Value::as_bool).unwrap_or(false);
        let obj = json!({
            "kind": "warm",
            "url": url,
            "selector": selector,
            "audio": audio,
        });
        if append_to_queue(obj, cookies).is_ok() {
            queued += 1;
        }
    }
    // no ensure_app_running here (m-3): the gate above already
    // established the app is running and will drain the queue
    json!({ "ok": true, "queued": queued })
}

/// Validate a `captured_streams` message and build its queue
/// object — pure, so the shape is testable without a queue on
/// disk. The browser-capture door's one message: the streams the
/// browser itself played on one page, sent only when the user
/// clicked the capture action. The app stores the set and opens
/// its video picker on the page; the streams' own validation
/// (http(s), bounded) happens here so a malformed send can never
/// reach the queue.
fn captured_queue_object(message: &Value) -> Result<Value, String> {
    let page_url = message
        .get("pageUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !(page_url.starts_with("http://") || page_url.starts_with("https://")) {
        return Err("pageUrl must be a non-empty http(s) URL".to_string());
    }
    let title = message.get("title").and_then(Value::as_str).unwrap_or("");
    if title.contains(['\r', '\n']) {
        return Err("title must not contain line breaks".to_string());
    }
    let title: String = title.chars().take(300).collect();
    let raw = message
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| "streams must be an array".to_string())?;
    if raw.is_empty() || raw.len() > 32 {
        return Err("streams must hold between 1 and 32 entries".to_string());
    }
    let mut streams = Vec::new();
    for s in raw {
        let url = s.get("url").and_then(Value::as_str).unwrap_or("").trim();
        if !(url.starts_with("http://") || url.starts_with("https://"))
            || url.len() > 8192
            || url.contains(['\r', '\n'])
        {
            return Err("stream url must be a non-empty http(s) URL".to_string());
        }
        let mime: String = s
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(128)
            .collect();
        let quality = s
            .get("qualityLabel")
            .and_then(Value::as_str)
            .map(|q| q.chars().take(64).collect::<String>());
        streams.push(json!({
            "url": url,
            "mimeType": mime,
            "qualityLabel": quality,
            "hasAudio": s.get("hasAudio").and_then(Value::as_bool).unwrap_or(false),
            "hasVideo": s.get("hasVideo").and_then(Value::as_bool).unwrap_or(false),
        }));
    }
    Ok(json!({
        "kind": "captured_streams",
        "url": page_url,
        "pageUrl": page_url,
        "title": title,
        "streams": streams,
    }))
}

/// Queue one captured stream set for the app, with the same
/// ack discipline as a download: the reply is the app's own
/// verdict on the drain (stored + picker opened), or the queued
/// truth when the ack outruns the wait.
fn handle_captured_streams(message: &Value) -> Value {
    let mut obj = match captured_queue_object(message) {
        Ok(o) => o,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let raw_cookies = message.get("cookies").and_then(Value::as_str).unwrap_or("");
    let cookies = if raw_cookies.len() <= 600_000 { raw_cookies } else { "" };
    let id = new_entry_id();
    obj["id"] = json!(id);
    match append_to_queue(obj, cookies) {
        Ok(()) => {
            ensure_app_running(true);
            wait_for_ack_and_result(&id, 15, true)
        }
        Err(e) => {
            eprintln!("ndm-host: cannot write queue: {e}");
            json!({ "ok": false, "error": "cannot write queue file" })
        }
    }
}

fn handle_download(message: &Value) -> Value {
    let kind = message.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind != "video" && kind != "file" && kind != "video_direct" && kind != "file_direct" && kind != "grab" {
        return json!({ "ok": false, "error": "kind must be \"video\", \"video_direct\", \"file\", \"file_direct\", or \"grab\"" });
    }
    let url = message.get("url").and_then(Value::as_str).unwrap_or("");
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return json!({ "ok": false, "error": "url must be a non-empty http(s) URL" });
    }
    let title = message.get("title").and_then(Value::as_str).unwrap_or("");
    let page_url = message.get("pageUrl").and_then(Value::as_str).unwrap_or("");
    if title.contains(['\r', '\n']) || page_url.contains(['\r', '\n']) {
        return json!({ "ok": false, "error": "title/pageUrl must not contain line breaks" });
    }
    let title: String = title.chars().take(300).collect();
    let page_url: String = page_url.chars().take(2048).collect();
    let raw_cookies = message.get("cookies").and_then(Value::as_str).unwrap_or("");
    let cookies = if raw_cookies.len() <= 600_000 { raw_cookies } else { "" };

    // A `file` carrying later:true is a queued download, not the
    // app's file dialog: queue it through the file_direct path so
    // its dir/total are honoured and it lands as a paused entry.
    let kind = if kind == "file"
        && message.get("later").and_then(Value::as_bool).unwrap_or(false)
    {
        "file_direct"
    } else {
        kind
    };

    let id = new_entry_id();
    let mut obj = json!({
        "id": id,
        "kind": kind,
        "url": url,
        "title": title,
        "pageUrl": page_url,
    });
    if kind == "video_direct" || kind == "file_direct" {
        let selector = message
            .get("selector")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(512)
            .collect::<String>();
        let dir = message
            .get("dir")
            .and_then(Value::as_str)
            .unwrap_or("~/Downloads")
            .chars()
            .take(512)
            .collect::<String>();
        if selector.contains(['\r', '\n']) || dir.contains(['\r', '\n']) {
            return json!({ "ok": false, "error": "invalid selector or dir" });
        }
        let audio = message.get("audio").and_then(Value::as_bool).unwrap_or(false);
        let later = message.get("later").and_then(Value::as_bool).unwrap_or(false);
        let total = message.get("total").and_then(Value::as_u64).unwrap_or(0);
        let force = message.get("force").and_then(Value::as_bool).unwrap_or(false);
        obj["selector"] = json!(selector);
        obj["dir"] = json!(dir);
        obj["audio"] = json!(audio);
        obj["later"] = json!(later);
        obj["total"] = json!(total);
        obj["force"] = json!(force);
    }

    let is_direct = kind == "video_direct" || kind == "file_direct";
    match append_to_queue(obj, cookies) {
        Ok(()) => {
            ensure_app_running(is_direct);
            wait_for_ack_and_result(&id, 15, true)
        }
        Err(e) => {
            eprintln!("ndm-host: cannot write queue: {e}");
            json!({ "ok": false, "error": "cannot write queue file" })
        }
    }
}

/// The entry-id rules shared by the control and progress paths:
/// the id becomes a queue field and a file name, so it must stay a
/// plain token (the same charset the app accepts for ack ids).
fn valid_entry_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Live progress for one entry, read from the progress file the app
/// maintains beside its acks/results (progress/<id>.json) and
/// returned verbatim with ok:true merged in. No file (or an
/// unparseable one) is a plain {ok:false} — the in-page card polls
/// this once a second and degrades to a static state, so a missing
/// file is an expected answer, not an error worth surfacing.
fn handle_progress(message: &Value) -> Value {
    let id = message.get("id").and_then(Value::as_str).unwrap_or("");
    if !valid_entry_id(id) {
        return json!({ "ok": false, "error": "invalid id" });
    }
    let Ok(base) = app_data_dir() else {
        return json!({ "ok": false, "error": "no progress" });
    };
    let path = base.join("progress").join(format!("{id}.json"));
    let parsed = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    match parsed {
        Some(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("ok".to_string(), json!(true));
            }
            v
        }
        None => json!({ "ok": false, "error": "no progress" }),
    }
}

/// Pause, cancel or resume one existing entry by id — used by the
/// browser dialog's Download Later / Cancel buttons now that the
/// download starts while the dialog is open (IDM's File Info
/// behavior), and by the panel's progress card (Pause / Resume /
/// Cancel): the entry already exists by the time the user decides.
fn handle_entry_control(message: &Value, kind: &str) -> Value {
    let entry = message.get("entry").and_then(Value::as_str).unwrap_or("");
    if !valid_entry_id(entry) {
        return json!({ "ok": false, "error": "invalid entry id" });
    }
    let id = new_entry_id();
    let obj = json!({ "id": id, "kind": kind, "entry": entry });
    match append_to_queue(obj, "") {
        Ok(()) => {
            ensure_app_running(true);
            wait_for_ack_and_result(&id, 15, false)
        }
        Err(e) => {
            eprintln!("ndm-host: cannot write queue: {e}");
            json!({ "ok": false, "error": "cannot write queue file" })
        }
    }
}

/// The largest limit the bridge will forward: 1 TB/sec is beyond
/// any real link, so anything above it is a malformed request,
/// not a preference.
const MAX_LIMIT_BPS: u64 = 1_000_000_000_000;

/// Set or clear one existing entry's own speed limit — the
/// progress card's Speed Limiter tab. Same queue plumbing as the
/// other entry controls, plus the `limit_bps` value carried
/// through verbatim (a JSON number in bytes/sec; null clears the
/// limit). The app runs the very command its own per-file
/// Speed Limiter uses, so the change applies to the live
/// engine/job immediately — this bridge adds no logic of its own.
fn handle_set_speed_limit(message: &Value) -> Value {
    let entry = message.get("entry").and_then(Value::as_str).unwrap_or("");
    if !valid_entry_id(entry) {
        return json!({ "ok": false, "error": "invalid entry id" });
    }
    let limit = match message.get("limit_bps") {
        None | Some(Value::Null) => Value::Null,
        Some(v) => match v.as_u64() {
            Some(n) if n <= MAX_LIMIT_BPS => json!(n),
            _ => return json!({ "ok": false, "error": "invalid limit_bps" }),
        },
    };
    let id = new_entry_id();
    let obj = json!({ "id": id, "kind": "set_speed_limit", "entry": entry, "limit_bps": limit });
    match append_to_queue(obj, "") {
        Ok(()) => {
            ensure_app_running(true);
            wait_for_ack_and_result(&id, 15, false)
        }
        Err(e) => {
            eprintln!("ndm-host: cannot write queue: {e}");
            json!({ "ok": false, "error": "cannot write queue file" })
        }
    }
}

/// Is an `ndm-app` process alive on this machine? The warm path
/// (m-3) uses it to stay purely opportunistic — never the reason
/// the app starts — and ensure_app_running uses it to avoid
/// spawning a twin.
#[cfg(unix)]
fn app_is_running() -> bool {
    std::fs::read_dir("/proc")
        .map(|rd| {
            rd.flatten().any(|e| {
                if !e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                let comm_ok = std::fs::read_to_string(e.path().join("comm"))
                    .map(|s| s.trim() == "ndm-app")
                    .unwrap_or(false);
                if !comm_ok {
                    return false;
                }
                // Ignore zombie processes
                let zombie = std::fs::read_to_string(e.path().join("stat"))
                    .ok()
                    .and_then(|s| s.rsplit_once(')').map(|(_, rest)| rest.trim_start().starts_with('Z')))
                    .unwrap_or(false);
                !zombie
            })
        })
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn app_is_running() -> bool {
    // no cheap process scan on this platform: report "running" so
    // warm keeps its previous reach — queued lines are only ever
    // drained by a live app, and the app's own single-instance
    // guard absorbs a redundant spawn
    true
}

/// Launch `ndm-app` detached if it is not already running.
/// When `background` is true, passes `--background` so `ndm-app` starts
/// hidden in the background without stealing focus from the user's browser.
#[cfg(unix)]
fn ensure_app_running(background: bool) {
    use std::os::unix::process::CommandExt;
    if app_is_running() {
        return;
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("ndm-app"));
            candidates.push(dir.join("../../bin/ndm-app"));
            candidates.push(dir.join("../../../src-tauri/target/release/ndm-app"));
        }
    }
    candidates.push(PathBuf::from("/usr/bin/ndm-app"));
    candidates.push(PathBuf::from("/usr/local/bin/ndm-app"));

    for cand in candidates {
        if !cand.is_file() {
            continue;
        }
        let mut cmd = std::process::Command::new(&cand);
        if background {
            cmd.arg("--background");
        }
        if cmd
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
    // Last fallback: PATH lookup
    let mut cmd = std::process::Command::new("ndm-app");
    if background {
        cmd.arg("--background");
    }
    let _ = cmd
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(windows)]
fn ensure_app_running(background: bool) {
    use std::os::windows::process::CommandExt;
    const DETACHED: u32 = 0x0000_0208; // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("ndm-app.exe");
            if cand.is_file() {
                let mut cmd = std::process::Command::new(&cand);
                if background {
                    cmd.arg("--background");
                }
                let _ = cmd
                    .creation_flags(DETACHED)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn ensure_app_running(_background: bool) {}

fn read_message(input: &mut impl Read) -> io::Result<Option<Result<Value, String>>> {
    let mut len_buf = [0u8; 4];
    match input.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {len} bytes"),
        ));
    }
    // F6: do NOT pre-allocate the declared length — a lying header
    // could force the full cap's allocation before a single body
    // byte arrives. Read in small chunks into a buffer that grows
    // only with bytes actually delivered, up to the declared
    // length (itself already capped at MAX_MESSAGE_BYTES above).
    let len = len as usize;
    let mut buf = Vec::with_capacity(len.min(8192));
    let mut chunk = [0u8; 8192];
    while buf.len() < len {
        let want = (len - buf.len()).min(chunk.len());
        match input.read(&mut chunk[..want]) {
            // EOF mid-frame is the old read_exact behaviour:
            // an UnexpectedEof error, never a silent short parse
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "failed to fill whole buffer",
                ))
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(Some(
        serde_json::from_slice(&buf).map_err(|e| format!("malformed message: {e}")),
    ))
}

fn write_reply(output: &mut impl Write, reply: &Value) -> io::Result<()> {
    let body = reply.to_string();
    // Chrome kills the native-messaging channel when a single
    // message exceeds 1 MiB, taking every other in-flight request
    // with it (finding #11): an oversized reply (a probe whose
    // thumbnail slipped past its own cap) is replaced by a small
    // error reply the extension can surface, so the channel lives.
    let body = if body.len() > MAX_MESSAGE_BYTES as usize {
        json!({ "ok": false, "error": "reply too large" }).to_string()
    } else {
        body
    };
    let len = (body.len() as u32).to_le_bytes();
    output.write_all(&len)?;
    output.write_all(body.as_bytes())?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_body(written: &[u8]) -> Value {
        let len = u32::from_le_bytes(written[0..4].try_into().unwrap()) as usize;
        assert_eq!(written.len(), 4 + len, "frame must be exactly one message");
        serde_json::from_slice(&written[4..]).unwrap()
    }

    #[test]
    fn captured_streams_object_is_validated_and_normalized() {
        // the browser-capture door's message: a well-formed send
        // becomes the queue object the app drains
        let msg = json!({
            "type": "captured_streams",
            "pageUrl": "https://www.youtube.com/watch?v=abcDEF123_-",
            "title": "A Video",
            "streams": [
                { "url": "https://rr1.googlevideo.com/videoplayback?expire=9999999999&itag=136",
                  "mimeType": "video/mp4", "qualityLabel": "720p",
                  "hasAudio": false, "hasVideo": true },
                { "url": "https://rr1.googlevideo.com/videoplayback?expire=9999999999&itag=140",
                  "mimeType": "audio/mp4",
                  "hasAudio": true, "hasVideo": false }
            ]
        });
        let obj = captured_queue_object(&msg).expect("a valid capture is accepted");
        assert_eq!(obj["kind"], json!("captured_streams"));
        assert_eq!(obj["url"], json!("https://www.youtube.com/watch?v=abcDEF123_-"));
        let streams = obj["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0]["qualityLabel"], json!("720p"));
        assert_eq!(streams[1]["qualityLabel"], Value::Null);
        assert_eq!(streams[1]["hasAudio"], json!(true));

        // refusals: no page, no streams, a flood of streams, a
        // non-http stream, a title with a line break
        let bad_page = json!({ "pageUrl": "about:blank", "streams": [ { "url": "https://x/y.mp4" } ] });
        assert!(captured_queue_object(&bad_page).is_err());
        let no_streams = json!({ "pageUrl": "https://x/watch", "streams": [] });
        assert!(captured_queue_object(&no_streams).is_err());
        let flood = json!({
            "pageUrl": "https://x/watch",
            "streams": (0..33).map(|i| json!({ "url": format!("https://x/{i}.mp4") })).collect::<Vec<_>>()
        });
        assert!(captured_queue_object(&flood).is_err());
        let bad_stream = json!({
            "pageUrl": "https://x/watch",
            "streams": [ { "url": "file:///etc/passwd" } ]
        });
        assert!(captured_queue_object(&bad_stream).is_err());
        let bad_title = json!({
            "pageUrl": "https://x/watch", "title": "a\nb",
            "streams": [ { "url": "https://x/y.mp4" } ]
        });
        assert!(captured_queue_object(&bad_title).is_err());

        // dispatch reaches the handler: an invalid message is a
        // clean {ok:false}, never a queue write or a panic
        let reply = handle_at(&bad_stream, None, 0);
        assert_eq!(reply["ok"], json!(false));
    }

    #[test]
    fn oversized_reply_is_replaced_by_a_small_error() {
        // finding #11 backstop: a reply over Chrome's 1 MiB channel
        // cap must go out as a small {ok:false} instead of killing
        // the channel for every other request
        let big = json!({ "ok": true, "info": { "thumbnail": "x".repeat(2 * 1024 * 1024) } });
        let mut out: Vec<u8> = Vec::new();
        write_reply(&mut out, &big).unwrap();
        assert!(out.len() < 1024, "replacement reply must be tiny: {} bytes", out.len());
        let v = frame_body(&out);
        assert_eq!(v["ok"], json!(false));
        // and a normal reply passes through untouched
        let small = json!({ "ok": true, "pong": true });
        let mut out: Vec<u8> = Vec::new();
        write_reply(&mut out, &small).unwrap();
        assert_eq!(frame_body(&out), small);
    }

    #[test]
    fn cookie_conversion_honours_secure_and_expiry() {
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let jar = [
            format!("example.com\tTRUE\t/\tFALSE\t{future}\tplain\tv1"),
            format!("example.com\tTRUE\t/\tTRUE\t{future}\tsecure\tv2"),
            "example.com\tTRUE\t/\tFALSE\t1000000000\texpired\tv3".to_string(),
            format!("example.com\tTRUE\t/\tFALSE\t0\tsession\tv4"),
        ]
        .join("\n");
        let http = cookie_header_from_netscape(&jar, "http://example.com/f.bin")
            .expect("some cookies must survive for http");
        assert!(http.contains("plain=v1"), "http jar: {http}");
        assert!(http.contains("session=v4"), "session cookies are kept: {http}");
        assert!(!http.contains("secure=v2"), "Secure must never go to http://: {http}");
        assert!(!http.contains("expired=v3"), "expired must be dropped: {http}");
        let https = cookie_header_from_netscape(&jar, "https://example.com/f.bin")
            .expect("some cookies must survive for https");
        assert!(https.contains("secure=v2"), "Secure is fine on https: {https}");
        assert!(!https.contains("expired=v3"), "expired dropped on https too: {https}");
    }

    #[test]
    fn cookie_conversion_honours_domain_flag_and_path_boundary() {
        // SW2-3 regression lock: the Netscape flag field decides
        // subdomain reach, and the cookie path must match at a '/'
        // boundary (RFC 6265), not by raw prefix.
        let jar = [
            "example.com\tFALSE\t/\tFALSE\t0\thostonly\th1",
            "example.com\tTRUE\t/\tFALSE\t0\tdomainwide\td1",
            "example.com\tTRUE\t/account\tFALSE\t0\tacct\ta1",
        ]
        .join("\n");
        let exact = cookie_header_from_netscape(&jar, "https://example.com/account")
            .expect("cookies must survive for the exact host+path");
        assert!(exact.contains("hostonly=h1"), "exact host: {exact}");
        assert!(exact.contains("domainwide=d1"), "exact host: {exact}");
        assert!(exact.contains("acct=a1"), "exact path: {exact}");
        let sub = cookie_header_from_netscape(&jar, "https://sub.example.com/")
            .expect("the domain cookie must survive for a subdomain");
        assert!(
            !sub.contains("hostonly=h1"),
            "a host-only (flag FALSE) cookie must never leak to a subdomain: {sub}"
        );
        assert!(sub.contains("domainwide=d1"), "flag TRUE covers subdomains: {sub}");
        let evil = cookie_header_from_netscape(&jar, "https://example.com/accountevil")
            .expect("the root cookies must survive");
        assert!(
            !evil.contains("acct=a1"),
            "/account must not match /accountevil (no '/' boundary): {evil}"
        );
        let nested = cookie_header_from_netscape(&jar, "https://example.com/account/x")
            .expect("cookies must survive for a nested path");
        assert!(nested.contains("acct=a1"), "'/' boundary matches: {nested}");
    }

    #[test]
    fn dead_owner_lock_is_stale_live_owner_lock_is_not() {
        let dir = std::env::temp_dir().join(format!("ndm-host-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("incoming.lock");
        // a lock recording a pid that cannot be alive: debris,
        // reaped immediately (well under the old 30 s age rule)
        std::fs::write(&lock, "4194304").unwrap();
        assert!(lock_is_stale(&lock), "dead owner's lock must be stale at once");
        // a lock recording OUR live pid: a live hand-off, waited on
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
        assert!(!lock_is_stale(&lock), "live owner's lock must not be stale");
        // a fresh pid-less lock (legacy, or the instant before the
        // owner writes its pid): still inside the grace window
        std::fs::write(&lock, "").unwrap();
        assert!(!lock_is_stale(&lock), "fresh legacy lock must get its grace window");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn zombie_owner_lock_is_reaped_promptly() {
        // W1 F12: a child that exits and is never waited on stays a
        // zombie — /proc/<pid> exists, but it owns nothing. The old
        // existence-only probe called it alive, so every hand-off
        // burned its full spin and appended unlocked.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let stat_path = format!("/proc/{pid}/stat");
        let mut is_zombie = false;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&stat_path) {
                if s.rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().starts_with('Z'))
                    .unwrap_or(false)
                {
                    is_zombie = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(is_zombie, "the test child must reach zombie state");
        assert!(!process_alive(pid), "a zombie must not count as alive");
        let dir = std::env::temp_dir().join(format!("ndm-host-zombie-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("incoming.lock");
        std::fs::write(&lock, pid.to_string()).unwrap();
        assert!(lock_is_stale(&lock), "a zombie owner's lock is stale at once");
        assert!(reap_stale_lock(&lock), "a zombie owner's lock must be reaped");
        assert!(!lock.exists(), "the reaped lock must be gone");
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_owner_lock_survives_a_stale_reap() {
        // W1 F12 TOCTOU: a lock judged stale is fingerprinted; if a
        // fresh owner re-created the lock in the race window, the
        // removal aimed at the old file must not touch the new one.
        let dir = std::env::temp_dir().join(format!("ndm-host-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("incoming.lock");
        std::fs::write(&lock, "4194304").unwrap();
        let judged = lock_fingerprint(&lock).expect("lock must fingerprint");
        assert!(lock_is_stale(&lock));
        // the fresh owner arrives between the judgement and the removal
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
        assert!(
            !remove_lock_if_matches(&lock, &judged),
            "a fresh owner's lock must never be removed on a stale fingerprint"
        );
        assert!(lock.exists());
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap().trim(),
            std::process::id().to_string()
        );
        // and the reap path itself leaves a live owner's lock alone
        assert!(!reap_stale_lock(&lock));
        assert!(lock.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn stamp_test_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ndm-host-stamp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stamp = dir.join("ext_seen.json");
        (dir, stamp)
    }

    #[test]
    fn ping_with_ext_version_writes_stamp() {
        let (dir, stamp) = stamp_test_path("with");
        let reply = handle_at(
            &json!({ "type": "ping", "ext_version": "2.0.8" }),
            Some(&stamp),
            1_000,
        );
        assert_eq!(reply, json!({ "ok": true, "pong": true }), "ping reply is unchanged");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&stamp).unwrap()).unwrap();
        assert_eq!(v["ext_version"], json!("2.0.8"));
        assert_eq!(v["at"], json!(1_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ping_without_ext_version_writes_null_stamp() {
        // a pre-handshake extension's ping has no field: the stamp
        // must record null — positive evidence an old extension lives
        let (dir, stamp) = stamp_test_path("without");
        let reply = handle_at(&json!({ "type": "ping" }), Some(&stamp), 2_000);
        assert_eq!(reply, json!({ "ok": true, "pong": true }));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&stamp).unwrap()).unwrap();
        assert!(v["ext_version"].is_null(), "absent field stamps null: {v}");
        assert_eq!(v["at"], json!(2_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_identical_ping_does_not_rewrite_stamp() {
        let (dir, stamp) = stamp_test_path("changed");
        handle_at(&json!({ "type": "ping", "ext_version": "2.0.8" }), Some(&stamp), 1_000);
        handle_at(&json!({ "type": "ping", "ext_version": "2.0.8" }), Some(&stamp), 2_000);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&stamp).unwrap()).unwrap();
        assert_eq!(v["at"], json!(1_000), "an identical ping must not rewrite the stamp");
        // a changed version rewrites at once
        handle_at(&json!({ "type": "ping", "ext_version": "2.0.9" }), Some(&stamp), 2_000);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&stamp).unwrap()).unwrap();
        assert_eq!(v["ext_version"], json!("2.0.9"));
        assert_eq!(v["at"], json!(2_000));
        // and the same version refreshes once the stamp is >24 h old
        let later = 2_000 + 25 * 60 * 60;
        handle_at(&json!({ "type": "ping", "ext_version": "2.0.9" }), Some(&stamp), later);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&stamp).unwrap()).unwrap();
        assert_eq!(v["at"], json!(later), "a stamp older than 24 h is refreshed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_ping_message_stamps_only_when_it_carries_ext_version() {
        let (dir, stamp) = stamp_test_path("nonping");
        let _ = handle_at(
            &json!({ "type": "progress", "id": "abc123" }),
            Some(&stamp),
            1_000,
        );
        assert!(!stamp.exists(), "a message without ext_version must not create a stamp");
        handle_at(&json!({ "type": "ping", "ext_version": "2.0.8" }), Some(&stamp), 1_000);
        let before = std::fs::read_to_string(&stamp).unwrap();
        let _ = handle_at(
            &json!({ "type": "progress", "id": "abc123" }),
            Some(&stamp),
            5_000,
        );
        assert_eq!(
            std::fs::read_to_string(&stamp).unwrap(),
            before,
            "a message without ext_version must not modify an existing stamp"
        );
        // E3: any message carrying the field stamps it, so a
        // download-only session refreshes the stamp too
        let _ = handle_at(
            &json!({ "type": "progress", "id": "abc123", "ext_version": "9.9.9" }),
            Some(&stamp),
            5_000,
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&stamp).unwrap()).unwrap();
        assert_eq!(v["ext_version"], json!("9.9.9"));
        assert_eq!(v["at"], json!(5_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reader that never returns more than 3 bytes per call —
    /// the chunked frame reader (F6) must assemble a message from
    /// dribbles exactly as the old one-shot read_exact did.
    struct Dribble<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl Read for Dribble<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let n = (self.data.len() - self.pos).min(out.len()).min(3);
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn framed_message_arriving_in_dribbles_still_parses() {
        let body = br#"{"type":"ping"}"#;
        let mut framed = Vec::new();
        framed.extend_from_slice(&(body.len() as u32).to_le_bytes());
        framed.extend_from_slice(body);
        let mut reader = Dribble { data: &framed, pos: 0 };
        let v = read_message(&mut reader).unwrap().unwrap().unwrap();
        assert_eq!(v["type"], json!("ping"));
        // a header that promises more than ever arrives is
        // UnexpectedEof, exactly as read_exact reported it
        let mut short = Vec::new();
        short.extend_from_slice(&((body.len() + 10) as u32).to_le_bytes());
        short.extend_from_slice(body);
        let mut reader = Dribble { data: &short, pos: 0 };
        let err = read_message(&mut reader).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_inbound_length_is_invalid_data() {
        // the COSMETIC finding's mechanism: main() exits nonzero
        // exactly for InvalidData framing errors — make sure a
        // >1 MiB length prefix produces that kind, not a read error
        let mut framed = Vec::new();
        framed.extend_from_slice(&2_000_000u32.to_le_bytes());
        framed.extend_from_slice(b"{}");
        let err = read_message(&mut &framed[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    loop {
        let message = match read_message(&mut input) {
            Ok(Some(value)) => value,
            Ok(None) => break,
            Err(e) => {
                eprintln!("ndm-host: framing error, exiting: {e}");
                // an invalid length prefix (e.g. >1 MiB inbound) is
                // a protocol violation, not a clean close: exit
                // nonzero so the failure is visible instead of a
                // silent code-0 channel death
                if e.kind() == io::ErrorKind::InvalidData {
                    std::process::exit(2);
                }
                break;
            }
        };
        let reply = match message {
            Ok(value) => handle(&value),
            Err(detail) => json!({ "ok": false, "error": detail }),
        };
        if let Err(e) = write_reply(&mut output, &reply) {
            eprintln!("ndm-host: cannot write reply, exiting: {e}");
            break;
        }
    }
}
