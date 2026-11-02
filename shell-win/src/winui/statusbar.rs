//! Status bar (msctls_statusbar32) wrapper: parts + per-part text.

use super::{instance, send, wide};
use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::Controls::{SB_SETPARTS, SB_SETTEXTW, STATUSCLASSNAMEW};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, GetWindowRect, HMENU, WM_SIZE, WS_CHILD, WS_VISIBLE};

pub struct StatusBar {
    pub hwnd: HWND,
}

impl StatusBar {
    /// Create the bar and set its parts. Part widths are right-edge
    /// coordinates in pixels; the last part is conventionally -1
    /// (extends to the window's right edge).
    pub fn create(parent: HWND, control_id: i32, parts: &[i32]) -> Result<Self> {
        unsafe {
            let hwnd = CreateWindowExW(
                Default::default(),
                STATUSCLASSNAMEW,
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                0,
                0,
                Some(parent),
                Some(HMENU(control_id as *mut core::ffi::c_void)),
                Some(instance()),
                None,
            )?;
            let bar = Self { hwnd };
            bar.set_parts(parts);
            Ok(bar)
        }
    }

    pub fn set_parts(&self, parts: &[i32]) {
        unsafe {
            send(self.hwnd, SB_SETPARTS, WPARAM(parts.len()), LPARAM(parts.as_ptr() as isize));
        }
    }

    pub fn set_text(&self, part: usize, text: &str) {
        let mut buf = wide(text);
        unsafe {
            send(self.hwnd, SB_SETTEXTW, WPARAM(part), LPARAM(buf.as_mut_ptr() as isize));
        }
    }

    /// Forward a parent WM_SIZE so the bar re-docks, return height.
    pub fn on_parent_size(&self) -> i32 {
        unsafe {
            send(self.hwnd, WM_SIZE, WPARAM(0), LPARAM(0));
            let mut rc = RECT::default();
            if GetWindowRect(self.hwnd, &mut rc).is_ok() {
                rc.bottom - rc.top
            } else {
                0
            }
        }
    }
}
