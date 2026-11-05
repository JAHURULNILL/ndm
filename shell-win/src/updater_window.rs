//! 2.3.7 — the standalone update checker: `ndm-app.exe
//! --ndm-update-check`. The Windows twin of the GTK shell's
//! updater_window: an update announces itself even when the app
//! is closed — one small window in the centre of the screen,
//! Update Now updates right there (core's own download +
//! finish_windows_update, the identical steps the in-app update
//! runs), and the setup/helper relaunches NDM on the new version.
//! No resident process: run, decide, exit.
//!
//! Discipline mirrors the GTK side: the app already running
//! (the instance guard answers a ping) → exit silently; no
//! offer / failed fetch → exit silently; "Later" snoozes the
//! same version for 24 h (core's snooze helpers).
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell — it is compile-checked against
//! both Windows targets and rides the release's Windows gate.

use crate::winui::dialog as dlg;
use crate::winui::{pcw, send, set_window_text, wide};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::Controls::{PBM_SETPOS, PBM_SETRANGE32};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetMessageW,
    GetSystemMetrics, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, ShowWindow, TranslateMessage, DispatchMessageW, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, IDC_ARROW, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOW,
    WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WNDCLASSW,
    WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_VISIBLE,
};

pub const FLAG: &str = "--ndm-update-check";

const ID_TITLE: i32 = 7101;
const ID_SUB: i32 = 7102;
const ID_NOTES: i32 = 7103;
const ID_BAR: i32 = 7104;
const ID_BARTEXT: i32 = 7105;
const ID_BTN_UPDATE: i32 = 7110;
const ID_BTN_LATER: i32 = 7111;

const WM_UC_PROGRESS: u32 = WM_APP + 21; // LPARAM = Box<Value>
const WM_UC_EXIT: u32 = WM_APP + 22; // install handed over: quit

const WIN_W: i32 = 480;
const WIN_H: i32 = 316;

struct ChkDlg {
    title: isize,
    bar: isize,
    bar_text: isize,
    btn_update: isize,
    btn_later: isize,
    installing: bool,
    bar_pos: i32,
    last_text: String,
}

static DLG: Mutex<Option<ChkDlg>> = Mutex::new(None);
static WIN_RAW: AtomicIsize = AtomicIsize::new(0);
static OFFER: Mutex<Option<ndm_core::updater::ReleaseInfo>> = Mutex::new(None);

fn h(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}
fn raw(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// First-run registration (called from bootstrap): a logon
/// Scheduled Task that runs the checker. Installer flavour only
/// — a portable copy must not register machine tasks. Query
/// first; an existing task is left exactly as it is.
pub fn register_update_task() {
    if !ndm_core::windows_is_installer_flavour() {
        return;
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let query = std::process::Command::new("schtasks")
        .args(["/Query", "/TN", "NDM Update Check"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if query.map(|s| s.success()).unwrap_or(false) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let tr = format!("\"{}\" {FLAG}", exe.to_string_lossy());
    let _ = std::process::Command::new("schtasks")
        .args([
            "/Create", "/TN", "NDM Update Check", "/TR", &tr, "/SC", "ONLOGON",
            "/RL", "LIMITED", "/F",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Best-effort +6 h one-shot re-check (the systemd-run twin):
/// a machine that stays logged in hears about the update the
/// same day. Local-time fields via the Win32 time APIs, so the
/// one-shot lands ~6 h from now in the user's own timezone.
fn rearm() {
    if !ndm_core::windows_is_installer_flavour() {
        return;
    }
    use std::os::windows::process::CommandExt;
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::System::Time::{
        FileTimeToSystemTime, SystemTimeToFileTime,
    };
    use windows::Win32::Foundation::FILETIME;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = std::env::current_exe() else { return };
    let (date, time) = unsafe {
        let mut st = GetLocalTime();
        let mut ft = FILETIME::default();
        if SystemTimeToFileTime(&st, &mut ft).is_err() {
            return;
        }
        let ticks = ((ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64)
            + 6 * 3600 * 10_000_000;
        ft = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        if FileTimeToSystemTime(&ft, &mut st).is_err() {
            return;
        }
        (
            format!("{:02}/{:02}/{:04}", st.wMonth, st.wDay, st.wYear),
            format!("{:02}:{:02}", st.wHour, st.wMinute),
        )
    };
    let tr = format!("\"{}\" {FLAG}", exe.to_string_lossy());
    let _ = std::process::Command::new("schtasks")
        .args([
            "/Create", "/TN", "NDM Update Check Once", "/TR", &tr, "/SC", "ONCE",
            "/SD", &date, "/ST", &time, "/F",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let class_buf = wide("NdmUpdateChecker");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(chk_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

fn set_bar_text(d: &mut ChkDlg, text: &str) {
    if d.last_text != text {
        unsafe { set_window_text(h(d.bar_text), text) };
        d.last_text = text.to_string();
    }
}

fn apply_progress(v: &serde_json::Value) {
    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_mut() else { return };
    let phase = v.get("phase").and_then(|x| x.as_str()).unwrap_or("");
    let mb = |b: u64| b as f64 / 1_048_576.0;
    unsafe {
        match phase {
            "downloading" => {
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
                set_bar_text(d, "Installing the update — NDM will reopen on the new version…");
                if d.bar_pos != 1000 {
                    send(h(d.bar), PBM_SETPOS, WPARAM(1000), LPARAM(0));
                    d.bar_pos = 1000;
                }
            }
            "error" => {
                d.installing = false;
                let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("unknown error");
                set_bar_text(d, &format!("Update failed: {msg}"));
                let _ = ShowWindow(h(d.btn_update), SW_SHOW);
                let _ = ShowWindow(h(d.btn_later), SW_SHOW);
            }
            _ => {}
        }
    }
}

fn start_update() {
    {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_mut() else { return };
        if d.installing {
            return;
        }
        d.installing = true;
        unsafe {
            let _ = ShowWindow(h(d.btn_update), SW_HIDE);
            let _ = ShowWindow(h(d.btn_later), SW_HIDE);
            let _ = ShowWindow(h(d.bar), SW_SHOW);
            set_window_text(h(d.bar_text), "Downloading the update…");
            d.last_text = "Downloading the update…".to_string();
        }
    }
    let info = OFFER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(info) = info else { return };
    std::thread::spawn(move || {
        let dir = crate::app_data_dir().join("updates");
        let cb = |done: u64, total: u64| {
            forward(
                WM_UC_PROGRESS,
                serde_json::json!({
                    "phase": "downloading", "downloaded": done, "total": total
                }),
            );
        };
        let outcome = ndm_core::download_update_package(&info, &dir, &cb).and_then(|dest| {
            forward(
                WM_UC_PROGRESS,
                serde_json::json!({ "phase": "installing" }),
            );
            let pkg = ndm_core::update_package(&info)
                .ok_or("this update has no installable package for your copy of NDM")?;
            ndm_core::finish_windows_update(&dest, pkg.installer)
        });
        match outcome {
            // the setup / the detached helper now owns the
            // relaunch — this process simply leaves
            Ok(()) => {
                let r = WIN_RAW.load(Ordering::SeqCst);
                if r != 0 {
                    unsafe {
                        let _ = PostMessageW(Some(h(r)), WM_UC_EXIT, WPARAM(0), LPARAM(0));
                    }
                }
            }
            Err(e) => forward(
                WM_UC_PROGRESS,
                serde_json::json!({ "phase": "error", "message": e }),
            ),
        }
    });
}

fn on_command(id: i32) {
    match id {
        ID_BTN_UPDATE => start_update(),
        ID_BTN_LATER => {
            let r = WIN_RAW.load(Ordering::SeqCst);
            if r != 0 {
                unsafe {
                    let _ = DestroyWindow(h(r));
                }
            }
        }
        _ => {}
    }
}

fn build(hwnd: HWND, info: &ndm_core::updater::ReleaseInfo) {
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
        let title = label(&format!("NDM {} is available", info.version), 14, 22, ID_TITLE);
        let _ = title;
        let _ = label(
            &format!(
                "You have NDM {}. The update downloads and installs here — NDM opens on the new version when it is done.",
                env!("CARGO_PKG_VERSION")
            ),
            40,
            34,
            ID_SUB,
        );
        let _ = label(&info.notes, 80, 96, ID_NOTES);
        let bar = dlg::create_child(
            hwnd,
            "msctls_progress32",
            "",
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(1), // PBS_SMOOTH
            16,
            190,
            WIN_W - 32,
            18,
            ID_BAR,
        );
        send(bar, PBM_SETRANGE32, WPARAM(0), LPARAM(1000));
        let _ = ShowWindow(bar, SW_HIDE);
        let bar_text = label("", 214, 20, ID_BARTEXT);
        let btn = |text: &str, x: i32, id: i32| {
            dlg::create_child(hwnd, "BUTTON", text, WS_CHILD | WS_VISIBLE, x, WIN_H - 44, 104, 28, id)
        };
        let btn_update = btn("Update Now", WIN_W - 16 - 104, ID_BTN_UPDATE);
        let btn_later = btn("Later", WIN_W - 16 - 104 * 2 - 8, ID_BTN_LATER);
        *DLG.lock().unwrap_or_else(|e| e.into_inner()) = Some(ChkDlg {
            title: raw(title),
            bar: raw(bar),
            bar_text: raw(bar_text),
            btn_update: raw(btn_update),
            btn_later: raw(btn_later),
            installing: false,
            bar_pos: -1,
            last_text: String::new(),
        });
    }
}

unsafe extern "system" fn chk_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            WIN_RAW.store(raw(hwnd), Ordering::SeqCst);
            let cs = l.0 as *const CREATESTRUCTW;
            let info = Box::from_raw((*cs).lpCreateParams as *mut ndm_core::updater::ReleaseInfo);
            build(hwnd, &info);
            LRESULT(0)
        }
        WM_UC_PROGRESS => {
            if l.0 != 0 {
                let v = *Box::from_raw(l.0 as *mut serde_json::Value);
                apply_progress(&v);
            }
            LRESULT(0)
        }
        WM_UC_EXIT => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command((w.0 & 0xffff) as i32);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            WIN_RAW.store(0, Ordering::SeqCst);
            *DLG.lock().unwrap_or_else(|e| e.into_inner()) = None;
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

/// The checker's whole life: probe, decide, maybe one window,
/// exit. Called from main() beside the --apply-update seam;
/// never returns.
pub fn run() -> ! {
    crate::winui::window::init_common_controls();
    // the app itself is up → its own offer owns the update
    if crate::instance_ping(crate::instance_port(), "ping") {
        std::process::exit(0);
    }
    let info = match ndm_core::update_offer() {
        Ok(Some(info)) => info,
        _ => {
            rearm();
            std::process::exit(0);
        }
    };
    let dir = crate::app_data_dir();
    let snooze = dir.join("update-check.json");
    if ndm_core::update_shown_recently(&snooze, &info.version, now_unix()) {
        rearm();
        std::process::exit(0);
    }
    ndm_core::record_update_shown(&snooze, &info.version, now_unix());
    *OFFER.lock().unwrap_or_else(|e| e.into_inner()) = Some(info.clone());

    register_class();
    unsafe {
        let class_buf = wide("NdmUpdateChecker");
        let title_buf = wide("NDM Update");
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rect = RECT { left: 0, top: 0, right: WIN_W, bottom: WIN_H };
        let _ = AdjustWindowRectEx(&mut rect, style, false, Default::default());
        let w = rect.right - rect.left;
        let hh = rect.bottom - rect.top;
        // his spec: the window sits in the middle of the display
        let x = (GetSystemMetrics(SM_CXSCREEN) - w) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - hh) / 2;
        let boxed = Box::into_raw(Box::new(info));
        match CreateWindowExW(
            Default::default(),
            pcw(&class_buf),
            pcw(&title_buf),
            style,
            x,
            y,
            w,
            hh,
            None,
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
                std::process::exit(0);
            }
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
    std::process::exit(0);
}
