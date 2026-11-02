//! Customize Toolbar (ui/app.js tbmodal / openToolbarCfg) —
//! staged edit of which toolbar buttons show plus the button
//! size. OK persists through `prefs` under the web UI's own key
//! and shape (`ndm-toolbar-v1`: {hidden: [DOM ids], size}) and
//! applies to the main toolbar at once (main.rs
//! `set_toolbar_cfg`); Defaults restores all-visible + large.
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell.

use crate::winui::dialog as dlg;
use crate::winui::listview::{ColumnSpec, ListView};
use crate::winui::{pcw, send, wide};
use crate::ToolbarCfg;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW,
    RegisterClassW, SetForegroundWindow, ShowWindow, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, IDC_ARROW, SW_RESTORE, SW_SHOW, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE,
    WM_DESTROY, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE,
};

const ID_LIST: i32 = 8501;
const ID_SIZE: i32 = 8502;
const ID_RESET: i32 = 8503;
const ID_OK: i32 = 8504;
const ID_CANCEL: i32 = 8505;
const WIN_W: i32 = 400;
const WIN_H: i32 = 500;

struct TbDlg {
    list: isize,
    size: isize,
}

static DLG: Mutex<Option<TbDlg>> = Mutex::new(None);
static WIN_RAW: AtomicIsize = AtomicIsize::new(0);

fn h(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}
fn raw(hwnd: HWND) -> isize {
    hwnd.0 as isize
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
    register_class();
    unsafe {
        let class_buf = wide("NdmToolbarDialog");
        let title_buf = wide("Customize Toolbar");
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rect = RECT { left: 0, top: 0, right: WIN_W, bottom: WIN_H };
        let _ = AdjustWindowRectEx(&mut rect, style, false, Default::default());
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
            None,
        ) {
            Ok(hwnd) => {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
            }
            Err(_) => {}
        }
    }
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let class_buf = wide("NdmToolbarDialog");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(tb_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

fn render(cfg: &ToolbarCfg) {
    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_ref() else { return };
    let lv = ListView { hwnd: h(d.list), block_map_column: None };
    lv.clear();
    for (i, (_, dom, label)) in crate::TOOLBAR_DEFS.iter().enumerate() {
        lv.insert_row(i as i32, &[label.to_string()]);
        lv.set_checked(i as i32, !cfg.hidden.iter().any(|x| x == dom));
    }
    unsafe {
        send(h(d.size), 0x014E, WPARAM(if cfg.small { 1 } else { 0 }), LPARAM(0));
    }
}

fn read_back() -> ToolbarCfg {
    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_ref() else {
        return ToolbarCfg { hidden: Vec::new(), small: false };
    };
    let lv = ListView { hwnd: h(d.list), block_map_column: None };
    let hidden = crate::TOOLBAR_DEFS
        .iter()
        .enumerate()
        .filter(|(i, _)| !lv.is_checked(*i as i32))
        .map(|(_, (_, dom, _))| dom.to_string())
        .collect();
    let small = unsafe { send(h(d.size), 0x0147, WPARAM(0), LPARAM(0)) } == 1;
    ToolbarCfg { hidden, small }
}

fn on_command(hwnd: HWND, id: i32) {
    match id {
        ID_RESET => render(&ToolbarCfg { hidden: Vec::new(), small: false }),
        ID_OK => {
            let cfg = read_back();
            crate::set_toolbar_cfg(cfg);
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        ID_CANCEL => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        _ => {}
    }
}

fn build(hwnd: HWND) {
    unsafe {
        dlg::create_child(hwnd, "STATIC", "Choose which buttons the toolbar shows. Saved on this computer.", WS_CHILD | WS_VISIBLE, 16, 14, 360, 20, 0);
        let list = ListView::create_ex(
            hwnd,
            ID_LIST,
            &[ColumnSpec { label: "Button", width: 300 }],
            true,
        )
        .map(|lv| {
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(lv.hwnd, 16, 40, 352, 330, true);
            lv.hwnd
        })
        .unwrap_or_default();
        dlg::create_child(hwnd, "STATIC", "Button size", WS_CHILD | WS_VISIBLE, 16, 383, 100, 20, 0);
        let size = dlg::create_child(hwnd, "COMBOBOX", "", WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0003), 120, 380, 180, 120, ID_SIZE);
        for label in ["Large buttons", "Small buttons"] {
            let mut buf = wide(label);
            send(size, 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
        }
        dlg::create_child(hwnd, "BUTTON", "Defaults", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 16, WIN_H - 44, 92, 28, ID_RESET);
        dlg::create_child(hwnd, "BUTTON", "OK", WS_CHILD | WS_VISIBLE | WS_TABSTOP, WIN_W - 208, WIN_H - 44, 92, 28, ID_OK);
        dlg::create_child(hwnd, "BUTTON", "Cancel", WS_CHILD | WS_VISIBLE | WS_TABSTOP, WIN_W - 108, WIN_H - 44, 92, 28, ID_CANCEL);
        *DLG.lock().unwrap_or_else(|e| e.into_inner()) = Some(TbDlg { list: raw(list), size: raw(size) });
    }
    render(&crate::toolbar_cfg());
}

unsafe extern "system" fn tb_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            WIN_RAW.store(raw(hwnd), Ordering::SeqCst);
            build(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, (w.0 & 0xffff) as i32);
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
