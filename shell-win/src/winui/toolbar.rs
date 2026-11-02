//! ToolbarWindow32 builder from a button table.

use super::{instance, pcw, send, wide};
use windows::core::{PCWSTR, Result};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{
    BTNS_AUTOSIZE, BTNS_BUTTON, BTNS_SEP, TBBUTTON, TBSTATE_ENABLED, TBSTATE_HIDDEN, TBSTYLE_FLAT,
    TBSTYLE_LIST, TB_ADDBUTTONSW, TB_ADDSTRINGW, TB_AUTOSIZE, TB_BUTTONSTRUCTSIZE, TB_SETBUTTONSIZE,
    TB_SETSTATE, TOOLBARCLASSNAMEW,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WINDOW_STYLE, WS_CHILD, WS_VISIBLE};

/// One row of the toolbar table. Separators ignore id/label.
#[derive(Clone, Debug)]
pub enum ToolbarItem {
    Button { id: i32, label: &'static str },
    Separator,
}

pub struct Toolbar {
    pub hwnd: HWND,
}

impl Toolbar {
    /// Create a flat, list-style toolbar (text labels, IDM-style) as a
    /// child of `parent` and fill it from `items`.
    pub fn create(parent: HWND, control_id: i32, items: &[ToolbarItem]) -> Result<Self> {
        unsafe {
            let hwnd = CreateWindowExW(
                Default::default(),
                TOOLBARCLASSNAMEW,
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(TBSTYLE_FLAT | TBSTYLE_LIST),
                0,
                0,
                0,
                0,
                Some(parent),
                Some(windows::Win32::UI::WindowsAndMessaging::HMENU(control_id as *mut core::ffi::c_void)),
                Some(instance()),
                None,
            )?;
            send(hwnd, TB_BUTTONSTRUCTSIZE, WPARAM(core::mem::size_of::<TBBUTTON>()), LPARAM(0));
            for item in items {
                match item {
                    ToolbarItem::Separator => {
                        let btn = TBBUTTON {
                            iBitmap: -1,
                            idCommand: 0,
                            fsState: TBSTATE_ENABLED as u8,
                            fsStyle: BTNS_SEP as u8,
                            iString: -1,
                            ..Default::default()
                        };
                        send(hwnd, TB_ADDBUTTONSW, WPARAM(1), LPARAM(&btn as *const TBBUTTON as isize));
                    }
                    ToolbarItem::Button { id, label } => {
                        let label_buf = wide(label);
                        let idx = send(hwnd, TB_ADDSTRINGW, WPARAM(0), LPARAM(label_buf.as_ptr() as isize));
                        let btn = TBBUTTON {
                            iBitmap: -1,
                            idCommand: *id,
                            fsState: TBSTATE_ENABLED as u8,
                            fsStyle: (BTNS_BUTTON | BTNS_AUTOSIZE) as u8,
                            iString: idx as isize,
                            ..Default::default()
                        };
                        send(hwnd, TB_ADDBUTTONSW, WPARAM(1), LPARAM(&btn as *const TBBUTTON as isize));
                    }
                }
            }
            send(hwnd, TB_AUTOSIZE, WPARAM(0), LPARAM(0));
            Ok(Self { hwnd })
        }
    }

    /// Enable or grey one button by command id (TB_SETSTATE; a
    /// disabled button keeps its place and label, visibly greyed).
    pub fn set_enabled(&self, id: i32, enabled: bool) {
        let state = if enabled { TBSTATE_ENABLED as isize } else { 0 };
        unsafe {
            send(self.hwnd, TB_SETSTATE, WPARAM(id as usize), LPARAM(state));
        }
    }

    /// Show or hide one button (TBSTATE_HIDDEN — the Customize
    /// Toolbar dialog's off state; the button keeps its slot in
    /// the table, it just leaves the bar).
    pub fn set_hidden(&self, id: i32, hidden: bool) {
        let state = if hidden {
            TBSTATE_HIDDEN as isize
        } else {
            TBSTATE_ENABLED as isize
        };
        unsafe {
            send(self.hwnd, TB_SETSTATE, WPARAM(id as usize), LPARAM(state));
        }
    }

    /// Set the button size (the Customize dialog's small bar);
    /// width 0 keeps the autosized width.
    pub fn set_button_size(&self, width: i32, height: i32) {
        unsafe {
            send(
                self.hwnd,
                TB_SETBUTTONSIZE,
                WPARAM(0),
                LPARAM(((height as isize) << 16) | (width as isize & 0xffff)),
            );
            send(self.hwnd, TB_AUTOSIZE, WPARAM(0), LPARAM(0));
        }
    }

    /// Re-run TB_AUTOSIZE (WM_SIZE relayout) and return pixel height.
    pub fn autosize_height(&self) -> i32 {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        unsafe {
            send(self.hwnd, TB_AUTOSIZE, WPARAM(0), LPARAM(0));
            let mut rc = RECT::default();
            if GetWindowRect(self.hwnd, &mut rc).is_ok() {
                rc.bottom - rc.top
            } else {
                0
            }
        }
    }
}

// Keep the PCWSTR helper referenced even if label handling changes.
#[allow(dead_code)]
fn _assert_pcw() {
    let b = wide("x");
    let _ = pcw(&b);
}
