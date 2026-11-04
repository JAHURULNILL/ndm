//! Site Grabber wizard (ui/app.js gmodal) — one modeless
//! window, app.js's five steps: 1 Start page (project name,
//! saved projects, start address, template), 2 Save files to,
//! 3 Explorer filters, 4 File filters, 5 Exploration results.
//! Projects live in Settings.grab_projects (saved/loaded/deleted
//! through the settings JSON seam, core sanitize owning the
//! rules); exploration runs core `grab_explore` on a worker;
//! Download Selected adds each chosen file through
//! `add_download` with category "Grabber", honouring the
//! original-subfolders rule (grabDirFor port) and Download-Later
//! queue mode. Selection is global across filter views (F-25):
//! unchecked state is keyed by item index, Select All/None mean
//! the whole result set, and the Download button carries the
//! true count.
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell.

use crate::winui::dialog as dlg;
use crate::winui::listview::{ColumnSpec, ListView};
use crate::winui::{pcw, send, set_window_text, wide, window_text};
use std::collections::{HashMap, HashSet};
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

const ID_TITLE: i32 = 8601;
const ID_STEPS: i32 = 8602;
const ID_PROJ_LOAD: i32 = 8610;
const ID_PROJ_SAVE: i32 = 8611;
const ID_PROJ_DELETE: i32 = 8612;
const ID_KINDS: i32 = 8620;
const ID_RESULTS: i32 = 8621;
const ID_SEL_ALL: i32 = 8622;
const ID_SEL_NONE: i32 = 8623;
const ID_BACK: i32 = 8630;
const ID_NEXT: i32 = 8631;
const ID_FETCH: i32 = 8632;
const ID_DOWNLOAD: i32 = 8633;
const ID_CANCEL: i32 = 8634;
const ID_STATUS: i32 = 8635;
const ID_COUNT: i32 = 8636;
const FIELD_ID_BASE: i32 = 8700;

const WM_GB_RESULT: u32 = WM_APP + 31; // LPARAM = Box<Result<Value, String>>
const WM_GB_DOWNLOADED: u32 = WM_APP + 32; // LPARAM = Box<String>: "" ok, else status text
const CBN_SELCHANGE_VAL: u32 = 1;
const EN_CHANGE_VAL: u32 = 0x0300;
const LVN_ITEMCHANGED_CODE: i32 = -101;

const WIN_W: i32 = 680;
const WIN_H: i32 = 620;
const STEP_TITLES: [&str; 5] = [
    "Site Grabber — Step 1: Set start page",
    "Site Grabber — Step 2: Save files to",
    "Site Grabber — Step 3: Set explorer filters",
    "Site Grabber — Step 4: Set file filters",
    "Site Grabber — Exploration results",
];
const STEP_NAMES: [&str; 5] = ["Start page", "Save files", "Explore", "File filters", "Results"];
const TEMPLATES: [(&str, &str); 7] = [
    ("custom", "Custom settings"),
    ("pictures", "All pictures of the web site"),
    ("audio", "Audio files from the start page and one next level"),
    ("video", "All video files of the web site"),
    ("all_except_pages", "All files of the web site except web pages and images"),
    ("all_files", "The whole web site — all files"),
    ("complete_page", "Complete start page with all pictures, styles and scripts (offline browsing)"),
];
const PRESETS: [(&str, &str); 8] = [
    ("", "All file types"),
    ("images", "Images files"),
    ("video", "Video files"),
    ("audio", "Audio files"),
    ("pdf", "PDF document files"),
    ("exe", "Executable files"),
    ("compressed", "Compressed files"),
    ("page_assets", "Complete start page (assets)"),
];

fn preset_exts(name: &str) -> Option<Vec<&'static str>> {
    Some(match name {
        "images" => vec!["gif", "jpg", "jpeg", "jpe", "bmp", "png", "webp", "svg", "ico", "tif", "tiff"],
        "video" => vec!["mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "m4v", "ts"],
        "audio" => vec!["mp3", "m4a", "m4b", "flac", "wav", "ogg", "opus", "aac", "wma"],
        "pdf" => vec!["pdf"],
        "exe" => vec!["exe", "msi", "deb", "rpm", "dmg", "pkg", "appimage", "apk", "bin", "run"],
        "compressed" => vec!["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "tgz"],
        "page_assets" => vec!["html", "htm", "js", "css", "png", "jpg", "jpeg", "gif", "svg", "ico"],
        _ => return None,
    })
}

/// GRAB_TEMPLATES: (include, exclude, depth_same, whole_site).
fn template_of(name: &str) -> (Option<Vec<&'static str>>, Option<Vec<&'static str>>, Option<u32>, bool) {
    match name {
        "pictures" => (Some(vec!["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico", "tif", "tiff"]), None, Some(10), true),
        "audio" => (Some(vec!["mp3", "m4a", "m4b", "flac", "wav", "ogg", "opus", "aac", "wma"]), None, Some(1), false),
        "video" => (Some(vec!["mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "m4v", "ts"]), None, Some(10), true),
        "all_except_pages" => (None, Some(vec!["html", "htm", "php", "asp", "aspx", "png", "jpg", "jpeg", "gif", "bmp", "webp", "svg"]), Some(10), true),
        "all_files" => (None, None, Some(10), true),
        "complete_page" => (Some(vec!["html", "htm", "js", "css", "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "woff", "woff2", "ttf"]), None, Some(0), false),
        _ => (None, None, None, false),
    }
}

struct GrabDlg {
    fields: HashMap<String, isize>,
    ids: HashMap<i32, String>,
    combo_values: HashMap<String, Vec<String>>,
    pages: [Vec<isize>; 5],
    title: isize,
    steps_line: isize,
    status: isize,
    count: isize,
    btn_back: isize,
    btn_next: isize,
    btn_fetch: isize,
    btn_download: isize,
    kinds_lv: isize,
    results_lv: isize,
    step: i32,
    next_id: i32,
    items: Vec<serde_json::Value>,
    unchecked: HashSet<usize>,
    kinds_off: HashSet<String>,
    kind_rows: Vec<String>,
    visible_rows: Vec<usize>,
    exploring: bool,
}

static DLG: Mutex<Option<GrabDlg>> = Mutex::new(None);
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

pub fn show(parent: HWND, prefill: Option<String>) {
    if WIN_RAW.load(Ordering::SeqCst) != 0 {
        let hwnd = h(WIN_RAW.load(Ordering::SeqCst));
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        if let Some(url) = prefill {
            open_with(hwnd, url);
        }
        return;
    }
    register_class();
    unsafe {
        let class_buf = wide("NdmGrabberDialog");
        let title_buf = wide("Site Grabber");
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rect = RECT { left: 0, top: 0, right: WIN_W, bottom: WIN_H };
        let _ = AdjustWindowRectEx(&mut rect, style, false, Default::default());
        let boxed = Box::into_raw(Box::new(prefill));
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
        let class_buf = wide("NdmGrabberDialog");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(grab_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

/// A browser "grab all links" hand-off: the wizard opens with
/// the address filled in and the exploration already running
/// (app.js pumpGrabLinks).
fn open_with(hwnd: HWND, url: String) {
    {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_mut() {
            reset_results(d);
            if let Some(&r) = d.fields.get("url") {
                unsafe { set_window_text(h(r), &url) };
            }
        }
    }
    fetch_grab(hwnd);
}

fn reset_results(d: &mut GrabDlg) {
    d.items.clear();
    d.unchecked.clear();
    d.kinds_off.clear();
    d.kind_rows.clear();
    d.visible_rows.clear();
}

fn get_text(d: &GrabDlg, name: &str) -> String {
    d.fields.get(name).map(|&r| unsafe { window_text(h(r)) }).unwrap_or_default()
}
fn set_text(d: &GrabDlg, name: &str, text: &str) {
    if let Some(&r) = d.fields.get(name) {
        unsafe { set_window_text(h(r), text) };
    }
}
fn get_check(d: &GrabDlg, name: &str) -> bool {
    d.fields.get(name).map(|&r| dlg::is_checked(h(r))).unwrap_or(false)
}
fn set_check(d: &GrabDlg, name: &str, on: bool) {
    if let Some(&r) = d.fields.get(name) {
        dlg::set_checked(h(r), on);
    }
}
fn get_combo(d: &GrabDlg, name: &str) -> String {
    let (Some(&r), Some(vals)) = (d.fields.get(name), d.combo_values.get(name)) else {
        return String::new();
    };
    let idx = unsafe { send(h(r), 0x0147, WPARAM(0), LPARAM(0)) };
    vals.get(idx.max(0) as usize).cloned().unwrap_or_default()
}
fn set_combo(d: &GrabDlg, name: &str, value: &str) {
    let (Some(&r), Some(vals)) = (d.fields.get(name), d.combo_values.get(name)) else {
        return;
    };
    let idx = vals.iter().position(|v| v == value).unwrap_or(0);
    unsafe {
        send(h(r), 0x014E, WPARAM(idx), LPARAM(0));
    }
}
fn set_status(d: &GrabDlg, text: &str) {
    unsafe { set_window_text(h(d.status), text) };
}

fn parse_exts(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}
fn num(text: &str) -> u64 {
    text.trim().parse().unwrap_or(0)
}

/// grabWizardState: the whole wizard as a GrabProject JSON.
fn wizard_state(d: &GrabDlg) -> serde_json::Value {
    serde_json::json!({
        "name": get_text(d, "proj_name").trim(),
        "start_url": get_text(d, "url").trim(),
        "template": get_combo(d, "template"),
        "save_dir": get_text(d, "dir").trim(),
        "use_subfolders": get_check(d, "subfolders"),
        "add_as_queue": get_check(d, "add_queue"),
        "convert_links": get_check(d, "convert_links"),
        "queue": get_combo(d, "queue"),
        "depth_same": num(&get_text(d, "depth_same")).min(10),
        "depth_other": num(&get_text(d, "depth_other")).min(10),
        "whole_site": get_check(d, "mode_whole"),
        "no_parent": get_check(d, "no_parent"),
        "same_site_files_only": get_check(d, "same_site"),
        "include_exts": parse_exts(&get_text(d, "incl_exts")),
        "exclude_exts": parse_exts(&get_text(d, "excl_exts")),
        "min_kb": num(&get_text(d, "min_kb")),
        "max_mb": num(&get_text(d, "max_mb")),
    })
}

fn apply_project(d: &GrabDlg, p: &serde_json::Value) {
    set_text(d, "proj_name", js(p, "name"));
    set_text(d, "url", js(p, "start_url"));
    set_combo(d, "template", {
        let t = js(p, "template");
        if t.is_empty() { "custom" } else { t }
    });
    let dir = js(p, "save_dir");
    let fallback = crate::settings_value()
        .get("default_dir")
        .and_then(|x| x.as_str())
        .unwrap_or("~/Downloads")
        .to_string();
    set_text(d, "dir", if dir.is_empty() { &fallback } else { dir });
    set_check(d, "subfolders", p.get("use_subfolders").and_then(|x| x.as_bool()).unwrap_or(false));
    set_check(d, "add_queue", p.get("add_as_queue").and_then(|x| x.as_bool()).unwrap_or(false));
    // a project saved by an older build has no convert_links:
    // it migrates OFF (core's serde default)
    set_check(d, "convert_links", p.get("convert_links").and_then(|x| x.as_bool()).unwrap_or(false));
    set_combo(d, "queue", js(p, "queue"));
    set_text(d, "depth_same", &p.get("depth_same").and_then(|x| x.as_u64()).unwrap_or(1).to_string());
    set_text(d, "depth_other", &p.get("depth_other").and_then(|x| x.as_u64()).unwrap_or(0).to_string());
    let whole = p.get("whole_site").and_then(|x| x.as_bool()).unwrap_or(false);
    set_check(d, "mode_whole", whole);
    set_check(d, "mode_levels", !whole);
    set_check(d, "no_parent", p.get("no_parent").and_then(|x| x.as_bool()).unwrap_or(false));
    set_check(
        d,
        "same_site",
        p.get("same_site_files_only").and_then(|x| x.as_bool()).unwrap_or(true),
    );
    let join = |key: &str| {
        p.get(key)
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_default()
    };
    set_text(d, "incl_exts", &join("include_exts"));
    set_text(d, "excl_exts", &join("exclude_exts"));
    set_text(d, "min_kb", &p.get("min_kb").and_then(|x| x.as_u64()).unwrap_or(0).to_string());
    set_text(d, "max_mb", &p.get("max_mb").and_then(|x| x.as_u64()).unwrap_or(0).to_string());
}

fn fill_projects(d: &GrabDlg) {
    let Some(&r) = d.fields.get("proj_sel") else { return };
    let projects = crate::settings_value()
        .get("grab_projects")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    unsafe {
        send(h(r), 0x014B, WPARAM(0), LPARAM(0)); // CB_RESETCONTENT
        let mut buf = wide("(no saved project selected)");
        send(h(r), 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
        for p in &projects {
            let mut buf = wide(js(p, "name"));
            send(h(r), 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
        }
        send(h(r), 0x014E, WPARAM(0), LPARAM(0));
    }
    // keep combo_values in step with the combo's rows
}

fn queue_names() -> Vec<String> {
    let mut names = vec![String::new()];
    if let Some(arr) = crate::settings_value().get("queues").and_then(|x| x.as_array()) {
        for q in arr {
            names.push(js(q, "name").to_string());
        }
    }
    names
}

/// The queue dropdown's rows: (no queue) + the settings' queue
/// names — mirrors fill_projects.
fn fill_queues(d: &GrabDlg) {
    let Some(&r) = d.fields.get("queue") else { return };
    let queues = crate::settings_value()
        .get("queues")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    unsafe {
        send(h(r), 0x014B, WPARAM(0), LPARAM(0)); // CB_RESETCONTENT
        let mut buf = wide("(no queue)");
        send(h(r), 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
        for q in &queues {
            let mut buf = wide(js(q, "name"));
            send(h(r), 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
        }
        send(h(r), 0x014E, WPARAM(0), LPARAM(0));
    }
}

fn project_names() -> Vec<String> {
    let mut names = vec![String::new()];
    if let Some(arr) = crate::settings_value().get("grab_projects").and_then(|x| x.as_array()) {
        for p in arr {
            names.push(js(p, "name").to_string());
        }
    }
    names
}

fn show_step(n: i32) {
    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_mut() else { return };
    d.step = n;
    unsafe {
        set_window_text(h(d.title), STEP_TITLES[(n - 1) as usize]);
        let line = STEP_NAMES
            .iter()
            .enumerate()
            .map(|(i, name)| {
                if i as i32 + 1 == n {
                    format!("[{} · {}]", i + 1, name)
                } else {
                    format!("{} · {}", i + 1, name)
                }
            })
            .collect::<Vec<_>>()
            .join("   ");
        set_window_text(h(d.steps_line), &line);
    }
    for p in 0..5 {
        let on = p as i32 + 1 == n;
        for &r in &d.pages[p].clone() {
            unsafe {
                let _ = ShowWindow(h(r), if on { SW_SHOW } else { windows::Win32::UI::WindowsAndMessaging::SW_HIDE });
            }
        }
    }
    dlg::set_enabled(h(d.btn_back), n > 1);
    let show_btn = |hwnd: isize, on: bool| unsafe {
        let _ = ShowWindow(h(hwnd), if on { SW_SHOW } else { windows::Win32::UI::WindowsAndMessaging::SW_HIDE });
    };
    show_btn(d.btn_next, n < 4);
    show_btn(d.btn_fetch, n == 4);
    show_btn(d.btn_download, n == 5);
}

// ----------------------------------------------------------------
// Results (step 5)
// ----------------------------------------------------------------
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

fn grab_visible(d: &GrabDlg, it: &serde_json::Value) -> bool {
    if d.kinds_off.contains(js(it, "kind")) {
        return false;
    }
    let q = get_text(d, "filter").trim().to_lowercase();
    q.is_empty() || format!("{} {}", js(it, "name"), js(it, "url")).to_lowercase().contains(&q)
}

fn sync_kinds(d: &mut GrabDlg) {
    let lv = ListView { hwnd: h(d.kinds_lv) };
    for (row, kind) in d.kind_rows.clone().iter().enumerate() {
        if lv.is_checked(row as i32) {
            d.kinds_off.remove(kind);
        } else {
            d.kinds_off.insert(kind.clone());
        }
    }
}

fn sync_results(d: &mut GrabDlg) {
    let lv = ListView { hwnd: h(d.results_lv) };
    for (row, &idx) in d.visible_rows.clone().iter().enumerate() {
        if lv.is_checked(row as i32) {
            d.unchecked.remove(&idx);
        } else {
            d.unchecked.insert(idx);
        }
    }
}

fn render_kinds() {
    // Snapshot under the lock, write to the control AFTER dropping
    // it: set_checked fires LVN_ITEMCHANGED synchronously and the
    // notify handler takes this same non-reentrant mutex — writing
    // under the lock deadlocks the UI thread (AUDIT-233 CRITICAL).
    let (kinds_raw, rows) = {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_mut() else { return };
        let mut counts: Vec<(String, usize)> = Vec::new();
        for it in &d.items {
            let kind = js(it, "kind").to_string();
            if let Some(slot) = counts.iter_mut().find(|(k, _)| *k == kind) {
                slot.1 += 1;
            } else {
                counts.push((kind, 1));
            }
        }
        d.kind_rows = counts.iter().map(|(k, _)| k.clone()).collect();
        let rows = counts
            .iter()
            .map(|(kind, n)| (format!("{kind} ({n})"), !d.kinds_off.contains(kind)))
            .collect::<Vec<_>>();
        (d.kinds_lv, rows)
    };
    let lv = ListView { hwnd: h(kinds_raw) };
    lv.clear();
    for (i, (label, checked)) in rows.iter().enumerate() {
        lv.insert_row(i as i32, &[label.clone()]);
        lv.set_checked(i as i32, *checked);
    }
}

fn render_list() {
    let (results_raw, rows) = {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_mut() else { return };
        sync_kinds(d);
        d.visible_rows.clear();
        let mut rows: Vec<(Vec<String>, bool)> = Vec::new();
        for (i, it) in d.items.clone().iter().enumerate() {
            if !grab_visible(d, it) {
                continue;
            }
            let size = it
                .get("size")
                .and_then(|x| x.as_u64())
                .map(fmt_size)
                .unwrap_or_else(|| "—".to_string());
            let label = {
                let l = js(it, "label");
                if l.is_empty() || l == js(it, "name") { String::new() } else { l.to_string() }
            };
            rows.push((
                vec![js(it, "name").to_string(), label, js(it, "kind").to_string(), size],
                !d.unchecked.contains(&i),
            ));
            d.visible_rows.push(i);
        }
        (d.results_lv, rows)
    };
    // Control writes happen lock-free (see render_kinds).
    let lv = ListView { hwnd: h(results_raw) };
    lv.clear();
    for (row, (cells, checked)) in rows.iter().enumerate() {
        lv.insert_row(row as i32, cells);
        lv.set_checked(row as i32, *checked);
    }
    update_count();
}

fn update_count() {
    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_ref() else { return };
    let sel: Vec<usize> = (0..d.items.len()).filter(|i| !d.unchecked.contains(i)).collect();
    let vis = sel
        .iter()
        .filter(|&&i| d.items.get(i).map(|it| grab_visible(d, it)).unwrap_or(false))
        .count();
    let text = if vis == sel.len() {
        format!("{} selected", sel.len())
    } else {
        format!("{} selected ({} in current view)", sel.len(), vis)
    };
    unsafe {
        set_window_text(h(d.count), &text);
        set_window_text(h(d.btn_download), &format!("Download Selected ({})", sel.len()));
    }
    dlg::set_enabled(h(d.btn_download), !sel.is_empty());
}

fn fetch_grab(hwnd: HWND) {
    let Some(c) = crate::ctx() else { return };
    let p = {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_ref() else { return };
        wizard_state(d)
    };
    if js(&p, "start_url").is_empty() {
        show_step(1);
        return;
    }
    show_step(5);
    {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_mut() {
            if d.exploring {
                return;
            }
            d.exploring = true;
            set_status(d, "Exploring…");
        }
    }
    let args_json = serde_json::json!({
        "url": js(&p, "start_url"),
        "depth_same": p.get("depth_same").cloned().unwrap_or(serde_json::json!(1)),
        "depth_other": p.get("depth_other").cloned().unwrap_or(serde_json::json!(0)),
        "whole_site": p.get("whole_site").cloned().unwrap_or(serde_json::json!(false)),
        "no_parent": p.get("no_parent").cloned().unwrap_or(serde_json::json!(false)),
        "same_site_files_only": p.get("same_site_files_only").cloned().unwrap_or(serde_json::json!(true)),
        "include_exts": p.get("include_exts").cloned().unwrap_or(serde_json::json!([])),
        "exclude_exts": p.get("exclude_exts").cloned().unwrap_or(serde_json::json!([])),
        "min_kb": p.get("min_kb").cloned().unwrap_or(serde_json::json!(0)),
        "max_mb": p.get("max_mb").cloned().unwrap_or(serde_json::json!(0)),
    });
    let target = raw(hwnd);
    std::thread::spawn(move || {
        let result: Result<serde_json::Value, String> = (|| {
            let args: ndm_core::GrabExploreArgs =
                serde_json::from_value(args_json).map_err(|e| e.to_string())?;
            let dto = ndm_core::rt::block_on(ndm_core::grab_explore(c, args))?;
            serde_json::to_value(&dto).map_err(|e| e.to_string())
        })();
        let lparam = LPARAM(Box::into_raw(Box::new(result)) as isize);
        unsafe {
            if PostMessageW(Some(h(target)), WM_GB_RESULT, WPARAM(0), lparam).is_err() {
                drop(Box::from_raw(lparam.0 as *mut Result<serde_json::Value, String>));
            }
        }
    });
}

/// grabDirFor port: <save dir>/<the file's folders below the
/// start page's own folder>.
fn grab_dir_for(base: &str, start_url: &str, item_url: &str) -> String {
    fn split(u: &str) -> (String, String) {
        let rest = u.split("://").nth(1).unwrap_or(u);
        match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
            None => (rest.to_string(), "/".to_string()),
        }
    }
    let (i_host, i_path) = split(item_url);
    let mut parts: Vec<&str> = i_path.split('/').filter(|s| !s.is_empty()).collect();
    parts.pop(); // the file name itself
    let (s_host, s_path) = split(start_url);
    if s_host == i_host {
        let mut root: Vec<&str> = s_path.split('/').filter(|s| !s.is_empty()).collect();
        if !s_path.ends_with('/') {
            root.pop();
        }
        if root.iter().enumerate().all(|(i, seg)| parts.get(i) == Some(seg)) {
            parts.drain(..root.len());
        }
    }
    if parts.is_empty() {
        return base.to_string();
    }
    let sep = if base.contains('\\') && !base.contains('/') { "\\" } else { "/" };
    format!("{}{}{}", base.trim_end_matches(['/', '\\']), sep, parts.join(sep))
}

fn download_selected(hwnd: HWND) {
    let Some(c) = crate::ctx() else { return };
    let (p, chosen) = {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_mut() else { return };
        sync_results(d);
        let chosen: Vec<serde_json::Value> = d
            .items
            .iter()
            .enumerate()
            .filter(|(i, _)| !d.unchecked.contains(i))
            .map(|(_, it)| it.clone())
            .collect();
        (wizard_state(d), chosen)
    };
    if chosen.is_empty() {
        return;
    }
    let base = {
        let dir = js(&p, "save_dir");
        if dir.is_empty() { "~/Downloads".to_string() } else { dir.to_string() }
    };
    let start_url = js(&p, "start_url").to_string();
    let subfolders = p.get("use_subfolders").and_then(|x| x.as_bool()).unwrap_or(false);
    let later = p.get("add_as_queue").and_then(|x| x.as_bool()).unwrap_or(false);
    let total = chosen.len();
    let target = raw(hwnd);
    std::thread::spawn(move || {
        // F1: with conversion on, the start page itself joins the
        // run (exploration crawls pages but returns only files)
        // and the whole run is registered for the post-completion
        // rewrite
        let convert = p.get("convert_links").and_then(|x| x.as_bool()).unwrap_or(false);
        let mut run_urls: Vec<String> = chosen.iter().map(|it| js(it, "url").to_string()).collect();
        if convert {
            run_urls.push(start_url.clone());
            let _ = ndm_core::rt::block_on(ndm_core::add_download(
                c.clone(),
                start_url.clone(),
                base.clone(),
                Some(later),
                Some("Grabber".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ));
        }
        let mut failed = 0usize;
        let mut first_err = String::new();
        for it in &chosen {
            let url = js(it, "url").to_string();
            let dir = if subfolders {
                grab_dir_for(&base, &start_url, &url)
            } else {
                base.clone()
            };
            let r = ndm_core::rt::block_on(ndm_core::add_download(
                c.clone(),
                url,
                dir,
                Some(later),
                Some("Grabber".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ));
            if let Err(e) = r {
                failed += 1;
                if first_err.is_empty() {
                    first_err = e;
                }
            }
        }
        if convert {
            ndm_core::grab_register_run(run_urls);
        }
        let text = if failed == 0 {
            String::new()
        } else {
            let why = if first_err.starts_with("duplicate") {
                "already in the download list".to_string()
            } else if first_err.len() > 90 {
                // Char-boundary-safe cut (see video_dialog's
                // plain_add_error — a fixed byte slice panics on
                // multi-byte text).
                let mut cut = 90;
                while cut > 0 && !first_err.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…", &first_err[..cut])
            } else {
                first_err
            };
            format!("{failed} of {total} could not be added — {why}")
        };
        let lparam = LPARAM(Box::into_raw(Box::new(text)) as isize);
        unsafe {
            if PostMessageW(Some(h(target)), WM_GB_DOWNLOADED, WPARAM(0), lparam).is_err() {
                drop(Box::from_raw(lparam.0 as *mut String));
            }
        }
    });
}

// ----------------------------------------------------------------
// Build
// ----------------------------------------------------------------
struct B {
    parent: HWND,
}

impl B {
    unsafe fn ctl(&self, d: &mut GrabDlg, page: usize, name: &str, class: &str, text: &str, style: u32, x: i32, y: i32, w: i32, ht: i32) -> HWND {
        let id = if name.is_empty() { 0 } else { d.next_id };
        if !name.is_empty() {
            d.next_id += 1;
            d.ids.insert(id, name.to_string());
        }
        let hwnd = dlg::create_child(
            self.parent,
            class,
            text,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(style),
            x,
            y,
            w,
            ht,
            id,
        );
        if !name.is_empty() {
            d.fields.insert(name.to_string(), raw(hwnd));
        }
        d.pages[page].push(raw(hwnd));
        hwnd
    }
    unsafe fn label(&self, d: &mut GrabDlg, page: usize, text: &str, x: i32, y: i32, w: i32) {
        let hwnd = dlg::create_child(self.parent, "STATIC", text, WS_CHILD | WS_VISIBLE, x, y, w, 20, 0);
        d.pages[page].push(raw(hwnd));
    }
    unsafe fn edit(&self, d: &mut GrabDlg, page: usize, name: &str, label: &str, y: i32, value: &str) {
        self.label(d, page, label, 20, y, 640);
        self.ctl(d, page, name, "EDIT", value, WS_BORDER.0 | 0x0080, 20, y + 22, 640, 23);
    }
    unsafe fn row_edit(&self, d: &mut GrabDlg, page: usize, name: &str, label: &str, y: i32, value: &str) {
        self.label(d, page, label, 20, y + 3, 430);
        self.ctl(d, page, name, "EDIT", value, WS_BORDER.0 | 0x0080, 460, y, 90, 23);
    }
    unsafe fn check(&self, d: &mut GrabDlg, page: usize, name: &str, label: &str, y: i32, on: bool) {
        let hwnd = self.ctl(d, page, name, "BUTTON", label, 0x0002, 20, y, 640, 22);
        dlg::set_checked(hwnd, on);
    }
    unsafe fn combo(&self, d: &mut GrabDlg, page: usize, name: &str, label: &str, y: i32, pairs: &[(&str, &str)], value: &str) {
        self.label(d, page, label, 20, y, 640);
        let hwnd = self.ctl(d, page, name, "COMBOBOX", "", 0x0003, 20, y + 22, 460, 120);
        let mut values = Vec::new();
        for (v, l) in pairs {
            let mut buf = wide(l);
            send(hwnd, 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
            values.push(v.to_string());
        }
        let sel = pairs.iter().position(|(v, _)| *v == value).unwrap_or(0);
        send(hwnd, 0x014E, WPARAM(sel), LPARAM(0));
        d.combo_values.insert(name.to_string(), values);
    }
}

fn build(hwnd: HWND, prefill: Option<String>) {
    let b = B { parent: hwnd };
    let settings = crate::settings_value();
    let default_dir = settings
        .get("default_dir")
        .and_then(|x| x.as_str())
        .unwrap_or("~/Downloads")
        .to_string();
    let mut d = GrabDlg {
        fields: HashMap::new(),
        ids: HashMap::new(),
        combo_values: HashMap::new(),
        pages: [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        title: 0,
        steps_line: 0,
        status: 0,
        count: 0,
        btn_back: 0,
        btn_next: 0,
        btn_fetch: 0,
        btn_download: 0,
        kinds_lv: 0,
        results_lv: 0,
        step: 1,
        next_id: FIELD_ID_BASE,
        items: Vec::new(),
        unchecked: HashSet::new(),
        kinds_off: HashSet::new(),
        kind_rows: Vec::new(),
        visible_rows: Vec::new(),
        exploring: false,
    };
    unsafe {
        d.title = raw(dlg::create_child(hwnd, "STATIC", STEP_TITLES[0], WS_CHILD | WS_VISIBLE, 16, 12, 640, 22, ID_TITLE));
        d.steps_line = raw(dlg::create_child(hwnd, "STATIC", "", WS_CHILD | WS_VISIBLE, 16, 38, 640, 20, ID_STEPS));

        // Step 1
        b.edit(&mut d, 0, "proj_name", "Grabber project name", 70, "");
        b.label(&mut d, 0, "Saved projects", 20, 122, 640);
        let sel = b.ctl(&mut d, 0, "proj_sel", "COMBOBOX", "", 0x0003, 20, 144, 300, 120);
        let _ = sel;
        for (id, label, x) in [(ID_PROJ_LOAD, "Load", 328), (ID_PROJ_SAVE, "Save project", 412), (ID_PROJ_DELETE, "Delete", 532)] {
            let btn = dlg::create_child(hwnd, "BUTTON", label, WS_CHILD | WS_VISIBLE | WS_TABSTOP, x, 143, if id == ID_PROJ_SAVE { 112 } else { 76 }, 25, id);
            d.pages[0].push(raw(btn));
        }
        b.edit(&mut d, 0, "url", "Start page / address", 178, prefill.as_deref().unwrap_or(""));
        b.combo(&mut d, 0, "template", "Project template", 230, &TEMPLATES, "custom");
        b.label(&mut d, 0, "Downloads from a site with a saved login (Options → Site Logins) use that login automatically.", 20, 306, 640);

        // Step 2
        b.edit(&mut d, 1, "dir", "Save all files to the following folder", 70, &default_dir);
        b.check(&mut d, 1, "subfolders", "Use the original relative subfolders", 130, false);
        b.check(&mut d, 1, "add_queue", "Add the files as a queue (Download Later) instead of starting them at once", 158, false);
        // F1: on for projects the wizard creates; a loaded project
        // restores its own saved value (older projects: off)
        b.check(&mut d, 1, "convert_links", "Convert links in the downloaded pages for offline browsing", 186, true);
        // F2: the queue whose schedule re-runs this project
        b.label(&mut d, 1, "Re-run on this queue's schedule", 20, 214, 640);
        let _ = b.ctl(&mut d, 1, "queue", "COMBOBOX", "", 0x0003, 20, 236, 300, 120);

        // Step 3
        let whole = b.ctl(&mut d, 2, "mode_whole", "BUTTON", "Explore the whole site", 0x0009, 20, 70, 500, 22);
        let levels = b.ctl(&mut d, 2, "mode_levels", "BUTTON", "Explore the specified number of link levels:", 0x0009, 20, 98, 500, 22);
        dlg::set_checked(levels, true);
        let _ = whole;
        b.row_edit(&mut d, 2, "depth_same", "Levels within the start site", 128, "1");
        b.row_edit(&mut d, 2, "depth_other", "Levels on other sites", 160, "0");
        b.check(&mut d, 2, "no_parent", "Don't explore parent directories", 192, false);
        b.label(&mut d, 2, "Exploration follows page links in static HTML. Pages built entirely by JavaScript are the browser extension's territory.", 20, 226, 640);

        // Step 4
        b.combo(&mut d, 3, "incl_preset", "Download the following files (filter preset)", 70, &PRESETS, "");
        b.edit(&mut d, 3, "incl_exts", "…or list extensions yourself (comma separated)", 126, "");
        b.edit(&mut d, 3, "excl_exts", "Don't download these extensions", 182, "");
        b.check(&mut d, 3, "same_site", "Search files on this site only", 238, true);
        b.row_edit(&mut d, 3, "min_kb", "Download if file size is not less than (KB)", 268, "0");
        b.row_edit(&mut d, 3, "max_mb", "…and not more than (MB, 0 = no limit)", 300, "0");

        // Step 5
        d.status = raw(dlg::create_child(hwnd, "STATIC", "Enter a page URL and click Explore", WS_CHILD | WS_VISIBLE, 20, 70, 640, 20, ID_STATUS));
        d.pages[4].push(d.status);
        let kinds = ListView::create_ex(hwnd, ID_KINDS, &[ColumnSpec { label: "Kind", width: 200 }], true)
            .map(|lv| {
                let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(lv.hwnd, 20, 96, 300, 62, true);
                lv.hwnd
            })
            .unwrap_or_default();
        d.kinds_lv = raw(kinds);
        d.pages[4].push(d.kinds_lv);
        let filter = b.ctl(&mut d, 4, "filter", "EDIT", "", WS_BORDER.0 | 0x0080, 20, 164, 640, 23);
        let _ = filter;
        for (id, label, x, w) in [(ID_SEL_ALL, "Select All", 20, 100), (ID_SEL_NONE, "Select None", 128, 100)] {
            let btn = dlg::create_child(hwnd, "BUTTON", label, WS_CHILD | WS_VISIBLE | WS_TABSTOP, x, 194, w, 25, id);
            d.pages[4].push(raw(btn));
        }
        d.count = raw(dlg::create_child(hwnd, "STATIC", "0 selected", WS_CHILD | WS_VISIBLE, 240, 197, 300, 20, ID_COUNT));
        d.pages[4].push(d.count);
        let results = ListView::create_ex(
            hwnd,
            ID_RESULTS,
            &[
                ColumnSpec { label: "Name", width: 250 },
                ColumnSpec { label: "Label", width: 170 },
                ColumnSpec { label: "Kind", width: 90 },
                ColumnSpec { label: "Size", width: 90 },
            ],
            true,
        )
        .map(|lv| {
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(lv.hwnd, 20, 226, 640, 300, true);
            lv.hwnd
        })
        .unwrap_or_default();
        d.results_lv = raw(results);
        d.pages[4].push(d.results_lv);

        // Button row
        d.btn_back = raw(dlg::create_child(hwnd, "BUTTON", "‹ Back", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 16, WIN_H - 44, 92, 28, ID_BACK));
        d.btn_next = raw(dlg::create_child(hwnd, "BUTTON", "Next ›", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 116, WIN_H - 44, 92, 28, ID_NEXT));
        d.btn_fetch = raw(dlg::create_child(hwnd, "BUTTON", "Explore", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 116, WIN_H - 44, 92, 28, ID_FETCH));
        d.btn_download = raw(dlg::create_child(hwnd, "BUTTON", "Download Selected", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 116, WIN_H - 44, 190, 28, ID_DOWNLOAD));
        dlg::create_child(hwnd, "BUTTON", "Cancel", WS_CHILD | WS_VISIBLE | WS_TABSTOP, WIN_W - 108, WIN_H - 44, 92, 28, ID_CANCEL);
    }
    // project combo values mirror its rows ("" + names)
    d.combo_values.insert("proj_sel".to_string(), project_names());
    d.combo_values.insert("queue".to_string(), queue_names());
    *DLG.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
    {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_ref() {
            fill_projects(d);
            fill_queues(d);
        }
    }
    show_step(1);
    update_count();
    if prefill.is_some() {
        fetch_grab(hwnd);
    }
}

fn on_command(hwnd: HWND, id: i32, code: u32) {
    match id {
        ID_BACK => {
            let step = DLG.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|d| d.step).unwrap_or(1);
            show_step((step - 1).max(1));
        }
        ID_NEXT => {
            let (step, has_url) = {
                let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_ref() {
                    Some(d) => (d.step, !get_text(d, "url").trim().is_empty()),
                    None => return,
                }
            };
            if step == 1 && !has_url {
                return;
            }
            show_step((step + 1).min(4));
        }
        ID_FETCH => fetch_grab(hwnd),
        ID_DOWNLOAD => download_selected(hwnd),
        ID_CANCEL => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        ID_SEL_ALL => {
            {
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    d.unchecked.clear();
                }
            }
            render_list();
        }
        ID_SEL_NONE => {
            {
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    d.unchecked = (0..d.items.len()).collect();
                }
            }
            render_list();
        }
        ID_PROJ_LOAD => {
            let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            let Some(d) = guard.as_ref() else { return };
            let name = get_combo(d, "proj_sel");
            if name.is_empty() {
                return;
            }
            let p = crate::settings_value()
                .get("grab_projects")
                .and_then(|x| x.as_array())
                .and_then(|arr| arr.iter().find(|p| js(p, "name") == name).cloned());
            if let Some(p) = p {
                apply_project(d, &p);
                crate::set_status(1, &format!("Project \"{name}\" loaded"));
            }
        }
        ID_PROJ_SAVE => {
            let p = {
                let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                let Some(d) = guard.as_ref() else { return };
                wizard_state(d)
            };
            if js(&p, "name").is_empty() {
                crate::set_status(1, "Give the project a name first");
                return;
            }
            if js(&p, "start_url").is_empty() {
                crate::set_status(1, "Give the project a start page first");
                return;
            }
            let mut out = crate::settings_value();
            // F3 (audit-233): re-saving a project must carry its
            // last_run over — the wizard state has no such field,
            // and replacing the stored project wholesale wiped
            // it, so the next scheduled run re-downloaded
            // everything the project had already fetched.
            let mut p = p;
            let existing = out
                .get("grab_projects")
                .and_then(|x| x.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|x| js(x, "name").eq_ignore_ascii_case(js(&p, "name")))
                        .cloned()
                });
            if let Some(old) = existing {
                if let Some(lr) = old.get("last_run").cloned() {
                    if let Some(obj) = p.as_object_mut() {
                        obj.insert("last_run".to_string(), lr);
                    }
                }
            }
            let mut list: Vec<serde_json::Value> = out
                .get("grab_projects")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|x| !js(x, "name").eq_ignore_ascii_case(js(&p, "name")))
                .collect();
            list.push(p.clone());
            out["grab_projects"] = serde_json::Value::Array(list);
            match crate::save_settings_value(out) {
                Ok(()) => {
                    let name = js(&p, "name").to_string();
                    {
                        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(d) = guard.as_mut() {
                            d.combo_values.insert("proj_sel".to_string(), project_names());
                            fill_projects(d);
                            set_combo(d, "proj_sel", &name);
                        }
                    }
                    crate::set_status(1, &format!("Grabber project \"{name}\" saved"));
                }
                Err(e) => crate::set_status(1, &format!("Could not save the project: {e}")),
            }
        }
        ID_PROJ_DELETE => {
            let name = {
                let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_ref() {
                    Some(d) => get_combo(d, "proj_sel"),
                    None => return,
                }
            };
            if name.is_empty() {
                return;
            }
            let mut out = crate::settings_value();
            if let Some(arr) = out.get_mut("grab_projects").and_then(|x| x.as_array_mut()) {
                arr.retain(|x| js(x, "name") != name);
            }
            match crate::save_settings_value(out) {
                Ok(()) => {
                    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_mut() {
                        d.combo_values.insert("proj_sel".to_string(), project_names());
                        fill_projects(d);
                    }
                    crate::set_status(1, &format!("Project \"{name}\" deleted"));
                }
                Err(e) => crate::set_status(1, &format!("Could not delete the project: {e}")),
            }
        }
        _ => {
            // Named fields: template / preset combos and the two
            // explore-mode radios.
            let name = {
                let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                guard.as_ref().and_then(|d| d.ids.get(&id).cloned())
            };
            let Some(name) = name else { return };
            match name.as_str() {
                "template" if code == CBN_SELCHANGE_VAL => {
                    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(d) = guard.as_mut() else { return };
                    let t = template_of(&get_combo(d, "template"));
                    if let Some(inc) = t.0 {
                        set_text(d, "incl_exts", &inc.join(", "));
                    }
                    if let Some(exc) = t.1 {
                        set_text(d, "excl_exts", &exc.join(", "));
                    }
                    // a whole-site template sets whole mode; a
                    // depth-limited one switches back to levels
                    set_check(d, "mode_whole", t.3);
                    set_check(d, "mode_levels", !t.3);
                    if let Some(depth) = t.2 {
                        set_text(d, "depth_same", &depth.to_string());
                    }
                }
                "incl_preset" if code == CBN_SELCHANGE_VAL => {
                    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(d) = guard.as_mut() else { return };
                    if let Some(list) = preset_exts(&get_combo(d, "incl_preset")) {
                        set_text(d, "incl_exts", &list.join(", "));
                    }
                }
                "mode_whole" | "mode_levels" => {
                    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_mut() {
                        set_check(d, "mode_whole", name == "mode_whole");
                        set_check(d, "mode_levels", name == "mode_levels");
                    }
                }
                "filter" if code == EN_CHANGE_VAL => {
                    {
                        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(d) = guard.as_mut() {
                            sync_results(d);
                        }
                    }
                    render_list();
                }
                _ => {}
            }
        }
    }
}

unsafe extern "system" fn grab_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            WIN_RAW.store(raw(hwnd), Ordering::SeqCst);
            let cs = l.0 as *const CREATESTRUCTW;
            let prefill = Box::from_raw((*cs).lpCreateParams as *mut Option<String>);
            build(hwnd, *prefill);
            LRESULT(0)
        }
        WM_GB_RESULT => {
            if l.0 != 0 {
                let result = *Box::from_raw(l.0 as *mut Result<serde_json::Value, String>);
                {
                    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_mut() {
                        d.exploring = false;
                    }
                }
                match result {
                    Ok(dto) => {
                        let pages = dto.get("pages_visited").and_then(|x| x.as_u64()).unwrap_or(0);
                        let title = js(&dto, "title").to_string();
                        {
                            let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(d) = guard.as_mut() {
                                d.items = dto
                                    .get("items")
                                    .and_then(|x| x.as_array())
                                    .cloned()
                                    .unwrap_or_default();
                                d.unchecked.clear();
                                d.kinds_off.clear();
                                let n = d.items.len();
                                let start = get_text(d, "url");
                                set_status(
                                    d,
                                    &format!(
                                        "Explored {pages} page{} — found {n} files on \"{}\"",
                                        if pages == 1 { "" } else { "s" },
                                        if title.is_empty() { start.trim() } else { &title }
                                    ),
                                );
                            }
                        }
                        render_kinds();
                        render_list();
                    }
                    Err(e) => {
                        let guard = DLG.lock().unwrap_or_else(|e2| e2.into_inner());
                        if let Some(d) = guard.as_ref() {
                            set_status(d, &e);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_GB_DOWNLOADED => {
            if l.0 != 0 {
                let text = *Box::from_raw(l.0 as *mut String);
                if text.is_empty() {
                    crate::post_refresh();
                    let _ = DestroyWindow(hwnd);
                } else {
                    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_ref() {
                        set_status(d, &text);
                    }
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
            if !hdr.is_null() && (*hdr).code as i32 == LVN_ITEMCHANGED_CODE {
                if (*hdr).idFrom == ID_KINDS as usize {
                    render_list();
                } else if (*hdr).idFrom == ID_RESULTS as usize {
                    {
                        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(d) = guard.as_mut() {
                            sync_results(d);
                        }
                    }
                    update_count();
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
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
