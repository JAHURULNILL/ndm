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
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

/// Handles one parsed message; returns the reply object to frame back.
fn handle(message: &Value) -> Value {
    match message.get("type").and_then(Value::as_str) {
        Some("ping") => json!({ "ok": true, "pong": true }),
        Some("probe") => handle_probe(message),
        Some("download") => handle_download(message),
        Some(other) => json!({ "ok": false, "error": format!("unknown type: {other}") }),
        None => json!({ "ok": false, "error": "missing type" }),
    }
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
        ensure_app_running();
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

fn handle_download(message: &Value) -> Value {
    let kind = message.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind != "video" && kind != "file" && kind != "video_direct" {
        return json!({ "ok": false, "error": "kind must be \"video\", \"video_direct\", or \"file\"" });
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

    let mut obj = json!({
        "kind": kind,
        "url": url,
        "title": title,
        "pageUrl": page_url,
    });
    if kind == "video_direct" {
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
        obj["selector"] = json!(selector);
        obj["dir"] = json!(dir);
        obj["audio"] = json!(audio);
        obj["later"] = json!(later);
        obj["total"] = json!(total);
    }

    match append_to_queue(obj, cookies) {
        Ok(()) => {
            ensure_app_running();
            json!({ "ok": true })
        }
        Err(e) => {
            eprintln!("ndm-host: cannot write queue: {e}");
            json!({ "ok": false, "error": "cannot write queue file" })
        }
    }
}

/// Launch `ndm-app` detached if it is not already running.
/// Verifies candidate binaries actually exist on disk before spawning and
/// detaches via `process_group(0)` so `spawn()` only succeeds when `ndm-app`
/// itself is executed (fixing the old `setsid` false-positive bug).
#[cfg(unix)]
fn ensure_app_running() {
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
        if std::process::Command::new(&cand)
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
    let _ = std::process::Command::new("ndm-app")
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(windows)]
fn ensure_app_running() {
    use std::os::windows::process::CommandExt;
    const DETACHED: u32 = 0x0000_0208; // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("ndm-app.exe");
            if cand.is_file() {
                let _ = std::process::Command::new(&cand)
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
fn ensure_app_running() {}

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
