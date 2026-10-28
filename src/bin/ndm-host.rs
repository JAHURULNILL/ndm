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

fn queue_dir() -> io::Result<PathBuf> {
    let base = data_home()?;
    Ok(base.join(HOST_DIR_NAME))
}

fn queue_path() -> io::Result<PathBuf> {
    Ok(queue_dir()?.join(QUEUE_FILE_NAME))
}

/// Unique id for one queue entry: the app echoes it back as an ack
/// file once it has actually surfaced the entry, which is what lets
/// the extension say "Sent" honestly (see wait_for_ack).
fn new_entry_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", std::process::id())
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

fn append_to_queue(
    kind: &str,
    url: &str,
    title: &str,
    page_url: &str,
    cookies: &str,
    extra: &[(&str, Value)],
) -> io::Result<String> {
    let path = queue_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let id = new_entry_id();
    // One buffer, one write: two host processes (Chrome + Firefox) appending
    // at the same moment must never interleave into a corrupt queue line.
    let mut obj = json!({ "id": id, "kind": kind, "url": url, "title": title, "pageUrl": page_url });
    for (k, v) in extra {
        obj[*k] = v.clone();
    }
    // the browser's captured session for this page (video downloads);
    // the app persists it under a token and feeds it to yt-dlp
    if !cookies.is_empty() {
        obj["cookies"] = json!(cookies);
    }
    let line = obj.to_string() + "\n";
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    Ok(id)
}

/// Wait until the app confirms it has surfaced this entry (it writes
/// `acks/<id>.ack` next to the queue file as it processes the line).
/// The browser pill turns green on this signal alone: "the link was
/// written to a queue file" is not "the app showed the download
/// dialog", and claiming otherwise left users staring at a green
/// "Sent ✓" with no dialog anywhere. 10 s covers a cold app launch;
/// the extension's own reply timeout is 25 s.
fn wait_for_ack(id: &str, budget_secs: u64) -> bool {
    let Ok(dir) = queue_dir() else { return false };
    let ack = dir.join("acks").join(format!("{id}.ack"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(budget_secs);
    loop {
        if ack.exists() {
            let _ = std::fs::remove_file(&ack);
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// The result file the app wrote for one panel request (it lands
/// BEFORE the ack, so by the time wait_for_ack returns it is
/// complete). Read once and removed — results are single-use.
fn read_result(id: &str) -> Option<Value> {
    let dir = queue_dir().ok()?;
    let path = dir.join("results").join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(&text).ok()
}

/// Handles one parsed message; returns the reply object to frame back.
fn handle(message: &Value) -> Value {
    match message.get("type").and_then(Value::as_str) {
        Some("ping") => json!({ "ok": true, "pong": true }),
        Some("download") => match message.get("kind").and_then(Value::as_str) {
            // the in-page panel's two requests: the reply carries
            // the app's own result payload back into the page
            Some("probe") => handle_panel(message, true),
            Some("start") => handle_panel(message, false),
            _ => handle_download(message),
        },
        Some(other) => json!({ "ok": false, "error": format!("unknown type: {other}") }),
        None => json!({ "ok": false, "error": "missing type" }),
    }
}

/// A panel request round trip: queue the entry, make sure the app
/// is up, then wait for the app to answer (ack + result file) and
/// relay the result. The probe budget is generous — a cold app
/// launch plus a real yt-dlp extraction takes seconds; the start
/// budget only covers creating the entry.
fn handle_panel(message: &Value, is_probe: bool) -> Value {
    let kind = if is_probe { "probe" } else { "start" };
    let url = message.get("url").and_then(Value::as_str).unwrap_or("");
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return json!({ "ok": false, "error": "url must be a non-empty http(s) URL" });
    }
    let clean = |s: &str, n: usize| -> String {
        s.replace(['\r', '\n'], " ").chars().take(n).collect()
    };
    let title = clean(message.get("title").and_then(Value::as_str).unwrap_or(""), 300);
    let page_url = clean(message.get("pageUrl").and_then(Value::as_str).unwrap_or(""), 2048);
    let raw_cookies = message.get("cookies").and_then(Value::as_str).unwrap_or("");
    let cookies = if raw_cookies.len() <= 600_000 { raw_cookies } else { "" };
    let mut extra: Vec<(&str, Value)> = Vec::new();
    if !is_probe {
        // sniffed direct starts carry a mode + the captured stream
        // address instead of a page selector
        let mode = message.get("mode").and_then(Value::as_str).unwrap_or("");
        if mode == "manifest" || mode == "file" {
            let direct = message.get("directUrl").and_then(Value::as_str).unwrap_or("");
            if !(direct.starts_with("http://") || direct.starts_with("https://")) || direct.len() > 2048 {
                return json!({ "ok": false, "error": "a valid stream address is required" });
            }
            extra.push(("mode", json!(mode)));
            extra.push(("directUrl", json!(direct)));
            extra.push(("height", json!(message.get("height").and_then(Value::as_u64).unwrap_or(0).min(10_000))));
            extra.push(("hasAudio", json!(message.get("hasAudio").and_then(Value::as_bool).unwrap_or(false))));
            // separate HLS audio rendition for the app to pair & mux
            let audio_url = message.get("audioUrl").and_then(Value::as_str).unwrap_or("");
            if !audio_url.is_empty() {
                if !(audio_url.starts_with("http://") || audio_url.starts_with("https://")) || audio_url.len() > 2048 {
                    return json!({ "ok": false, "error": "a valid audio address is required" });
                }
                extra.push(("audioUrl", json!(audio_url)));
            }
            let fname = clean(message.get("fileName").and_then(Value::as_str).unwrap_or(""), 300);
            extra.push(("fileName", json!(fname)));
        } else {
            let selector = message.get("selector").and_then(Value::as_str).unwrap_or("");
            if selector.is_empty() || selector.len() > 2000 {
                return json!({ "ok": false, "error": "a quality selector is required" });
            }
            extra.push(("selector", json!(selector)));
            extra.push(("audio", json!(message.get("audio").and_then(Value::as_bool).unwrap_or(false))));
            extra.push(("hasAudio", json!(message.get("hasAudio").and_then(Value::as_bool).unwrap_or(false))));
        }
    }
    match append_to_queue(kind, url, &title, &page_url, cookies, &extra) {
        Ok(id) => {
            ensure_app_running();
            let budget = if is_probe { 40 } else { 15 };
            if !wait_for_ack(&id, budget) {
                return json!({ "ok": false, "error": "the NDM app did not respond" });
            }
            match read_result(&id) {
                Some(result) => json!({ "ok": true, "result": result }),
                None => json!({ "ok": false, "error": "the NDM app sent no result" }),
            }
        }
        Err(e) => {
            eprintln!("ndm-host: cannot write queue: {e}");
            json!({ "ok": false, "error": "cannot write queue file" })
        }
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
    match append_to_queue(kind, url, title, page_url, cookies, &[]) {
        Ok(id) => {
            ensure_app_running();
            if kind == "video" {
                // `picked` is the app's own confirmation that the
                // dialog surfaced — the pill stays amber without it.
                // Files keep the instant reply: the browser's
                // interception verdict waits on it, and a caught file
                // makes no on-screen promise the pill must police.
                let picked = wait_for_ack(&id, 10);
                json!({ "ok": true, "picked": picked })
            } else {
                json!({ "ok": true })
            }
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
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            candidates.push(dir.join("ndm-app"));
        }
    }
    for cand in candidates {
        // setsid itself spawns fine even when the program it should
        // run does not exist — the failure happens inside its child
        // and spawn() still reports success. Handing it the first
        // candidate blindly therefore "launched" nothing whenever
        // the sibling path was absent (the .deb layout: the host
        // sits in /usr/lib/ndm, the app in /usr/bin) and the app
        // silently never opened. Only existing binaries qualify.
        if !cand.is_file() {
            continue;
        }
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
