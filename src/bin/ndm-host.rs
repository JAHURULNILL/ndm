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
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(content)
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
    // is generous (~15 s) because the unlocked fallback can corrupt
    // BOTH lines, while a late append only delays this one.
    let lock_path = path.with_extension("lock");
    let mut _guard: Option<std::fs::File> = None;
    for _ in 0..300 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(f) => {
                _guard = Some(f);
                break;
            }
            Err(_) => {
                // a lock older than 30 s cannot belong to a live
                // hand-off (the append it guards takes milliseconds):
                // its owner died mid-handover, and without recovery
                // EVERY later hand-off would first burn the full spin
                // and then append unlocked, forever
                if lock_is_stale(&lock_path) {
                    let _ = std::fs::remove_file(&lock_path);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    if _guard.is_none() && lock_is_stale(&lock_path) {
        // one last recovery attempt after the spin before falling
        // back to the unlocked append
        let _ = std::fs::remove_file(&lock_path);
        if let Ok(f) = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            _guard = Some(f);
        }
    }
    let result = (|| {
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(line.as_bytes())
    })();
    if _guard.is_some() {
        let _ = std::fs::remove_file(&lock_path);
    }
    result
}

/// A queue lock older than 30 s is debris from a host process that
/// was killed between creating it and removing it.
fn lock_is_stale(lock_path: &std::path::Path) -> bool {
    std::fs::metadata(lock_path)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|d| d.as_secs() > 30).unwrap_or(false))
        .unwrap_or(false)
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

/// Handles one parsed message; returns the reply object to frame back.
fn handle(message: &Value) -> Value {
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
            let domain_match =
                !dom.is_empty() && (host == dom || host.ends_with(&format!(".{dom}")));
            if !domain_match || !path.starts_with(f[2]) {
                continue;
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
        return json!({
            "ok": false,
            "error": "Video engine is being set up by NDM — please wait a few seconds and click again."
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
    let mut buf = vec![0u8; len as usize];
    input.read_exact(&mut buf)?;
    Ok(Some(
        serde_json::from_slice(&buf).map_err(|e| format!("malformed message: {e}")),
    ))
}

fn write_reply(output: &mut impl Write, reply: &Value) -> io::Result<()> {
    let body = reply.to_string();
    let len = (body.len() as u32).to_le_bytes();
    output.write_all(&len)?;
    output.write_all(body.as_bytes())?;
    output.flush()
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
