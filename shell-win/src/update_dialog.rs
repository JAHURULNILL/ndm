//! Update dialog (ui/app.js umodal / showUpdate /
//! onUpdateProgress) — one modeless window for the latched
//! update offer: version title, notes, a progress bar fed by
//! core's UpdateProgress events, Update Now / Restart / Later.
//!
//! MANDATORY mode (app.js: the mandatory dialog cannot be
//! dismissed): no Later button, no × (the window is created
//! without WS_SYSMENU), WM_CLOSE and SC_CLOSE are refused, and
//! Escape is swallowed — all at the window-style/message level —
//! until an install lands, which restores the close box.
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell.

use crate::winui::dialog as dlg;
use crate::winui::{pcw, send, set_window_text, wide};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::Controls::{PBM_SETPOS, PBM_SETRANGE32};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW,
    LoadCursorW, PostMessageW, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWL_STYLE,
    IDC_ARROW,
    SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW,
    WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_KEYDOWN, WM_SYSCOMMAND,
    WNDCLASSW, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_VISIBLE,
};

const ID_TITLE: i32 = 7001;
const ID_NOTES: i32 = 7002;
const ID_BAR: i32 = 7003;
const ID_BARTEXT: i32 = 7004;
const ID_BTN_UPDATE: i32 = 7010;
const ID_BTN_RESTART: i32 = 7011;
const ID_BTN_LATER: i32 = 7012;

const WM_UD_PROGRESS: u32 = WM_APP + 11; // LPARAM = Box<Value>
const WM_UD_INSTALLED: u32 = WM_APP + 12; // LPARAM = Box<Value>
const SC_CLOSE_VAL: usize = 0xF060;
const VK_ESCAPE_VAL: usize = 0x1B;

const WIN_W: i32 = 480;
const WIN_H: i32 = 300;

/// What `show` needs from one offer payload.
struct Info {
    version: String,
    notes: String,
    mandatory: bool,
}

struct UpdDlg {
    title: isize,
    notes: isize,
    bar: isize,
    bar_text: isize,
    btn_update: isize,
    btn_restart: isize,
    btn_later: isize,
    mandatory: bool,
    installing: bool,
    installed: bool,
    bar_pos: i32,
    last_text: String,
}

static DLG: Mutex<Option<UpdDlg>> = Mutex::new(None);
static WIN_RAW: AtomicIsize = AtomicIsize::new(0);

fn h(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}
fn raw(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn info_of(v: &serde_json::Value) -> Info {
    Info {
        version: v.get("version").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        notes: v.get("notes").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        mandatory: v.get("mandatory").and_then(|x| x.as_bool()).unwrap_or(false),
    }
}

/// Show the dialog for an offer, or raise + refresh the one
/// already open (app.js showUpdate re-arms the same modal).
pub fn show(parent: HWND, v: &serde_json::Value) {
    let info = info_of(v);
    if WIN_RAW.load(Ordering::SeqCst) != 0 {
        let hwnd = h(WIN_RAW.load(Ordering::SeqCst));
        apply_info(hwnd, &info);
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        return;
    }
    register_class();
    unsafe {
        let class_buf = wide("NdmUpdateDialog");
        let title_buf = wide("NDM Update");
        let mut style = WS_OVERLAPPED | WS_CAPTION | WS_MINIMIZEBOX;
        if !info.mandatory {
            style |= WS_SYSMENU;
        }
        let mut rect = RECT { left: 0, top: 0, right: WIN_W, bottom: WIN_H };
        let _ = AdjustWindowRectEx(&mut rect, style, false, Default::default());
        let boxed = Box::into_raw(Box::new(info));
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
            Err(_) => {
                drop(Box::from_raw(boxed));
            }
        }
    }
}

pub fn is_open() -> bool {
    WIN_RAW.load(Ordering::SeqCst) != 0
}

/// Forward one UpdateProgress payload (any thread; boxed,
/// reclaimed on post failure — the sink's pattern).
pub fn forward_progress(v: serde_json::Value) {
    forward(WM_UD_PROGRESS, v);
}

pub fn forward_installed(v: serde_json::Value) {
    forward(WM_UD_INSTALLED, v);
}

fn forward(msg: u32, v: serde_json::Value) {
    let r = WIN_RAW.load(Ordering::SeqCst);
    if r == 0 {
        return;
    }
    let lparam = LPARAM(Box::into_raw(Box::new(v)) as isize);
    unsafe {
        if PostMessageW(Some(h(r)), msg, WPARAM(0), lparam).is_err() {
            drop(Box::from_raw(lparam.0 as *mut serde_json::Value));
        }
    }
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let class_buf = wide("NdmUpdateDialog");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(upd_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

fn set_closable(hwnd: HWND, on: bool) {
    unsafe {
        // Widen to isize for the flag arithmetic and let
        // `new as _` narrow back at the call: on 32-bit
        // Windows the windows crate aliases the *Ptr APIs
        // to the i32 Get/SetWindowLongW, on x86_64 they
        // take isize — the style bits are 32 on both.
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as isize;
        let new = if on {
            style | WS_SYSMENU.0 as isize
        } else {
            style & !(WS_SYSMENU.0 as isize)
        };
        SetWindowLongPtrW(hwnd, GWL_STYLE, new as _);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
}

fn apply_info(hwnd: HWND, info: &Info) {
    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_mut() else { return };
    d.mandatory = info.mandatory;
    d.installed = false;
    d.installing = false;
    unsafe {
        set_window_text(h(d.title), &format!("NDM {} is available", info.version));
        let notes = if info.mandatory {
            format!(
                "This update is required — downloads pause until it is installed. {}",
                info.notes
            )
        } else {
            info.notes.clone()
        };
        set_window_text(h(d.notes), &notes);
        let _ = ShowWindow(h(d.btn_later), if info.mandatory { SW_HIDE } else { SW_SHOW });
        let _ = ShowWindow(h(d.btn_update), SW_SHOW);
        let _ = ShowWindow(h(d.btn_restart), SW_HIDE);
        let _ = ShowWindow(h(d.bar), SW_HIDE);
        set_window_text(h(d.bar_text), "");
        d.last_text = String::new();
        d.bar_pos = -1;
    }
    if info.mandatory {
        set_closable(hwnd, false);
    }
}

fn set_bar_text(d: &mut UpdDlg, text: &str) {
    if d.last_text != text {
        unsafe { set_window_text(h(d.bar_text), text) };
        d.last_text = text.to_string();
    }
}

fn apply_progress(hwnd: HWND, v: &serde_json::Value) {
    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_mut() else { return };
    let phase = v.get("phase").and_then(|x| x.as_str()).unwrap_or("");
    let mb = |b: u64| b as f64 / 1_048_576.0;
    unsafe {
        match phase {
            "downloading" => {
                d.installing = true;
                let _ = ShowWindow(h(d.bar), SW_SHOW);
                let done = v.get("downloaded").and_then(|x| x.as_u64()).unwrap_or(0);
                let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
                set_bar_text(
                    d,
                    &if total > 0 {
                        format!("Downloading the update — {:.1} of {:.1} MB", mb(done), mb(total))
                    } else {
                        format!("Downloading the update — {:.1} MB", mb(done))
                    },
                );
                if total > 0 {
                    let pos = (done * 1000 / total) as i32;
                    if pos != d.bar_pos {
                        send(h(d.bar), PBM_SETPOS, WPARAM(pos as usize), LPARAM(0));
                        d.bar_pos = pos;
                    }
                }
            }
            "installing" => {
                let _ = ShowWindow(h(d.bar), SW_SHOW);
                let msg = v
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("Installing — your password may be asked by the system…");
                set_bar_text(d, msg);
                if d.bar_pos != 1000 {
                    send(h(d.bar), PBM_SETPOS, WPARAM(1000), LPARAM(0));
                    d.bar_pos = 1000;
                }
            }
            "installed" => {
                d.installing = false;
                d.installed = true;
                set_bar_text(d, "Update installed.");
                let _ = ShowWindow(h(d.btn_update), SW_HIDE);
                let _ = ShowWindow(h(d.btn_later), SW_HIDE);
                let _ = ShowWindow(h(d.btn_restart), SW_SHOW);
                set_closable(hwnd, true);
            }
            "opened" => {
                d.installing = false;
                set_bar_text(
                    d,
                    "The installer opened — finish the installation there, then restart NDM.",
                );
                let _ = ShowWindow(h(d.btn_update), SW_HIDE);
                let _ = ShowWindow(h(d.btn_restart), SW_SHOW);
            }
            "error" => {
                d.installing = false;
                let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("unknown error");
                set_bar_text(d, &format!("Update failed: {msg}"));
                let _ = ShowWindow(h(d.btn_update), SW_SHOW);
            }
            _ => {}
        }
    }
}

/// Restart (app.js restart_app): relaunch this exe shortly after
/// this process exits — the delay lets the single-instance guard
/// port free up, so the new copy becomes Primary — then exit
/// through the main window (WM_APP_EXIT), exactly core's exit path.
fn restart_app(hwnd: HWND) {
    if let Ok(exe) = std::env::current_exe() {
        use std::os::windows::process::CommandExt;
        let cmd = format!(
            "timeout /t 2 /nobreak > NUL & start \"\" \"{}\"",
            exe.to_string_lossy()
        );
        let _ = std::process::Command::new("cmd")
            .args(["/C", &cmd])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();
    }
    let _ = hwnd;
    crate::post_main(crate::WM_APP_EXIT, None);
}

fn on_command(hwnd: HWND, id: i32) {
    match id {
        ID_BTN_UPDATE => {
            let busy = {
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_mut() {
                    Some(d) if !d.installing => {
                        d.installing = true;
                        unsafe {
                            let _ = ShowWindow(h(d.btn_update), SW_HIDE);
                            let _ = ShowWindow(h(d.bar), SW_SHOW);
                            set_window_text(h(d.bar_text), "Downloading the update…");
                            d.last_text = "Downloading the update…".to_string();
                        }
                        false
                    }
                    _ => true,
                }
            };
            if busy {
                return;
            }
            if let Some(c) = crate::ctx() {
                ndm_core::rt::spawn(async move {
                    if let Err(e) = ndm_core::install_update(c).await {
                        forward_progress(serde_json::json!({
                            "phase": "error", "message": e
                        }));
                    }
                });
            }
        }
        ID_BTN_RESTART => restart_app(hwnd),
        ID_BTN_LATER => {
            let blocked = DLG
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|d| d.mandatory && !d.installed)
                .unwrap_or(false);
            if !blocked {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
        _ => {}
    }
}

fn build(hwnd: HWND, info: Info) {
    unsafe {
        let label = |text: &str, y: i32, ht: i32, id: i32| {
            dlg::create_child(
                hwnd,
                "STATIC",
                text,
                WS_CHILD | WS_VISIBLE,
                16,
                y,
                WIN_W - 32,
                ht,
                id,
            )
        };
        let title = label("", 14, 22, ID_TITLE);
        let notes = label("", 42, 90, ID_NOTES);
        let bar = dlg::create_child(
            hwnd,
            "msctls_progress32",
            "",
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(1), // PBS_SMOOTH
            16,
            150,
            WIN_W - 32,
            18,
            ID_BAR,
        );
        send(bar, PBM_SETRANGE32, WPARAM(0), LPARAM(1000));
        let _ = ShowWindow(bar, SW_HIDE);
        let bar_text = label("", 174, 20, ID_BARTEXT);
        let btn = |text: &str, x: i32, id: i32| {
            dlg::create_child(hwnd, "BUTTON", text, WS_CHILD | WS_VISIBLE, x, WIN_H - 44, 104, 28, id)
        };
        let btn_restart = btn("Restart NDM", WIN_W - 16 - 104, ID_BTN_RESTART);
        let btn_update = btn("Update Now", WIN_W - 16 - 104, ID_BTN_UPDATE);
        let btn_later = btn("Later", WIN_W - 16 - 104 * 2 - 8, ID_BTN_LATER);
        let _ = ShowWindow(btn_restart, SW_HIDE);
        let state = UpdDlg {
            title: raw(title),
            notes: raw(notes),
            bar: raw(bar),
            bar_text: raw(bar_text),
            btn_update: raw(btn_update),
            btn_restart: raw(btn_restart),
            btn_later: raw(btn_later),
            mandatory: info.mandatory,
            installing: false,
            installed: false,
            bar_pos: -1,
            last_text: String::new(),
        };
        *DLG.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
    }
    apply_info(hwnd, &info);
}

unsafe extern "system" fn upd_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            WIN_RAW.store(raw(hwnd), Ordering::SeqCst);
            let cs = l.0 as *const CREATESTRUCTW;
            let info = Box::from_raw((*cs).lpCreateParams as *mut Info);
            build(hwnd, *info);
            LRESULT(0)
        }
        WM_UD_PROGRESS => {
            if l.0 != 0 {
                let v = *Box::from_raw(l.0 as *mut serde_json::Value);
                apply_progress(hwnd, &v);
            }
            LRESULT(0)
        }
        WM_UD_INSTALLED => {
            if l.0 != 0 {
                let v = *Box::from_raw(l.0 as *mut serde_json::Value);
                let ver = v.get("version").and_then(|x| x.as_str()).unwrap_or("");
                apply_progress(
                    hwnd,
                    &serde_json::json!({ "phase": "installed" }),
                );
                if !ver.is_empty() {
                    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_mut() {
                        set_bar_text(d, &format!("NDM {ver} is installed — restart NDM to use it."));
                    }
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, (w.0 & 0xffff) as i32);
            LRESULT(0)
        }
        WM_CLOSE | WM_SYSCOMMAND => {
            // Mandatory: refused until installed (WM_CLOSE from
            // the × / Alt+F4, SC_CLOSE from the system menu).
            if msg == WM_SYSCOMMAND && (w.0 & 0xFFF0) != SC_CLOSE_VAL {
                return DefWindowProcW(hwnd, msg, w, l);
            }
            let blocked = DLG
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|d| d.mandatory && !d.installed)
                .unwrap_or(false);
            if blocked {
                return LRESULT(0);
            }
            if msg == WM_CLOSE {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_KEYDOWN => {
            if w.0 == VK_ESCAPE_VAL {
                let blocked = DLG
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|d| d.mandatory && !d.installed)
                    .unwrap_or(false);
                if blocked {
                    return LRESULT(0);
                }
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_DESTROY => {
            WIN_RAW.store(0, Ordering::SeqCst);
            *DLG.lock().unwrap_or_else(|e| e.into_inner()) = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}
