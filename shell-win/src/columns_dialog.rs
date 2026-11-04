//! Columns chooser (ui/app.js colmodal / openColumns) — staged
//! edit of the list's column set: per-column visibility (the
//! File Name anchor cannot be hidden), width (0 = automatic) and
//! order via Up/Down. OK persists through `prefs` under the web
//! UI's own key/shape (`ndm-columns-v2`) and applies to the main
//! ListView at once (main.rs `set_column_cfg`); Defaults restores
//! COLUMN_DEFS' defaults; Cancel discards.
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell.

use crate::winui::dialog as dlg;
use crate::winui::listview::{ColumnSpec, ListView};
use crate::winui::{pcw, set_window_text, wide, window_text};
use crate::ColumnCfg;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::Controls::NMHDR;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW,
    RegisterClassW, SetForegroundWindow, ShowWindow, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, IDC_ARROW, SW_RESTORE, SW_SHOW, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE,
    WM_DESTROY, WM_NOTIFY, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

const ID_LIST: i32 = 8401;
const ID_UP: i32 = 8402;
const ID_DOWN: i32 = 8403;
const ID_WIDTH: i32 = 8404;
const ID_RESET: i32 = 8405;
const ID_OK: i32 = 8406;
const ID_CANCEL: i32 = 8407;
const EN_KILLFOCUS_VAL: u32 = 0x0200;
const LVN_ITEMCHANGED_CODE: i32 = -101;
const WIN_W: i32 = 440;
const WIN_H: i32 = 480;

struct ColDlg {
    list: isize,
    width: isize,
    staged: Vec<ColumnCfg>,
}

static DLG: Mutex<Option<ColDlg>> = Mutex::new(None);
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
        let class_buf = wide("NdmColumnsDialog");
        let title_buf = wide("Columns");
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
        let class_buf = wide("NdmColumnsDialog");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(col_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

fn lv(d: &ColDlg) -> ListView {
    ListView { hwnd: h(d.list) }
}

fn label_of(id: &str) -> &'static str {
    crate::COLUMN_DEFS
        .iter()
        .find(|(cid, _, _, _)| *cid == id)
        .map(|(_, label, _, _)| *label)
        .unwrap_or("")
}

/// Pull the live checkbox states into the staged copy (the
/// ListView owns check state between renders).
fn sync_checks(d: &mut ColDlg) {
    let view = lv(d);
    for (i, c) in d.staged.iter_mut().enumerate() {
        c.visible = view.is_checked(i as i32);
    }
    if let Some(nm) = d.staged.iter_mut().find(|c| c.id == "name") {
        nm.visible = true; // the anchor column cannot be hidden
    }
}

fn render(select: i32) {
    // Snapshot everything the control writes need, then DROP the
    // lock before touching the ListView: set_checked fires
    // LVN_ITEMCHANGED synchronously, and the notify handler takes
    // this same (non-reentrant) mutex — writing under the lock
    // deadlocks the UI thread (AUDIT-233 CRITICAL). All handlers
    // run on this one UI thread, so the snapshot stays consistent
    // for the writes below.
    let (list_raw, width_raw, rows) = {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_ref() else { return };
        (
            d.list,
            d.width,
            d.staged
                .iter()
                .map(|c| (label_of(&c.id).to_string(), c.width, c.visible))
                .collect::<Vec<_>>(),
        )
    };
    let view = ListView { hwnd: h(list_raw) };
    view.clear();
    for (i, (label, width, visible)) in rows.iter().enumerate() {
        view.insert_row(i as i32, &[label.clone(), width.to_string()]);
        view.set_checked(i as i32, *visible);
    }
    if select >= 0 {
        view.set_selected(select, true);
        if let Some((_, width, _)) = rows.get(select as usize) {
            unsafe { set_window_text(h(width_raw), &width.to_string()) };
        }
    }
}

fn apply_width(d: &mut ColDlg) {
    let view = lv(d);
    let Some(row) = view.selected_rows().into_iter().next() else { return };
    let text = unsafe { window_text(h(d.width)) };
    // An unreadable width keeps the column's current width —
    // it used to silently become 0 and hide the column.
    let Ok(w) = text.trim().parse::<i32>() else { return };
    if let Some(c) = d.staged.get_mut(row as usize) {
        c.width = w.clamp(0, 800);
    }
}

fn on_command(hwnd: HWND, id: i32, code: u32) {
    match id {
        ID_UP | ID_DOWN => {
            let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            let Some(d) = guard.as_mut() else { return };
            sync_checks(d);
            let view = lv(d);
            let Some(row) = view.selected_rows().into_iter().next() else { return };
            let j = if id == ID_UP { row - 1 } else { row + 1 };
            if row >= 0 && j >= 0 && (j as usize) < d.staged.len() {
                d.staged.swap(row as usize, j as usize);
                drop(guard);
                render(j);
            }
        }
        ID_WIDTH => {
            if code == EN_KILLFOCUS_VAL {
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    sync_checks(d);
                    apply_width(d);
                    let sel = lv(d).selected_rows().into_iter().next().unwrap_or(-1);
                    drop(guard);
                    render(sel);
                }
            }
        }
        ID_RESET => {
            {
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    d.staged = crate::default_column_cfg();
                }
            }
            render(0);
        }
        ID_OK => {
            let staged = {
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                let Some(d) = guard.as_mut() else { return };
                sync_checks(d);
                apply_width(d);
                d.staged.clone()
            };
            crate::set_column_cfg(staged);
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
        dlg::create_child(hwnd, "STATIC", "Show, order and size the download list's columns:", WS_CHILD | WS_VISIBLE, 16, 14, 400, 20, 0);
        let list = ListView::create_ex(
            hwnd,
            ID_LIST,
            &[ColumnSpec { label: "Column", width: 240 }, ColumnSpec { label: "Width", width: 80 }],
            true,
        )
        .map(|lv| {
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(lv.hwnd, 16, 40, 330, 320, true);
            lv.hwnd
        })
        .unwrap_or_default();
        dlg::create_child(hwnd, "BUTTON", "↑", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 354, 40, 44, 28, ID_UP);
        dlg::create_child(hwnd, "BUTTON", "↓", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 354, 76, 44, 28, ID_DOWN);
        dlg::create_child(hwnd, "STATIC", "Width of the selected column (0 = automatic):", WS_CHILD | WS_VISIBLE, 16, 368, 300, 20, 0);
        let width = dlg::create_child(
            hwnd,
            "EDIT",
            "0",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | WINDOW_STYLE(0x0080),
            320,
            366,
            70,
            23,
            ID_WIDTH,
        );
        dlg::create_child(hwnd, "BUTTON", "Defaults", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 16, WIN_H - 44, 92, 28, ID_RESET);
        dlg::create_child(hwnd, "BUTTON", "OK", WS_CHILD | WS_VISIBLE | WS_TABSTOP, WIN_W - 208, WIN_H - 44, 92, 28, ID_OK);
        dlg::create_child(hwnd, "BUTTON", "Cancel", WS_CHILD | WS_VISIBLE | WS_TABSTOP, WIN_W - 108, WIN_H - 44, 92, 28, ID_CANCEL);
        let state = ColDlg {
            list: raw(list),
            width: raw(width),
            staged: crate::column_cfg(),
        };
        *DLG.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
    }
    render(0);
}

unsafe extern "system" fn col_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            WIN_RAW.store(raw(hwnd), Ordering::SeqCst);
            build(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, (w.0 & 0xffff) as i32, ((w.0 >> 16) & 0xffff) as u32);
            LRESULT(0)
        }
        WM_NOTIFY => {
            let hdr = l.0 as *const NMHDR;
            if !hdr.is_null() && (*hdr).idFrom == ID_LIST as usize && (*hdr).code as i32 == LVN_ITEMCHANGED_CODE {
                // selection moved: show that row's width
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    if let Some(row) = lv(d).selected_rows().into_iter().next() {
                        if let Some(c) = d.staged.get(row as usize) {
                            let w = c.width;
                            unsafe { set_window_text(h(d.width), &w.to_string()) };
                        }
                    }
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
