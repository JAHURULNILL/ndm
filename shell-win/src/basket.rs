//! Drop Basket (ui/basket.js + the Tauri basket window): a
//! small frameless always-on-top tool window in the app's blue
//! (#1f6fd0), 232x118, hidden until shown (View → Drop Basket).
//! Two drop paths, mirroring basket.js:
//! - OLE text drops (a link dragged from a browser): the first
//!   candidate must start http(s)://, else the note "That was
//!   not a web link"; a good link goes through core `basket_add`
//!   and the note answers "Sent to NDM ✓" / "Could not send
//!   the link".
//! - File drops (WM_DROPFILES): each path goes to core
//!   `handle_dropped_path(.., on_basket = true)`, which answers
//!   through AppEvent::BasketNote — the main loop forwards those
//!   notes here too, so feedback always lands in the basket.
//! Notes show for ~2.6 s, as in basket.js. The window drags by
//! its background/title, the ✕ hides it, and visibility is the
//! window's real state (no Tauri set_basket_visible command
//! exists in core — the native shell owns the window, exactly
//! the semantics app.js reads back as basket_visible).
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell.

use crate::winui::dialog as dlg;
use crate::winui::{pcw, set_window_text, wide};
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::core::implement;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINTL, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, SetBkMode, SetTextColor, BACKGROUND_MODE, HBRUSH, HDC};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium,
    RevokeDragDrop, DROPEFFECT, DROPEFFECT_LINK,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, IsWindowVisible, KillTimer, LoadCursorW,
    RegisterClassW, SendMessageW, SetTimer, ShowWindow, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, HTCAPTION, IDC_ARROW, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DROPFILES, WM_LBUTTONDOWN, WM_NCLBUTTONDOWN,
    WM_TIMER, WNDCLASSW, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

const ID_B_TITLE: i32 = 8801;
const ID_B_HINT: i32 = 8802;
const ID_B_NOTE: i32 = 8803;
const ID_B_CLOSE: i32 = 8804;
const NOTE_TIMER: usize = 1;
const BASKET_W: i32 = 232;
const BASKET_H: i32 = 118;

static WIN_RAW: AtomicIsize = AtomicIsize::new(0);
static NOTE_RAW: AtomicIsize = AtomicIsize::new(0);
static BRUSH_RAW: AtomicIsize = AtomicIsize::new(0);
// The registered drop target is kept for the window's whole
// lifetime (the basket is only destroyed at process exit).
static DROP_TARGET_RAW: AtomicIsize = AtomicIsize::new(0);

fn h(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}
fn raw(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn brush() -> HBRUSH {
    let b = BRUSH_RAW.load(Ordering::SeqCst);
    if b != 0 {
        return HBRUSH(b as *mut core::ffi::c_void);
    }
    // #1f6fd0 as COLORREF 0x00BBGGRR
    let created = unsafe { CreateSolidBrush(COLORREF(0x00D0_6F1F)) };
    BRUSH_RAW.store(created.0 as isize, Ordering::SeqCst);
    created
}

pub fn create(parent: HWND) {
    if WIN_RAW.load(Ordering::SeqCst) != 0 {
        return;
    }
    register_class();
    unsafe {
        let _ = OleInitialize(None);
        let class_buf = wide("NdmBasketWindow");
        let title_buf = wide("NDM Basket");
        // created hidden, per the Tauri window config (visible:false)
        if let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0),
            pcw(&class_buf),
            pcw(&title_buf),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            BASKET_W,
            BASKET_H,
            Some(parent),
            None,
            Some(crate::winui::instance()),
            None,
        ) {
            WIN_RAW.store(raw(hwnd), Ordering::SeqCst);
        }
    }
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let class_buf = wide("NdmBasketWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(basket_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: brush(),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

pub fn set_visible(on: bool) {
    let r = WIN_RAW.load(Ordering::SeqCst);
    if r != 0 {
        unsafe {
            let _ = ShowWindow(h(r), if on { SW_SHOW } else { SW_HIDE });
        }
    }
}

pub fn is_visible() -> bool {
    let r = WIN_RAW.load(Ordering::SeqCst);
    r != 0 && unsafe { IsWindowVisible(h(r)).as_bool() }
}

pub fn toggle() {
    set_visible(!is_visible());
}

/// note(): the basket's transient feedback line (2.6 s).
pub fn show_note(text: &str) {
    let (win, note) = (WIN_RAW.load(Ordering::SeqCst), NOTE_RAW.load(Ordering::SeqCst));
    if win == 0 || note == 0 {
        return;
    }
    unsafe {
        set_window_text(h(note), text);
        SetTimer(Some(h(win)), NOTE_TIMER, 2600, None);
    }
}

fn start_drag(hwnd: HWND) {
    unsafe {
        let _ = ReleaseCapture();
        let _ = SendMessageW(hwnd, WM_NCLBUTTONDOWN, Some(WPARAM(HTCAPTION as usize)), Some(LPARAM(0)));
    }
}

/// The OLE side of basket.js's drop handler.
fn accept_text(text: &str) {
    let cand = text.trim();
    if !(cand.starts_with("http://")
        || cand.starts_with("https://")
        || cand.starts_with("ftp://"))
    {
        show_note("That was not a web link");
        return;
    }
    match crate::ctx() {
        Some(c) => match ndm_core::basket_add(c, cand.to_string()) {
            Ok(()) => show_note("Sent to NDM ✓"),
            Err(_) => show_note("Could not send the link"),
        },
        None => show_note("Could not send the link"),
    }
}

fn read_format(data: &IDataObject, cf: u16) -> Option<String> {
    let fmt = FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    unsafe {
        let mut medium: STGMEDIUM = data.GetData(&fmt).ok()?;
        let hg = medium.u.hGlobal;
        let out = if hg.is_invalid() {
            None
        } else {
            let ptr = GlobalLock(hg);
            let size = GlobalSize(hg);
            let s = if ptr.is_null() || size == 0 {
                None
            } else if cf == 13 {
                // CF_UNICODETEXT
                let slice = std::slice::from_raw_parts(ptr as *const u16, size / 2);
                let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                Some(String::from_utf16_lossy(&slice[..end]))
            } else {
                // CF_TEXT (ANSI bytes)
                let slice = std::slice::from_raw_parts(ptr as *const u8, size);
                let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                Some(String::from_utf8_lossy(&slice[..end]).into_owned())
            };
            let _ = GlobalUnlock(hg);
            s
        };
        ReleaseStgMedium(&mut medium);
        out
    }
}

#[implement(IDropTarget)]
struct BasketDropTarget;

impl IDropTarget_Impl for BasketDropTarget_Impl {
    fn DragEnter(
        &self,
        _data: windows::core::Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            if !effect.is_null() {
                *effect = DROPEFFECT_LINK;
            }
        }
        Ok(())
    }
    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            if !effect.is_null() {
                *effect = DROPEFFECT_LINK;
            }
        }
        Ok(())
    }
    fn DragLeave(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn Drop(
        &self,
        data: windows::core::Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            if !effect.is_null() {
                *effect = DROPEFFECT_LINK;
            }
        }
        if let Ok(data) = data.ok() {
            if let Some(text) = read_format(&data, 13).or_else(|| read_format(&data, 1)) {
                accept_text(&text);
            } else {
                show_note("That was not a web link");
            }
        }
        Ok(())
    }
}

fn build(hwnd: HWND) {
    unsafe {
        let notify = 0x0100u32; // SS_NOTIFY so clicks reach us for dragging
        let title = dlg::create_child(
            hwnd,
            "STATIC",
            "⬇ NDM Basket",
            WS_CHILD | WS_VISIBLE | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(notify),
            12,
            10,
            170,
            20,
            ID_B_TITLE,
        );
        let hint = dlg::create_child(
            hwnd,
            "STATIC",
            "Drop a download link here",
            WS_CHILD | WS_VISIBLE | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(notify),
            12,
            44,
            208,
            20,
            ID_B_HINT,
        );
        let _ = (title, hint);
        let note = dlg::create_child(hwnd, "STATIC", "", WS_CHILD | WS_VISIBLE, 12, 78, 208, 32, ID_B_NOTE);
        NOTE_RAW.store(raw(note), Ordering::SeqCst);
        dlg::create_child(hwnd, "BUTTON", "✕", WS_CHILD | WS_VISIBLE, BASKET_W - 34, 6, 26, 24, ID_B_CLOSE);
        DragAcceptFiles(hwnd, true);
        let target: IDropTarget = BasketDropTarget.into();
        let _ = RegisterDragDrop(hwnd, &target);
        DROP_TARGET_RAW.store(windows::core::Interface::into_raw(target) as isize, Ordering::SeqCst);
    }
}

unsafe extern "system" fn basket_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            build(hwnd);
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let hdc = HDC(w.0 as *mut core::ffi::c_void);
            SetTextColor(hdc, COLORREF(0x00FF_FFFF));
            let _ = SetBkMode(hdc, BACKGROUND_MODE(1)); // TRANSPARENT
            LRESULT(brush().0 as isize)
        }
        WM_LBUTTONDOWN => {
            start_drag(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (w.0 & 0xffff) as i32;
            let code = (w.0 >> 16) & 0xffff;
            if id == ID_B_CLOSE {
                set_visible(false);
            } else if (id == ID_B_TITLE || id == ID_B_HINT) && code == 0 {
                start_drag(hwnd);
            }
            LRESULT(0)
        }
        WM_DROPFILES => {
            let hdrop = HDROP(l.0 as *mut core::ffi::c_void);
            if let Some(c) = crate::ctx() {
                let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, None) as usize;
                    let mut buf = vec![0u16; len + 1];
                    DragQueryFileW(hdrop, i, Some(&mut buf));
                    let path = String::from_utf16_lossy(&buf[..len]);
                    ndm_core::handle_dropped_path(&c, std::path::Path::new(&path), true);
                }
            }
            DragFinish(hdrop);
            LRESULT(0)
        }
        WM_TIMER => {
            if w.0 == NOTE_TIMER {
                let _ = KillTimer(Some(hwnd), NOTE_TIMER);
                let note = NOTE_RAW.load(Ordering::SeqCst);
                if note != 0 {
                    set_window_text(h(note), "");
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            set_visible(false);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = RevokeDragDrop(hwnd);
            WIN_RAW.store(0, Ordering::SeqCst);
            NOTE_RAW.store(0, Ordering::SeqCst);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}
