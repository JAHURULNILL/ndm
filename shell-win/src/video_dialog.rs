//! Video picker (ui/app.js vmodal / openVideoModal /
//! fetchVideoInfo / submitVideo) — one modeless window: URL +
//! Fetch Info, the probe's format rows in a ListView (label,
//! format badge, size with the "~" estimate mark, note), the
//! preselect rule from Options' default quality, and Start /
//! Download all / Download Later through core `add_video`.
//!
//! Core owns the warm-probe / double-resolve semantics
//! (probe_video seeds the resolution cache; add_video warms the
//! submitted selector) — the shell just calls them in app.js's
//! order and never resolves twice itself. A duplicate answer
//! gets app.js's confirm + force retry. Closing without starting
//! discards the captured browser session (discard_cookies), like
//! closeVideoModal. The component gate mirrors gateComponents:
//! while the engine is still arriving, the dialog reads as an
//! ordinary slow probe and re-probes when core reports done.
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell.

use crate::winui::dialog as dlg;
use crate::winui::listview::{ColumnSpec, ListView};
use crate::winui::{pcw, set_window_text, wide, window_text};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::Controls::NMHDR;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW, PostMessageW,
    RegisterClassW, SetForegroundWindow, ShowWindow, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, IDC_ARROW, SW_RESTORE, SW_SHOW, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_DESTROY, WM_NOTIFY, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

const ID_URL: i32 = 8201;
const ID_FETCH: i32 = 8202;
const ID_DIR: i32 = 8203;
const ID_QUEUE: i32 = 8204;
const ID_DESC: i32 = 8205;
const ID_STATUS: i32 = 8206;
const ID_TITLE: i32 = 8207;
const ID_META: i32 = 8208;
const ID_OPTS: i32 = 8209;
const ID_START: i32 = 8210;
const ID_ALL: i32 = 8211;
const ID_LATER: i32 = 8212;
const ID_CANCEL: i32 = 8213;

const WM_VD_PROBE: u32 = WM_APP + 21; // WPARAM 1 = ok (Box<Value>), 0 = err (Box<String>)
const WM_VD_ADDED: u32 = WM_APP + 22; // LPARAM = Box<String>: "" | "duplicate" | error text
const EN_CHANGE_VAL: u32 = 0x0300;
const LVN_ITEMCHANGED_CODE: i32 = -101;

const WIN_W: i32 = 640;
const WIN_H: i32 = 540;

struct VidDlg {
    url: isize,
    dir: isize,
    queue: isize,
    desc: isize,
    status: isize,
    title: isize,
    meta: isize,
    opts: isize,
    btn_start: isize,
    btn_all: isize,
    btn_later: isize,
    info: Option<serde_json::Value>,
    probed_url: String,
    presented_url: String,
    presented_cookies: String,
    token_consumed: bool,
    dir_touched: bool,
    setup_wanted: bool,
    submitting: bool,
    pending_later: bool,
    last_audio: Option<bool>,
    queues: Vec<String>,
}

static DLG: Mutex<Option<VidDlg>> = Mutex::new(None);
static WIN_RAW: AtomicIsize = AtomicIsize::new(0);

fn h(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}
fn raw(hwnd: HWND) -> isize {
    hwnd.0 as isize
}
fn js<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

struct Incoming {
    url: String,
    cookies: String,
}

/// Show the picker (fresh, or raise the open one). With an
/// incoming browser link, present it and start the probe, as
/// app.js's queueVideoLink → openVideoModal does.
pub fn show(parent: HWND, incoming: Option<(String, String)>) {
    if WIN_RAW.load(Ordering::SeqCst) != 0 {
        let hwnd = h(WIN_RAW.load(Ordering::SeqCst));
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        if let Some((url, cookies)) = incoming {
            present(hwnd, url, cookies);
        }
        return;
    }
    register_class();
    let inc = incoming.map(|(url, cookies)| Incoming { url, cookies });
    unsafe {
        let class_buf = wide("NdmVideoDialog");
        let title_buf = wide("Download Video");
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rect = RECT { left: 0, top: 0, right: WIN_W, bottom: WIN_H };
        let _ = AdjustWindowRectEx(&mut rect, style, false, Default::default());
        let boxed = Box::into_raw(Box::new(inc));
        match CreateWindowExW(
            Default::default(),
            pcw(&class_buf),
            pcw(&title_buf),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            Some(parent),
            None,
            Some(crate::winui::instance()),
            Some(boxed as *const core::ffi::c_void),
        ) {
            Ok(hwnd) => {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
            }
            Err(_) => drop(Box::from_raw(boxed)),
        }
    }
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let class_buf = wide("NdmVideoDialog");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(vid_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

fn opts_lv(d: &VidDlg) -> ListView {
    ListView { hwnd: h(d.opts) }
}

fn routed_dir(cat: &str) -> String {
    let s = crate::settings_value();
    s.get("category_dirs")
        .and_then(|d| d.get(cat))
        .and_then(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .map(|x| x.to_string())
        .unwrap_or_else(|| {
            s.get("default_dir")
                .and_then(|x| x.as_str())
                .unwrap_or("~/Downloads")
                .to_string()
        })
}

fn fmt_duration(sec: u64) -> String {
    if sec == 0 {
        return String::new();
    }
    let (hh, m, s) = (sec / 3600, (sec % 3600) / 60, sec % 60);
    if hh > 0 {
        format!("{hh}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn fmt_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// app.js pickQualityIndex, verbatim logic.
fn pick_quality_index(options: &[serde_json::Value]) -> usize {
    let q = crate::settings_value()
        .get("video_quality")
        .and_then(|x| x.as_str())
        .unwrap_or("best")
        .to_string();
    if q == "best" {
        return 0;
    }
    if q == "audio" {
        return options
            .iter()
            .position(|o| o.get("audio").and_then(|x| x.as_bool()) == Some(true))
            .unwrap_or(0);
    }
    let target: u64 = q.parse().unwrap_or(0);
    if let Some(i) = options.iter().position(|o| {
        o.get("audio").and_then(|x| x.as_bool()) != Some(true) && js(o, "label").starts_with(&q)
    }) {
        return i;
    }
    options
        .iter()
        .position(|o| {
            let height = o
                .get("height")
                .and_then(|x| x.as_u64())
                .unwrap_or_else(|| js(o, "label").parse().unwrap_or(0));
            o.get("audio").and_then(|x| x.as_bool()) != Some(true) && height > 0 && height <= target
        })
        .unwrap_or(0)
}

/// app.js plainAddError's plain-language mapping.
fn plain_add_error(e: &str) -> String {
    let low = e.to_lowercase();
    if ["bad url", "parse", "invalid url", "not a valid"].iter().any(|k| low.contains(k)) {
        "That address could not be understood — check it and try again.".to_string()
    } else if ["probe", "connect", "network", "timed out", "timeout", "dns", "refused"]
        .iter()
        .any(|k| low.contains(k))
    {
        "NDM could not reach that address — check the address and your connection, then try again.".to_string()
    } else if e.len() > 120 {
        format!("Could not add the download. {}…", &e[..120])
    } else {
        format!("Could not add the download. {e}")
    }
}

fn set_status(d: &VidDlg, text: &str) {
    unsafe { set_window_text(h(d.status), text) };
}

fn set_submit_enabled(d: &VidDlg, on: bool) {
    dlg::set_enabled(h(d.btn_start), on);
    dlg::set_enabled(h(d.btn_all), on);
    dlg::set_enabled(h(d.btn_later), on);
}

fn present(hwnd: HWND, url: String, cookies: String) {
    let retained = {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_mut() else { return };
        d.presented_url = url.clone();
        d.presented_cookies = cookies;
        d.token_consumed = false;
        unsafe { set_window_text(h(d.url), &url) };
        // retained probe for the same address comes back usable
        // (W193-7); anything else starts from a clean slate
        let retained = d.info.is_some() && d.probed_url == url;
        if !retained {
            d.info = None;
            opts_lv(d).clear();
            set_submit_enabled(d, false);
            set_status(d, "");
        }
        d.dir_touched = false;
        let dir = routed_dir("Video");
        unsafe { set_window_text(h(d.dir), &dir) };
        retained
    };
    if !retained {
        fetch(hwnd);
    }
}

fn fetch(hwnd: HWND) {
    let Some(c) = crate::ctx() else { return };
    let url = {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_mut() else { return };
        let url = unsafe { window_text(h(d.url)) }.trim().to_string();
        if url.is_empty() {
            return;
        }
        set_status(d, "Reading video qualities & file sizes…");
        d.info = None;
        opts_lv(d).clear();
        set_submit_enabled(d, false);
        url
    };
    // Component gate (gateComponents): never surface a setup —
    // the probe waits invisibly and re-runs on the done event.
    let st = ndm_core::components_status(c.clone());
    if st.get("ready").and_then(|x| x.as_bool()) != Some(true) {
        {
            let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(d) = guard.as_mut() {
                d.setup_wanted = true;
            }
        }
        ndm_core::ensure_components(c);
        return;
    }
    probe(hwnd, url);
}

fn probe(hwnd: HWND, url: String) {
    let Some(c) = crate::ctx() else { return };
    let cookies = {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().and_then(|d| {
            if !d.presented_cookies.is_empty() && d.presented_url == url {
                Some(d.presented_cookies.clone())
            } else {
                None
            }
        })
    };
    let probe_url = url.clone();
    let target = raw(hwnd);
    std::thread::spawn(move || {
        let hwnd = h(target);
        let result = ndm_core::rt::block_on(ndm_core::probe_video(c, url, cookies));
        let url = probe_url;
        match result {
            Ok(info) => {
                let v = serde_json::to_value(&info).unwrap_or(serde_json::Value::Null);
                post(hwnd, WM_VD_PROBE, 1, Box::new((url, v)));
            }
            Err(e) => post(hwnd, WM_VD_PROBE, 0, Box::new(e)),
        }
    });
}

fn post<T: 'static>(hwnd: HWND, msg: u32, ok: usize, boxed: Box<T>) {
    let lparam = LPARAM(Box::into_raw(boxed) as isize);
    unsafe {
        if PostMessageW(Some(hwnd), msg, WPARAM(ok), lparam).is_err() {
            drop(Box::from_raw(lparam.0 as *mut T));
        }
    }
}

/// main.rs forwards core's ComponentsProgress payloads here.
pub fn on_components_progress(v: &serde_json::Value) {
    let r = WIN_RAW.load(Ordering::SeqCst);
    if r == 0 {
        return;
    }
    let hwnd = h(r);
    let phase = v.get("phase").and_then(|x| x.as_str()).unwrap_or("");
    let wanted = {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(d) if d.setup_wanted => {
                d.setup_wanted = false;
                true
            }
            _ => false,
        }
    };
    if !wanted {
        return;
    }
    match phase {
        "done" => fetch(hwnd),
        "error" => {
            let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(d) = guard.as_ref() {
                set_status(d, "Could not read this page — check the connection, then try again.");
            }
        }
        _ => {}
    }
}

fn apply_probe(info: serde_json::Value, probed_url: &str) {
    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_mut() else { return };
    let options = info
        .get("options")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if options.is_empty() {
        set_status(d, "No downloadable formats found for this page");
        return;
    }
    d.probed_url = probed_url.to_string();
    unsafe {
        set_window_text(h(d.title), js(&info, "title"));
        let dur = fmt_duration(info.get("duration").and_then(|x| x.as_u64()).unwrap_or(0));
        let meta = [js(&info, "uploader"), &dur]
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(" • ");
        let meta = if meta.is_empty() {
            format!("{} quality options", options.len())
        } else {
            format!("{meta} • {} quality options", options.len())
        };
        set_window_text(h(d.meta), &meta);
    }
    let lv = opts_lv(d);
    lv.clear();
    for (i, o) in options.iter().enumerate() {
        // the badge names the format the job will really produce
        // (N-09): probe ext, else the selector's [ext=…], else
        // Audio/MP4 by kind
        let audio = o.get("audio").and_then(|x| x.as_bool()) == Some(true);
        let sel = js(o, "selector");
        let sel_ext = sel
            .split("[ext=")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .unwrap_or("");
        let ext = js(o, "ext");
        let badge = if !ext.is_empty() {
            ext.to_uppercase()
        } else if !sel_ext.is_empty() {
            sel_ext.to_uppercase()
        } else if audio {
            "Audio".to_string()
        } else {
            "MP4".to_string()
        };
        let approx = o.get("size_approx").and_then(|x| x.as_bool()) == Some(true);
        let size_bytes = o.get("size_bytes").and_then(|x| x.as_u64()).unwrap_or(0);
        let mut size = {
            let t = js(o, "size_text");
            if !t.is_empty() {
                t.to_string()
            } else if size_bytes > 0 {
                fmt_size(size_bytes)
            } else if sel == "best" {
                "Auto".to_string()
            } else {
                "Stream".to_string()
            }
        };
        if approx && size != "Auto" && size != "Stream" && !size.starts_with('~') {
            size = format!("~{size}");
        }
        let note = js(o, "note")
            .split('•')
            .filter(|p| {
                let p = p.to_lowercase();
                !(p.contains("mb") || p.contains("gb") || p.contains("kb"))
            })
            .collect::<Vec<_>>()
            .join("•")
            .trim()
            .to_string();
        lv.insert_row(
            i as i32,
            &[js(o, "label").to_string(), badge, size, note],
        );
    }
    let pick = pick_quality_index(&options);
    lv.set_selected(pick as i32, true);
    d.last_audio = options
        .get(pick)
        .map(|o| o.get("audio").and_then(|x| x.as_bool()) == Some(true));
    d.info = Some(info);
    set_submit_enabled(d, true);
    set_status(d, "");
}

fn selected_option(d: &VidDlg) -> Option<serde_json::Value> {
    let row = opts_lv(d).selected_rows().into_iter().next()?;
    d.info
        .as_ref()?
        .get("options")?
        .as_array()?
        .get(row as usize)
        .cloned()
}

fn submit(hwnd: HWND, later: bool, force: bool) {
    let Some(c) = crate::ctx() else { return };
    let args = {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_mut() else { return };
        if d.submitting {
            return;
        }
        let url = unsafe { window_text(h(d.url)) }.trim().to_string();
        if d.info.is_none() || d.probed_url != url {
            return;
        }
        let Some(opt) = selected_option(d) else { return };
        let dir = {
            let t = unsafe { window_text(h(d.dir)) }.trim().to_string();
            if t.is_empty() { "~/Downloads".to_string() } else { t }
        };
        let title = {
            let t = unsafe { window_text(h(d.title)) }.trim().to_string();
            if t.is_empty() {
                d.info.as_ref().map(|i| js(i, "title").to_string()).unwrap_or_default()
            } else {
                t
            }
        };
        let cookies = if !d.presented_cookies.is_empty() && d.presented_url == url {
            Some(d.presented_cookies.clone())
        } else {
            None
        };
        let queue = {
            let idx = unsafe { crate::winui::send(h(d.queue), 0x0147, WPARAM(0), LPARAM(0)) };
            d.queues.get(idx.max(0) as usize).cloned()
        };
        let desc = {
            let t = unsafe { window_text(h(d.desc)) }.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        };
        let duration = d
            .info
            .as_ref()
            .and_then(|i| i.get("duration"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as f64;
        d.submitting = true;
        d.pending_later = later;
        set_submit_enabled(d, false);
        (
            url,
            dir,
            js(&opt, "selector").to_string(),
            opt.get("audio").and_then(|x| x.as_bool()) == Some(true),
            title,
            cookies,
            later,
            opt.get("size_bytes").and_then(|x| x.as_u64()).unwrap_or(0),
            duration,
            queue,
            desc,
        )
    };
    let (url, dir, selector, audio, title, cookies, later, total, duration, queue, desc) = args;
    let target = raw(hwnd);
    std::thread::spawn(move || {
        let hwnd = h(target);
        let result = ndm_core::rt::block_on(ndm_core::add_video(
            c,
            url,
            dir,
            selector,
            audio,
            title,
            cookies,
            Some(later),
            Some(total),
            Some(duration),
            Some(force),
            queue,
            desc,
            Some(true), // the shown folder is final (W193-3)
        ));
        let text = match result {
            Ok(_) => String::new(),
            Err(e) if e.starts_with("duplicate:") => "duplicate".to_string(),
            Err(e) => e,
        };
        post(hwnd, WM_VD_ADDED, 0, Box::new(text));
    });
}

fn download_all(hwnd: HWND) {
    let Some(c) = crate::ctx() else { return };
    let job = {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_ref() else { return };
        let url = unsafe { window_text(h(d.url)) }.trim().to_string();
        if d.info.is_none() || d.probed_url != url {
            return;
        }
        let options = d
            .info
            .as_ref()
            .and_then(|i| i.get("options"))
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let dir = {
            let t = unsafe { window_text(h(d.dir)) }.trim().to_string();
            if t.is_empty() { "~/Downloads".to_string() } else { t }
        };
        let title = {
            let t = unsafe { window_text(h(d.title)) }.trim().to_string();
            if t.is_empty() {
                d.info.as_ref().map(|i| js(i, "title").to_string()).unwrap_or_default()
            } else {
                t
            }
        };
        let cookies = if !d.presented_cookies.is_empty() && d.presented_url == url {
            Some(d.presented_cookies.clone())
        } else {
            None
        };
        let queue = {
            let idx = unsafe { crate::winui::send(h(d.queue), 0x0147, WPARAM(0), LPARAM(0)) };
            d.queues.get(idx.max(0) as usize).cloned()
        };
        let duration = d
            .info
            .as_ref()
            .and_then(|i| i.get("duration"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as f64;
        (url, dir, title, cookies, queue, duration, options)
    };
    let (url, dir, title, cookies, queue, duration, options) = job;
    // app.js closes first, then adds each quality; duplicates are
    // skipped, not confirmed one by one
    close_window(hwnd, true);
    std::thread::spawn(move || {
        let (mut sent, mut skipped) = (0usize, 0usize);
        for opt in &options {
            let r = ndm_core::rt::block_on(ndm_core::add_video(
                c.clone(),
                url.clone(),
                dir.clone(),
                js(opt, "selector").to_string(),
                opt.get("audio").and_then(|x| x.as_bool()) == Some(true),
                title.clone(),
                cookies.clone(),
                Some(false),
                Some(opt.get("size_bytes").and_then(|x| x.as_u64()).unwrap_or(0)),
                Some(duration),
                Some(false),
                queue.clone(),
                None,
                Some(true),
            ));
            match r {
                Ok(_) => sent += 1,
                Err(_) => skipped += 1,
            }
        }
        crate::post_status(format!(
            "Download all: {sent} qualities added{}",
            if skipped > 0 { format!(", {skipped} skipped") } else { String::new() }
        ));
        crate::post_refresh();
    });
}

/// Close path (app.js closeVideoModal): an unconsumed captured
/// session is discarded; `consumed` marks the token as taken by
/// a download (or by Download-all) so it survives.
fn close_window(hwnd: HWND, consumed: bool) {
    let token = {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(d) => {
                if consumed {
                    d.token_consumed = true;
                }
                if !d.token_consumed && !d.presented_cookies.is_empty() {
                    d.presented_cookies.clone()
                } else {
                    String::new()
                }
            }
            None => String::new(),
        }
    };
    if !token.is_empty() {
        if let Some(c) = crate::ctx() {
            ndm_core::discard_cookies(c, token);
        }
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn build(hwnd: HWND, incoming: Option<Incoming>) {
    let queues = match crate::ctx() {
        Some(c) => crate::queue_names(&ndm_core::get_settings(c)),
        None => vec![ndm_core::main_queue_name()],
    };
    unsafe {
        let label = |text: &str, x: i32, y: i32, w: i32, id: i32| {
            dlg::create_child(hwnd, "STATIC", text, WS_CHILD | WS_VISIBLE, x, y, w, 20, id)
        };
        let edit = |id: i32, x: i32, y: i32, w: i32| {
            dlg::create_child(
                hwnd,
                "EDIT",
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | WINDOW_STYLE(0x0080),
                x,
                y,
                w,
                23,
                id,
            )
        };
        label("Video address", 16, 17, 110, 0);
        let url = edit(ID_URL, 130, 14, 380, );
        let fetch = dlg::create_child(hwnd, "BUTTON", "Fetch Info", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 518, 13, 106, 25, ID_FETCH);
        label("Save to folder", 16, 49, 110, 0);
        let dir = edit(ID_DIR, 130, 46, 494);
        label("Queue", 16, 81, 110, 0);
        let queue = dlg::create_child(hwnd, "COMBOBOX", "", WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0003), 130, 78, 240, 120, ID_QUEUE);
        for q in &queues {
            let mut buf = wide(q);
            crate::winui::send(queue, 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
        }
        crate::winui::send(queue, 0x014E, WPARAM(0), LPARAM(0));
        label("Description", 384, 81, 80, 0);
        let desc = edit(ID_DESC, 448, 78, 176);
        let status = label("", 16, 108, 608, ID_STATUS);
        label("Title", 16, 137, 110, 0);
        let title = edit(ID_TITLE, 130, 134, 494);
        let meta = label("", 16, 164, 608, ID_META);
        let opts = ListView::create(
            hwnd,
            ID_OPTS,
            &[
                ColumnSpec { label: "Quality", width: 210 },
                ColumnSpec { label: "Format", width: 70 },
                ColumnSpec { label: "Size", width: 100 },
                ColumnSpec { label: "Note", width: 220 },
            ],
        )
        .map(|lv| {
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(lv.hwnd, 16, 190, 608, 240, true);
            lv.hwnd
        })
        .unwrap_or_default();
        let btn = |text: &str, x: i32, id: i32| {
            dlg::create_child(hwnd, "BUTTON", text, WS_CHILD | WS_VISIBLE | WS_TABSTOP, x, WIN_H - 42, 118, 28, id)
        };
        let btn_start = btn("Start Download", 16, ID_START);
        let btn_all = btn("Download all", 142, ID_ALL);
        let btn_later = btn("Download Later", 268, ID_LATER);
        btn("Cancel", 506, ID_CANCEL);
        let state = VidDlg {
            url: raw(url),
            dir: raw(dir),
            queue: raw(queue),
            desc: raw(desc),
            status: raw(status),
            title: raw(title),
            meta: raw(meta),
            opts: raw(opts),
            btn_start: raw(btn_start),
            btn_all: raw(btn_all),
            btn_later: raw(btn_later),
            info: None,
            probed_url: String::new(),
            presented_url: String::new(),
            presented_cookies: String::new(),
            token_consumed: false,
            dir_touched: false,
            setup_wanted: false,
            submitting: false,
            pending_later: false,
            last_audio: None,
            queues,
        };
        *DLG.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
        let _ = fetch;
    }
    // Default folder: the routed Video folder (W193-3).
    {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_ref() {
            unsafe { set_window_text(h(d.dir), &routed_dir("Video")) };
        }
    }
    if let Some(inc) = incoming {
        if !inc.url.is_empty() {
            present(hwnd, inc.url, inc.cookies);
        }
    }
}

fn on_command(hwnd: HWND, id: i32, code: u32) {
    match id {
        ID_FETCH => fetch(hwnd),
        ID_START => submit(hwnd, false, false),
        ID_LATER => submit(hwnd, true, false),
        ID_ALL => download_all(hwnd),
        ID_CANCEL => close_window(hwnd, false),
        ID_DIR => {
            if code == EN_CHANGE_VAL {
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    d.dir_touched = true;
                }
            }
        }
        ID_URL => {
            if code == EN_CHANGE_VAL {
                // editing the address invalidates the probe
                // (app.js vInUrl input handler)
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    if d.info.is_some() {
                        let url = unsafe { window_text(h(d.url)) }.trim().to_string();
                        if url != d.probed_url {
                            d.info = None;
                            opts_lv(d).clear();
                            set_submit_enabled(d, false);
                            set_status(d, "");
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn vid_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            WIN_RAW.store(raw(hwnd), Ordering::SeqCst);
            let cs = l.0 as *const CREATESTRUCTW;
            let inc = Box::from_raw((*cs).lpCreateParams as *mut Option<Incoming>);
            build(hwnd, *inc);
            LRESULT(0)
        }
        WM_VD_PROBE => {
            if w.0 == 1 {
                let (probed_url, v) = *Box::from_raw(l.0 as *mut (String, serde_json::Value));
                // a probe that lands after the address changed is
                // stale (app.js probeSeq): drop it
                let current = {
                    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    guard.as_ref().map(|d| unsafe { window_text(h(d.url)) }.trim().to_string())
                };
                if current.as_deref() == Some(probed_url.as_str()) {
                    apply_probe(v, &probed_url);
                }
            } else if l.0 != 0 {
                let e = *Box::from_raw(l.0 as *mut String);
                let guard = DLG.lock().unwrap_or_else(|e2| e2.into_inner());
                if let Some(d) = guard.as_ref() {
                    set_status(d, &e);
                }
            }
            LRESULT(0)
        }
        WM_VD_ADDED => {
            let text = if l.0 != 0 {
                *Box::from_raw(l.0 as *mut String)
            } else {
                String::new()
            };
            if text.is_empty() {
                close_window(hwnd, true);
                crate::post_refresh();
            } else if text == "duplicate" {
                {
                    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_mut() {
                        d.submitting = false;
                        set_submit_enabled(d, true);
                    }
                }
                if dlg::show_confirm(
                    hwnd,
                    "Download again",
                    "This video quality is already in the download list.\nDownload it again as a new copy?",
                    "Download again",
                ) {
                    let later = DLG
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .map(|d| d.pending_later)
                        .unwrap_or(false);
                    submit(hwnd, later, true);
                }
            } else {
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    d.submitting = false;
                    set_submit_enabled(d, true);
                    set_status(d, &plain_add_error(&text));
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, (w.0 & 0xffff) as i32, ((w.0 >> 16) & 0xffff) as u32);
            LRESULT(0)
        }
        WM_NOTIFY => {
            let hdr = l.0 as *const NMHDR;
            if !hdr.is_null() && (*hdr).idFrom == ID_OPTS as usize && (*hdr).code as i32 == LVN_ITEMCHANGED_CODE {
                // an audio pick routes to Music, a video pick to
                // Video — keep the shown folder truthful (W193-3)
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    if !d.dir_touched {
                        if let Some(opt) = selected_option(d) {
                            let audio = opt.get("audio").and_then(|x| x.as_bool()) == Some(true);
                            if d.last_audio != Some(audio) {
                                d.last_audio = Some(audio);
                                let dir = routed_dir(if audio { "Music" } else { "Video" });
                                unsafe { set_window_text(h(d.dir), &dir) };
                            }
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            close_window(hwnd, false);
            LRESULT(0)
        }
        WM_DESTROY => {
            WIN_RAW.store(0, Ordering::SeqCst);
            *DLG.lock().unwrap_or_else(|e| e.into_inner()) = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}
