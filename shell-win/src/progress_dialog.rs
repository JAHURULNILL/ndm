//! Per-download progress dialog (PROGRESS-DIALOG-SPEC, locked) —
//! a real modeless Win32 window per entry: a SysTabControl32 with
//! the spec's three tabs in order, the six IDM fields, a plain
//! progress bar, the owner-drawn segment strip
//! (winui::customdraw, G5), a SysListView32 connections table,
//! and the NDM properties group inside the collapsible details
//! region (§1.1's ruling, §1.2's collapse).
//!
//! Live data: the main window proc forwards each
//! AppEvent::Progress for an entry that has a dialog to that
//! dialog's HWND as WM_PD_PROGRESS with the boxed ProgressView —
//! the sink's own boxing pattern. `apply` is change-gated field
//! by field: a tick that changes nothing touches no control, the
//! title is rewritten only when its integer percent changes, the
//! strip invalidates only when its payload fields change, and
//! the connections table patches only changed cells. No timers.
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell.

use crate::sink::ProgressView;
use crate::winui::dialog as dlg;
use crate::winui::listview::{ColumnSpec, ListView};
use crate::winui::tab::TabControl;
use crate::winui::{pcw, send, set_window_text, wide, window_text};
use std::collections::HashMap;
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, NMHDR, PBM_SETMARQUEE, PBM_SETPOS, PBM_SETRANGE32, PBS_MARQUEE, TCN_SELCHANGE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW,
    GetWindowRect, LoadCursorW, PostMessageW, RegisterClassW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, BS_AUTOCHECKBOX, BS_AUTORADIOBUTTON, BS_GROUPBOX,
    CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CBS_DROPDOWNLIST, CBN_SELCHANGE, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, EN_KILLFOCUS, ES_AUTOHSCROLL, ES_NUMBER, GWLP_USERDATA,
    GWL_STYLE, IDC_ARROW, SWP_NOMOVE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW,
    SW_SHOWMINIMIZED, WINDOW_STYLE, WM_ACTIVATE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE,
    WM_DESTROY, WM_DRAWITEM, WM_NOTIFY, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

// Control ids (private to this window class).
const ID_TAB: i32 = 6001;
const ID_URL: i32 = 6002;
const ID_FIELD_BASE: i32 = 6010; // + 0..6 field values
const ID_BAR: i32 = 6020;
const ID_BTN_HIDE: i32 = 6021;
const ID_BTN_RESUME: i32 = 6022;
const ID_BTN_PAUSE: i32 = 6023;
const ID_BTN_CANCEL: i32 = 6024;
const ID_CAPTION: i32 = 6030;
const ID_STRIP: i32 = 6031;
const ID_CONNS: i32 = 6032;
const ID_PROPS_GROUP: i32 = 6033;
const ID_SAVE_TO: i32 = 6034;
const ID_REFERER: i32 = 6035;
const ID_DESC: i32 = 6036;
const ID_QUEUE: i32 = 6037;
const ID_LIMIT_ON: i32 = 6040;
const ID_LIMIT_VAL: i32 = 6041;
const ID_LIMIT_NOTE: i32 = 6042;
const ID_C_SHOW: i32 = 6050;
const ID_C_FILE: i32 = 6051;
const ID_C_FOLDER: i32 = 6052;
const ID_C_EXIT: i32 = 6053;
const ID_C_POWER: i32 = 6054;
const ID_R_SHUTDOWN: i32 = 6055;
const ID_R_RESTART: i32 = 6056;

/// Private message: LPARAM = Box<ProgressView> (see header).
const WM_PD_PROGRESS: u32 = WM_APP + 10;

// Static-control styles the windows crate does not wrap.
const SS_LEFTNOWORDWRAP: u32 = 0x000C;
const SS_ENDELLIPSIS: u32 = 0x4000;
const SS_OWNERDRAW: u32 = 0x000D;

// Layout (client pixels; the tab page area is x 20..636).
const WIN_W: i32 = 648;
const EXPANDED_H: i32 = 594;
const DETAILS_H: i32 = 296; // caption top (298) to expanded bottom, less margin
const TAB_LABELS: [&str; 3] = ["Download status", "Speed Limiter", "Options on completion"];
const FIELD_LABELS: [&str; 6] = [
    "Status",
    "File size",
    "Downloaded",
    "Transfer rate",
    "Time left",
    "Resume capability",
];

/// What `create` needs beyond the entry's current payload.
pub struct CreateArgs {
    pub view: ProgressView,
    pub queues: Vec<String>,
    pub global_limit: u64,
    pub show_complete_global: bool,
}

/// Per-dialog state. HWNDs are stored as isize so the registry is
/// Send (the dialog.rs DialogState pattern). The `last_*` fields
/// are the change gates described in the module header.
struct ProgDlg {
    id: String,
    tab: isize,
    url: isize,
    fields: [isize; 6],
    bar: isize,
    strip: isize,
    conns: isize,
    desc: isize,
    queue: isize,
    limit_on: isize,
    limit_val: isize,
    limit_note: isize,
    c_show: isize,
    c_file: isize,
    c_folder: isize,
    c_exit: isize,
    c_power: isize,
    r_shutdown: isize,
    r_restart: isize,
    page1: Vec<isize>,
    page2: Vec<isize>,
    page3: Vec<isize>,
    details: Vec<isize>,
    caption: isize,
    video: bool,
    expanded: bool,
    tab_index: i32,
    tab_h: i32,
    outer_h: i32,
    queues: Vec<String>,
    global_limit: u64,
    show_complete_global: bool,
    // change gates
    last_title: String,
    last_url: String,
    last_fields: [String; 6],
    bar_pos: i32,
    marquee: bool,
    btn_states: (bool, bool, bool),
    btn_hide: isize,
    btn_resume: isize,
    btn_pause: isize,
    btn_cancel: isize,
    save_to: isize,
    referer: isize,
    last_save_to: String,
    last_referer: String,
    strip_sig: (String, Vec<u64>, Vec<i64>, Vec<u8>, bool, u64),
    table_rows: Vec<[String; 3]>,
    last_desc: String,
    last_queue: String,
    limit_cache: Option<(bool, String, String)>,
    completion_cache: Option<[bool; 6]>,
    // last payload facts the command handlers act on
    speed_limit: u64,
    actions: Vec<String>,
}

static DLGS: std::sync::LazyLock<Mutex<HashMap<String, ProgDlg>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
/// Entries whose progress dialog the user has closed: a
/// background reveal must not rebuild it (QA-220 m3 — the GTK
/// shell keeps the same mark). An explicit open clears it.
static USER_CLOSED: std::sync::LazyLock<Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// Whether the user has closed this entry's progress dialog
/// and no explicit open has happened since.
pub fn user_closed(id: &str) -> bool {
    USER_CLOSED.lock().unwrap_or_else(|e| e.into_inner()).contains(id)
}

/// An explicit open (double-click / "Show progress") clears
/// the user-closed mark.
pub fn note_explicit_open(id: &str) {
    USER_CLOSED.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
}
/// The dialog whose payload currently feeds the (single) strip
/// snapshot in winui::customdraw — set on apply/activation.
static STRIP_OWNER: Mutex<String> = Mutex::new(String::new());

fn h(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}
fn raw(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

/// The payload's video test (the web UI's isVideoEntry): the
/// payload carries no kind field; a video job is recognised by
/// its yt-dlp resume marker.
fn is_video(view: &ProgressView) -> bool {
    view.resume_capability == "Yes (yt-dlp resume)"
}

// ----------------------------------------------------------------
// Public surface used by the main window proc.
// ----------------------------------------------------------------

/// Raise the entry's dialog if one is open. Returns false when
/// the entry has no dialog yet.
pub fn raise_existing(id: &str) -> bool {
    match hwnd_of(id) {
        Some(hwnd) => unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
            true
        },
        None => false,
    }
}

/// Whether the entry currently has a dialog window.
pub fn exists(id: &str) -> bool {
    hwnd_of(id).is_some()
}

/// Forward one progress payload to the entry's dialog, if any
/// (boxed, reclaimed on post failure — the sink's pattern).
pub fn forward(view: &ProgressView) {
    let Some(hwnd) = hwnd_of(&view.id) else { return };
    let lparam = LPARAM(Box::into_raw(Box::new(view.clone())) as isize);
    unsafe {
        if PostMessageW(Some(hwnd), WM_PD_PROGRESS, WPARAM(0), lparam).is_err() {
            drop(Box::from_raw(lparam.0 as *mut ProgressView));
        }
    }
}

/// Close the entry's dialog (its entry left the list).
pub fn close_for(id: &str) {
    if let Some(hwnd) = hwnd_of(id) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// The dialog HWND for an entry id, via the window user-data the
/// dialog itself maintains (registry of live windows).
fn hwnd_of(id: &str) -> Option<HWND> {
    let guard = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    guard.get(id).map(|&r| h(r))
}

/// Live dialog windows: entry id -> HWND (isize). The wndproc
/// registers on WM_CREATE and unregisters on WM_DESTROY.
static WINDOWS: std::sync::LazyLock<Mutex<HashMap<String, isize>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Create and show the entry's dialog (the caller has already
/// checked `raise_existing`). `minimized` honours the
/// progress_dialog_view start view (spec §1.3/G7).
pub fn create(parent: HWND, args: CreateArgs, minimized: bool, activate: bool) {
    register_class();
    unsafe {
        let class_buf = wide("NdmProgressDialog");
        let title_buf = wide(&args.view.name);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rect = RECT { left: 0, top: 0, right: WIN_W, bottom: EXPANDED_H };
        let _ = AdjustWindowRectEx(&mut rect, style, false, Default::default());
        let boxed = Box::into_raw(Box::new(args));
        let hwnd = CreateWindowExW(
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
        );
        match hwnd {
            Ok(hwnd) => {
                if minimized {
                    let _ = ShowWindow(hwnd, SW_SHOWMINIMIZED);
                } else {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    if activate {
                        let _ = SetForegroundWindow(hwnd);
                    }
                }
            }
            Err(_) => {
                drop(Box::from_raw(boxed));
            }
        }
    }
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let class_buf = wide("NdmProgressDialog");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(prog_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

unsafe fn child(
    parent: HWND,
    class: &str,
    text: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    ht: i32,
    id: i32,
) -> HWND {
    dlg::create_child(
        parent,
        class,
        text,
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | style,
        x,
        y,
        w,
        ht,
        id,
    )
}

unsafe fn label(parent: HWND, text: &str, style: u32, x: i32, y: i32, w: i32, id: i32) -> HWND {
    dlg::create_child(
        parent,
        "STATIC",
        text,
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
        x,
        y,
        w,
        20,
        id,
    )
}

// ----------------------------------------------------------------
// Construction (WM_CREATE): every control is created once, at
// fixed layout positions; tab pages are shown/hidden, never
// rebuilt. Page-1's details region is the `details` handle set.
// ----------------------------------------------------------------
fn build(hwnd: HWND, args: CreateArgs) -> ProgDlg {
    let view = &args.view;
    let video = is_video(view);
    let mut page1: Vec<isize> = Vec::new();
    let mut page2: Vec<isize> = Vec::new();
    let mut page3: Vec<isize> = Vec::new();
    let mut details: Vec<isize> = Vec::new();
    unsafe {
        // Tab control + its three tabs, in spec order.
        let tab = TabControl::create(hwnd, ID_TAB, &TAB_LABELS)
            .map(|t| {
                let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(t.hwnd, 8, 8, WIN_W - 16, EXPANDED_H - 16, true);
                t.hwnd
            })
            .unwrap_or_default();

        // --- Tab 1: Download status ---
        let url = label(hwnd, "", SS_LEFTNOWORDWRAP | SS_ENDELLIPSIS, 20, 40, 616, ID_URL);
        page1.push(raw(url));
        let mut fields = [HWND::default(); 6];
        for (i, name) in FIELD_LABELS.iter().enumerate() {
            let y = 68 + i as i32 * 24;
            page1.push(raw(label(hwnd, name, SS_LEFTNOWORDWRAP, 20, y, 130, 0)));
            let v = label(hwnd, "", SS_LEFTNOWORDWRAP | SS_ENDELLIPSIS, 158, y, 478, ID_FIELD_BASE + i as i32);
            fields[i] = v;
            page1.push(raw(v));
        }
        let bar = dlg::create_child(
            hwnd,
            "msctls_progress32",
            "",
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(1), // PBS_SMOOTH
            20,
            232,
            616,
            20,
            ID_BAR,
        );
        send(bar, PBM_SETRANGE32, WPARAM(0), LPARAM(1000));
        page1.push(raw(bar));
        let btn_hide = child(hwnd, "BUTTON", "<< Hide details", WINDOW_STYLE(0), 20, 260, 120, 26, ID_BTN_HIDE);
        let btn_resume = child(hwnd, "BUTTON", "Resume", WINDOW_STYLE(0), 380, 260, 80, 26, ID_BTN_RESUME);
        let btn_pause = child(hwnd, "BUTTON", "Pause", WINDOW_STYLE(0), 468, 260, 80, 26, ID_BTN_PAUSE);
        let btn_cancel = child(hwnd, "BUTTON", "Cancel", WINDOW_STYLE(0), 556, 260, 80, 26, ID_BTN_CANCEL);
        for b in [btn_hide, btn_resume, btn_pause, btn_cancel] {
            page1.push(raw(b));
        }

        // Details region (§1.2): caption + strip + table + the
        // NDM properties group, in that order, all collapsible.
        let caption_text = if video {
            "Download progress"
        } else {
            "Start positions and download progress by connections"
        };
        let caption = label(hwnd, caption_text, SS_LEFTNOWORDWRAP, 20, 298, 616, ID_CAPTION);
        let strip = dlg::create_child(
            hwnd,
            "STATIC",
            "",
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_OWNERDRAW),
            20,
            320,
            616,
            26,
            ID_STRIP,
        );
        details.extend([raw(caption), raw(strip)]);
        let conns = ListView::create(
            hwnd,
            ID_CONNS,
            &[
                ColumnSpec { label: "N.", width: 40 },
                ColumnSpec { label: "Downloaded", width: 130 },
                ColumnSpec { label: "Info", width: 430 },
            ],
        )
        .map(|lv| {
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(lv.hwnd, 20, 352, 616, 104, true);
            lv.hwnd
        })
        .unwrap_or_default();
        if video {
            // G8: no real connections — the table never shows.
            let _ = ShowWindow(conns, SW_HIDE);
        } else {
            details.push(raw(conns));
        }
        let group = child(hwnd, "BUTTON", "NDM properties", WINDOW_STYLE(BS_GROUPBOX as u32), 20, 464, 616, 118, ID_PROPS_GROUP);
        details.push(raw(group));
        let mut prop = |text: &str, y: i32, id: i32| -> HWND {
            details.push(raw(label(hwnd, text, SS_LEFTNOWORDWRAP, 32, y, 84, 0)));
            let v = label(hwnd, "", SS_LEFTNOWORDWRAP | SS_ENDELLIPSIS, 120, y, 504, id);
            details.push(raw(v));
            v
        };
        let save_to = prop("Save to", 482, ID_SAVE_TO);
        let referer = prop("Referer", 506, ID_REFERER);
        details.push(raw(label(hwnd, "Description", SS_LEFTNOWORDWRAP, 32, 530, 84, 0)));
        let desc = child(
            hwnd,
            "EDIT",
            "",
            WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            120,
            528,
            504,
            23,
            ID_DESC,
        );
        details.push(raw(desc));
        details.push(raw(label(hwnd, "Queue", SS_LEFTNOWORDWRAP, 32, 554, 84, 0)));
        let queue = child(hwnd, "COMBOBOX", "", WINDOW_STYLE(CBS_DROPDOWNLIST as u32), 120, 552, 504, 120, ID_QUEUE);
        for q in &args.queues {
            let mut buf = wide(q);
            send(queue, CB_ADDSTRING, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
        }
        let sel = args.queues.iter().position(|q| q == &view.queue).unwrap_or(0);
        send(queue, CB_SETCURSEL, WPARAM(sel), LPARAM(0));
        details.push(raw(queue));

        // --- Tab 2: Speed Limiter (§2.2/G9) ---
        let limit_on = child(
            hwnd,
            "BUTTON",
            "Use Speed Limiter for this download",
            WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            20,
            48,
            420,
            22,
            ID_LIMIT_ON,
        );
        page2.push(raw(limit_on));
        page2.push(raw(label(hwnd, "Maximum download speed:", SS_LEFTNOWORDWRAP, 20, 84, 180, 0)));
        let limit_val = child(
            hwnd,
            "EDIT",
            "",
            WS_BORDER | WINDOW_STYLE((ES_AUTOHSCROLL | ES_NUMBER) as u32),
            204,
            82,
            90,
            23,
            ID_LIMIT_VAL,
        );
        page2.push(raw(limit_val));
        page2.push(raw(label(hwnd, "KB/s", SS_LEFTNOWORDWRAP, 300, 84, 50, 0)));
        let limit_note = label(hwnd, "", SS_LEFTNOWORDWRAP, 20, 116, 616, ID_LIMIT_NOTE);
        page2.push(raw(limit_note));

        // --- Tab 3: Options on completion (§2.3) ---
        let mut mk_check = |text: &str, y: i32, id: i32| -> HWND {
            let c = child(hwnd, "BUTTON", text, WINDOW_STYLE(BS_AUTOCHECKBOX as u32), 20, y, 420, 22, id);
            page3.push(raw(c));
            c
        };
        let c_show = mk_check("Show \"Download complete\" dialog", 48, ID_C_SHOW);
        let c_file = mk_check("Open the file when done", 76, ID_C_FILE);
        let c_folder = mk_check("Open the folder when done", 104, ID_C_FOLDER);
        let c_exit = mk_check("Exit NDM when done", 132, ID_C_EXIT);
        let c_power = mk_check("Turn off the computer when done", 160, ID_C_POWER);
        let r_shutdown = child(hwnd, "BUTTON", "Shut down", WINDOW_STYLE(BS_AUTORADIOBUTTON as u32), 290, 160, 100, 22, ID_R_SHUTDOWN);
        let r_restart = child(hwnd, "BUTTON", "Restart", WINDOW_STYLE(BS_AUTORADIOBUTTON as u32), 396, 160, 100, 22, ID_R_RESTART);
        page3.extend([raw(r_shutdown), raw(r_restart)]);

        let mut outer = RECT::default();
        let _ = GetWindowRect(hwnd, &mut outer);
        ProgDlg {
            id: view.id.clone(),
            tab: raw(tab),
            url: raw(url),
            fields: fields.map(raw),
            bar: raw(bar),
            strip: raw(strip),
            conns: raw(conns),
            desc: raw(desc),
            queue: raw(queue),
            limit_on: raw(limit_on),
            limit_val: raw(limit_val),
            limit_note: raw(limit_note),
            c_show: raw(c_show),
            c_file: raw(c_file),
            c_folder: raw(c_folder),
            c_exit: raw(c_exit),
            c_power: raw(c_power),
            r_shutdown: raw(r_shutdown),
            r_restart: raw(r_restart),
            page1,
            page2,
            page3,
            details,
            caption: raw(caption),
            video,
            expanded: true,
            tab_index: 0,
            tab_h: EXPANDED_H - 16,
            outer_h: outer.bottom - outer.top,
            queues: args.queues.clone(),
            global_limit: args.global_limit,
            show_complete_global: args.show_complete_global,
            last_title: String::new(),
            last_url: String::new(),
            last_fields: Default::default(),
            bar_pos: -1,
            marquee: false,
            btn_states: (true, true, true),
            btn_hide: raw(btn_hide),
            btn_resume: raw(btn_resume),
            btn_pause: raw(btn_pause),
            btn_cancel: raw(btn_cancel),
            save_to: raw(save_to),
            referer: raw(referer),
            last_save_to: String::new(),
            last_referer: String::new(),
            strip_sig: Default::default(),
            table_rows: Vec::new(),
            last_desc: view.description.clone(),
            last_queue: view.queue.clone(),
            limit_cache: None,
            completion_cache: None,
            speed_limit: view.speed_limit,
            actions: view.completion_actions.clone(),
        }
    }
}

// ----------------------------------------------------------------
// Formatting — the dialog's own shapes (spec §3/G6): sizes with
// 2 decimals, the rate with "/sec", time left via fmtListLeft
// with a zero seconds part dropped.
// ----------------------------------------------------------------
fn fmt_size(n: u64) -> String {
    fmt_size_f(n as f64)
}

fn fmt_size_f(v: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = v;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

fn fmt_left(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs} sec");
    }
    let m = secs / 60;
    let s = secs % 60;
    if m < 60 {
        if s == 0 {
            format!("{m} min")
        } else {
            format!("{m} min {s} sec")
        }
    } else {
        format!("{} hour(s) {} min", m / 60, m % 60)
    }
}

/// The spec §3 status-text table for this dialog.
fn status_text(view: &ProgressView) -> String {
    match view.status.as_str() {
        "queued" => "Queued".to_string(),
        "starting" => {
            if view.detail.is_empty() {
                "Starting…".to_string()
            } else {
                view.detail.clone()
            }
        }
        "downloading" => "Receiving data…".to_string(),
        "merging" => "Merging…".to_string(),
        "paused" => "Paused".to_string(),
        "finished" => "Finished".to_string(),
        "error" => {
            if view.error.is_empty() {
                "Error".to_string()
            } else {
                ndm_core::display_error(&view.error)
            }
        }
        "cancelled" => "Cancelled".to_string(),
        other => other.to_string(),
    }
}

fn field_values(view: &ProgressView, video: bool) -> [String; 6] {
    let size = if view.total > 0 || view.status == "finished" {
        fmt_size(view.total)
    } else {
        "Unknown".to_string()
    };
    let downloaded = if view.total > 0 {
        format!(
            "{} ({:.2}%)",
            fmt_size(view.done_bytes),
            view.done_bytes as f64 * 100.0 / view.total as f64
        )
    } else {
        format!("{} (size unknown)", fmt_size(view.done_bytes))
    };
    let rate = if view.status == "downloading" && view.speed_bps > 0.0 {
        format!("{}/sec", fmt_size_f(view.speed_bps))
    } else {
        "—".to_string()
    };
    let left = if view.total == 0 || view.speed_bps <= 0.0 || view.done_bytes >= view.total {
        "—".to_string()
    } else {
        fmt_left(((view.total - view.done_bytes) as f64 / view.speed_bps).round() as u64)
    };
    let resume = if video {
        "Yes".to_string()
    } else if view.resume_capability.starts_with("Yes") {
        "Yes".to_string()
    } else if view.resume_capability.is_empty() {
        "—".to_string()
    } else {
        "No".to_string()
    };
    [status_text(view), size, downloaded, rate, left, resume]
}

/// "{pct}% {filename}" with a floor percent (§1.1); the filename
/// alone while the total is unknown; a finished download reads
/// 100% (a 0-byte file included).
fn title_for(view: &ProgressView) -> String {
    if view.total > 0 {
        format!("{}% {}", view.done_bytes * 100 / view.total, view.name)
    } else if view.status == "finished" {
        format!("100% {}", view.name)
    } else {
        view.name.clone()
    }
}

/// The §1.4 enablement matrix: (Resume, Pause, Cancel).
fn button_states(status: &str) -> (bool, bool, bool) {
    match status {
        "downloading" => (false, true, true),
        // Pause stays live during the resolve ("starting") and
        // stall-recovery ("resuming") phases, mirroring the GTK
        // matrix — core parks the job in both (2.3.1 bug 4).
        "starting" | "resuming" => (false, true, true),
        "queued" | "paused" => (true, false, true),
        "error" | "cancelled" => (true, false, false),
        _ => (false, false, false), // merging, finished, anything else
    }
}

// ----------------------------------------------------------------
// Live updates. Every write below sits behind a change gate; see
// the module header for the cost contract.
// ----------------------------------------------------------------
fn set_text_if(slot: &mut String, hwnd: isize, text: &str) {
    if slot != text {
        unsafe { set_window_text(h(hwnd), text) };
        *slot = text.to_string();
    }
}

fn apply(hwnd: HWND, view: &ProgressView) {
    let Some(id) = id_of(hwnd) else { return };
    let mut guard = DLGS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.get_mut(&id) else { return };
    d.speed_limit = view.speed_limit;
    d.actions = effective_actions(view);

    // G8, live: the video variant is re-evaluated per payload,
    // not frozen at open (the GTK shell does the same) — a dialog
    // opened before the entry's first video payload landed must
    // still drop its connections table and caption. Presentation
    // only: the strip keeps painting the payload's (synthesized
    // ratio) block map either way, and update_table stays a
    // no-op for video.
    let video_now = is_video(view);
    if video_now != d.video {
        d.video = video_now;
        unsafe {
            set_window_text(
                h(d.caption),
                if video_now {
                    "Download progress"
                } else {
                    "Start positions and download progress by connections"
                },
            );
            let show_conns = !video_now && d.tab_index == 0 && d.expanded;
            let _ = ShowWindow(h(d.conns), if show_conns { SW_SHOW } else { SW_HIDE });
        }
    }
    let title = title_for(view);
    if d.last_title != title {
        unsafe { set_window_text(hwnd, &title) };
        d.last_title = title;
    }
    set_text_if(&mut d.last_url, d.url, &view.url);
    let values = field_values(view, d.video);
    let targets = d.fields;
    for i in 0..6 {
        set_text_if(&mut d.last_fields[i], targets[i], &values[i]);
    }
    update_bar(d, view);
    let states = button_states(&view.status);
    if d.btn_states != states {
        dlg::set_enabled(h(d.btn_resume), states.0);
        dlg::set_enabled(h(d.btn_pause), states.1);
        dlg::set_enabled(h(d.btn_cancel), states.2);
        d.btn_states = states;
    }
    update_strip(d, view);
    update_table(d, view);

    // NDM properties group: read-only lines follow the payload;
    // the editable fields sync only while they do not have focus
    // (a progress tick must never clobber an edit in flight).
    let save_to = if view.save_to.is_empty() { "—" } else { &view.save_to };
    set_text_if(&mut d.last_save_to, d.save_to, save_to);
    let referer = if view.referer.is_empty() { "—" } else { &view.referer };
    set_text_if(&mut d.last_referer, d.referer, referer);
    let focus = unsafe { GetFocus() };
    if focus != h(d.desc) && d.last_desc != view.description {
        unsafe { set_window_text(h(d.desc), &view.description) };
        d.last_desc = view.description.clone();
    }
    if focus != h(d.queue) && d.last_queue != view.queue {
        if let Some(pos) = d.queues.iter().position(|q| q == &view.queue) {
            unsafe {
                send(h(d.queue), CB_SETCURSEL, WPARAM(pos), LPARAM(0));
            }
        }
        d.last_queue = view.queue.clone();
    }
    sync_limiter(d, view);
    sync_completion(d, view);
}

fn update_bar(d: &mut ProgDlg, view: &ProgressView) {
    let bar = h(d.bar);
    let indeterminate = view.status == "downloading" && view.total == 0;
    if indeterminate != d.marquee {
        unsafe {
            // Widen to isize for the flag arithmetic and let
            // `new as _` narrow back at the call: on 32-bit
            // Windows the windows crate aliases the *Ptr APIs
            // to the i32 Get/SetWindowLongW, on x86_64 they
            // take isize — the style bits are 32 on both.
            let style = GetWindowLongPtrW(bar, GWL_STYLE) as isize;
            let new = if indeterminate {
                style | PBS_MARQUEE as isize
            } else {
                style & !(PBS_MARQUEE as isize)
            };
            SetWindowLongPtrW(bar, GWL_STYLE, new as _);
            send(bar, PBM_SETMARQUEE, WPARAM(if indeterminate { 1 } else { 0 }), LPARAM(0));
        }
        d.marquee = indeterminate;
        d.bar_pos = -1; // re-assert the position after a style flip
    }
    if !indeterminate {
        let pos = if view.total > 0 {
            (view.done_bytes * 1000 / view.total) as i32
        } else if view.status == "finished" {
            1000
        } else {
            0
        };
        if pos != d.bar_pos {
            unsafe {
                send(bar, PBM_SETPOS, WPARAM(pos as usize), LPARAM(0));
            }
            d.bar_pos = pos;
        }
    }
}

fn update_strip(d: &mut ProgDlg, view: &ProgressView) {
    let sig = (
        view.blocks.clone(),
        view.conns_partial.clone(),
        view.conns_block.clone(),
        view.conns_state.clone(),
        view.status == "finished",
        view.total,
    );
    if d.strip_sig == sig {
        return;
    }
    d.strip_sig = sig;
    // The strip snapshot is shared: only the strip's owner (the
    // active dialog) publishes, so two open dialogs never fight
    // over the painter's data. A non-owner's inputs stay recorded
    // in its strip_sig and are published when it is activated.
    let is_owner = {
        let owner = STRIP_OWNER.lock().unwrap_or_else(|e| e.into_inner());
        *owner == d.id
    };
    if is_owner {
        publish_strip(d);
    }
}

/// Publish this dialog's recorded strip inputs to the shared
/// painter snapshot and repaint its strip.
fn publish_strip(d: &ProgDlg) {
    let (blocks, partial, block, state, finished, total) = &d.strip_sig;
    crate::winui::customdraw::set_strip_data(crate::winui::customdraw::StripData {
        blocks: blocks.clone(),
        conns_partial: partial.clone(),
        conns_block: block.clone(),
        conns_state: state.clone(),
        total: *total,
        finished: *finished,
    });
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(h(d.strip)), None, true);
    }
}

fn update_table(d: &mut ProgDlg, view: &ProgressView) {
    if d.video {
        return;
    }
    let finished = view.status == "finished";
    let mut rows: Vec<[String; 3]> = Vec::new();
    for (i, bytes) in view.conns_detail.iter().enumerate() {
        let active = view.status == "downloading" && i < view.conns;
        if *bytes == 0 && !active {
            continue;
        }
        // §3 Info wording; a finished entry's rows all read
        // "Done" (batch-3's N1 fix — finished workers sit at
        // phase 0, which must not read as "Waiting…"). A paused
        // entry's rows read "Paused": the row Info follows the
        // entry's state, not a stale live code (m3).
        let info = if finished {
            "Done"
        } else if view.status == "paused" {
            "Paused"
        } else {
            match view.conns_state.get(i).copied().unwrap_or(0) {
                1 => "Send GET…",
                2 => "Receiving data…",
                _ => "Waiting…",
            }
        };
        rows.push([format!("{}", i + 1), fmt_size(*bytes), info.to_string()]);
    }
    if rows == d.table_rows {
        return;
    }
    let lv = ListView { hwnd: h(d.conns) };
    let same_shape = rows.len() == d.table_rows.len()
        && rows.iter().zip(&d.table_rows).all(|(a, b)| a[0] == b[0]);
    if same_shape {
        // Patch only the cells that actually changed.
        for (r, row) in rows.iter().enumerate() {
            for c in 1..3 {
                if row[c] != d.table_rows[r][c] {
                    lv.set_text(r as i32, c as i32, &row[c]);
                }
            }
        }
    } else {
        lv.clear();
        for (r, row) in rows.iter().enumerate() {
            let cells: Vec<String> = row.iter().cloned().collect();
            lv.insert_row(r as i32, &cells);
        }
    }
    d.table_rows = rows;
}

// ----------------------------------------------------------------
// Tab 2 — Speed Limiter (§2.2/G9): immediate apply through
// set_entry_limit, no Apply button; the field is never rewritten
// while the user is editing it.
// ----------------------------------------------------------------
fn prefill_kb(own_bps: u64, global_bps: u64) -> u64 {
    if own_bps > 0 {
        ((own_bps as f64 / 1024.0).round() as u64).max(1)
    } else if global_bps > 0 {
        ((global_bps as f64 / 1024.0).round() as u64).max(1)
    } else {
        512
    }
}

fn sync_limiter(d: &mut ProgDlg, view: &ProgressView) {
    let focus = unsafe { GetFocus() };
    if focus == h(d.limit_on) || focus == h(d.limit_val) {
        return; // mid-edit or mid-click: the payload waits
    }
    let checked = view.speed_limit > 0;
    let text = prefill_kb(view.speed_limit, d.global_limit).to_string();
    let note = if d.global_limit > 0 {
        format!(
            "A global limit of {}/sec also applies (Options) — the lower of the two wins.",
            fmt_size(d.global_limit)
        )
    } else {
        String::new()
    };
    let want = (checked, text, note);
    if d.limit_cache.as_ref() != Some(&want) {
        dlg::set_checked(h(d.limit_on), want.0);
        unsafe { set_window_text(h(d.limit_val), &want.1) };
        unsafe { set_window_text(h(d.limit_note), &want.2) };
        dlg::set_enabled(h(d.limit_val), want.0);
        d.limit_cache = Some(want);
    }
}

fn apply_limit(d: &mut ProgDlg) {
    let on = dlg::is_checked(h(d.limit_on));
    dlg::set_enabled(h(d.limit_val), on);
    let kb: u64 = unsafe { window_text(h(d.limit_val)) }
        .trim()
        .parse()
        .unwrap_or(512);
    let bps = if on { kb.max(1) * 1024 } else { 0 };
    d.limit_cache = None; // the next tick re-syncs from the payload
    if bps == d.speed_limit {
        return;
    }
    if let Some(c) = crate::ctx() {
        let _ = ndm_core::set_entry_limit(c, d.id.clone(), bps);
    }
}

// ----------------------------------------------------------------
// Tab 3 — Options on completion (§2.3/G2): independent checkboxes
// over the entry's completion-action set; the show-complete box
// writes the tri-state override, the rest rewrite the set. No
// hang-up modem (the spec's deliberate omission).
// ----------------------------------------------------------------

/// The entry's action set, with the legacy single-string field
/// as the seed when the set is empty (the web UI's
/// completionActionsOf, same rule).
fn effective_actions(view: &ProgressView) -> Vec<String> {
    if !view.completion_actions.is_empty() {
        view.completion_actions.clone()
    } else if !view.completion.is_empty() {
        vec![view.completion.clone()]
    } else {
        Vec::new()
    }
}

fn sync_completion(d: &mut ProgDlg, view: &ProgressView) {
    let focus = unsafe { GetFocus() };
    let panel = [d.c_show, d.c_file, d.c_folder, d.c_exit, d.c_power, d.r_shutdown, d.r_restart];
    if panel.contains(&raw(focus)) {
        return; // never flip a box under the user's click
    }
    let actions = effective_actions(view);
    let show = match view.show_complete_override {
        Some(b) => b,
        None => actions.iter().any(|a| a == "show_complete") || d.show_complete_global,
    };
    let has = |name: &str| actions.iter().any(|a| a == name);
    let power = has("turn_off_pc") || has("restart_pc");
    let want = [
        show,
        has("open_file"),
        has("open_folder"),
        has("exit_app"),
        power,
        has("restart_pc"),
    ];
    if d.completion_cache.as_ref() != Some(&want) {
        dlg::set_checked(h(d.c_show), want[0]);
        dlg::set_checked(h(d.c_file), want[1]);
        dlg::set_checked(h(d.c_folder), want[2]);
        dlg::set_checked(h(d.c_exit), want[3]);
        dlg::set_checked(h(d.c_power), want[4]);
        dlg::set_checked(h(d.r_restart), want[5]);
        dlg::set_checked(h(d.r_shutdown), !want[5]);
        dlg::set_enabled(h(d.r_shutdown), want[4]);
        dlg::set_enabled(h(d.r_restart), want[4]);
        d.completion_cache = Some(want);
    }
}

/// Rewrite the whole action set from the tab's checkboxes. The
/// power checkbox owns one set slot (turn_off_pc or restart_pc,
/// per the Shut down/Restart choice beside it); unmanaged
/// members (show_complete) are preserved untouched.
fn push_completion(d: &mut ProgDlg) {
    let mut actions: Vec<String> = d
        .actions
        .iter()
        .filter(|a| {
            !matches!(
                a.as_str(),
                "open_file" | "open_folder" | "exit_app" | "turn_off_pc" | "restart_pc"
            )
        })
        .cloned()
        .collect();
    if dlg::is_checked(h(d.c_file)) {
        actions.push("open_file".to_string());
    }
    if dlg::is_checked(h(d.c_folder)) {
        actions.push("open_folder".to_string());
    }
    if dlg::is_checked(h(d.c_exit)) {
        actions.push("exit_app".to_string());
    }
    let power = dlg::is_checked(h(d.c_power));
    dlg::set_enabled(h(d.r_shutdown), power);
    dlg::set_enabled(h(d.r_restart), power);
    if power {
        actions.push(
            if dlg::is_checked(h(d.r_restart)) {
                "restart_pc"
            } else {
                "turn_off_pc"
            }
            .to_string(),
        );
    }
    d.completion_cache = None; // re-sync from the next payload
    if let Some(c) = crate::ctx() {
        let _ = ndm_core::set_completion_actions(c, d.id.clone(), actions);
    }
}

// ----------------------------------------------------------------
// Command handlers + visibility.
// ----------------------------------------------------------------
fn with_dlg(hwnd: HWND, f: impl FnOnce(&mut ProgDlg)) {
    let Some(id) = id_of(hwnd) else { return };
    let mut guard = DLGS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = guard.get_mut(&id) {
        f(d);
    }
}

fn refresh_visibility(d: &ProgDlg) {
    let show = |list: &[isize], on: bool| {
        for &r in list {
            unsafe {
                let _ = ShowWindow(h(r), if on { SW_SHOW } else { SW_HIDE });
            }
        }
    };
    show(&d.page2, d.tab_index == 1);
    show(&d.page3, d.tab_index == 2);
    show(&d.page1, d.tab_index == 0);
    if d.tab_index == 0 {
        show(&d.details, d.expanded);
        if d.video {
            // G8: the connections table stays hidden for video.
            unsafe {
                let _ = ShowWindow(h(d.conns), SW_HIDE);
            }
        }
    }
}

fn toggle_details(hwnd: HWND) {
    with_dlg(hwnd, |d| {
        d.expanded = !d.expanded;
        let expanded = d.expanded;
        unsafe {
            set_window_text(
                h(d.btn_hide),
                if expanded { "<< Hide details" } else { "Show details >>" },
            );
            let mut rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rect);
            let target = if expanded {
                d.outer_h
            } else {
                d.outer_h - DETAILS_H
            };
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                rect.right - rect.left,
                target,
                SWP_NOMOVE | SWP_NOZORDER,
            );
            d.tab_h += if expanded { DETAILS_H } else { -DETAILS_H };
            let tab_h = d.tab_h;
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(h(d.tab), 8, 8, WIN_W - 16, tab_h, true);
        }
        refresh_visibility(d);
    });
}

fn on_command(hwnd: HWND, id: i32, code: u32) {
    match id {
        ID_BTN_RESUME => with_dlg(hwnd, |d| {
            if let Some(c) = crate::ctx() {
                let id = d.id.clone();
                ndm_core::rt::spawn(async move {
                    let _ = ndm_core::resume_download(c, id).await;
                    crate::post_refresh();
                });
            }
        }),
        ID_BTN_PAUSE => with_dlg(hwnd, |d| {
            if let Some(c) = crate::ctx() {
                let _ = ndm_core::pause_download(c, d.id.clone());
            }
            crate::post_refresh();
        }),
        ID_BTN_CANCEL => {
            // §1.4/G3: Cancel = the pause path + close; the entry
            // and its partial progress stay, resumable.
            with_dlg(hwnd, |d| {
                if let Some(c) = crate::ctx() {
                    let _ = ndm_core::pause_download(c, d.id.clone());
                }
            });
            crate::post_refresh();
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        ID_BTN_HIDE => toggle_details(hwnd),
        ID_LIMIT_ON => with_dlg(hwnd, apply_limit),
        ID_LIMIT_VAL => {
            if code == EN_KILLFOCUS {
                with_dlg(hwnd, apply_limit);
            }
        }
        ID_C_SHOW => with_dlg(hwnd, |d| {
            let on = dlg::is_checked(h(d.c_show));
            d.completion_cache = None;
            if let Some(c) = crate::ctx() {
                let _ = ndm_core::set_show_complete_override(c, d.id.clone(), Some(on));
            }
        }),
        ID_C_FILE | ID_C_FOLDER | ID_C_EXIT | ID_C_POWER | ID_R_SHUTDOWN | ID_R_RESTART => {
            with_dlg(hwnd, push_completion)
        }
        ID_DESC => {
            if code == EN_KILLFOCUS {
                with_dlg(hwnd, |d| {
                    let text = unsafe { window_text(h(d.desc)) }.trim().to_string();
                    if text != d.last_desc {
                        d.last_desc = text.clone();
                        if let Some(c) = crate::ctx() {
                            let _ = ndm_core::set_entry_description(c, d.id.clone(), text);
                        }
                    }
                });
            }
        }
        ID_QUEUE => {
            if code == CBN_SELCHANGE {
                with_dlg(hwnd, |d| {
                    let idx = unsafe { send(h(d.queue), CB_GETCURSEL, WPARAM(0), LPARAM(0)) };
                    if idx >= 0 {
                        if let Some(q) = d.queues.get(idx as usize).cloned() {
                            if q != d.last_queue {
                                d.last_queue = q.clone();
                                if let Some(c) = crate::ctx() {
                                    let _ = ndm_core::set_entry_queue(c, d.id.clone(), q);
                                }
                            }
                        }
                    }
                });
            }
        }
        _ => {}
    }
}

fn id_of(hwnd: HWND) -> Option<String> {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if p == 0 {
            None
        } else {
            Some((*(p as *const String)).clone())
        }
    }
}

unsafe extern "system" fn prog_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = l.0 as *const CREATESTRUCTW;
            let args = Box::from_raw((*cs).lpCreateParams as *mut CreateArgs);
            let view = args.view.clone();
            let id = view.id.clone();
            // `as _`: SetWindowLongPtrW's value is isize on
            // x86_64 but i32 on 32-bit Windows (see update_bar).
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(Box::new(id.clone())) as _);
            let state = build(hwnd, *args);
            {
                let mut guard = DLGS.lock().unwrap_or_else(|e| e.into_inner());
                guard.insert(id.clone(), state);
            }
            {
                let mut wins = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
                wins.insert(id.clone(), raw(hwnd));
            }
            {
                let mut owner = STRIP_OWNER.lock().unwrap_or_else(|e| e.into_inner());
                if owner.is_empty() {
                    *owner = id.clone();
                }
            }
            // Page 1 is the opening tab; pages 2/3 start hidden.
            with_dlg(hwnd, |d| refresh_visibility(d));
            apply(hwnd, &view);
            // If this dialog is the strip owner and its first
            // payload matched the empty signature, the shared
            // snapshot could still hold a closed dialog's map —
            // publish unconditionally once at open.
            let is_owner = {
                let owner = STRIP_OWNER.lock().unwrap_or_else(|e| e.into_inner());
                *owner == id
            };
            if is_owner {
                with_dlg(hwnd, |d| publish_strip(d));
            }
            LRESULT(0)
        }
        WM_PD_PROGRESS => {
            if l.0 != 0 {
                let view = *Box::from_raw(l.0 as *mut ProgressView);
                apply(hwnd, &view);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, (w.0 & 0xffff) as i32, ((w.0 >> 16) & 0xffff) as u32);
            LRESULT(0)
        }
        WM_NOTIFY => {
            let hdr = l.0 as *const NMHDR;
            if !hdr.is_null() && (*hdr).idFrom == ID_TAB as usize && (*hdr).code == TCN_SELCHANGE {
                with_dlg(hwnd, |d| {
                    d.tab_index = TabControl { hwnd: h(d.tab) }.cur_sel().max(0);
                    refresh_visibility(d);
                });
            }
            LRESULT(0)
        }
        WM_DRAWITEM => {
            let dis = l.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() && (*dis).CtlID == ID_STRIP as u32 {
                // SS_OWNERDRAW: paint the strip snapshot into the
                // item rect on the item DC (the platform pairing).
                crate::winui::customdraw::paint_segment_strip((*dis).hDC, &(*dis).rcItem);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_ACTIVATE => {
            // Becoming the active dialog makes this dialog the
            // strip owner: republish its recorded strip inputs.
            if (w.0 & 0xffff) != 0 {
                if let Some(id) = id_of(hwnd) {
                    *STRIP_OWNER.lock().unwrap_or_else(|e| e.into_inner()) = id;
                    with_dlg(hwnd, |d| publish_strip(d));
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // Closing never stops the download (only the Cancel
            // button does, §1.3).
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(id) = id_of(hwnd) {
                DLGS.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                WINDOWS.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                USER_CLOSED
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone());
                let mut owner = STRIP_OWNER.lock().unwrap_or_else(|e| e.into_inner());
                if *owner == id {
                    owner.clear();
                }
            }
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                drop(Box::from_raw(p as *mut String));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}
