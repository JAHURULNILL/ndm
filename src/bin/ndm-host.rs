// ndm-host — native messaging bridge between the NDM browser extension and
// the NDM app. The browser launches this binary and speaks the native
// messaging protocol: 4-byte little-endian length prefix + JSON, both ways.
//
// Download requests are appended as one-line JSON records to the app's
// incoming queue file; the app polls that file and picks the downloads up.
// stdout carries framed replies ONLY — all diagnostics go to stderr.

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::PathBuf;

const HOST_DIR_NAME: &str = "com.ndm.downloadmanager";
const QUEUE_FILE_NAME: &str = "incoming.jsonl";
const MAX_MESSAGE_BYTES: u32 = 1 << 20; // native messaging messages are small

fn queue_path() -> io::Result<PathBuf> {
    let base = data_home()?;
    Ok(base.join(HOST_DIR_NAME).join(QUEUE_FILE_NAME))
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

fn append_to_queue(kind: &str, url: &str, title: &str, page_url: &str, cookies: &str) -> io::Result<()> {
    let path = queue_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // One buffer, one write: two host processes (Chrome + Firefox) appending
    // at the same moment must never interleave into a corrupt queue line.
    let mut obj = json!({ "kind": kind, "url": url, "title": title, "pageUrl": page_url });
    // the browser's captured session for this page (video downloads);
    // the app persists it under a token and feeds it to yt-dlp
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
        Some("download") => handle_download(message),
        Some(other) => json!({ "ok": false, "error": format!("unknown type: {other}") }),
        None => json!({ "ok": false, "error": "missing type" }),
    }
}

fn handle_download(message: &Value) -> Value {
    let kind = message.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind != "video" && kind != "file" {
        return json!({ "ok": false, "error": "kind must be \"video\" or \"file\"" });
    }
    let url = message.get("url").and_then(Value::as_str).unwrap_or("");
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return json!({ "ok": false, "error": "url must be a non-empty http(s) URL" });
    }
    let title = message.get("title").and_then(Value::as_str).unwrap_or("");
    let page_url = message.get("pageUrl").and_then(Value::as_str).unwrap_or("");
    // these strings land in a JSONL queue and, for files, in an HTTP
    // Referer header later: line breaks would corrupt the queue line
    // and surface as an opaque engine error far from the cause
    if title.contains(['\r', '\n']) || page_url.contains(['\r', '\n']) {
        return json!({ "ok": false, "error": "title/pageUrl must not contain line breaks" });
    }
    let title: String = title.chars().take(300).collect();
    let page_url: String = page_url.chars().take(2048).collect();
    let title = title.as_str();
    let page_url = page_url.as_str();
    let raw_cookies = message.get("cookies").and_then(Value::as_str).unwrap_or("");
    let cookies = if raw_cookies.len() <= 600_000 { raw_cookies } else { "" };
    match append_to_queue(kind, url, title, page_url, cookies) {
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

/// IDM-style: a click in the browser must surface the app even when
/// it is closed — a queue file alone sits unseen until the user opens
/// NDM by hand. Launch the app detached; it drains the queue on
/// startup and opens the video dialog itself.
#[cfg(unix)]
fn ensure_app_running() {
    let running = std::fs::read_dir("/proc")
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit())
                    && std::fs::read_to_string(e.path().join("comm"))
                        .map(|s| s.trim() == "ndm-app")
                        .unwrap_or(false)
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
        }
    }
    candidates.push(PathBuf::from("ndm-app")); // PATH lookup (/usr/bin)
    for cand in candidates {
        let spawned = std::process::Command::new("setsid")
            .arg(&cand)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return;
        }
    }
}

#[cfg(not(unix))]
fn ensure_app_running() {}

/// Reads one framed message. Ok(None) means a clean EOF — time to exit.
/// A malformed JSON payload comes back as Err(text) describing it; the
/// caller turns that into an error reply and keeps the loop going.
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
