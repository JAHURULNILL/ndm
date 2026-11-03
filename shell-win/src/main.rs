#![windows_subsystem = "windows"]
//! ndm-shell-win — NDM's native Windows shell (Phase 2: functional
//! wiring).
//!
//! Raw Win32 via the `windows` crate behind the in-house `winui`
//! helper layer (NATIVE-PLAN §2 verdict). The shell owns only
//! window/process plumbing: WinMain bootstrap of the real
//! ndm-core (mirroring the Tauri shell's setup() order), the
//! ListView/TreeView main window, command routing into core, the
//! single-instance guard (ported — same loopback protocol as the
//! Tauri shell, so the two shells hand off to each other), and
//! WM_DROPFILES drag-drop into core's incoming path. All download
//! behaviour lives in ndm-core.
//!
//! Remaining Phase-3 surface (named, never faked): the video picker,
//! the Grabber wizard, the Options/Scheduler settings dialogs
//! (their menu items open an honestly-labelled under-construction
//! dialog; the scheduler/queue command BODIES are wired), the
//! basket window, and Add Batch /
//! Columns / Customize Toolbar / Keyboard Shortcuts dialogs.
//!
//! Runtime on a real Windows is UNVERIFIED from this machine —
//! the gates are cargo check + link for x86_64-pc-windows-gnu and
//! the PE import audit, plus faithful mirroring of the Tauri
//! shell's proven logic in src-tauri/src/main.rs.

mod columns_dialog;
mod basket;
mod grabber_dialog;
mod toolbar_dialog;
mod options_dialog;
mod prefs;
mod progress_dialog;
mod scheduler_dialog;
mod video_dialog;
mod sink;
mod update_dialog;
mod winui;

use sink::{AppEvent, AppState, Ctx, EventSink, ProgressView};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::implement;
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, LPARAM, LRESULT,
    POINT, WPARAM,
};
use windows::Win32::System::Com::{
    CoTaskMemFree, IDataObject, IDataObject_Impl, DVASPECT_CONTENT, FORMATETC, STGMEDIUM,
    STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, DROPEFFECT, DROPEFFECT_COPY,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Controls::{
    NMHDR, NMITEMACTIVATE, NMLISTVIEW, NMTREEVIEWW, LVN_BEGINDRAG, NM_CUSTOMDRAW, NM_DBLCLK,
    NM_RCLICK, TVN_SELCHANGEDW,
};
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, DROPFILES, FOLDERID_RoamingAppData, HDROP,
    SHGetKnownFolderPath, KF_FLAG_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, MessageBoxW, MoveWindow, PostMessageW, PostQuitMessage,
    SetForegroundWindow, ShowWindow, TrackPopupMenu, TranslateMessage, ASFW_ANY,
    MB_OK, MINMAXINFO, MSG, SW_RESTORE, SW_SHOW,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_DROPFILES, WM_GETMINMAXINFO, WM_NOTIFY, WM_SIZE,
};
use winui::listview::{ColumnSpec, ListView};
use winui::menu::MenuItem;
use winui::statusbar::StatusBar;
use winui::toolbar::{Toolbar, ToolbarItem};
use winui::treeview::TreeView;

// ----------------------------------------------------------------
// Control ids
// ----------------------------------------------------------------
const ID_LIST: i32 = 1001;
const ID_TOOLBAR: i32 = 1002;
const ID_STATUS: i32 = 1003;
const ID_TREE: i32 = 1004;

// Toolbar command ids — the app's real button set and order, taken
// from ui/index.html's toolbar (Add URL, Resume, Stop, Stop All,
// separator, Delete, Delete Completed, Options, Scheduler,
// Start Queue, Stop Queue, Grabber).
const TB_ADD_URL: i32 = 2001;
const TB_RESUME: i32 = 2002;
const TB_STOP: i32 = 2003;
const TB_STOP_ALL: i32 = 2004;
const TB_DELETE: i32 = 2005;
const TB_DELETE_COMPLETED: i32 = 2006;
const TB_OPTIONS: i32 = 2007;
const TB_SCHEDULER: i32 = 2008;
const TB_START_QUEUE: i32 = 2009;
const TB_STOP_QUEUE: i32 = 2010;
const TB_GRABBER: i32 = 2011;

// Menu command ids (menubar from ui/index.html).
const M_ADD: i32 = 3001;
const M_VIDEO: i32 = 3002;
const M_BATCH: i32 = 3003;
const M_IMPORT: i32 = 3004;
const M_EXPORT: i32 = 3005;
const M_QUIT: i32 = 3006;
const M_START_ALL: i32 = 3007;
const M_PAUSE_ALL: i32 = 3008;
const M_STOP_ALL: i32 = 3009;
const M_SELECT_ALL: i32 = 3010;
const M_DELETE: i32 = 3011;
const M_DELETE_DONE: i32 = 3012;
const M_DELETE_ALL: i32 = 3013;
const M_GRABBER: i32 = 3014;
const M_SCHEDULER: i32 = 3015;
const M_OPTIONS: i32 = 3016;
const M_COLUMNS: i32 = 3017;
const M_TOOLBAR_CUSTOMIZE: i32 = 3018;
const M_REFRESH: i32 = 3019;
const M_UPDATE: i32 = 3020;
const M_SHORTCUTS: i32 = 3021;
const M_ABOUT: i32 = 3022;
const M_BASKET: i32 = 3023;

// Row context-menu command ids (share on_command's table).
const CTX_RESUME: i32 = 4001;
const CTX_STOP: i32 = 4002;
const CTX_REFRESH: i32 = 4006;
const CTX_SHOW_PROGRESS: i32 = 4007;
const CTX_OPEN_FILE: i32 = 4003;
const CTX_OPEN_FOLDER: i32 = 4004;
const CTX_DELETE: i32 = 4005;
const CTX_QUEUE_BASE: i32 = 4100; // + queue index (see QUEUE_MENU)

// Private window messages for the EventSink bridge + worker posts.
const WM_APP_EVENT: u32 = WM_APP + 1; // LPARAM = Box<AppEvent> pointer
const WM_APP_RAISE: u32 = WM_APP + 2; // raise_main_window()
const WM_APP_EXIT: u32 = WM_APP + 3; // exit_app()
const WM_APP_RESTORED: u32 = WM_APP + 4; // core: state restored -> show
const WM_APP_STATUS: u32 = WM_APP + 5; // LPARAM = Box<String> status text
const WM_APP_REFRESH: u32 = WM_APP + 6; // re-read list_downloads
const WM_APP_DUP: u32 = WM_APP + 7; // LPARAM = Box<DupAdd> duplicate-add confirm
const WM_APP_UPDATE_OFFER: u32 = WM_APP + 8; // LPARAM = Box<Value> update offer
const WM_APP_BATCH: u32 = WM_APP + 9; // LPARAM = Box<BatchReport> batch-add outcome

fn post_offer(v: serde_json::Value) {
    let raw = MAIN_RAW.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let lparam = LPARAM(Box::into_raw(Box::new(v)) as isize);
    unsafe {
        if PostMessageW(Some(hwnd), WM_APP_UPDATE_OFFER, WPARAM(0), lparam).is_err() {
            drop(Box::from_raw(lparam.0 as *mut serde_json::Value));
        }
    }
}

/// A duplicate-add awaiting the user's "download again?" answer
/// (app.js:2299): everything the force retry needs, carried from
/// the add worker to the UI thread.
struct DupAdd {
    url: String,
    dir: String,
    category: Option<String>,
    name: Option<String>,
    later: bool,
    total: Option<u64>,
    dir_explicit: bool,
}

fn post_dup(dup: DupAdd) {
    let raw = MAIN_RAW.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let lparam = LPARAM(Box::into_raw(Box::new(dup)) as isize);
    unsafe {
        if PostMessageW(Some(hwnd), WM_APP_DUP, WPARAM(0), lparam).is_err() {
            drop(Box::from_raw(lparam.0 as *mut DupAdd));
        }
    }
}

/// The batch-add outcome, carried from the add worker to the UI
/// thread (see WM_APP_BATCH): counts plus, on partial failure,
/// every failed URL with its reason and the textarea lines that
/// still need attention.
struct BatchReport {
    ok: usize,
    url_count: usize,
    total_lines: usize,
    failed: Vec<(String, String)>, // (url, reason)
    invalid: usize,
    remaining: String,
    dir: String,
}

fn post_batch(report: BatchReport) {
    let raw = MAIN_RAW.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let lparam = LPARAM(Box::into_raw(Box::new(report)) as isize);
    unsafe {
        if PostMessageW(Some(hwnd), WM_APP_BATCH, WPARAM(0), lparam).is_err() {
            drop(Box::from_raw(lparam.0 as *mut BatchReport));
        }
    }
}

const TREE_WIDTH: i32 = 220;

// ----------------------------------------------------------------
// UI-thread state. Win32 controls live on the UI thread only; the
// raw handles are kept in statics exactly as in the spike.
// ----------------------------------------------------------------
static mut H_MAIN: HWND = HWND(std::ptr::null_mut());
static mut H_TOOLBAR: HWND = HWND(std::ptr::null_mut());
static mut H_TREE: HWND = HWND(std::ptr::null_mut());
static mut H_LIST: HWND = HWND(std::ptr::null_mut());
static mut H_STATUS: HWND = HWND(std::ptr::null_mut());

/// The core handle every command forwards into (set in WinMain
/// before the window exists; cloned out per use).
static CTX: Mutex<Option<Ctx>> = Mutex::new(None);

fn ctx() -> Option<Ctx> {
    CTX.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Every known download (the last ProgressView per id, in core's
/// list order). Progress events update this cache; the ListView
/// shows the filter's view of it.
static CACHE: Mutex<Vec<ProgressView>> = Mutex::new(Vec::new());
/// Entry ids currently displayed, parallel to ListView rows.
static DISPLAY: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Per-display-row "has a block map" flags, parallel to DISPLAY —
/// the custom-draw snapshot is a clone of this, never a rescan.
static BLOCK_FLAGS: Mutex<Vec<bool>> = Mutex::new(Vec::new());
/// The active tree filter, in the web UI's currentCat vocabulary:
/// "All Downloads" | a category | "Unfinished" | "Finished" |
/// "Grabber" | "queue:<name>". Default mirrors ui/app.js line 33.
static FILTER: Mutex<String> = Mutex::new(String::new());
/// Tree node tags: NMTREEVIEWW lParam indexes into this table.
static TREE_TAGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Signature of the last tree build (categories + queues); the
/// tree is rebuilt only when it changes.
static TREE_SIG: Mutex<String> = Mutex::new(String::new());
/// Suppresses filter changes while the tree is being rebuilt.
static TREE_BUILDING: AtomicBool = AtomicBool::new(false);
/// Queue names behind the context menu's Add-to-queue ids.
static QUEUE_MENU: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Main-window handle as an atomic, for worker-thread posts.
static MAIN_RAW: AtomicIsize = AtomicIsize::new(0);

fn list_view() -> ListView {
    ListView { hwnd: unsafe { H_LIST } }
}
fn status_bar() -> StatusBar {
    StatusBar { hwnd: unsafe { H_STATUS } }
}
fn set_status(part: usize, text: &str) {
    status_bar().set_text(part, text);
}
fn toolbar() -> Toolbar {
    Toolbar { hwnd: unsafe { H_TOOLBAR } }
}

/// Post a private message to the main window from any thread;
/// `boxed` (if any) is reclaimed on failure so nothing leaks.
fn post_main(msg: u32, boxed: Option<Box<String>>) {
    let raw = MAIN_RAW.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let lparam = match boxed {
        Some(b) => LPARAM(Box::into_raw(b) as isize),
        None => LPARAM(0),
    };
    unsafe {
        if PostMessageW(Some(hwnd), msg, WPARAM(0), lparam).is_err() && lparam.0 != 0 {
            drop(Box::from_raw(lparam.0 as *mut String));
        }
    }
}
fn post_status(text: String) {
    post_main(WM_APP_STATUS, Some(Box::new(text)));
}
fn post_refresh() {
    post_main(WM_APP_REFRESH, None);
}

// ----------------------------------------------------------------
// EventSink implementation: any thread -> PostMessageW -> UI thread
// ----------------------------------------------------------------
pub struct WinSink {
    hwnd: AtomicIsize,
}

impl WinSink {
    fn new() -> Self {
        Self { hwnd: AtomicIsize::new(0) }
    }
    fn attach(&self, hwnd: HWND) {
        self.hwnd.store(hwnd.0 as isize, Ordering::SeqCst);
        MAIN_RAW.store(hwnd.0 as isize, Ordering::SeqCst);
    }
    fn target(&self) -> Option<HWND> {
        let raw = self.hwnd.load(Ordering::SeqCst);
        if raw == 0 {
            None
        } else {
            Some(HWND(raw as *mut core::ffi::c_void))
        }
    }
    fn post(&self, msg: u32, boxed: Option<Box<AppEvent>>) {
        let Some(hwnd) = self.target() else {
            return; // window not created yet / already gone: drop
        };
        let lparam = match boxed {
            Some(b) => LPARAM(Box::into_raw(b) as isize),
            None => LPARAM(0),
        };
        unsafe {
            if PostMessageW(Some(hwnd), msg, WPARAM(0), lparam).is_err() {
                // Post failed (window dying): reclaim the box so the
                // event is dropped, not leaked.
                if lparam.0 != 0 {
                    drop(Box::from_raw(lparam.0 as *mut AppEvent));
                }
            }
        }
    }
}

impl EventSink for WinSink {
    fn emit(&self, event: AppEvent) {
        self.post(WM_APP_EVENT, Some(Box::new(event)));
    }
    fn raise_main_window(&self) {
        self.post(WM_APP_RAISE, None);
    }
    fn exit_app(&self) {
        self.post(WM_APP_EXIT, None);
    }
}

// ----------------------------------------------------------------
// Formatting helpers for the row update (UI-thread only)
// ----------------------------------------------------------------
fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n == 0 {
        return "—".to_string();
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn fmt_rate(bps: f64) -> String {
    if bps <= 0.0 {
        return "—".to_string();
    }
    format!("{}/s", fmt_bytes(bps as u64))
}

fn fmt_eta(p: &ProgressView) -> String {
    if p.speed_bps <= 0.0 || p.total <= p.done_bytes {
        return "—".to_string();
    }
    let secs = ((p.total - p.done_bytes) as f64 / p.speed_bps).ceil() as u64;
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// One column's cell for a payload (ui/app.js cellHtml's column
/// ids; the text shapes are this shell's wave-1 row model).
fn cell_for(p: &ProgressView, id: &str) -> String {
    match id {
        "name" => p.name.clone(),
        "q" => p.queue.clone(),
        "size" => fmt_bytes(p.total),
        "status" => {
            if p.detail.is_empty() {
                p.status.clone()
            } else {
                format!("{} — {}", p.status, p.detail)
            }
        }
        "timeleft" => fmt_eta(p),
        "rate" => fmt_rate(p.speed_bps),
        "lasttry" => p.last_try.clone(),
        "desc" => p.description.clone(),
        "added" => p.added.clone(),
        "saveto" => p.save_to.clone(),
        _ => String::new(),
    }
}

/// The ListView cells for one payload, in the visible-column
/// order of the current column configuration.
fn row_cells(p: &ProgressView) -> Vec<String> {
    visible_columns().iter().map(|c| cell_for(p, &c.id)).collect()
}

// ----------------------------------------------------------------
// List columns — ui/app.js COLUMN_DEFS + the colmodal storage
// contract: the config (order / visibility / width per column id)
// persists under the web UI's own localStorage key and shape
// (`ndm-columns-v2`), mirrored into prefs (ui-prefs.json) since a
// native window has no localStorage. File Name is the anchor:
// it cannot be hidden (loadColumnCfg forces it visible).
// ----------------------------------------------------------------
pub const COLUMN_DEFS: [(&str, &str, i32, bool); 10] = [
    ("name", "File Name", 0, false),
    ("q", "Q", 30, false),
    ("size", "Size", 84, false),
    ("status", "Status", 92, false),
    ("timeleft", "Time left", 96, false),
    ("rate", "Transfer rate", 96, false),
    ("lasttry", "Last Try Date", 138, false),
    ("desc", "Description", 150, false),
    ("added", "Date Added", 138, true),
    ("saveto", "Save To", 240, true),
];

#[derive(Clone, Debug)]
pub struct ColumnCfg {
    pub id: String,
    pub visible: bool,
    pub width: i32,
}

pub fn default_column_cfg() -> Vec<ColumnCfg> {
    COLUMN_DEFS
        .iter()
        .map(|(id, _, w, off)| ColumnCfg { id: id.to_string(), visible: !off, width: *w })
        .collect()
}

fn load_column_cfg() -> Vec<ColumnCfg> {
    let mut out: Vec<ColumnCfg> = Vec::new();
    if let Some(arr) = prefs::get("ndm-columns-v2").and_then(|v| v.as_array().cloned()) {
        for r in &arr {
            let Some(id) = r.get("id").and_then(|x| x.as_str()) else { continue };
            if !COLUMN_DEFS.iter().any(|(cid, _, _, _)| cid == &id) || out.iter().any(|c| c.id == id) {
                continue;
            }
            out.push(ColumnCfg {
                id: id.to_string(),
                visible: r.get("visible").and_then(|x| x.as_bool()).unwrap_or(true),
                width: r.get("width").and_then(|x| x.as_i64()).unwrap_or(0).clamp(0, 800) as i32,
            });
        }
    }
    for c in default_column_cfg() {
        if !out.iter().any(|o| o.id == c.id) {
            out.push(c);
        }
    }
    if let Some(nm) = out.iter_mut().find(|c| c.id == "name") {
        nm.visible = true;
    }
    out
}

static COLUMN_CFG: Mutex<Vec<ColumnCfg>> = Mutex::new(Vec::new());
/// Columns currently inserted in the main ListView.
static COLUMN_COUNT: Mutex<i32> = Mutex::new(0);
/// Visible index of the Status column (the block map's subitem),
/// or -1 while Status is hidden.
static BLOCK_SUBITEM: Mutex<i32> = Mutex::new(3);

pub fn column_cfg() -> Vec<ColumnCfg> {
    let mut guard = COLUMN_CFG.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_empty() {
        *guard = load_column_cfg();
    }
    guard.clone()
}

pub fn visible_columns() -> Vec<ColumnCfg> {
    column_cfg().into_iter().filter(|c| c.visible).collect()
}

fn column_width(c: &ColumnCfg) -> i32 {
    if c.width > 0 {
        c.width
    } else if c.id == "name" {
        340 // app.js defW 0 = flex; the native flex width
    } else {
        COLUMN_DEFS
            .iter()
            .find(|(id, _, _, _)| *id == c.id)
            .map(|(_, _, w, _)| *w)
            .unwrap_or(80)
    }
}

/// Persist + apply a new column configuration (the colmodal OK).
pub fn set_column_cfg(cfg: Vec<ColumnCfg>) {
    let arr: Vec<serde_json::Value> = cfg
        .iter()
        .map(|c| serde_json::json!({ "id": c.id, "visible": c.visible, "width": c.width }))
        .collect();
    prefs::set("ndm-columns-v2", serde_json::Value::Array(arr));
    *COLUMN_CFG.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    apply_columns();
}

// ----------------------------------------------------------------
// Toolbar customization — ui/app.js TOOLBAR_DEFS + the tbmodal
// storage contract (`ndm-toolbar-v1`: {hidden: [DOM ids], size}),
// mirrored into prefs like the columns config. The defs map the
// web UI's DOM ids onto this shell's TB_* command ids.
// ----------------------------------------------------------------
pub const TOOLBAR_DEFS: [(i32, &str, &str); 11] = [
    (TB_ADD_URL, "btnAdd", "Add URL"),
    (TB_RESUME, "btnResume", "Resume"),
    (TB_STOP, "btnStop", "Stop"),
    (TB_STOP_ALL, "btnStopAll", "Stop All"),
    (TB_DELETE, "btnDelete", "Delete"),
    (TB_DELETE_COMPLETED, "btnDelDone", "Delete Completed"),
    (TB_OPTIONS, "btnOptions", "Options"),
    (TB_SCHEDULER, "btnScheduler", "Scheduler"),
    (TB_START_QUEUE, "btnStartQueue", "Start Queue"),
    (TB_STOP_QUEUE, "btnStopQueue", "Stop Queue"),
    (TB_GRABBER, "btnGrabber", "Grabber"),
];

#[derive(Clone, Debug)]
pub struct ToolbarCfg {
    pub hidden: Vec<String>,
    pub small: bool,
}

static TOOLBAR_CFG: Mutex<Option<ToolbarCfg>> = Mutex::new(None);

pub fn toolbar_cfg() -> ToolbarCfg {
    let mut guard = TOOLBAR_CFG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = guard.as_ref() {
        return c.clone();
    }
    let mut cfg = ToolbarCfg { hidden: Vec::new(), small: false };
    if let Some(v) = prefs::get("ndm-toolbar-v1") {
        if let Some(arr) = v.get("hidden").and_then(|x| x.as_array()) {
            cfg.hidden = arr
                .iter()
                .filter_map(|x| x.as_str())
                .filter(|id| TOOLBAR_DEFS.iter().any(|(_, dom, _)| dom == id))
                .map(|s| s.to_string())
                .collect();
        }
        cfg.small = v.get("size").and_then(|x| x.as_str()) == Some("small");
    }
    *guard = Some(cfg.clone());
    cfg
}

/// Persist + apply a toolbar configuration (the tbmodal OK).
pub fn set_toolbar_cfg(cfg: ToolbarCfg) {
    prefs::set(
        "ndm-toolbar-v1",
        serde_json::json!({
            "hidden": cfg.hidden,
            "size": if cfg.small { "small" } else { "large" },
        }),
    );
    *TOOLBAR_CFG.lock().unwrap_or_else(|e| e.into_inner()) = Some(cfg);
    apply_toolbar();
}

/// Push the configuration onto the main toolbar (hidden states +
/// button size) and relayout. Called by set_toolbar_cfg and once
/// at window creation.
pub fn apply_toolbar() {
    let cfg = toolbar_cfg();
    let tb = toolbar();
    for (id, dom, _) in &TOOLBAR_DEFS {
        tb.set_hidden(*id, cfg.hidden.iter().any(|x| x == dom));
    }
    // The web UI's small bar shrinks its buttons; the native
    // equivalent is an explicit button height (large restores
    // the standard height the bar was created with).
    tb.set_button_size(0, if cfg.small { 20 } else { 30 });
    relayout_main();
}

/// Re-run the main window's child layout at its current client
/// size (the WM_SIZE body's second half, factored out so a
/// toolbar change can relayout without a resize).
fn relayout_main() {
    let hwnd = unsafe { H_MAIN };
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_ok() {
            layout_children(rc.right - rc.left, rc.bottom - rc.top);
        }
    }
}

fn layout_children(cx: i32, cy: i32) {
    let tb = Toolbar { hwnd: unsafe { H_TOOLBAR } };
    let tb_h = tb.autosize_height();
    let sb_h = status_bar().on_parent_size();
    let body_h = (cy - tb_h - sb_h).max(0);
    unsafe {
        let _ = MoveWindow(H_TREE, 0, tb_h, TREE_WIDTH, body_h, true);
        let _ = MoveWindow(H_LIST, TREE_WIDTH, tb_h, (cx - TREE_WIDTH).max(0), body_h, true);
    }
}

/// Rebuild the main ListView's columns from the configuration
/// and re-render the rows under it (the block map follows the
/// Status column to its new subitem, or disarms while hidden).
pub fn apply_columns() {
    let vis = visible_columns();
    let lv = list_view();
    let old = *COLUMN_COUNT.lock().unwrap_or_else(|e| e.into_inner());
    if old > 0 {
        lv.delete_columns(old);
    }
    for (i, c) in vis.iter().enumerate() {
        let label = COLUMN_DEFS
            .iter()
            .find(|(id, _, _, _)| *id == c.id)
            .map(|(_, l, _, _)| *l)
            .unwrap_or("");
        lv.insert_column(i as i32, &ColumnSpec { label, width: column_width(c) });
    }
    *COLUMN_COUNT.lock().unwrap_or_else(|e| e.into_inner()) = vis.len() as i32;
    *BLOCK_SUBITEM.lock().unwrap_or_else(|e| e.into_inner()) =
        vis.iter().position(|c| c.id == "status").map(|i| i as i32).unwrap_or(-1);
    rebuild_display();
}

// ----------------------------------------------------------------
// Filtering — ui/app.js visibleItems() semantics (app.js:422-427):
// Finished → status == "finished"; Unfinished → status !=
// "finished"; "queue:<name>" → entry.queue == name; "Grabber"
// lands in the category branch there (data-cat="Grabber"), i.e.
// category == "Grabber"; "All Downloads" → everything; anything
// else → category equality.
// ----------------------------------------------------------------
/// The settings' queue names (Settings' fields are core-private;
/// its JSON wire shape is the shell's reading — same seam as
/// ProgressView).
fn queue_names(s: &ndm_core::Settings) -> Vec<String> {
    let v = serde_json::to_value(s).unwrap_or(serde_json::Value::Null);
    v.get("queues")
        .and_then(|q| q.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|q| q.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// The settings object as JSON — the shell's read side of the
/// Settings seam (fields are core-private; the wire shape is the
/// contract, exactly as for ProgressView / queue_names).
fn settings_value() -> serde_json::Value {
    let Some(c) = ctx() else {
        return serde_json::Value::Null;
    };
    serde_json::to_value(ndm_core::get_settings(c)).unwrap_or(serde_json::Value::Null)
}

/// Write a whole settings object back through core's
/// `set_settings` (the Options/Scheduler/Grabber save path):
/// deserialize through the same JSON seam so sanitize() and the
/// engine side effects (global limit, proxy, queue adoption) all
/// run in core, never in the shell.
fn save_settings_value(v: serde_json::Value) -> Result<(), String> {
    let Some(c) = ctx() else {
        return Err("core is not ready".to_string());
    };
    let s: ndm_core::Settings = serde_json::from_value(v).map_err(|e| e.to_string())?;
    ndm_core::set_settings(c, s)
}

/// A snapshot of the row cache for dialogs that list categories
/// or queue files (Scheduler / Options category manager).
fn cache_snapshot() -> Vec<ProgressView> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn current_filter() -> String {
    let f = FILTER.lock().unwrap_or_else(|e| e.into_inner());
    if f.is_empty() {
        "All Downloads".to_string()
    } else {
        f.clone()
    }
}

fn matches_filter(p: &ProgressView, filter: &str) -> bool {
    match filter {
        "All Downloads" => true,
        "Finished" => p.status == "finished",
        "Unfinished" => p.status != "finished",
        _ => {
            if let Some(q) = filter.strip_prefix("queue:") {
                p.queue == q
            } else {
                p.category == filter
            }
        }
    }
}

/// Rebuild the ListView from CACHE under the active filter
/// (selection is not preserved across a rebuild — same trade the
/// web UI's full render() makes).
fn rebuild_display() {
    let filter = current_filter();
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let lv = list_view();
    lv.clear();
    let mut display = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
    display.clear();
    let mut flags = BLOCK_FLAGS.lock().unwrap_or_else(|e| e.into_inner());
    flags.clear();
    let mut row = 0;
    for p in cache.iter() {
        if matches_filter(p, &filter) {
            lv.insert_row(row, &row_cells(p));
            display.push(p.id.clone());
            flags.push(!p.blocks.is_empty());
            row += 1;
        }
    }
    drop(flags);
    let total = cache.len();
    drop(cache);
    drop(display);
    if filter == "All Downloads" {
        set_status(0, &format!("{total} downloads"));
    } else {
        set_status(0, &format!("{row} of {total} downloads"));
    }
    refresh_command_states();
}

/// Open (or, for an explicit user gesture, raise) the
/// per-download progress dialog for an entry — the one creation
/// path behind double-click, the context menu, core's
/// ProgressDialogReveal and the start-transition auto-open.
fn open_progress_dialog(parent: HWND, id: &str, minimized: bool, activate: bool) {
    if activate {
        progress_dialog::note_explicit_open(id);
    } else if progress_dialog::user_closed(id) {
        // the user closed this dialog; a background reveal
        // (retry, queue promotion) must not resurrect it (m3)
        return;
    }
    if activate && progress_dialog::raise_existing(id) {
        return;
    }
    if progress_dialog::exists(id) {
        return;
    }
    let view = {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.iter().find(|v| v.id == id).cloned()
    }
    .or_else(|| {
        let c = ctx()?;
        ndm_core::list_downloads(c)
            .iter()
            .map(ProgressView::from)
            .find(|v| v.id == id)
    });
    let Some(view) = view else {
        set_status(1, "That download is no longer in the list");
        return;
    };
    let Some(c) = ctx() else { return };
    let settings = ndm_core::get_settings(c);
    let sj = serde_json::to_value(&settings).unwrap_or(serde_json::Value::Null);
    let global_limit = sj
        .get("speed_limit_bps")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let show_complete_global = sj
        .get("show_complete_dialog")
        .and_then(|x| x.as_bool())
        .unwrap_or(true);
    let queues = queue_names(&settings);
    progress_dialog::create(
        parent,
        progress_dialog::CreateArgs { view, queues, global_limit, show_complete_global },
        minimized,
        activate,
    );
}

/// The current progress_dialog_view setting ("show" default).
fn progress_dialog_setting() -> String {
    let Some(c) = ctx() else {
        return "show".to_string();
    };
    let settings = ndm_core::get_settings(c);
    serde_json::to_value(&settings)
        .ok()
        .and_then(|j| {
            j.get("progress_dialog_view")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "show".to_string())
}

// ----------------------------------------------------------------
// Download Complete dialog (ui/app.js cmodal + showNextComplete /
// closeComplete). Core emits DownloadComplete only when its
// completion_show_dialog rule passes (per-entry override, then
// the entry's show_complete action, then the global setting), so
// the shell does not re-gate on the global flag. One dialog at a
// time: finishes landing while one is open are not announced
// again, rapid successive finishes fold into ONE summary, and
// the dialog yields while any other modal is up (the pump is
// retried when that modal's command returns).
// ----------------------------------------------------------------
static COMPLETE_QUEUE: Mutex<Vec<serde_json::Value>> = Mutex::new(Vec::new());
static COMPLETE_SHOWING: AtomicBool = AtomicBool::new(false);

/// The dialogs' 2-decimal size shape (app.js fmtSize).
fn fmt_size_dlg(n: u64) -> String {
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

/// "Don't show this dialog again" writes the SAME global setting
/// app.js's closeComplete saves (Settings.show_complete_dialog,
/// through set_settings — the fields are core-private, so the
/// shell round-trips the settings JSON, the queue_names seam).
fn set_show_complete_global(on: bool) -> Result<(), String> {
    let Some(c) = ctx() else { return Ok(()) };
    let mut v = serde_json::to_value(ndm_core::get_settings(c.clone()))
        .map_err(|e| e.to_string())?;
    v["show_complete_dialog"] = serde_json::json!(on);
    let s: ndm_core::Settings =
        serde_json::from_value(v).map_err(|e| e.to_string())?;
    ndm_core::set_settings(c, s)
}

fn pump_complete(hwnd: HWND) {
    if COMPLETE_SHOWING.load(Ordering::SeqCst) || winui::dialog::any_modal_open() {
        return;
    }
    // Drop announcements whose entry has since been deleted
    // (app.js checks at show time, not queue time).
    let batch: Vec<serde_json::Value> = {
        let mut q = COMPLETE_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        q.retain(|p| {
            p.get("id")
                .and_then(|x| x.as_str())
                .map(|id| cache.iter().any(|v| v.id == id))
                .unwrap_or(true)
        });
        if q.is_empty() {
            return;
        }
        q.drain(..).collect()
    };
    COMPLETE_SHOWING.store(true, Ordering::SeqCst);
    if batch.len() > 1 {
        // Summary (app.js F-26): no single file to open.
        let names: Vec<String> = batch
            .iter()
            .map(|p| p.get("name").and_then(|x| x.as_str()).unwrap_or("Download").to_string())
            .collect();
        let mut listing = names[..names.len().min(12)].join(", ");
        if names.len() > 12 {
            listing += &format!(", …and {} more", names.len() - 12);
        }
        let total: u64 = batch.iter().filter_map(|p| p.get("total").and_then(|x| x.as_u64())).sum();
        let head: &'static str =
            Box::leak(format!("{} downloads completed", batch.len()).into_boxed_str());
        let list_line: &'static str = Box::leak(listing.into_boxed_str());
        let size_line: &'static str = Box::leak(if total > 0 {
            format!("Downloaded {} in total", fmt_size_dlg(total))
        } else {
            String::new()
        }
        .into_boxed_str());
        let spec = winui::dialog::DialogSpec {
            title: "Download complete",
            width: 460,
            height: 0,
            fields: vec![
                winui::dialog::FieldSpec::label(head),
                winui::dialog::FieldSpec::label(list_line),
                winui::dialog::FieldSpec::label(size_line),
                winui::dialog::FieldSpec::checkbox("Don't show this dialog again", false),
            ],
            ok_label: "Close",
            cancel_label: None,
            extra_label: None,
        };
        let outcome = winui::dialog::show_modal(hwnd, &spec);
        if matches!(outcome.values.get(3), Some(winui::dialog::FieldValue::Checked(true))) {
            if let Err(e) = set_show_complete_global(false) {
                set_status(1, &format!("Could not save that choice: {e}"));
            }
        }
    } else {
        let p = &batch[0];
        let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("Download");
        let url = p.get("url").and_then(|x| x.as_str()).unwrap_or("");
        let path = p.get("path").and_then(|x| x.as_str()).unwrap_or("");
        let total = p.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
        let name_line: &'static str = Box::leak(name.to_string().into_boxed_str());
        let addr_line: &'static str =
            Box::leak(if url.is_empty() { String::new() } else { format!("Address: {url}") }.into_boxed_str());
        let path_line: &'static str = Box::leak(format!("Saved to: {path}").into_boxed_str());
        let size_line: &'static str = Box::leak(if total > 0 {
            format!("Downloaded {} ({} bytes)", fmt_size_dlg(total), total)
        } else {
            String::new()
        }
        .into_boxed_str());
        let spec = winui::dialog::DialogSpec {
            title: "Download complete",
            width: 460,
            height: 0,
            fields: vec![
                winui::dialog::FieldSpec::label(name_line),
                winui::dialog::FieldSpec::label(addr_line),
                winui::dialog::FieldSpec::label(path_line),
                winui::dialog::FieldSpec::label(size_line),
                winui::dialog::FieldSpec::checkbox("Don't show this dialog again", false),
            ],
            ok_label: "Open",
            cancel_label: Some("Close"),
            extra_label: Some("Open folder"),
        };
        let outcome = winui::dialog::show_modal(hwnd, &spec);
        if matches!(outcome.values.get(4), Some(winui::dialog::FieldValue::Checked(true))) {
            if let Err(e) = set_show_complete_global(false) {
                set_status(1, &format!("Could not save that choice: {e}"));
            }
        }
        let folder = if outcome.extra {
            Some(true)
        } else if outcome.accepted {
            Some(false)
        } else {
            None
        };
        if let (Some(folder), Some(c)) = (folder, ctx()) {
            if !id.is_empty() {
                if let Err(e) = ndm_core::open_path(c, id, folder) {
                    set_status(1, &format!("Could not open it: {e}"));
                }
            }
        }
    }
    COMPLETE_SHOWING.store(false, Ordering::SeqCst);
}

/// Spec §1.3/G7 for the native shells: a start transition — an
/// entry entering starting/downloading from anything else,
/// including a brand-new entry, which is how browser takeovers
/// arrive (native takes takeovers too) — opens the entry's
/// progress dialog per the progress_dialog_view setting. An
/// already-open dialog is left alone: a retry transition must
/// not steal focus back.
fn maybe_auto_open_progress(p: &ProgressView, old_status: Option<&str>) {
    if !matches!(p.status.as_str(), "starting" | "downloading") {
        return;
    }
    if matches!(old_status, Some("starting") | Some("downloading")) {
        return;
    }
    if progress_dialog::exists(&p.id) {
        return;
    }
    let parent = unsafe { H_MAIN };
    match progress_dialog_setting().as_str() {
        "never" => {}
        "minimized" => open_progress_dialog(parent, &p.id, true, false),
        _ => open_progress_dialog(parent, &p.id, false, false),
    }
}

/// Insert or update the row for a progress payload, honouring the
/// active filter (the native replacement for the web UI's
/// render() row patch).
fn upsert_progress_row(p: &ProgressView) {
    let filter = current_filter();
    // Toolbar state depends only on statuses + selection: a pure
    // progress tick (same status) must not recompute it.
    let (mut state_dirty, old_status) = {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = cache.iter_mut().find(|v| v.id == p.id) {
            let old = slot.status.clone();
            let changed = old != p.status;
            *slot = p.clone();
            (changed, Some(old))
        } else {
            cache.push(p.clone());
            (true, None)
        }
    };
    let lv = list_view();
    let mut display = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
    let mut flags = BLOCK_FLAGS.lock().unwrap_or_else(|e| e.into_inner());
    let in_display = display.iter().position(|id| id == &p.id);
    if matches_filter(p, &filter) {
        match in_display {
            Some(row) => {
                lv.update_row(row as i32, &row_cells(p));
                if let Some(f) = flags.get_mut(row) {
                    *f = !p.blocks.is_empty();
                }
            }
            None => {
                // display order follows cache order: count the
                // matching cache entries ahead of this one
                let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                let pos = cache
                    .iter()
                    .take_while(|v| v.id != p.id)
                    .filter(|v| matches_filter(v, &filter))
                    .count();
                lv.insert_row(pos as i32, &row_cells(p));
                display.insert(pos, p.id.clone());
                flags.insert(pos, !p.blocks.is_empty());
                state_dirty = true;
            }
        }
    } else if let Some(row) = in_display {
        lv.delete_row(row as i32);
        display.remove(row);
        if row < flags.len() {
            flags.remove(row);
        }
        state_dirty = true;
    }
    drop(flags);
    drop(display);
    if state_dirty {
        refresh_command_states();
    }
    maybe_auto_open_progress(p, old_status.as_deref());
    maybe_rebuild_tree();
}

/// Drop one entry from cache + display (EntryRemoved event).
fn remove_row(id: &str) {
    {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|v| v.id != id);
    }
    let mut display = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(row) = display.iter().position(|x| x == id) {
        list_view().delete_row(row as i32);
        display.remove(row);
        let mut flags = BLOCK_FLAGS.lock().unwrap_or_else(|e| e.into_inner());
        if row < flags.len() {
            flags.remove(row);
        }
    }
    drop(display);
    refresh_command_states();
}

/// Full re-read from core (bootstrap fill, Refresh, post-op sync).
fn refresh_list() {
    let Some(c) = ctx() else { return };
    let rows = ndm_core::list_downloads(c);
    {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
        for p in &rows {
            cache.push(ProgressView::from(p));
        }
    }
    rebuild_display();
    maybe_rebuild_tree();
}

/// The selected entries' ids, in display order.
fn selected_ids() -> Vec<String> {
    let display = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
    list_view()
        .selected_rows()
        .into_iter()
        .filter_map(|r| display.get(r as usize).cloned())
        .collect()
}

/// Enable/grey the selection-dependent toolbar buttons from the
/// real selection + statuses (no fake affordances).
fn refresh_command_states() {
    let ids = selected_ids();
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let sel: Vec<&ProgressView> = cache.iter().filter(|v| ids.contains(&v.id)).collect();
    let can_resume = sel.iter().any(|v| {
        !matches!(v.status.as_str(), "finished" | "downloading" | "starting" | "merging")
    });
    let can_stop = sel.iter().any(|v| {
        matches!(v.status.as_str(), "downloading" | "starting" | "merging" | "queued")
    });
    // Stop All only while something is downloading; Delete
    // Completed only while a finished row exists (IDM logic —
    // the GTK shell applies the same two rules).
    let any_downloading = cache.iter().any(|v| v.status == "downloading");
    let any_finished = cache.iter().any(|v| v.status == "finished");
    let tb = toolbar();
    tb.set_enabled(TB_RESUME, can_resume);
    tb.set_enabled(TB_STOP, can_stop);
    tb.set_enabled(TB_DELETE, !sel.is_empty());
    tb.set_enabled(TB_STOP_ALL, any_downloading);
    tb.set_enabled(TB_DELETE_COMPLETED, any_finished);
}

// ----------------------------------------------------------------
// Category tree — structure mirrors ui/index.html #catTree +
// app.js renderCategoryFilters()/renderQueueFilters(): All
// Downloads with the five built-in categories plus any custom /
// in-use categories nested (General and Grabber excluded there),
// then Unfinished, Finished, Grabber projects, and Queues with one
// child per settings queue. Rebuilt only when the signature
// (category names + queue names) changes.
// ----------------------------------------------------------------
fn tree_tag(filter: &str) -> isize {
    let mut tags = TREE_TAGS.lock().unwrap_or_else(|e| e.into_inner());
    tags.push(filter.to_string());
    (tags.len() - 1) as isize
}

/// Last tree-signature check (the check clones Settings — it is
/// throttled to one run per 2 s; a new category or queue shows
/// within that, and progress ticks stay free of it).
static TREE_CHECKED: Mutex<Option<Instant>> = Mutex::new(None);

fn maybe_rebuild_tree() {
    {
        let mut last = TREE_CHECKED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = *last {
            if t.elapsed() < Duration::from_secs(2) {
                return;
            }
        }
        *last = Some(Instant::now());
    }
    let Some(c) = ctx() else { return };
    let state = c.state();
    let settings = ndm_core::current_settings(state);
    let mut cats: Vec<String> = vec![
        "Compressed".into(),
        "Documents".into(),
        "Music".into(),
        "Programs".into(),
        "Video".into(),
    ];
    for name in ndm_core::category_names_in_use(state, &settings) {
        if !cats.iter().any(|x| x.eq_ignore_ascii_case(&name))
            && !name.eq_ignore_ascii_case("General")
            && !name.eq_ignore_ascii_case("Grabber")
        {
            cats.push(name);
        }
    }
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        for v in cache.iter() {
            if !v.category.is_empty()
                && !cats.iter().any(|x| x.eq_ignore_ascii_case(&v.category))
                && !v.category.eq_ignore_ascii_case("General")
                && !v.category.eq_ignore_ascii_case("Grabber")
            {
                cats.push(v.category.clone());
            }
        }
    }
    let queues = queue_names(&settings);
    let sig = format!("{cats:?}|{queues:?}");
    {
        let mut stored = TREE_SIG.lock().unwrap_or_else(|e| e.into_inner());
        if *stored == sig {
            return;
        }
        *stored = sig;
    }
    TREE_BUILDING.store(true, Ordering::SeqCst);
    {
        let mut tags = TREE_TAGS.lock().unwrap_or_else(|e| e.into_inner());
        tags.clear();
    }
    let tree = TreeView { hwnd: unsafe { H_TREE } };
    tree.delete_all();
    let all = tree.insert_tagged(None, "All Downloads", tree_tag("All Downloads"));
    for cat in &cats {
        tree.insert_tagged(Some(all), cat, tree_tag(cat));
    }
    tree.expand(all);
    tree.insert_tagged(None, "Unfinished", tree_tag("Unfinished"));
    tree.insert_tagged(None, "Finished", tree_tag("Finished"));
    // index.html's node is labelled "Grabber projects", data-cat
    // "Grabber" — the category branch of visibleItems().
    tree.insert_tagged(None, "Grabber projects", tree_tag("Grabber"));
    // The Queues parent only expands in the web UI (no filter of
    // its own); its tag is the no-op marker.
    let queues_node = tree.insert_tagged(None, "Queues", tree_tag("__queues__"));
    for q in &queues {
        tree.insert_tagged(Some(queues_node), q, tree_tag(&format!("queue:{q}")));
    }
    tree.expand(queues_node);
    TREE_BUILDING.store(false, Ordering::SeqCst);
}

// ----------------------------------------------------------------
// Dialogs + confirms (winui dialog builder; Phase-3 dialogs are
// honestly labelled placeholders — settings stay untouched).
// ----------------------------------------------------------------
/// Keyboard Shortcuts dialog (ui/app.js kmodal) — the static
/// list exactly as index.html shows it, on the dialog builder.
/// The keys themselves are real in this shell: the accelerator
/// table built in WinMain routes them to the same menu commands
/// (Escape is the dialogs'/menus' own close, as in app.js).
fn show_shortcuts(hwnd: HWND) {
    let rows: Vec<winui::dialog::FieldSpec> = [
        ("Ctrl+N", "Add New Download"),
        ("Ctrl+Shift+V", "Add Video"),
        ("F5", "Refresh List"),
        ("Esc", "Close Dialog / Menu"),
        ("Ctrl+Q", "Exit NDM"),
        ("Ctrl+A", "Select All Downloads"),
    ]
    .iter()
    .map(|(key, desc)| {
        let line: &'static str =
            Box::leak(format!("{key:<15} {desc}").into_boxed_str());
        winui::dialog::FieldSpec::label(line)
    })
    .collect();
    let spec = winui::dialog::DialogSpec {
        title: "Keyboard Shortcuts",
        width: 420,
        height: 0,
        fields: rows,
        ok_label: "OK",
        cancel_label: None,
        extra_label: None,
    };
    let _ = winui::dialog::show_modal(hwnd, &spec);
}

/// The Delete confirmation (ui/app.js delmodal /
/// openDeleteConfirm): ONE dialog for the whole batch — the
/// name/count line, one "also delete the file(s)" checkbox that
/// applies to every status, Delete/Cancel. Returns
/// Some(delete_file) on Delete, None on cancel.
fn show_delete_confirm(hwnd: HWND, ids: &[String]) -> Option<bool> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let (line, file_label) = if ids.len() == 1 {
        let name = cache
            .iter()
            .find(|v| v.id == ids[0])
            .map(|v| if v.name.is_empty() { "download".to_string() } else { v.name.clone() })
            .unwrap_or_else(|| "download".to_string());
        (
            format!("Delete \"{name}\" from the list?"),
            "Also delete the file from the disk (including a partial download)",
        )
    } else {
        (
            format!("Delete {} downloads from the list?", ids.len()),
            "Also delete the files from the disk (including partial downloads)",
        )
    };
    drop(cache);
    let line_s: &'static str = Box::leak(line.into_boxed_str());
    let spec = winui::dialog::DialogSpec {
        title: "Delete download",
        width: 460,
        height: 0,
        fields: vec![
            winui::dialog::FieldSpec::label(line_s),
            winui::dialog::FieldSpec::checkbox(file_label, false),
        ],
        ok_label: "Delete",
        cancel_label: Some("Cancel"),
        extra_label: None,
    };
    let outcome = winui::dialog::show_modal(hwnd, &spec);
    if !outcome.accepted {
        return None;
    }
    Some(matches!(
        outcome.values.get(1),
        Some(winui::dialog::FieldValue::Checked(true))
    ))
}

/// Add New Download (Tasks/File menu, toolbar, incoming offers).
/// Mirrors the Tauri add path's user-visible behaviour: the link
/// is probed first (core `probe_file` — the dialog's size /
/// resume / category source in the web UI), then `add_download`
/// runs with the probed total/name/category carried in, exactly
/// as ui/app.js's accept handler passes them. Probe and add run
/// on a worker thread (they are network + engine work); results
/// come back through WM_APP_STATUS / WM_APP_REFRESH.
fn show_add_dialog(hwnd: HWND, prefill_url: Option<&str>) {
    let Some(c) = ctx() else { return };
    let settings = ndm_core::get_settings(c.clone());
    let settings_json = serde_json::to_value(&settings).unwrap_or(serde_json::Value::Null);
    let default_dir = settings_json
        .get("default_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("~/Downloads")
        .to_string();
    let mut cat_names: Vec<&'static str> = vec!["Auto"];
    for name in ndm_core::category_names_in_use(c.state(), &settings) {
        cat_names.push(Box::leak(name.into_boxed_str()));
    }
    let spec = winui::dialog::DialogSpec::new(
        "Add New Download",
        vec![
            winui::dialog::FieldSpec::edit("Download URL", prefill_url.unwrap_or("")),
            winui::dialog::FieldSpec::edit("Save to folder", default_dir.clone()),
            winui::dialog::FieldSpec::combo("Category", cat_names.clone(), 0),
            winui::dialog::FieldSpec::checkbox("Download later", false),
        ],
    );
    let outcome = winui::dialog::show_modal(hwnd, &spec);
    if !outcome.accepted {
        return;
    }
    let url = match outcome.values.first() {
        Some(winui::dialog::FieldValue::Text(t)) => t.trim().to_string(),
        _ => String::new(),
    };
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("ftp://")) {
        set_status(1, "Add failed: the URL must start with http://, https:// or ftp://");
        return;
    }
    let dir = match outcome.values.get(1) {
        Some(winui::dialog::FieldValue::Text(t)) if !t.trim().is_empty() => t.trim().to_string(),
        _ => default_dir.clone(),
    };
    let dir_explicit = dir != default_dir;
    let category = match outcome.values.get(2) {
        Some(winui::dialog::FieldValue::Choice { index, text }) if *index > 0 => {
            Some(text.clone())
        }
        _ => None,
    };
    let later = matches!(
        outcome.values.get(3),
        Some(winui::dialog::FieldValue::Checked(true))
    );
    set_status(1, "Probing link…");
    std::thread::spawn(move || {
        // Probe first (the add's size/name/category source), then
        // add — two block_on steps so a duplicate answer can carry
        // the exact add parameters to the UI thread's confirm.
        let probe = ndm_core::rt::block_on(ndm_core::probe_file(
            c.clone(),
            url.clone(),
            None,
            None,
            None,
            None,
        ));
        let (total, probed_name, probed_cat, probed_dir) = match &probe {
            Ok(v) => (
                v.get("total").and_then(|x| x.as_u64()),
                v.get("suggested_name")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
                v.get("category")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
                v.get("dir")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
            ),
            Err(_) => (None, None, None, None),
        };
        // an unedited folder follows the probe's routed
        // folder (the web dialog shows and uses exactly that)
        let dir = if !dir_explicit {
            probed_dir.unwrap_or(dir)
        } else {
            dir
        };
        let category = category.or(probed_cat);
        let dup = DupAdd {
            url: url.clone(),
            dir: dir.clone(),
            category: category.clone(),
            name: probed_name.clone(),
            later,
            total,
            dir_explicit,
        };
        let result = ndm_core::rt::block_on(ndm_core::add_download(
            c,
            url,
            dir,
            Some(later),
            category,
            probed_name,
            None,
            None,
            Some(false),
            None,
            None,
            None,
            None,
            total,
            Some(dir_explicit),
        ));
        match result {
            Ok(id) => {
                post_status(format!("Added download {id}"));
                post_refresh();
            }
            Err(e) => {
                if e.starts_with("duplicate:") {
                    // app.js:2299 — ask on the UI thread; a Yes
                    // retries with force (see WM_APP_DUP).
                    post_dup(dup);
                } else {
                    post_status(format!("Add failed: {e}"));
                }
            }
        }
    });
}

// ----------------------------------------------------------------
// AppEvent handling on the UI thread.
// ----------------------------------------------------------------
fn handle_app_event(hwnd: HWND, event: AppEvent) {
    match event {
        AppEvent::Progress(p) => {
            let view = ProgressView::from(&p);
            // The entry's progress dialog (if open) gets the same
            // payload, forwarded to its own HWND.
            progress_dialog::forward(&view);
            upsert_progress_row(&view);
        }
        AppEvent::DownloadComplete(v) => {
            // The entry's progress dialog stays open on the final
            // state (§1.3): hand it the cached final payload.
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                let view = {
                    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                    cache.iter().find(|x| x.id == id).cloned()
                };
                if let Some(view) = view {
                    progress_dialog::forward(&view);
                }
            }
            set_status(1, "Download complete");
            // The Download Complete dialog: queue the announcement
            // (a finish landing while one already shows is dropped,
            // app.js's rule) and pump — the pump itself yields to
            // any other open modal.
            if !COMPLETE_SHOWING.load(Ordering::SeqCst) {
                COMPLETE_QUEUE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(v);
            }
            pump_complete(hwnd);
        }
        AppEvent::EntryRemoved(v) => {
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                progress_dialog::close_for(id);
                remove_row(id);
            }
            set_status(1, "Entry removed");
        }
        AppEvent::IncomingFile(v) => {
            // Host hand-off / basket / clipboard / drop offer:
            // the Add dialog prefilled with the offered URL (the
            // web UI opens the same dialog from this payload).
            let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
            if url.is_empty() {
                set_status(1, "Incoming file offer without a URL");
            } else {
                show_add_dialog(hwnd, Some(url));
            }
        }
        AppEvent::IncomingVideo(v) => {
            // The Video picker, presented with the browser link
            // (payload {url, title, cookies} — core's shape).
            let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let cookies = v.get("cookies").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if url.is_empty() {
                set_status(1, "Incoming video offer without a URL");
            } else {
                video_dialog::show(hwnd, Some((url, cookies)));
            }
        }
        AppEvent::IncomingGrab(v) => {
            // The extension's grab-all-links: the wizard opens
            // with the page filled in and exploration running
            // (app.js pumpGrabLinks). grabber_dialog::show with
            // a prefill does exactly that, for a fresh window
            // and for one already open (open_with + fetch).
            let url = v
                .get("url")
                .and_then(|x| x.as_str())
                .filter(|x| !x.is_empty())
                .or_else(|| v.get("pageUrl").and_then(|x| x.as_str()))
                .unwrap_or("")
                .to_string();
            if url.starts_with("http://") || url.starts_with("https://") {
                crate::grabber_dialog::show(hwnd, Some(url));
            } else {
                set_status(1, "Incoming grab without an address");
            }
        }
        AppEvent::BasketNote(v) => {
            // Core's answer to a file dropped on the basket (and
            // its other basket feedback): shown in the basket
            // itself, like basket.js's note().
            let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
            crate::basket::show_note(text);
        }
        AppEvent::ComponentsProgress(v) => {
            // The background component fetch is invisible by
            // design; the one consumer is a video probe waiting
            // on the engine (its dialog re-probes on "done").
            video_dialog::on_components_progress(&v);
        }
        AppEvent::UpdateAvailable(v) => {
            // The Update dialog (offer latched in core state).
            set_status(1, "Update available");
            update_dialog::show(hwnd, &v);
        }
        AppEvent::UpdateProgress(v) => {
            if update_dialog::is_open() {
                update_dialog::forward_progress(v);
            } else {
                set_status(1, "Updating…");
            }
        }
        AppEvent::UpdateInstalled(v) => {
            if update_dialog::is_open() {
                update_dialog::forward_installed(v);
            } else {
                let ver = v.get("version").and_then(|x| x.as_str()).unwrap_or("");
                if ver.is_empty() {
                    set_status(1, "Update installed — restart NDM to use it");
                } else {
                    set_status(1, &format!("NDM {ver} is installed — restart NDM to use it"));
                }
            }
        }
        AppEvent::QuotaExceeded(_v) => {
            set_status(2, "Download limit reached");
        }
        AppEvent::QuotaCleared(_v) => {
            set_status(2, "");
        }
        AppEvent::ExitVetoed(v) => {
            // Core vetoed a when-done exit (paused downloads
            // remain) — the exit simply does not happen; say so.
            let n = v.get("paused").and_then(|x| x.as_u64()).unwrap_or(0);
            set_status(1, &format!("Exit postponed — {n} paused download(s) remain"));
        }
        AppEvent::PersistFailed(_v) => {
            set_status(2, "Could not save the download list");
        }
        AppEvent::PersistRecovered(_v) => {
            set_status(2, "");
        }
        AppEvent::DropFileIgnored(v) => {
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
            set_status(1, &format!("Dropped file is not a web link: {name}"));
        }
        AppEvent::ExtUpdateNotice(_v) => {
            set_status(2, "Browser extension update pending — restart the browser");
        }
        AppEvent::ProgressDialogReveal(v) => {
            // Core asks for the entry's progress dialog on an
            // app-side start (PROGRESS-DIALOG-SPEC §1.3/G7). Core
            // now emits for "show" and "minimized" alike and
            // carries the view in the payload; the shell honours
            // that view directly ("minimized" opens the dialog
            // lowered), falling back to re-reading the setting
            // when a payload carries none — so "never" (which
            // core never emits for) still wins if an emit slips
            // through. Verified against core 770332f: no other
            // change needed — open_progress_dialog's `minimized`
            // arm already shows the window SW_SHOWMINIMIZED, and
            // maybe_auto_open_progress applies the same three-way
            // rule to start transitions core does not reveal for.
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
            if !id.is_empty() {
                let view = v
                    .get("view")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(progress_dialog_setting);
                match view.as_str() {
                    "never" => {}
                    "minimized" => open_progress_dialog(hwnd, id, true, false),
                    _ => open_progress_dialog(hwnd, id, false, false),
                }
            }
        }
    }
}

// ----------------------------------------------------------------
// Command bodies shared by toolbar / menus / context menu.
// ----------------------------------------------------------------
fn cmd_resume_selected() {
    let Some(c) = ctx() else { return };
    let ids = selected_ids();
    if ids.is_empty() {
        set_status(1, "Select a download first");
        return;
    }
    for id in ids {
        let c2 = c.clone();
        ndm_core::rt::spawn(async move {
            let _ = ndm_core::resume_download(c2, id).await;
            post_refresh();
        });
    }
}

fn cmd_stop_selected() {
    let Some(c) = ctx() else { return };
    let ids = selected_ids();
    if ids.is_empty() {
        set_status(1, "Select a download first");
        return;
    }
    let mut failed = String::new();
    for id in &ids {
        if let Err(e) = ndm_core::pause_download(c.clone(), id.clone()) {
            failed = e;
        }
    }
    if failed.is_empty() {
        set_status(1, &format!("Stopped {} download(s)", ids.len()));
    } else {
        set_status(1, &format!("Stop failed: {failed}"));
    }
    refresh_list();
}

fn cmd_delete(hwnd: HWND, ids: Vec<String>) {
    let Some(c) = ctx() else { return };
    if ids.is_empty() {
        set_status(1, "Select a download first");
        return;
    }
    // One delmodal for the whole batch (replaces the old
    // two-MessageBox sequence): the dialog carries the
    // delete-file choice itself.
    let Some(delete_file) = show_delete_confirm(hwnd, &ids) else {
        return;
    };
    let results = ndm_core::remove_downloads(c, ids, Some(delete_file));
    let failed: Vec<String> = results
        .iter()
        .filter(|r| r.get("ok").and_then(|x| x.as_bool()) != Some(true))
        .filter_map(|r| {
            r.get("error")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    if failed.is_empty() {
        set_status(1, "Deleted");
    } else {
        set_status(1, &format!("Some entries could not be deleted: {}", failed.join("; ")));
    }
    refresh_list();
}

fn cmd_delete_completed(_hwnd: HWND) {
    let Some(c) = ctx() else { return };
    let ids: Vec<String> = {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .iter()
            .filter(|v| v.status == "finished")
            .map(|v| v.id.clone())
            .collect()
    };
    if ids.is_empty() {
        set_status(1, "No completed downloads");
        return;
    }
    // app.js deleteCompleted: no confirmation dialog — the bulk
    // remove runs directly and files on disk are kept.
    let results = ndm_core::remove_downloads(c, ids, Some(false));
    let n_failed = results
        .iter()
        .filter(|r| r.get("ok").and_then(|x| x.as_bool()) != Some(true))
        .count();
    if n_failed == 0 {
        set_status(1, "Completed downloads deleted");
    } else {
        set_status(1, &format!("{n_failed} completed entries could not be deleted"));
    }
    refresh_list();
}

/// Import Downloads (ui/app.js imodal / iGo): paste an exported
/// list (JSON array); each row's url is required, its folder is
/// derived from save_to/path exactly as app.js does (the parent
/// folder when the last part looks like a file name, the value
/// itself when it is already a folder), and name/category/queue
/// ride along. Imports land paused (later=true). A parse failure
/// reopens the dialog with the text intact (app.js keeps it open
/// with an inline error; the status line carries it here).
/// Export needs no dialog: M_EXPORT already calls core's
/// export_downloads and reports the written path (app.js's toast).
fn show_import_dialog(hwnd: HWND) {
    let Some(c) = ctx() else { return };
    let default_dir = {
        let s = ndm_core::get_settings(c.clone());
        serde_json::to_value(&s)
            .ok()
            .and_then(|j| j.get("default_dir").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| "~/Downloads".to_string())
    };
    let mut text = String::new();
    loop {
        let initial: &'static str = Box::leak(text.clone().into_boxed_str());
        let spec = winui::dialog::DialogSpec {
            title: "Import Downloads",
            width: 560,
            height: 0,
            fields: vec![winui::dialog::FieldSpec::multiline(
                "Paste an exported NDM downloads list (JSON)",
                initial,
            )],
            ok_label: "Import",
            cancel_label: Some("Cancel"),
            extra_label: None,
        };
        let outcome = winui::dialog::show_modal(hwnd, &spec);
        if !outcome.accepted {
            return;
        }
        text = match outcome.values.first() {
            Some(winui::dialog::FieldValue::Text(t)) => t.clone(),
            _ => String::new(),
        };
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(text.trim());
        let rows = match parsed {
            Ok(v) => v.as_array().cloned().unwrap_or_default(),
            Err(_) => {
                set_status(1, "Import: that is not valid JSON");
                continue;
            }
        };
        // (url, dir, name, category, queue) per row.
        let mut jobs: Vec<(String, String, Option<String>, Option<String>, Option<String>)> =
            Vec::new();
        for r in &rows {
            let Some(url) = r.get("url").and_then(|x| x.as_str()).filter(|s| !s.is_empty())
            else {
                continue;
            };
            let saved_to = r
                .get("save_to")
                .and_then(|x| x.as_str())
                .or_else(|| r.get("path").and_then(|x| x.as_str()))
                .unwrap_or("");
            let dir = if saved_to.is_empty() {
                default_dir.clone()
            } else {
                let cut = saved_to.rfind(['/', '\\']).map(|i| i as i64).unwrap_or(-1);
                let last = if cut >= 0 {
                    &saved_to[(cut as usize) + 1..]
                } else {
                    saved_to
                };
                if cut > 0 && last.contains('.') {
                    saved_to[..cut as usize].to_string()
                } else {
                    saved_to.to_string()
                }
            };
            let pick = |k: &str| {
                r.get(k)
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            };
            jobs.push((url.to_string(), dir, pick("name"), pick("category"), pick("queue")));
        }
        if jobs.is_empty() {
            set_status(1, "Import: no downloads found in that list");
            continue;
        }
        let n = jobs.len();
        set_status(1, &format!("Importing {n} download(s)…"));
        std::thread::spawn(move || {
            let mut ok = 0usize;
            for (url, dir, name, category, queue) in jobs {
                let r = ndm_core::rt::block_on(ndm_core::add_download(
                    c.clone(),
                    url,
                    dir,
                    Some(true),
                    category,
                    name,
                    None,
                    None,
                    None,
                    queue,
                    None,
                    None,
                    None,
                    None,
                    None,
                ));
                if r.is_ok() {
                    ok += 1;
                }
            }
            post_status(format!(
                "Imported {ok} download{} (paused — start them when ready)",
                if ok == 1 { "" } else { "s" }
            ));
            post_refresh();
        });
        return;
    }
}

/// Add Batch Download (ui/app.js bmodal / bAdd): one http(s)
/// link per line into one folder, added in order with
/// later=false (app.js passes no queue/category/probe values).
/// EVERY pasted line counts in the report (W193-15): non-link
/// lines are counted, never silently filtered. A duplicate is
/// reported as "already in the download list" in the batch
/// report — the WM_APP_DUP confirm belongs to the single Add
/// flow only, exactly as in app.js. Validation failures reopen
/// the dialog with the text intact and the message on the status
/// line (the Import dialog's pattern); a partial failure reopens
/// it via WM_APP_BATCH with only the failed + non-link lines
/// left in the box, so a retry cannot duplicate the successes.
fn show_batch_dialog(hwnd: HWND, mut text: String, mut dir: String, mut report: Vec<String>) {
    let Some(c) = ctx() else { return };
    let default_dir = {
        let s = ndm_core::get_settings(c.clone());
        serde_json::to_value(&s)
            .ok()
            .and_then(|j| j.get("default_dir").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| "~/Downloads".to_string())
    };
    loop {
        let mut fields = vec![
            winui::dialog::FieldSpec::multiline("One download link per line", text.clone()),
            winui::dialog::FieldSpec::edit(
                "Save to folder",
                if dir.is_empty() { default_dir.clone() } else { dir.clone() },
            ),
        ];
        for line in std::mem::take(&mut report) {
            let line_s: &'static str = Box::leak(line.into_boxed_str());
            fields.push(winui::dialog::FieldSpec::label(line_s));
        }
        let spec = winui::dialog::DialogSpec {
            title: "Add Batch Download",
            width: 560,
            height: 0,
            fields,
            ok_label: "Add All",
            cancel_label: Some("Cancel"),
            extra_label: None,
        };
        let outcome = winui::dialog::show_modal(hwnd, &spec);
        if !outcome.accepted {
            return;
        }
        text = match outcome.values.first() {
            Some(winui::dialog::FieldValue::Text(t)) => t.clone(),
            _ => String::new(),
        };
        dir = match outcome.values.get(1) {
            Some(winui::dialog::FieldValue::Text(t)) => t.clone(),
            _ => String::new(),
        };
        let raw_lines: Vec<String> = text
            .lines()
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
        let urls: Vec<String> = raw_lines
            .iter()
            .filter(|x| {
                x.starts_with("http://") || x.starts_with("https://") || x.starts_with("ftp://")
            })
            .cloned()
            .collect();
        let invalid = raw_lines.len() - urls.len();
        if raw_lines.is_empty() {
            set_status(1, "Paste the links first — one link per line (http, https or ftp).");
            continue;
        }
        if urls.is_empty() {
            set_status(
                1,
                &format!(
                    "No valid links found — {invalid} line{} not a link. One http(s) link per line.",
                    if invalid == 1 { " is" } else { "s are" }
                ),
            );
            continue;
        }
        // bDir trimmed, falling back to the default folder.
        let dir = if dir.trim().is_empty() {
            default_dir.clone()
        } else {
            dir.trim().to_string()
        };
        let total_lines = raw_lines.len();
        let url_count = urls.len();
        set_status(1, &format!("Adding {url_count} download(s)…"));
        std::thread::spawn(move || {
            let mut ok = 0usize;
            let mut failed: Vec<(String, String)> = Vec::new();
            for url in &urls {
                let r = ndm_core::rt::block_on(ndm_core::add_download(
                    c.clone(),
                    url.clone(),
                    dir.clone(),
                    Some(false),
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
                    None,
                ));
                match r {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        // the duplicate marker is an internal id
                        // string ("duplicate: dl-16") — name what
                        // it means, and cap the reason as app.js
                        // does (140 chars + "…").
                        let mut reason = if e.starts_with("duplicate:") {
                            "already in the download list".to_string()
                        } else {
                            e
                        };
                        if reason.chars().count() > 140 {
                            reason =
                                format!("{}…", reason.chars().take(140).collect::<String>());
                        }
                        failed.push((url.clone(), reason));
                    }
                }
            }
            post_refresh();
            // What remains in the box is exactly what still needs
            // attention: the failed URLs and the non-link lines,
            // in their original order.
            let failed_urls: std::collections::HashSet<&str> =
                failed.iter().map(|(u, _)| u.as_str()).collect();
            let remaining = raw_lines
                .iter()
                .filter(|u| {
                    failed_urls.contains(u.as_str())
                        || !(u.starts_with("http://")
                            || u.starts_with("https://")
                            || u.starts_with("ftp://"))
                })
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            post_batch(BatchReport {
                ok,
                url_count,
                total_lines,
                failed,
                invalid,
                remaining,
                dir,
            });
        });
        return;
    }
}

/// Refresh download address (ui/app.js rmodal / openRefreshUrl):
/// the prompt is prefilled with the current (expired) address;
/// core's refresh_url decides — same file resumes with progress
/// kept, a different file restarts from zero at the next start —
/// and its errors (downloading / finished / video) surface
/// verbatim in the status bar.
fn cmd_refresh_address(hwnd: HWND) {
    let Some(c) = ctx() else { return };
    let Some(id) = selected_ids().first().cloned() else {
        set_status(1, "Select a download first");
        return;
    };
    let current = {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.iter().find(|v| v.id == id).map(|v| v.url.clone())
    };
    let Some(current) = current else {
        set_status(1, "That download is no longer in the list");
        return;
    };
    let Some(new_url) = winui::dialog::show_prompt(
        hwnd,
        "Refresh download address",
        "Download addresses often expire. Paste a fresh one for the same file: \
         if the new address serves the same file, the download continues from its \
         current progress; if it serves a different file, the download starts over. \
         Which of the two applies is decided when the download next starts.",
        &current,
    ) else {
        return;
    };
    let new_url = new_url.trim().to_string();
    if new_url.is_empty() {
        set_status(1, "Enter the new address first");
        return;
    }
    match ndm_core::refresh_url(c, id, new_url) {
        Ok(()) => {
            set_status(1, "Address refreshed — the download stays paused");
            refresh_list();
        }
        Err(e) => set_status(1, &format!("Could not refresh the address: {e}")),
    }
}

/// The queue Start/Stop toolbar acts on: the tree's queue filter
/// when one is selected, else the Main queue (the web Scheduler's
/// default queue).
fn active_queue() -> String {
    let f = current_filter();
    if let Some(q) = f.strip_prefix("queue:") {
        q.to_string()
    } else {
        ndm_core::main_queue_name()
    }
}

fn on_command(hwnd: HWND, id: i32) {
    let Some(c) = ctx() else { return };
    match id {
        TB_ADD_URL | M_ADD => show_add_dialog(hwnd, None),
        M_ABOUT => {
            let engine = {
                let v = ndm_core::components_status(c.clone());
                v.get("ffmpeg")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "not set up yet".to_string())
            };
            let engine_ref: &'static str = Box::leak(engine.into_boxed_str());
            winui::dialog::show_about(hwnd, env!("CARGO_PKG_VERSION"), engine_ref);
        }
        TB_RESUME | CTX_RESUME => cmd_resume_selected(),
        TB_STOP | CTX_STOP => cmd_stop_selected(),
        CTX_REFRESH => cmd_refresh_address(hwnd),
        CTX_SHOW_PROGRESS => match selected_ids().first() {
            Some(first) => {
                let first = first.clone();
                open_progress_dialog(hwnd, &first, false, true);
            }
            None => set_status(1, "Select a download first"),
        },
        TB_STOP_ALL | M_STOP_ALL | M_PAUSE_ALL => {
            ndm_core::pause_all(&c, false, None);
            set_status(1, "All downloads stopped");
            refresh_list();
        }
        M_START_ALL => {
            let n = ndm_core::start_all(c);
            set_status(1, &format!("Start All — {n} download(s) offered"));
            refresh_list();
        }
        TB_DELETE | M_DELETE | CTX_DELETE => {
            let ids = selected_ids();
            cmd_delete(hwnd, ids);
        }
        M_DELETE_ALL => {
            let ids: Vec<String> = {
                let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                cache.iter().map(|v| v.id.clone()).collect()
            };
            cmd_delete(hwnd, ids);
        }
        TB_DELETE_COMPLETED | M_DELETE_DONE => cmd_delete_completed(hwnd),
        TB_START_QUEUE => match ndm_core::scheduler_start_now(c, Some(active_queue())) {
            Ok(msg) => {
                set_status(1, &msg);
                refresh_list();
            }
            Err(e) => set_status(1, &e),
        },
        TB_STOP_QUEUE => {
            let q = active_queue();
            ndm_core::scheduler_stop_now(c, Some(q.clone()));
            set_status(1, &format!("Queue \"{q}\" stopped"));
            refresh_list();
        }
        CTX_OPEN_FILE | CTX_OPEN_FOLDER => {
            let folder = id == CTX_OPEN_FOLDER;
            match selected_ids().first() {
                Some(first) => match ndm_core::open_path(c, first.clone(), folder) {
                    Ok(()) => {}
                    Err(e) => set_status(1, &format!("Open failed: {e}")),
                },
                None => set_status(1, "Select a download first"),
            }
        }
        M_SELECT_ALL => {
            list_view().select_all();
            refresh_command_states();
        }
        M_EXPORT => match ndm_core::export_downloads(c) {
            Ok(path) => set_status(1, &format!("Exported to {path}")),
            Err(e) => set_status(1, &format!("Export failed: {e}")),
        },
        M_UPDATE => {
            // app.js manualUpdateCheck: an available result opens
            // the Update dialog (check_update also emits
            // UpdateAvailable, which opens it too — show() raises
            // the same window); current / error are reported.
            set_status(1, "Checking for updates…");
            ndm_core::rt::spawn(async move {
                match ndm_core::check_update(c).await {
                    Ok(v) => match v.get("status").and_then(|x| x.as_str()) {
                        Some("available") => post_offer(v),
                        Some("current") => {
                            let ver = v.get("version").and_then(|x| x.as_str()).unwrap_or("");
                            post_status(format!("NDM is up to date ({ver})"));
                        }
                        _ => post_status(
                            "Could not reach the update server — try again later".to_string(),
                        ),
                    },
                    Err(e) => post_status(format!("Update check failed: {e}")),
                }
                post_refresh();
            });
        }
        M_REFRESH => {
            refresh_list();
            set_status(1, "List refreshed");
        }
        TB_OPTIONS | M_OPTIONS => options_dialog::show(hwnd),
        // Phase-3 dialogs still pending: honest placeholders.
        TB_SCHEDULER | M_SCHEDULER => scheduler_dialog::show(hwnd),
        TB_GRABBER | M_GRABBER => crate::grabber_dialog::show(hwnd, None),
        M_VIDEO => video_dialog::show(hwnd, None),
        M_BATCH => show_batch_dialog(hwnd, String::new(), String::new(), Vec::new()),
        M_IMPORT => show_import_dialog(hwnd),
        M_COLUMNS => columns_dialog::show(hwnd),
        M_TOOLBAR_CUSTOMIZE => toolbar_dialog::show(hwnd),
        M_SHORTCUTS => show_shortcuts(hwnd),
        M_BASKET => crate::basket::toggle(),
        M_QUIT => attempt_close(hwnd),
        _ => {
            if (CTX_QUEUE_BASE..CTX_QUEUE_BASE + 64).contains(&id) {
                let idx = (id - CTX_QUEUE_BASE) as usize;
                let queue = {
                    let qm = QUEUE_MENU.lock().unwrap_or_else(|e| e.into_inner());
                    qm.get(idx).cloned()
                };
                match (queue, selected_ids().first().cloned()) {
                    (Some(q), Some(first)) => {
                        match ndm_core::set_entry_queue(c, first, q.clone()) {
                            Ok(()) => {
                                set_status(1, &format!("Moved to queue \"{q}\""));
                                refresh_list();
                            }
                            Err(e) => set_status(1, &format!("Move failed: {e}")),
                        }
                    }
                    _ => set_status(1, "Select a download first"),
                }
            }
        }
    }
    // A completion that yielded to the modal this command just
    // closed gets its dialog now (app.js calls showNextComplete
    // from every modal-close path).
    pump_complete(hwnd);
}

/// The row context menu (right-click): the selection's real
/// commands, TrackPopupMenu with TPM_RETURNCMD so the choice
/// routes through the same on_command table.
fn show_context_menu(hwnd: HWND) {
    let Some(c) = ctx() else { return };
    let settings = ndm_core::get_settings(c);
    let queues = queue_names(&settings);
    let mut items = vec![
        MenuItem::cmd(CTX_SHOW_PROGRESS, "Show progress"),
        MenuItem::Separator,
        MenuItem::cmd(CTX_RESUME, "Resume"),
        MenuItem::cmd(CTX_STOP, "Stop"),
        MenuItem::cmd(CTX_REFRESH, "Refresh download address…"),
        MenuItem::Separator,
        MenuItem::cmd(CTX_OPEN_FILE, "Open File"),
        MenuItem::cmd(CTX_OPEN_FOLDER, "Open Folder"),
    ];
    if !queues.is_empty() {
        let mut qm = QUEUE_MENU.lock().unwrap_or_else(|e| e.into_inner());
        *qm = queues.clone();
        drop(qm);
        let sub: Vec<MenuItem> = queues
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let label: &'static str = Box::leak(q.clone().into_boxed_str());
                MenuItem::cmd(CTX_QUEUE_BASE + i as i32, label)
            })
            .collect();
        items.push(MenuItem::Separator);
        items.push(MenuItem::submenu("Add to Queue", sub));
    }
    items.push(MenuItem::Separator);
    items.push(MenuItem::cmd(CTX_DELETE, "Delete…"));
    let Ok(menu) = winui::menu::build_popup(&items) else {
        return;
    };
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        if cmd.0 > 0 {
            on_command(hwnd, cmd.0 as i32);
        }
    }
}

// ----------------------------------------------------------------
// Single-instance raise seam + guard (Phase-2 port).
//
// The guard protocol is replicated EXACTLY from the Tauri shell
// (src-tauri/src/main.rs, "single-instance guard" block,
// lines 340-517): loopback TCP on a port derived from the
// app-data dir by FNV-1a (offset basis 0xcbf29ce484222325, prime
// 0x100000001b3) as `41000 + (hash % 12000)` — instance_port(),
// src-tauri/src/main.rs:418; verbs "ping" / "raise", reply
// "ndm-here\n" — bind_and_serve_instance(), :375 and
// instance_ping(), :447; a 2 s bind grace, then a 10 s handover
// watch before a newcomer exits as Secondary —
// acquire_instance(), :464. Because the derivation and protocol
// are identical, a native instance and a Tauri instance on the
// same machine see each other and hand off across shells.
//
// Local deltas, plumbing only: the Tauri guard raised through an
// AppHandle stashed by register_instance_handle() (:411); here
// the serve thread raises through the shell's WinSink (queued in
// INSTANCE_RAISE_PENDING when the window does not exist yet —
// the same C-MAJOR queue the Tauri guard keeps), and
// raise_main_window_impl() is the native raise the Tauri shell's
// raise_main_window() (:310) maps to.
// ----------------------------------------------------------------
static INSTANCE_SINK: Mutex<Option<Arc<WinSink>>> = Mutex::new(None);
static INSTANCE_RAISE_PENDING: AtomicBool = AtomicBool::new(false);

fn raise_via_sink() {
    let sink = INSTANCE_SINK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match sink {
        Some(s) if s.target().is_some() => s.raise_main_window(),
        _ => INSTANCE_RAISE_PENDING.store(true, Ordering::SeqCst),
    }
}

enum Instance {
    Primary,
    Secondary,
}

/// Bind the guard port; on success start serving the ping/raise
/// protocol IMMEDIATELY and report true. (Verbatim shape from
/// src-tauri/src/main.rs:375; the AppHandle raise became
/// raise_via_sink.)
fn bind_and_serve_instance(port: u16) -> bool {
    let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) else {
        return false;
    };
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).unwrap_or(0);
                let line = String::from_utf8_lossy(&buf[..n]);
                let line = line.trim();
                if line == "ping" || line == "raise" {
                    let _ = s.write_all(b"ndm-here\n");
                    if line == "raise" {
                        raise_via_sink();
                    }
                }
            });
        }
    });
    true
}

/// The guard port: FNV-1a over the canonical app-data dir,
/// `41000 + (hash % 12000)` (src-tauri/src/main.rs:418, verbatim
/// — including its APPDATA/XDG base computation, so the port
/// agrees with the Tauri shell's on the same profile).
fn instance_port() -> u16 {
    let dir = {
        #[cfg(windows)]
        let base = std::env::var_os("APPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = match std::env::var_os("XDG_DATA_HOME") {
            Some(x) if !x.is_empty() => Some(PathBuf::from(x)),
            _ => std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".local").join("share")),
        };
        base.map(|b| b.join("com.ndm.downloadmanager"))
            .unwrap_or_else(|| PathBuf::from("com.ndm.downloadmanager"))
    };
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in dir.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    41000 + (h % 12000) as u16
}

fn instance_ping(port: u16, verb: &str) -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
    if s.write_all(format!("{verb}\n").as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 32];
    let n = s.read(&mut buf).unwrap_or(0);
    buf[..n].starts_with(b"ndm-here")
}

/// Verbatim control flow from src-tauri/src/main.rs:464,
/// including the 2 s grace, the fail-open arm, the --background
/// raise suppression and the m-1 10 s handover watch.
fn acquire_instance() -> Instance {
    let port = instance_port();
    if bind_and_serve_instance(port) {
        return Instance::Primary;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut primary_alive = false;
    loop {
        if instance_ping(port, "ping") {
            primary_alive = true;
        }
        if bind_and_serve_instance(port) {
            return Instance::Primary;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !primary_alive {
        return Instance::Primary;
    }
    if !std::env::args().any(|a| a == "--background") {
        let _ = instance_ping(port, "raise");
    }
    let watch_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if bind_and_serve_instance(port) {
            return Instance::Primary;
        }
        if Instant::now() >= watch_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Instance::Secondary
}

/// The app-data dir: Tauri's app_data_dir() on Windows —
/// SHGetKnownFolderPath(FOLDERID_RoamingAppData) (Vista-era
/// Shell API) + the app identifier "com.ndm.downloadmanager".
/// The APPDATA-env fallback mirrors the guard's own base
/// computation for the (practically unreachable) failure arm.
fn app_data_dir() -> PathBuf {
    unsafe {
        if let Ok(pw) = SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, None) {
            let s = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as *const core::ffi::c_void));
            if !s.is_empty() {
                return PathBuf::from(s).join("com.ndm.downloadmanager");
            }
        }
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|b| b.join("com.ndm.downloadmanager"))
        .unwrap_or_else(|| PathBuf::from("com.ndm.downloadmanager"))
}

/// The native raise: restore + foreground (the Tauri shell's
/// raise_main_window unminimizes/shows/focuses; AllowSetForeground
/// -Window(ASFW_ANY) is the Win32 hand-off the second instance's
/// ping authorises in spirit — foreground rules are the OS's).
fn raise_main_window_impl(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Show the main window. Called when bootstrap completes — the
/// native replacement for the Tauri shell's page-load Finished
/// show (src-tauri/src/main.rs on_page_load): the journal is
/// restored and the first frame is laid out at this point.
fn on_state_restored(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    // The Drop Basket exists from startup, hidden until the
    // user asks for it (the Tauri basket window's visible:false).
    crate::basket::create(hwnd);
}

/// WM_CLOSE / File → Exit — the Tauri shell's CloseRequested
/// decision, mirrored faithfully (src-tauri/src/main.rs:562-580,
/// the N-12 block): closing the main window IS the user leaving.
/// There is NO settings-driven hide/close-to-tray anywhere in the
/// product — core's Settings struct carries no close-behaviour
/// field (verified against core/src/lib.rs), and Tauri's handler
/// exits unconditionally because the hidden basket window would
/// otherwise keep the process a zombie. The when-done veto is not
/// a close-time branch either: it lives inside core
/// (maybe_exit_when_done → AppEvent::ExitVetoed, or Ctx::exit →
/// WM_APP_EXIT here), so a vetoed when-done exit simply never
/// reaches this code. DestroyWindow → WM_DESTROY → PostQuit.
fn attempt_close(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Core bootstrap — src-tauri/src/main.rs setup() (:618-656),
/// same order, same calls: AppState/Ctx were built in WinMain;
/// here: cleanup_update_leftovers (Windows) → init_paths →
/// run_startup_sweeps → check_ext_notice → the background entry
/// points → run_incoming_loop. Then the shell's own first frame:
/// initial list fill + tree build, the startup notice, a queued
/// guard raise, and (unless --background) the RESTORED show
/// signal — posted, not self-faked: this IS the real signal
/// point the scaffold's placeholder stood in for.
/// Returns false when init_paths fails (the Tauri setup()'s `?`
/// aborts the run the same way).
fn bootstrap(hwnd: HWND, background: bool) -> bool {
    let Some(c) = ctx() else { return false };
    let state = c.state();
    #[cfg(windows)]
    ndm_core::cleanup_update_leftovers();
    let dir = app_data_dir();
    if let Err(e) = state.init_paths(&dir) {
        let text = format!("NDM could not open its data folder ({}): {e}", dir.display());
        let text_buf = winui::wide(&text);
        let title_buf = winui::wide("Nill Download Manager");
        unsafe {
            let _ = MessageBoxW(Some(hwnd), winui::pcw(&text_buf), winui::pcw(&title_buf), MB_OK);
        }
        return false;
    }
    ndm_core::run_startup_sweeps(state);
    ndm_core::check_ext_notice(&c, state);
    ndm_core::run_launch_update_check(c.clone());
    ndm_core::run_update_recheck_loop(c.clone());
    ndm_core::run_component_warmup(c.clone());
    ndm_core::startup_resume(&c);
    ndm_core::run_scheduler_loop(c.clone());
    ndm_core::run_clipboard_watcher(c.clone());
    ndm_core::run_snapshot_loop(c.clone());
    ndm_core::run_incoming_loop(c.clone(), dir);

    // First frame from real state.
    refresh_list();
    if let Some(notice) = ndm_core::take_startup_notice(c.clone()) {
        set_status(1, &notice);
    }
    // #28 (app.js): the startup check's emit can land before any
    // listener exists — pull the latched offer now that we can
    // present it.
    {
        let offer = ndm_core::get_pending_update(c.clone());
        if offer.get("status").and_then(|x| x.as_str()) == Some("available") {
            update_dialog::show(hwnd, &offer);
        }
    }
    // A guard raise queued while the window was still starting
    // (the Tauri register_instance_handle seam, :411).
    if INSTANCE_RAISE_PENDING.swap(false, Ordering::SeqCst) {
        raise_main_window_impl(hwnd);
    }
    if !background {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_RESTORED, WPARAM(0), LPARAM(0));
        }
    }
    true
}

// ----------------------------------------------------------------
// Drag-out (WS-J): drag a FINISHED row out of the list into
// Explorer / the desktop and get the actual file — the inverse
// of the Drop Basket. LVN_BEGINDRAG is only a notification; the
// shell itself runs the OLE drag (DoDragDrop with a CF_HDROP
// IDataObject, mirroring basket.rs's OLE idioms from the target
// side), and only when the dragged row(s) include a finished
// entry whose file is on disk. Otherwise nothing happens and no
// drag ever starts.
// ----------------------------------------------------------------

/// The file a finished entry drags out as — the same rule as
/// the GTK shell: save_to is core's out_path (the file itself
/// for plain downloads; for video entries the folder, whose
/// file is the entry's own name or stem + a media extension).
fn drag_out_file(p: &ProgressView) -> Option<PathBuf> {
    let path = PathBuf::from(&p.save_to);
    if path.is_file() {
        return Some(path);
    }
    if !path.is_dir() || p.name.is_empty() || p.name.contains(['/', '\\']) {
        return None;
    }
    let direct = path.join(&p.name);
    if direct.is_file() {
        return Some(direct);
    }
    let stem = p.name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&p.name);
    ["mp4", "mkv", "webm", "m4a", "mp3"]
        .iter()
        .map(|ext| path.join(format!("{stem}.{ext}")))
        .find(|c| c.is_file())
}

/// The files a drag starting on `row` would carry: the whole
/// selection when the row is part of it, else just that row —
/// finished entries with a file on disk only, in row order.
fn drag_out_files(row: i32) -> Vec<PathBuf> {
    let selected = list_view().selected_rows();
    let rows: Vec<i32> = if selected.contains(&row) { selected } else { vec![row] };
    let display = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    rows.iter()
        .filter_map(|r| display.get(*r as usize))
        .filter_map(|id| cache.iter().find(|p| &p.id == id))
        .filter(|p| p.status == "finished")
        .filter_map(drag_out_file)
        .collect()
}

/// A CF_HDROP payload: the DROPFILES header followed by the
/// double-null-terminated wide path list Explorer expects.
/// The caller (OLE, via ReleaseStgMedium) owns the HGLOBAL.
fn hdrop_global(paths: &[PathBuf]) -> windows::Win32::Foundation::HGLOBAL {
    let header = std::mem::size_of::<DROPFILES>();
    let mut body: Vec<u16> = Vec::new();
    for p in paths {
        body.extend(p.to_string_lossy().encode_utf16());
        body.push(0);
    }
    body.push(0);
    unsafe {
        let hg = GlobalAlloc(GMEM_MOVEABLE, header + body.len() * 2).unwrap_or_default();
        if !hg.is_invalid() {
            let ptr = GlobalLock(hg) as *mut u8;
            if !ptr.is_null() {
                let df = DROPFILES {
                    pFiles: header as u32,
                    pt: POINT::default(),
                    fNC: false.into(),
                    fWide: true.into(),
                };
                std::ptr::copy_nonoverlapping(
                    &df as *const DROPFILES as *const u8,
                    ptr,
                    header,
                );
                std::ptr::copy_nonoverlapping(
                    body.as_ptr() as *const u8,
                    ptr.add(header),
                    body.len() * 2,
                );
                let _ = GlobalUnlock(hg);
            }
        }
        hg
    }
}

#[implement(IDataObject)]
struct DragFiles {
    paths: Vec<PathBuf>,
}

impl IDataObject_Impl for DragFiles_Impl {
    fn GetData(&self, fmt: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let f = unsafe { &*fmt };
        if f.cfFormat != 15 || f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return Err(windows::Win32::Foundation::DV_E_FORMATETC.into());
        }
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: hdrop_global(&self.paths) },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }
    fn GetDataHere(
        &self,
        _fmt: *const FORMATETC,
        _medium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn QueryGetData(&self, fmt: *const FORMATETC) -> windows::core::HRESULT {
        let f = unsafe { &*fmt };
        if f.cfFormat == 15
            && f.dwAspect == DVASPECT_CONTENT.0
            && f.tymed & TYMED_HGLOBAL.0 as u32 != 0
        {
            windows::core::HRESULT(0)
        } else {
            windows::Win32::Foundation::DV_E_FORMATETC
        }
    }
    fn GetCanonicalFormatEtc(
        &self,
        _fin: *const FORMATETC,
        _fout: *mut FORMATETC,
    ) -> windows::core::HRESULT {
        windows::Win32::Foundation::E_NOTIMPL
    }
    fn SetData(
        &self,
        _fmt: *const FORMATETC,
        _medium: *const STGMEDIUM,
        _release: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn EnumFormatEtc(
        &self,
        _direction: u32,
    ) -> windows::core::Result<windows::Win32::System::Com::IEnumFORMATETC> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn DAdvise(
        &self,
        _fmt: *const FORMATETC,
        _advf: u32,
        _sink: windows::core::Ref<'_, windows::Win32::System::Com::IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn DUnadvise(&self, _connection: u32) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn EnumDAdvise(
        &self,
    ) -> windows::core::Result<windows::Win32::System::Com::IEnumSTATDATA> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
}

#[implement(IDropSource)]
struct DragSource;

impl IDropSource_Impl for DragSource_Impl {
    fn QueryContinueDrag(
        &self,
        escape: windows::core::BOOL,
        keys: MODIFIERKEYS_FLAGS,
    ) -> windows::core::HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if !keys.contains(MK_LBUTTON) {
            DRAGDROP_S_DROP
        } else {
            windows::core::HRESULT(0)
        }
    }
    fn GiveFeedback(&self, _effect: DROPEFFECT) -> windows::core::HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Run the OLE drag for a drag begun on `row`; a no-op when no
/// selected/dragged row is a finished entry with a file on disk.
fn begin_drag_out(row: i32) {
    let paths = drag_out_files(row);
    if paths.is_empty() {
        return;
    }
    unsafe {
        let _ = OleInitialize(None);
        let data: IDataObject = DragFiles { paths }.into();
        let source: IDropSource = DragSource.into();
        let mut effect = DROPEFFECT::default();
        let _ = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
    }
}

// ----------------------------------------------------------------
// Window procedure
// ----------------------------------------------------------------
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            H_MAIN = hwnd;

            // Menu bar — the web UI's five menus, same order/labels.
            let menus = vec![
                MenuItem::submenu("Tasks", vec![
                    MenuItem::cmd(M_BATCH, "Add Batch Download…"),
                    MenuItem::cmd(M_IMPORT, "Import Downloads…"),
                    MenuItem::cmd(M_EXPORT, "Export Downloads"),
                    MenuItem::Separator,
                    MenuItem::cmd(M_GRABBER, "Grabber"),
                    MenuItem::cmd(M_SCHEDULER, "Scheduler"),
                ]),
                MenuItem::submenu("File", vec![
                    MenuItem::cmd(M_ADD, "Add New Download…\tCtrl+N"),
                    MenuItem::cmd(M_VIDEO, "Add Video…\tCtrl+Shift+V"),
                    MenuItem::cmd(M_BATCH, "Add Batch Download…"),
                    MenuItem::cmd(M_IMPORT, "Import Downloads…"),
                    MenuItem::cmd(M_EXPORT, "Export Downloads"),
                    MenuItem::Separator,
                    MenuItem::cmd(M_QUIT, "Exit\tCtrl+Q"),
                ]),
                MenuItem::submenu("Downloads", vec![
                    MenuItem::cmd(M_START_ALL, "Start All"),
                    MenuItem::cmd(M_PAUSE_ALL, "Pause All"),
                    MenuItem::cmd(M_STOP_ALL, "Stop All"),
                    MenuItem::Separator,
                    MenuItem::cmd(M_SELECT_ALL, "Select All"),
                    MenuItem::cmd(M_DELETE, "Delete Selected"),
                    MenuItem::cmd(M_DELETE_DONE, "Delete All Completed"),
                    MenuItem::cmd(M_DELETE_ALL, "Delete All…"),
                    MenuItem::Separator,
                    MenuItem::cmd(M_GRABBER, "Grabber"),
                    MenuItem::cmd(M_SCHEDULER, "Scheduler"),
                    MenuItem::cmd(M_OPTIONS, "Options"),
                ]),
                MenuItem::submenu("View", vec![
                    MenuItem::cmd(M_COLUMNS, "Columns…"),
                    MenuItem::cmd(M_TOOLBAR_CUSTOMIZE, "Customize Toolbar…"),
                    MenuItem::cmd(M_BASKET, "Drop Basket"),
                    MenuItem::Separator,
                    MenuItem::cmd(M_REFRESH, "Refresh List\tF5"),
                ]),
                MenuItem::submenu("Help", vec![
                    MenuItem::cmd(M_UPDATE, "Check for Updates…"),
                    MenuItem::cmd(M_SHORTCUTS, "Keyboard Shortcuts"),
                    MenuItem::Separator,
                    MenuItem::cmd(M_ABOUT, "About NDM"),
                ]),
            ];
            let _ = winui::menu::set_menu_bar(hwnd, &menus);

            // Toolbar — real button set/order (see TB_* constants).
            let toolbar_items = [
                ToolbarItem::Button { id: TB_ADD_URL, label: "Add URL" },
                ToolbarItem::Button { id: TB_RESUME, label: "Resume" },
                ToolbarItem::Button { id: TB_STOP, label: "Stop" },
                ToolbarItem::Button { id: TB_STOP_ALL, label: "Stop All" },
                ToolbarItem::Separator,
                ToolbarItem::Button { id: TB_DELETE, label: "Delete" },
                ToolbarItem::Button { id: TB_DELETE_COMPLETED, label: "Delete Completed" },
                ToolbarItem::Button { id: TB_OPTIONS, label: "Options" },
                ToolbarItem::Button { id: TB_SCHEDULER, label: "Scheduler" },
                ToolbarItem::Button { id: TB_START_QUEUE, label: "Start Queue" },
                ToolbarItem::Button { id: TB_STOP_QUEUE, label: "Stop Queue" },
                ToolbarItem::Button { id: TB_GRABBER, label: "Grabber" },
            ];
            if let Ok(tb) = Toolbar::create(hwnd, ID_TOOLBAR, &toolbar_items) {
                H_TOOLBAR = tb.hwnd;
            }
            // The saved toolbar configuration applies from the
            // first frame (app.js applyToolbar on load).
            apply_toolbar();

            // Category tree skeleton; bootstrap's first
            // maybe_rebuild_tree() fills it from real core state
            // (categories in use + settings queues).
            if let Ok(tree) = TreeView::create(hwnd, ID_TREE) {
                H_TREE = tree.hwnd;
            }

            // Download list — the visible columns of the column
            // configuration (ui/app.js COLUMN_DEFS + the saved
            // colmodal layout; defaults: File Name, Q, Size,
            // Status, Time left, Transfer rate, Last Try Date,
            // Description — Date Added and Save To are defOff).
            let vis = visible_columns();
            let columns: Vec<ColumnSpec> = vis
                .iter()
                .map(|c| ColumnSpec {
                    label: COLUMN_DEFS
                        .iter()
                        .find(|(id, _, _, _)| *id == c.id)
                        .map(|(_, l, _, _)| *l)
                        .unwrap_or(""),
                    width: column_width(c),
                })
                .collect();
            *COLUMN_COUNT.lock().unwrap_or_else(|e| e.into_inner()) = columns.len() as i32;
            *BLOCK_SUBITEM.lock().unwrap_or_else(|e| e.into_inner()) =
                vis.iter().position(|c| c.id == "status").map(|i| i as i32).unwrap_or(-1);
            if let Ok(lv) = ListView::create(hwnd, ID_LIST, &columns) {
                H_LIST = lv.hwnd;
            }

            // Status bar, 3 parts (spike shape).
            if let Ok(sb) = StatusBar::create(hwnd, ID_STATUS, &[300, 560, -1]) {
                H_STATUS = sb.hwnd;
                sb.set_text(0, "0 downloads");
            }

            // Drag & drop target (WM_DROPFILES) — Vista-era Shell API.
            DragAcceptFiles(hwnd, true);
            LRESULT(0)
        }
        WM_SIZE => {
            let cx = (l.0 & 0xffff) as i32;
            let cy = ((l.0 >> 16) & 0xffff) as i32;
            layout_children(cx, cy);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mmi = l.0 as *mut MINMAXINFO;
            if !mmi.is_null() {
                (*mmi).ptMinTrackSize = POINT { x: 860, y: 540 };
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, (w.0 & 0xffff) as i32);
            LRESULT(0)
        }
        WM_NOTIFY => {
            let hdr = l.0 as *const NMHDR;
            if !hdr.is_null() {
                let code = (*hdr).code;
                if (*hdr).idFrom == ID_LIST as usize {
                    if code == NM_CUSTOMDRAW {
                        let has_map = {
                            let snapshot: Vec<bool> = BLOCK_FLAGS
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone();
                            move |row: i32| snapshot.get(row as usize).copied().unwrap_or(false)
                        };
                        // The closure borrows nothing after this call;
                        // on_custom_draw only invokes it synchronously.
                        return LRESULT(list_view().on_custom_draw(l, has_map));
                    }
                    if code == LVN_BEGINDRAG {
                        // Drag-out (WS-J): a drag begun on a
                        // finished row runs the OLE drag-out;
                        // begin_drag_out gates on finished + file
                        // on disk and is otherwise a no-op.
                        let nm = l.0 as *const NMLISTVIEW;
                        let row = if nm.is_null() { -1 } else { (*nm).iItem };
                        if row >= 0 {
                            begin_drag_out(row);
                        }
                    }
                    if code == NM_DBLCLK {
                        // Double-click a row opens the per-download
                        // progress dialog (spec §1.3, NDM's ruling).
                        let act = l.0 as *const NMITEMACTIVATE;
                        let row = if act.is_null() { -1 } else { (*act).iItem };
                        if row >= 0 {
                            let id = {
                                let display = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
                                display.get(row as usize).cloned()
                            };
                            if let Some(id) = id {
                                open_progress_dialog(hwnd, &id, false, true);
                            }
                        }
                    }
                    if code == NM_RCLICK {
                        let act = l.0 as *const NMITEMACTIVATE;
                        let row = if act.is_null() { -1 } else { (*act).iItem };
                        if row >= 0 {
                            let lv = list_view();
                            if !lv.selected_rows().contains(&row) {
                                for r in lv.selected_rows() {
                                    lv.set_selected(r, false);
                                }
                                lv.set_selected(row, true);
                            }
                            refresh_command_states();
                            show_context_menu(hwnd);
                        }
                    }
                } else if (*hdr).idFrom == ID_TREE as usize && code == TVN_SELCHANGEDW {
                    if !TREE_BUILDING.load(Ordering::SeqCst) {
                        let nmtv = l.0 as *const NMTREEVIEWW;
                        if !nmtv.is_null() {
                            let tag = (*nmtv).itemNew.lParam.0;
                            let filter = {
                                let tags = TREE_TAGS.lock().unwrap_or_else(|e| e.into_inner());
                                tags.get(tag as usize).cloned()
                            };
                            if let Some(f) = filter {
                                if f != "__queues__" {
                                    *FILTER.lock().unwrap_or_else(|e| e.into_inner()) = f;
                                    rebuild_display();
                                }
                            }
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_DROPFILES => {
            // The Tauri DragDrop arm (src-tauri/src/main.rs:583-595)
            // hands every dropped path to core's
            // handle_dropped_path(ctx, path, on_basket=false):
            // URL strings and shortcut files become incoming
            // offers, plain files become the DropFileIgnored
            // event. The basket window is Phase 3, so drops land
            // on the main window here (on_basket=false).
            let hdrop = HDROP(w.0 as *mut core::ffi::c_void);
            if let Some(c) = ctx() {
                let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, None);
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; (len + 1) as usize];
                    let n = DragQueryFileW(hdrop, i, Some(&mut buf));
                    if n == 0 {
                        continue;
                    }
                    let path = String::from_utf16_lossy(&buf[..n as usize]);
                    ndm_core::handle_dropped_path(&c, std::path::Path::new(&path), false);
                }
            }
            DragFinish(hdrop);
            LRESULT(0)
        }
        WM_APP_EVENT => {
            if l.0 != 0 {
                let event = *Box::from_raw(l.0 as *mut AppEvent);
                handle_app_event(hwnd, event);
            }
            LRESULT(0)
        }
        WM_APP_STATUS => {
            if l.0 != 0 {
                let text = *Box::from_raw(l.0 as *mut String);
                set_status(1, &text);
            }
            LRESULT(0)
        }
        WM_APP_REFRESH => {
            refresh_list();
            LRESULT(0)
        }
        WM_APP_UPDATE_OFFER => {
            if l.0 != 0 {
                let v = *Box::from_raw(l.0 as *mut serde_json::Value);
                update_dialog::show(hwnd, &v);
            }
            LRESULT(0)
        }
        WM_APP_DUP => {
            if l.0 != 0 {
                let dup = *Box::from_raw(l.0 as *mut DupAdd);
                // cfmodal (app.js:2299): the duplicate asks before
                // anything is added again.
                if winui::dialog::show_confirm(
                    hwnd,
                    "Confirm",
                    "This file is already in the download list.\nDownload it again as a new copy?",
                    "Download again",
                ) {
                    if let Some(c) = ctx() {
                        std::thread::spawn(move || {
                            let result = ndm_core::rt::block_on(ndm_core::add_download(
                                c,
                                dup.url,
                                dup.dir,
                                Some(dup.later),
                                dup.category,
                                dup.name,
                                None,
                                None,
                                Some(true),
                                None,
                                None,
                                None,
                                None,
                                dup.total,
                                Some(dup.dir_explicit),
                            ));
                            match result {
                                Ok(id) => {
                                    post_status(format!("Added download {id}"));
                                    post_refresh();
                                }
                                Err(e) => post_status(format!("Add failed: {e}")),
                            }
                        });
                    }
                } else {
                    set_status(1, "That link is already in the list — not added again");
                }
            }
            LRESULT(0)
        }
        WM_APP_BATCH => {
            if l.0 != 0 {
                let report = *Box::from_raw(l.0 as *mut BatchReport);
                if report.failed.is_empty() && report.invalid == 0 {
                    // Full success: app.js closes bmodal and
                    // toasts the count; the status line carries
                    // the toast here.
                    set_status(
                        1,
                        &format!("Added {} of {} downloads", report.ok, report.url_count),
                    );
                } else {
                    // Partial (or total) failure: reopen with the
                    // per-URL report as label rows and only the
                    // lines that still need attention in the box
                    // (app.js keeps bmodal open in this state).
                    let mut lines = vec![format!(
                        "Added {} of {} lines{}",
                        report.ok,
                        report.total_lines,
                        if report.failed.is_empty() {
                            String::new()
                        } else {
                            format!(" — {} failed:", report.failed.len())
                        }
                    )];
                    for (url, reason) in report.failed.iter().take(12) {
                        lines.push(format!("• {url} — {reason}"));
                    }
                    if report.failed.len() > 12 {
                        lines.push(format!("…and {} more", report.failed.len() - 12));
                    }
                    if report.invalid > 0 {
                        lines.push(format!(
                            "{} line{} not a link and could not be added.",
                            report.invalid,
                            if report.invalid == 1 { " is" } else { "s are" }
                        ));
                    }
                    show_batch_dialog(hwnd, report.remaining, report.dir, lines);
                }
            }
            LRESULT(0)
        }
        WM_APP_RAISE => {
            raise_main_window_impl(hwnd);
            LRESULT(0)
        }
        WM_APP_RESTORED => {
            on_state_restored(hwnd);
            LRESULT(0)
        }
        WM_APP_EXIT => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            attempt_close(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

fn main() {
    // Windows portable self-update hands the file swap to a copy
    // of this binary in temp, re-invoked with --apply-update: it
    // is not a second instance and must run BEFORE the guard and
    // before any state exists (it never returns) —
    // src-tauri/src/main.rs:519-535, verbatim.
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.len() >= 4 && args[1] == "--apply-update" {
            ndm_core::apply_staged_update(
                std::path::Path::new(&args[2]),
                std::path::Path::new(&args[3]),
            );
        }
    }

    winui::window::init_common_controls();

    // The seam, constructed exactly as the Tauri setup() builds
    // it: core's own AppState::new() + this shell's sink, as the
    // Ctx every command forwards into. The sink exists BEFORE the
    // guard so the guard's serve thread can raise through it.
    let sink = Arc::new(WinSink::new());
    *INSTANCE_SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink.clone());
    let ctx = Ctx::new(Arc::new(AppState::new()), sink.clone());
    *CTX.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx);

    // Single-instance guard FIRST (after the apply-update seam):
    // a second process must exit before it builds any window or
    // touches the journal — its persist() snapshots would
    // resurrect whatever the real window deletes.
    if matches!(acquire_instance(), Instance::Secondary) {
        eprintln!("[ndm] another NDM instance is already running; this launch hands over to it and exits");
        return;
    }

    let spec = winui::window::WindowSpec::main_window(
        "NdmShellWinMain",
        format!("Nill Download Manager {}", env!("CARGO_PKG_VERSION")),
        Some(wndproc),
    );
    let hwnd = match winui::window::create_top_level(&spec) {
        Ok(h) => h,
        Err(_) => return,
    };
    sink.attach(hwnd);

    // --background starts stay hidden (spec §1.1), as today.
    let background = std::env::args().any(|a| a == "--background");
    // Bootstrap core (journal, sweeps, background loops) BEFORE
    // the message loop: the window was created hidden, and
    // bootstrap posts the real RESTORED signal when init
    // completes. A failed init_paths aborts the run, exactly like
    // the Tauri setup()'s `?`.
    if !bootstrap(hwnd, background) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return;
    }
    // Selection-dependent buttons start greyed (nothing selected).
    refresh_command_states();

    // The kmodal shortcuts as a real accelerator table, routed
    // to the same menu commands the menus name (Escape excluded:
    // dialogs and menus close themselves, as in app.js).
    let accel = {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateAcceleratorTableW, ACCEL, FCONTROL, FSHIFT, FVIRTKEY,
        };
        let mk = |virt: windows::Win32::UI::WindowsAndMessaging::ACCEL_VIRT_FLAGS,
                  key: u16,
                  cmd: i32| ACCEL { fVirt: virt, key, cmd: cmd as u16 };
        let table = [
            mk(FCONTROL | FVIRTKEY, 0x4E, M_ADD),            // Ctrl+N
            mk(FCONTROL | FSHIFT | FVIRTKEY, 0x56, M_VIDEO), // Ctrl+Shift+V
            mk(FVIRTKEY, 0x74, M_REFRESH),                   // F5
            mk(FCONTROL | FVIRTKEY, 0x51, M_QUIT),           // Ctrl+Q
            mk(FCONTROL | FVIRTKEY, 0x41, M_SELECT_ALL),     // Ctrl+A
        ];
        unsafe { CreateAcceleratorTableW(&table).unwrap_or_default() }
    };
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if windows::Win32::UI::WindowsAndMessaging::TranslateAcceleratorW(hwnd, accel, &mut msg) != 0 {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
