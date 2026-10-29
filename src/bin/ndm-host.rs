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
    // a queue line. Take a small lock file first (bounded spin — after
    // ~2 s, fall back to the plain append rather than losing the job).
    let lock_path = path.with_extension("lock");
    let mut _guard: Option<std::fs::File> = None;
    for _ in 0..40 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(f) => {
                _guard = Some(f);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
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

/// Handles one parsed message; returns the reply object to frame back.
fn handle(message: &Value) -> Value {
    match message.get("type").and_then(Value::as_str) {
        Some("ping") => json!({ "ok": true, "pong": true }),
        Some("probe") => handle_probe(message),
        Some("probe_file") => handle_probe_file(message),
        Some("show_app") => handle_show_app(),
        Some("download") => handle_download(message),
        Some("pause_entry") => handle_entry_control(message, "pause_entry"),
        Some("cancel_entry") => handle_entry_control(message, "cancel_entry"),
        Some(other) => json!({ "ok": false, "error": format!("unknown type: {other}") }),
        None => json!({ "ok": false, "error": "missing type" }),
    }
}

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
        "exe" | "msi" | "deb" | "rpm" | "dmg" | "pkg" | "appimage" | "apk" | "bin" | "iso"
        | "img" | "flatpakref" => "Programs",
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" | "ts" => "Video",
        "mp3" | "m4a" | "flac" | "wav" | "ogg" | "opus" | "aac" | "wma" => "Music",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "csv"
        | "epub" | "mobi" => "Documents",
        _ => "General",
    }
}

fn default_file_name(url: &str) -> String {
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
        cookie: cookie_header_from_netscape(cookies_str),
        referer,
        authorization: None,
    };
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

fn wait_for_ack_and_result(id: &str, timeout_secs: u64) -> Value {
    let Ok(base) = app_data_dir() else {
        return json!({ "ok": false, "error": "cannot resolve app data directory" });
    };
    let ack_file = base.join("acks").join(format!("{id}.ack"));
    let res_file = base.join("results").join(format!("{id}.json"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if ack_file.is_file() {
            let _ = std::fs::remove_file(&ack_file);
            if let Ok(raw) = std::fs::read_to_string(&res_file) {
                let _ = std::fs::remove_file(&res_file);
                if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                    return v;
                }
            }
            return json!({ "ok": true, "id": id });
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    json!({
        "ok": false,
        "error": "NDM desktop app did not acknowledge the download in time"
    })
}

fn handle_download(message: &Value) -> Value {
    let kind = message.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind != "video" && kind != "file" && kind != "video_direct" && kind != "file_direct" {
        return json!({ "ok": false, "error": "kind must be \"video\", \"video_direct\", \"file\", or \"file_direct\"" });
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
            wait_for_ack_and_result(&id, 15)
        }
        Err(e) => {
            eprintln!("ndm-host: cannot write queue: {e}");
            json!({ "ok": false, "error": "cannot write queue file" })
        }
    }
}

/// Pause or cancel one existing entry by id — used by the browser
/// dialog's Download Later / Cancel buttons now that the download
/// starts while the dialog is open (IDM's File Info behavior): the
/// entry already exists by the time the user decides.
fn handle_entry_control(message: &Value, kind: &str) -> Value {
    let entry = message.get("entry").and_then(Value::as_str).unwrap_or("");
    if entry.is_empty()
        || entry.len() > 64
        || !entry
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return json!({ "ok": false, "error": "invalid entry id" });
    }
    let id = new_entry_id();
    let obj = json!({ "id": id, "kind": kind, "entry": entry });
    match append_to_queue(obj, "") {
        Ok(()) => {
            ensure_app_running(true);
            wait_for_ack_and_result(&id, 15)
        }
        Err(e) => {
            eprintln!("ndm-host: cannot write queue: {e}");
            json!({ "ok": false, "error": "cannot write queue file" })
        }
    }
}

/// Launch `ndm-app` detached if it is not already running.
/// When `background` is true, passes `--background` so `ndm-app` starts
/// hidden in the background without stealing focus from the user's browser.
#[cfg(unix)]
fn ensure_app_running(background: bool) {
    use std::os::unix::process::CommandExt;
    let running = std::fs::read_dir("/proc")
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
        .unwrap_or(false);
    if running {
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
