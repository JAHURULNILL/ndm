//! Options dialog (ui/app.js omodal / openOptions + oSave) — one
//! modeless window, five tabs exactly as the web UI groups them:
//! General / Connection / Downloads / Proxy & Site Logins /
//! Categories. Every field is staged from core's Settings (read
//! through the JSON seam, the queue_names pattern) and Save writes
//! the whole object back through `set_settings`; a failed save
//! keeps the dialog open and says so (app.js F-4). The Scheduler's
//! queue fields are NOT here — app.js keeps them in the Scheduler
//! dialog, and so does this shell (scheduler_dialog.rs).
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell.

use crate::winui::dialog as dlg;
use crate::winui::listview::{ColumnSpec, ListView};
use crate::winui::tab::TabControl;
use crate::winui::{pcw, send, set_window_text, wide, window_text};
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::Controls::NMHDR;
use windows::Win32::UI::Controls::TCN_SELCHANGE;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW,
    RegisterClassW, SetForegroundWindow, ShowWindow, BS_AUTORADIOBUTTON, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, ES_AUTOHSCROLL, ES_PASSWORD, IDC_ARROW, SW_RESTORE,
    SW_SHOW, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_NOTIFY, WNDCLASSW,
    WS_BORDER, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE,
};

const ID_TAB: i32 = 8001;
const ID_SAVE: i32 = 8901;
const ID_CANCEL: i32 = 8902;
const ID_LOGIN_ADD: i32 = 8903;
const ID_LOGIN_REMOVE: i32 = 8904;
const ID_CAT_RENAME: i32 = 8905;
const ID_CAT_DELETE: i32 = 8906;
const ID_LOGINS: i32 = 8907;
const ID_CATS: i32 = 8908;
const ID_STATUS: i32 = 8909;
const FIELD_ID_BASE: i32 = 8100;

const WIN_W: i32 = 680;
const WIN_H: i32 = 632;
const TAB_LABELS: [&str; 5] = ["General", "Connection", "Downloads", "Proxy & Site Logins", "Categories"];
const CATS: [&str; 6] = ["Compressed", "Documents", "Music", "Video", "Programs", "General"];
const CATS_WITH_TYPES: [&str; 5] = ["Compressed", "Documents", "Music", "Video", "Programs"];
const BUILTINS: [&str; 7] = ["General", "Compressed", "Documents", "Music", "Video", "Programs", "Grabber"];

/// app.js BUILTIN_CATEGORY_EXTS (the Categories editor compares
/// against these to decide what is worth storing).
fn builtin_exts(cat: &str) -> Vec<&'static str> {
    match cat {
        "Compressed" => vec!["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "tgz", "tbz2", "txz"],
        "Programs" => vec!["exe", "msi", "msix", "msixbundle", "deb", "rpm", "dmg", "pkg", "appimage", "apk", "bin", "run", "cab", "jar", "war", "vmdk", "qcow2", "vdi", "iso", "img", "flatpakref"],
        "Video" => vec!["mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "m4v", "ts"],
        "Music" => vec!["mp3", "m4a", "m4b", "flac", "wav", "ogg", "opus", "aac", "wma"],
        "Documents" => vec!["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "csv", "rtf", "epub", "mobi"],
        _ => vec![],
    }
}

struct OptDlg {
    fields: HashMap<String, isize>,
    ids: HashMap<i32, String>,
    combo_values: HashMap<String, Vec<String>>,
    pages: [Vec<isize>; 5],
    tab: isize,
    tab_index: i32,
    base: serde_json::Value,
    logins: Vec<(String, String, String)>,
    cat_names: Vec<String>,
    status: isize,
    next_id: i32,
}

static DLG: Mutex<Option<OptDlg>> = Mutex::new(None);
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
fn jb(v: &serde_json::Value, key: &str, dflt: bool) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(dflt)
}
fn ju(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

pub fn show(parent: HWND) {
    if WIN_RAW.load(Ordering::SeqCst) != 0 {
        let hwnd = h(WIN_RAW.load(Ordering::SeqCst));
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        return;
    }
    if crate::ctx().is_none() {
        return;
    }
    let base = crate::settings_value();
    register_class();
    unsafe {
        let class_buf = wide("NdmOptionsDialog");
        let title_buf = wide("Options");
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rect = RECT { left: 0, top: 0, right: WIN_W, bottom: WIN_H };
        let _ = AdjustWindowRectEx(&mut rect, style, false, Default::default());
        let boxed = Box::into_raw(Box::new(base));
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
        let class_buf = wide("NdmOptionsDialog");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(opt_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

// ----------------------------------------------------------------
// Build helpers — every named control lands in `fields` (by name)
// and its page's handle list (for tab show/hide).
// ----------------------------------------------------------------
struct B {
    parent: HWND,
}

impl B {
    unsafe fn ctl(&self, d: &mut OptDlg, page: usize, name: &str, class: &str, text: &str, style: u32, x: i32, y: i32, w: i32, ht: i32) -> HWND {
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
    unsafe fn label(&self, d: &mut OptDlg, page: usize, text: &str, x: i32, y: i32, w: i32) -> HWND {
        let hwnd = dlg::create_child(self.parent, "STATIC", text, WS_CHILD | WS_VISIBLE, x, y, w, 20, 0);
        d.pages[page].push(raw(hwnd));
        hwnd
    }
    unsafe fn edit(&self, d: &mut OptDlg, page: usize, name: &str, label: &str, y: i32, value: &str, password: bool) {
        self.label(d, page, label, 20, y + 3, 190);
        let mut style = WS_BORDER.0 | ES_AUTOHSCROLL as u32;
        if password {
            style |= ES_PASSWORD as u32;
        }
        self.ctl(d, page, name, "EDIT", value, style, 214, y, 446, 23);
    }
    unsafe fn check(&self, d: &mut OptDlg, page: usize, name: &str, label: &str, y: i32, on: bool) {
        let hwnd = self.ctl(d, page, name, "BUTTON", label, 0x0002, 20, y, 640, 22); // BS_AUTOCHECKBOX
        dlg::set_checked(hwnd, on);
    }
    unsafe fn combo(&self, d: &mut OptDlg, page: usize, name: &str, label: &str, y: i32, pairs: &[(&str, &str)], value: &str) {
        self.label(d, page, label, 20, y + 3, 190);
        let hwnd = self.ctl(d, page, name, "COMBOBOX", "", 0x0003, 214, y, 300, 120); // CBS_DROPDOWNLIST
        let mut values = Vec::new();
        for (i, (v, l)) in pairs.iter().enumerate() {
            let mut buf = wide(l);
            send(hwnd, 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize)); // CB_ADDSTRING
            values.push(v.to_string());
            let _ = i;
        }
        let sel = pairs.iter().position(|(v, _)| *v == value).unwrap_or(0);
        send(hwnd, 0x014E, WPARAM(sel), LPARAM(0)); // CB_SETCURSEL
        d.combo_values.insert(name.to_string(), values);
    }
}

fn get_text(d: &OptDlg, name: &str) -> String {
    d.fields.get(name).map(|&r| unsafe { window_text(h(r)) }).unwrap_or_default()
}
fn get_check(d: &OptDlg, name: &str) -> bool {
    d.fields.get(name).map(|&r| dlg::is_checked(h(r))).unwrap_or(false)
}
fn get_combo(d: &OptDlg, name: &str) -> String {
    let (Some(&r), Some(vals)) = (d.fields.get(name), d.combo_values.get(name)) else {
        return String::new();
    };
    let idx = unsafe { send(h(r), 0x0147, WPARAM(0), LPARAM(0)) }; // CB_GETCURSEL
    vals.get(idx.max(0) as usize).cloned().unwrap_or_default()
}
fn set_status(d: &OptDlg, text: &str) {
    unsafe { set_window_text(h(d.status), text) };
}

fn build(hwnd: HWND, base: serde_json::Value) {
    let b = B { parent: hwnd };
    let mut d = OptDlg {
        fields: HashMap::new(),
        ids: HashMap::new(),
        combo_values: HashMap::new(),
        pages: [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        tab: 0,
        tab_index: 0,
        logins: Vec::new(),
        cat_names: Vec::new(),
        status: 0,
        next_id: FIELD_ID_BASE,
        base: serde_json::Value::Null,
    };
    unsafe {
        let tab = TabControl::create(hwnd, ID_TAB, &TAB_LABELS)
            .map(|t| {
                let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(t.hwnd, 8, 8, WIN_W - 16, WIN_H - 64, true);
                t.hwnd
            })
            .unwrap_or_default();
        d.tab = raw(tab);
        let s = &base;

        // --- General ---
        b.edit(&mut d, 0, "default_dir", "Default download folder", 44, js(s, "default_dir"), false);
        b.check(&mut d, 0, "auto_resume", "Auto-resume unfinished downloads on start", 76, jb(s, "auto_resume", false));
        b.check(&mut d, 0, "auto_update", "Automatically check for updates", 102, jb(s, "auto_update", true));
        b.check(&mut d, 0, "clipboard_monitor", "Watch the clipboard for download links", 128, jb(s, "clipboard_monitor", false));
        b.check(&mut d, 0, "exit_when_done", "Exit NDM when all downloads finish", 154, jb(s, "exit_when_done", false));
        let engine = crate::ctx()
            .map(|c| ndm_core::components_status(c))
            .map(|v| {
                if v.get("ready").and_then(|x| x.as_bool()) == Some(true) {
                    "Ready (video engine installed)".to_string()
                } else {
                    "Sets itself up automatically in the background".to_string()
                }
            })
            .unwrap_or_else(|| "—".to_string());
        b.label(&mut d, 0, "Video engine", 20, 183, 190);
        b.label(&mut d, 0, &engine, 214, 183, 446);
        b.combo(
            &mut d, 0, "video_quality", "Default video quality", 208,
            &[("best", "Best quality"), ("2160", "2160p (4K)"), ("1440", "1440p"), ("1080", "1080p"), ("720", "720p"), ("480", "480p"), ("360", "360p"), ("240", "240p"), ("audio", "Audio only")],
            js(s, "video_quality"),
        );

        // --- Connection ---
        b.edit(&mut d, 1, "max_connections", "Max connections per download", 44, &ju(s, "max_connections").to_string(), false);
        let exc = s
            .get("conn_exceptions")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        b.edit(&mut d, 1, "conn_exceptions", "Per-server connections", 76, &exc, false);
        b.label(&mut d, 1, "Servers listed here get fewer parallel connections than the global setting.", 20, 104, 640);
        let limit_on = ju(s, "speed_limit_bps") > 0;
        b.check(&mut d, 1, "limit_on", "Limit download speed", 132, limit_on);
        let limit_val = if limit_on {
            format!("{}", (ju(s, "speed_limit_bps") as f64 / 1_048_576.0 * 1000.0).round() / 1000.0)
        } else {
            "2".to_string()
        };
        b.edit(&mut d, 1, "limit_val", "Speed limit (MB/s)", 160, &limit_val, false);
        b.edit(&mut d, 1, "quota_mb", "Download limit (MB per period, 0 = off)", 192, &ju(s, "quota_mb").to_string(), false);
        let qh = ju(s, "quota_hours");
        b.edit(&mut d, 1, "quota_hours", "Limit period (hours)", 224, &(if qh == 0 { 5 } else { qh }).to_string(), false);

        // --- Downloads ---
        b.edit(&mut d, 2, "max_simultaneous", "Max simultaneous downloads", 44, &ju(s, "max_simultaneous").to_string(), false);
        b.label(&mut d, 2, "0 = unlimited simultaneous downloads.", 20, 72, 640);
        b.check(&mut d, 2, "show_complete_dialog", "Show the \"Download complete\" dialog", 98, jb(s, "show_complete_dialog", true));
        b.combo(
            &mut d, 2, "progress_dialog_view", "Download progress dialog", 126,
            &[("show", "Show"), ("minimized", "Minimized"), ("never", "Don't show")],
            js(s, "progress_dialog_view"),
        );
        b.label(&mut d, 2, "How the per-download progress dialog opens when a download starts in the app.", 20, 154, 640);
        b.check(&mut d, 2, "sound_events", "Play a sound when a download completes or fails", 180, jb(s, "sound_events", true));
        b.label(&mut d, 2, "When all downloads finish", 20, 212, 300);
        b.combo(
            &mut d, 2, "power_when_done", "Computer power action", 236,
            &[("", "Do nothing"), ("shutdown", "Shut down"), ("restart", "Restart"), ("sleep", "Sleep"), ("hibernate", "Hibernate"), ("logoff", "Log off")],
            js(s, "power_when_done"),
        );
        b.check(&mut d, 2, "power_force", "Force running programs to close with the power action", 268, jb(s, "power_force", false));

        // --- Proxy & Site Logins ---
        b.label(&mut d, 3, "Proxy / Socks — how downloads reach the internet", 20, 36, 640);
        let mode = {
            let m = js(s, "proxy_mode");
            if !m.is_empty() {
                m.to_string()
            } else if !js(s, "proxy").is_empty() {
                "manual".to_string()
            } else {
                "none".to_string()
            }
        };
        for (i, (val, label)) in [
            ("none", "No proxy — direct connection"),
            ("system", "Use system settings"),
            ("pac", "Use automatic configuration script (PAC)"),
            ("manual", "Manual proxy configuration"),
        ]
        .iter()
        .enumerate()
        {
            let hwnd = b.ctl(&mut d, 3, &format!("proxy_mode_{val}"), "BUTTON", label, BS_AUTORADIOBUTTON as u32, 20, 60 + i as i32 * 24, 500, 22);
            dlg::set_checked(hwnd, *val == mode);
        }
        b.edit(&mut d, 3, "proxy_pac_url", "Configuration script address", 160, js(s, "proxy_pac_url"), false);
        b.combo(&mut d, 3, "proxy_scheme", "Proxy type", 186, &[("http", "HTTP"), ("socks5", "SOCKS5"), ("socks4", "SOCKS4")], {
            let v = js(s, "proxy_scheme");
            if v.is_empty() { "http" } else { v }
        });
        b.edit(&mut d, 3, "proxy_host", "Proxy server address", 212, js(s, "proxy_host"), false);
        let port = ju(s, "proxy_port");
        b.edit(&mut d, 3, "proxy_port", "Port", 238, &(if port == 0 { String::new() } else { port.to_string() }), false);
        b.edit(&mut d, 3, "proxy_user", "Proxy user name", 264, js(s, "proxy_user"), false);
        b.edit(&mut d, 3, "proxy_pass", "Proxy password", 290, js(s, "proxy_pass"), true);
        b.label(&mut d, 3, "Use this proxy for", 20, 323, 150);
        for (i, (name, label, on)) in [
            ("proxy_http", "HTTP", jb(s, "proxy_http", true)),
            ("proxy_https", "HTTPS", jb(s, "proxy_https", true)),
            ("proxy_ftp", "FTP", jb(s, "proxy_ftp", true)),
        ]
        .iter()
        .enumerate()
        {
            let hwnd = b.ctl(&mut d, 3, name, "BUTTON", label, 0x0002, 214 + i as i32 * 90, 320, 84, 22);
            dlg::set_checked(hwnd, *on);
        }
        let br = b.ctl(&mut d, 3, "proxy_from_browser_on_error", "BUTTON", "If a download caught from the browser fails, retry it once through the system proxy", 0x0002, 20, 348, 640, 22);
        dlg::set_checked(br, jb(s, "proxy_from_browser_on_error", false));
        b.label(&mut d, 3, "Site Logins — a saved login is used automatically for downloads from that site.", 20, 378, 640);
        let logins_lv = ListView::create(
            hwnd,
            ID_LOGINS,
            &[ColumnSpec { label: "Site / path", width: 240 }, ColumnSpec { label: "User name", width: 200 }, ColumnSpec { label: "Password", width: 100 }],
        )
        .map(|lv| {
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(lv.hwnd, 20, 402, 520, 58, true);
            lv.hwnd
        })
        .unwrap_or_default();
        d.pages[3].push(raw(logins_lv));
        let rm = dlg::create_child(hwnd, "BUTTON", "Remove", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 548, 402, 112, 26, ID_LOGIN_REMOVE);
        d.pages[3].push(raw(rm));
        b.edit(&mut d, 3, "login_site", "Site / path", 466, "", false);
        b.edit(&mut d, 3, "login_user", "User name", 492, "", false);
        b.edit(&mut d, 3, "login_pass", "Password", 518, "", true);
        let add = dlg::create_child(hwnd, "BUTTON", "Add login", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 20, 546, 112, 24, ID_LOGIN_ADD);
        d.pages[3].push(raw(add));

        // --- Categories ---
        b.label(&mut d, 4, "Each category has its own folder and its own file types.", 20, 40, 640);
        let dirs = s.get("category_dirs").cloned().unwrap_or(serde_json::json!({}));
        let exts = s.get("category_exts").cloned().unwrap_or(serde_json::json!({}));
        let mut y = 66;
        for cat in CATS {
            let dir = dirs.get(cat).and_then(|x| x.as_str()).unwrap_or("");
            b.edit(&mut d, 4, &format!("cat_dir_{cat}"), &format!("{cat} folder"), y, dir, false);
            y += 26;
            if CATS_WITH_TYPES.contains(&cat) {
                let list = exts
                    .get(cat)
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_else(|| builtin_exts(cat).join(", "));
                b.edit(&mut d, 4, &format!("cat_ext_{cat}"), &format!("{cat} types"), y, &list, false);
                y += 26;
            }
        }
        b.label(&mut d, 4, "Manage categories — built-in categories cannot be renamed or deleted.", 20, y + 4, 640);
        let cats_lv = ListView::create(
            hwnd,
            ID_CATS,
            &[ColumnSpec { label: "Category", width: 300 }, ColumnSpec { label: "", width: 120 }],
        )
        .map(|lv| {
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(lv.hwnd, 20, y + 28, 400, 92, true);
            lv.hwnd
        })
        .unwrap_or_default();
        d.pages[4].push(raw(cats_lv));
        let ren = dlg::create_child(hwnd, "BUTTON", "Rename", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 428, y + 28, 104, 26, ID_CAT_RENAME);
        let del = dlg::create_child(hwnd, "BUTTON", "Delete", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 428, y + 60, 104, 26, ID_CAT_DELETE);
        d.pages[4].extend([raw(ren), raw(del)]);

        // Status + Save/Cancel (outside the tab pages).
        let status = dlg::create_child(hwnd, "STATIC", "", WS_CHILD | WS_VISIBLE, 16, WIN_H - 40, 400, 20, ID_STATUS);
        d.status = raw(status);
        dlg::create_child(hwnd, "BUTTON", "Save", WS_CHILD | WS_VISIBLE | WS_TABSTOP, WIN_W - 208, WIN_H - 44, 92, 28, ID_SAVE);
        dlg::create_child(hwnd, "BUTTON", "Cancel", WS_CHILD | WS_VISIBLE | WS_TABSTOP, WIN_W - 108, WIN_H - 44, 92, 28, ID_CANCEL);

        d.base = base;
        // staged logins
        if let Some(arr) = d.base.get("site_logins").and_then(|x| x.as_array()) {
            for l in arr {
                d.logins.push((
                    js(l, "site").to_string(),
                    js(l, "login").to_string(),
                    js(l, "password").to_string(),
                ));
            }
        }
    }
    *DLG.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
    refresh_logins();
    refresh_cats();
    sync_proxy_mode();
    apply_tab_visibility();
}

fn pages_get(d: &OptDlg, page: usize) -> Vec<isize> {
    d.pages[page].clone()
}

fn apply_tab_visibility() {
    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_ref() else { return };
    for p in 0..5 {
        let on = p as i32 == d.tab_index;
        for &r in &pages_get(d, p) {
            unsafe {
                let _ = ShowWindow(h(r), if on { SW_SHOW } else { windows::Win32::UI::WindowsAndMessaging::SW_HIDE });
            }
        }
    }
}

fn proxy_mode_of(d: &OptDlg) -> String {
    for val in ["none", "system", "pac", "manual"] {
        if get_check(d, &format!("proxy_mode_{val}")) {
            return val.to_string();
        }
    }
    "none".to_string()
}

fn sync_proxy_mode() {
    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_ref() else { return };
    let mode = proxy_mode_of(d);
    if let Some(&r) = d.fields.get("proxy_pac_url") {
        dlg::set_enabled(h(r), mode == "pac");
    }
    for name in ["proxy_scheme", "proxy_host", "proxy_port", "proxy_user", "proxy_pass", "proxy_http", "proxy_https", "proxy_ftp"] {
        if let Some(&r) = d.fields.get(name) {
            dlg::set_enabled(h(r), mode == "manual");
        }
    }
}

fn logins_lv() -> Option<ListView> {
    // The logins ListView HWND is the one created with ID_LOGINS;
    // it is tracked as the first page-3 handle after the fields —
    // simpler: find it via GetDlgItem on the window.
    let r = WIN_RAW.load(Ordering::SeqCst);
    if r == 0 {
        return None;
    }
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
        GetDlgItem(Some(h(r)), ID_LOGINS)
            .ok()
            .map(|hwnd| ListView { hwnd })
    }
}

fn cats_lv() -> Option<ListView> {
    let r = WIN_RAW.load(Ordering::SeqCst);
    if r == 0 {
        return None;
    }
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
        GetDlgItem(Some(h(r)), ID_CATS)
            .ok()
            .map(|hwnd| ListView { hwnd })
    }
}

fn refresh_logins() {
    let Some(lv) = logins_lv() else { return };
    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_ref() else { return };
    lv.clear();
    for (i, (site, login, _)) in d.logins.iter().enumerate() {
        lv.insert_row(i as i32, &[site.clone(), login.clone(), "••••••".to_string()]);
    }
}

fn managed_category_names() -> Vec<String> {
    let mut names: Vec<String> = vec!["General".to_string()];
    for c in CATS_WITH_TYPES {
        names.push(c.to_string());
    }
    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = guard.as_ref() {
        if let Some(dirs) = d.base.get("category_dirs").and_then(|x| x.as_object()) {
            for k in dirs.keys() {
                if !names.iter().any(|n| n.eq_ignore_ascii_case(k)) {
                    names.push(k.clone());
                }
            }
        }
        if let Some(exts) = d.base.get("category_exts").and_then(|x| x.as_object()) {
            for k in exts.keys() {
                if !names.iter().any(|n| n.eq_ignore_ascii_case(k)) {
                    names.push(k.clone());
                }
            }
        }
    }
    drop(guard);
    if let Some(c) = crate::ctx() {
        let settings = ndm_core::get_settings(c.clone());
        for n in ndm_core::category_names_in_use(c.state(), &settings) {
            if !names.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                names.push(n);
            }
        }
    }
    {
        let cache = crate::cache_snapshot();
        for v in &cache {
            if !v.category.is_empty() && !names.iter().any(|x| x.eq_ignore_ascii_case(&v.category)) {
                names.push(v.category.clone());
            }
        }
    }
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    names
}

fn refresh_cats() {
    let names = managed_category_names();
    let Some(lv) = cats_lv() else { return };
    lv.clear();
    for (i, n) in names.iter().enumerate() {
        let tag = if BUILTINS.iter().any(|b| b.eq_ignore_ascii_case(n)) { "built-in" } else { "" };
        lv.insert_row(i as i32, &[n.clone(), tag.to_string()]);
    }
    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = guard.as_mut() {
        d.cat_names = names;
    }
}

fn save(hwnd: HWND) {
    let mut out;
    {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_ref() else { return };
        out = d.base.clone();
        let num = |name: &str, dflt: u64| get_text(d, name).trim().parse::<u64>().unwrap_or(dflt);
        let dir_text = get_text(d, "default_dir");
        out["default_dir"] = serde_json::json!(if dir_text.trim().is_empty() {
            "~/Downloads".to_string()
        } else {
            dir_text.trim().to_string()
        });
        out["max_connections"] = serde_json::json!(num("max_connections", 8).clamp(1, 32));
        out["speed_limit_bps"] = serde_json::json!(if get_check(d, "limit_on") {
            let v: f64 = get_text(d, "limit_val").trim().parse().unwrap_or(1.0);
            (v.max(0.1) * 1_048_576.0).round() as u64
        } else {
            0
        });
        out["max_simultaneous"] = serde_json::json!(num("max_simultaneous", 0).min(10));
        out["auto_resume"] = serde_json::json!(get_check(d, "auto_resume"));
        out["auto_update"] = serde_json::json!(get_check(d, "auto_update"));
        out["video_quality"] = serde_json::json!(get_combo(d, "video_quality"));
        out["proxy_mode"] = serde_json::json!(proxy_mode_of(d));
        out["proxy_pac_url"] = serde_json::json!(get_text(d, "proxy_pac_url").trim());
        out["proxy_scheme"] = serde_json::json!(get_combo(d, "proxy_scheme"));
        out["proxy_host"] = serde_json::json!(get_text(d, "proxy_host").trim());
        out["proxy_port"] = serde_json::json!(num("proxy_port", 0).min(65535));
        out["proxy_user"] = serde_json::json!(get_text(d, "proxy_user").trim());
        out["proxy_pass"] = serde_json::json!(get_text(d, "proxy_pass"));
        out["proxy_http"] = serde_json::json!(get_check(d, "proxy_http"));
        out["proxy_https"] = serde_json::json!(get_check(d, "proxy_https"));
        out["proxy_ftp"] = serde_json::json!(get_check(d, "proxy_ftp"));
        out["proxy_from_browser_on_error"] = serde_json::json!(get_check(d, "proxy_from_browser_on_error"));
        out["site_logins"] = serde_json::json!(d
            .logins
            .iter()
            .map(|(site, login, password)| serde_json::json!({ "site": site, "login": login, "password": password }))
            .collect::<Vec<_>>());
        out["power_when_done"] = serde_json::json!(get_combo(d, "power_when_done"));
        out["power_force"] = serde_json::json!(get_check(d, "power_force"));
        let mut dirs = serde_json::Map::new();
        for cat in CATS {
            let v = get_text(d, &format!("cat_dir_{cat}"));
            if !v.trim().is_empty() {
                dirs.insert(cat.to_string(), serde_json::json!(v.trim()));
            }
        }
        out["category_dirs"] = serde_json::Value::Object(dirs);
        // category_exts (app.js oSave): non-built-in keys survive;
        // a built-in is stored only when its list differs from the
        // built-in table (an emptied field stores [] on purpose).
        let mut exts = serde_json::Map::new();
        if let Some(old) = d.base.get("category_exts").and_then(|x| x.as_object()) {
            for (k, v) in old {
                if !CATS_WITH_TYPES.contains(&k.as_str()) {
                    exts.insert(k.clone(), v.clone());
                }
            }
        }
        for cat in CATS_WITH_TYPES {
            let text = get_text(d, &format!("cat_ext_{cat}"));
            let mut seen = std::collections::HashSet::new();
            let list: Vec<String> = text
                .split(|c: char| c == ',' || c.is_whitespace())
                .map(|s| s.trim().trim_start_matches('.').to_lowercase())
                .filter(|s| !s.is_empty() && seen.insert(s.clone()))
                .collect();
            let builtin = builtin_exts(cat);
            let same = list.len() == builtin.len() && list.iter().all(|x| builtin.contains(&x.as_str()));
            if !same {
                exts.insert(cat.to_string(), serde_json::json!(list));
            }
        }
        out["category_exts"] = serde_json::Value::Object(exts);
        out["conn_exceptions"] = serde_json::json!(get_text(d, "conn_exceptions")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>());
        out["exit_when_done"] = serde_json::json!(get_check(d, "exit_when_done"));
        out["sound_events"] = serde_json::json!(get_check(d, "sound_events"));
        out["show_complete_dialog"] = serde_json::json!(get_check(d, "show_complete_dialog"));
        out["progress_dialog_view"] = serde_json::json!(get_combo(d, "progress_dialog_view"));
        out["clipboard_monitor"] = serde_json::json!(get_check(d, "clipboard_monitor"));
        out["quota_mb"] = serde_json::json!(num("quota_mb", 0));
        out["quota_hours"] = serde_json::json!(num("quota_hours", 5).clamp(1, 168));
    }
    match crate::save_settings_value(std::mem::take(&mut out)) {
        Ok(()) => {
            crate::set_status(1, "Options saved");
            crate::post_refresh();
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        Err(e) => {
            // F-4: the dialog stays open and says so.
            let guard = DLG.lock().unwrap_or_else(|e2| e2.into_inner());
            if let Some(d) = guard.as_ref() {
                set_status(d, "These settings could not be saved — they apply until NDM closes, but will be lost.");
            }
            crate::set_status(1, &format!("Options not saved: {e}"));
        }
    }
}

fn on_command(hwnd: HWND, id: i32) {
    match id {
        ID_SAVE => save(hwnd),
        ID_CANCEL => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        ID_LOGIN_ADD => {
            let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            let Some(d) = guard.as_mut() else { return };
            let site = get_text(d, "login_site").trim().to_string();
            let login = get_text(d, "login_user").trim().to_string();
            let password = get_text(d, "login_pass");
            if site.is_empty() || login.is_empty() {
                set_status(d, "Give the site and the user name first");
                return;
            }
            if let Some(slot) = d
                .logins
                .iter_mut()
                .find(|(s, l, _)| s.eq_ignore_ascii_case(&site) && l.eq_ignore_ascii_case(&login))
            {
                *slot = (site, login, password);
            } else {
                d.logins.push((site, login, password));
            }
            for name in ["login_site", "login_user", "login_pass"] {
                if let Some(&r) = d.fields.get(name) {
                    unsafe { set_window_text(h(r), "") };
                }
            }
            drop(guard);
            refresh_logins();
        }
        ID_LOGIN_REMOVE => {
            let sel = logins_lv().and_then(|lv| lv.selected_rows().into_iter().next());
            if let Some(row) = sel {
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    if (row as usize) < d.logins.len() {
                        d.logins.remove(row as usize);
                    }
                }
                drop(guard);
                refresh_logins();
            }
        }
        ID_CAT_RENAME | ID_CAT_DELETE => {
            let sel = cats_lv().and_then(|lv| lv.selected_rows().into_iter().next());
            let name = {
                let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                match (guard.as_ref(), sel) {
                    (Some(d), Some(row)) => d.cat_names.get(row as usize).cloned(),
                    _ => None,
                }
            };
            let Some(name) = name else { return };
            if BUILTINS.iter().any(|b| b.eq_ignore_ascii_case(&name)) {
                return;
            }
            let Some(c) = crate::ctx() else { return };
            if id == ID_CAT_RENAME {
                if let Some(next) = dlg::show_prompt(hwnd, "Rename category", &format!("Rename the category \"{name}\" to:"), &name) {
                    let next = next.trim().to_string();
                    if !next.is_empty() && next != name {
                        match ndm_core::rename_category(c, name, next.clone()) {
                            Ok(()) => {
                                crate::set_status(1, &format!("Category renamed to \"{next}\""));
                                refresh_after_category_change(hwnd);
                            }
                            Err(e) => crate::set_status(1, &format!("Could not rename the category: {e}")),
                        }
                    }
                }
            } else if dlg::show_confirm(hwnd, "Delete category", &format!("Delete the category \"{name}\"?\nIts downloads move to General."), "Delete category") {
                match ndm_core::delete_category(c, name.clone()) {
                    Ok(()) => {
                        crate::set_status(1, &format!("Category \"{name}\" deleted — its downloads are in General now"));
                        refresh_after_category_change(hwnd);
                    }
                    Err(e) => crate::set_status(1, &format!("Could not delete the category: {e}")),
                }
            }
        }
        _ => {
            // Proxy-mode radios re-gate the mode's fields (N-01).
            let is_radio = {
                let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .as_ref()
                    .and_then(|d| d.ids.get(&id).cloned())
                    .map(|n| n.starts_with("proxy_mode_"))
                    .unwrap_or(false)
            };
            if is_radio {
                // Radio group behaviour: checking one unchecks the
                // rest (the buttons are plain BS_AUTORADIOBUTTON in
                // one visual group; enforce exclusivity here).
                let name = {
                    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    guard.as_ref().and_then(|d| d.ids.get(&id).cloned()).unwrap_or_default()
                };
                {
                    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_ref() {
                        for val in ["none", "system", "pac", "manual"] {
                            let n = format!("proxy_mode_{val}");
                            if let Some(&r) = d.fields.get(&n) {
                                dlg::set_checked(h(r), n == name);
                            }
                        }
                    }
                }
                sync_proxy_mode();
            }
        }
    }
}

/// After a rename/delete the backend already moved entries and
/// persisted — re-read settings as the dialog's new base and
/// refresh the lists (app.js refreshAfterCategoryChange, minus
/// the fields the user is mid-edit on: only the manage list and
/// the staged category keys change under them — the base swap
/// keeps Save from resurrecting the old name's dirs/exts).
fn refresh_after_category_change(hwnd: HWND) {
    let _ = hwnd;
    if let Some(c) = crate::ctx() {
        let fresh = serde_json::to_value(ndm_core::get_settings(c)).unwrap_or(serde_json::Value::Null);
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_mut() {
            // Preserve the user's in-flight category edits by
            // re-keying through the fresh object: simplest honest
            // merge — fresh dirs/exts win for renamed keys; the
            // field texts stay as typed and Save re-derives them.
            d.base["category_dirs"] = fresh.get("category_dirs").cloned().unwrap_or(serde_json::json!({}));
            d.base["category_exts"] = fresh.get("category_exts").cloned().unwrap_or(serde_json::json!({}));
        }
    }
    refresh_cats();
    crate::post_refresh();
}

unsafe extern "system" fn opt_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            WIN_RAW.store(raw(hwnd), Ordering::SeqCst);
            let cs = l.0 as *const CREATESTRUCTW;
            let base = Box::from_raw((*cs).lpCreateParams as *mut serde_json::Value);
            build(hwnd, *base);
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, (w.0 & 0xffff) as i32);
            LRESULT(0)
        }
        WM_NOTIFY => {
            let hdr = l.0 as *const NMHDR;
            if !hdr.is_null() && (*hdr).idFrom == ID_TAB as usize && (*hdr).code == TCN_SELCHANGE {
                {
                    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_mut() {
                        d.tab_index = TabControl { hwnd: h(d.tab) }.cur_sel().max(0);
                    }
                }
                apply_tab_visibility();
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
