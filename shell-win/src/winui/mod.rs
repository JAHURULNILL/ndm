//! winui — the thin in-house Win32 helper layer (NATIVE-PLAN §2).
//!
//! Raw Win32 via the `windows` crate is the toolkit; this module is
//! the reusable 20% written once and reused by every window/dialog:
//! window-class registration, toolbar/status builders, TreeView and
//! ListView wrappers, a table-driven menu builder, and a
//! table-driven modal dialog builder.
//!
//! Win7/8 discipline (plan §4 risk 4): only Vista/7-era APIs are
//! called anywhere in this crate. Anything newer would need a
//! runtime check with a fallback inside this layer — there is
//! currently no such call site (see the audit in the crate report).

pub mod customdraw;
pub mod dialog;
pub mod listview;
pub mod menu;
pub mod statusbar;
pub mod tab;
pub mod toolbar;
pub mod treeview;
pub mod window;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

/// Owned NUL-terminated UTF-16 buffer for Win32 text APIs. Keep the
/// buffer alive for as long as the API reads the pointer.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `PCWSTR` view of a buffer produced by [`wide`].
pub fn pcw(buf: &[u16]) -> PCWSTR {
    PCWSTR(buf.as_ptr())
}

/// The process instance handle.
pub fn instance() -> HINSTANCE {
    // GetModuleHandleW(None) returns the EXE's own module handle.
    unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map(|m| HINSTANCE(m.0))
        .unwrap_or_default()
}

/// Send a message and discard the result (fire-and-forget sends).
pub unsafe fn send(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> isize {
    SendMessageW(hwnd, msg, Some(w), Some(l)).0
}

/// Read a window's text into a String.
pub unsafe fn window_text(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    let mut buf = vec![0u16; 1024];
    let n = GetWindowTextW(hwnd, &mut buf);
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

/// Set a window's text.
pub unsafe fn set_window_text(hwnd: HWND, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::SetWindowTextW;
    let buf = wide(text);
    let _ = SetWindowTextW(hwnd, pcw(&buf));
}
