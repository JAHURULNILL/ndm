//! SysTabControl32 wrapper — creation from a label table + the
//! current-selection read. Page switching is the owner's job (it
//! shows/hides the page children on TCN_SELCHANGE); the control
//! itself is the ancient common control the progress dialog's
//! three tabs need, nothing more.

use super::{instance, send, wide};
use windows::core::{Result, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{TCIF_TEXT, TCITEMW, TCM_GETCURSEL, TCM_INSERTITEMW, WC_TABCONTROLW};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, HMENU, WS_CHILD, WS_VISIBLE};

pub struct TabControl {
    pub hwnd: HWND,
}

impl TabControl {
    pub fn create(parent: HWND, control_id: i32, labels: &[&str]) -> Result<Self> {
        unsafe {
            let hwnd = CreateWindowExW(
                Default::default(),
                WC_TABCONTROLW,
                windows::core::PCWSTR::null(),
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
            let tab = Self { hwnd };
            for (i, label) in labels.iter().enumerate() {
                let mut buf = wide(label);
                let item = TCITEMW {
                    mask: TCIF_TEXT,
                    pszText: PWSTR(buf.as_mut_ptr()),
                    ..Default::default()
                };
                send(hwnd, TCM_INSERTITEMW, WPARAM(i), LPARAM(&item as *const TCITEMW as isize));
            }
            Ok(tab)
        }
    }

    /// The selected tab index (0-based), or -1 when none.
    pub fn cur_sel(&self) -> i32 {
        unsafe { send(self.hwnd, TCM_GETCURSEL, WPARAM(0), LPARAM(0)) as i32 }
    }
}
