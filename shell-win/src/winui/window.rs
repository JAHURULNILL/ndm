//! Top-level window creation: class registration + CreateWindowExW.
//! Productised from the proven spike shape
//! (~/workspace/native-spike/win32-spike/src/main.rs).

use super::{instance, pcw, wide};
use windows::core::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, LoadCursorW, RegisterClassW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    HCURSOR, IDC_ARROW, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW, WNDPROC, WS_OVERLAPPEDWINDOW,
};

/// Spec for one top-level window class + window.
pub struct WindowSpec {
    pub class_name: &'static str,
    pub title: String,
    pub wndproc: WNDPROC,
    pub style: WINDOW_STYLE,
    pub ex_style: WINDOW_EX_STYLE,
    pub width: i32,
    pub height: i32,
    /// Minimum tracking size enforced by the caller on WM_GETMINMAXINFO
    /// (kept as data here so the spec is the single source of truth).
    pub min_width: i32,
    pub min_height: i32,
}

impl WindowSpec {
    /// A standard overlapped main window, created hidden: the caller
    /// decides when to ShowWindow (NDM shows on state-restored).
    pub fn main_window(class_name: &'static str, title: impl Into<String>, wndproc: WNDPROC) -> Self {
        Self {
            class_name,
            title: title.into(),
            wndproc,
            style: WS_OVERLAPPEDWINDOW,
            ex_style: WINDOW_EX_STYLE(0),
            width: 1120,
            height: 720,
            min_width: 860,
            min_height: 540,
        }
    }
}

/// Register the class (idempotent enough for a single-process shell:
/// a duplicate registration simply returns 0 and creation still
/// works against the first registration) and create the window.
/// The window is NOT shown — visibility is the caller's decision.
pub fn create_top_level(spec: &WindowSpec) -> Result<HWND> {
    unsafe {
        let class_buf = wide(spec.class_name);
        let cursor: HCURSOR = LoadCursorW(None, IDC_ARROW)?;
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: spec.wndproc,
            hInstance: instance(),
            hCursor: cursor,
            // COLOR_BTNFACE + 1, as in the spike.
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let title_buf = wide(&spec.title);
        CreateWindowExW(
            spec.ex_style,
            pcw(&class_buf),
            pcw(&title_buf),
            spec.style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            spec.width,
            spec.height,
            None,
            None,
            Some(instance()),
            None,
        )
    }
}

/// InitCommonControlsEx for the control classes the shell uses
/// (list view, tree view, toolbar, status bar, standard classes).
/// Vista-era API — safe on Win7/8.
pub fn init_common_controls() {
    use windows::Win32::UI::Controls::{
        InitCommonControlsEx, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, ICC_PROGRESS_CLASS,
        ICC_STANDARD_CLASSES, ICC_TAB_CLASSES, ICC_TREEVIEW_CLASSES, INITCOMMONCONTROLSEX,
    };
    let icc = INITCOMMONCONTROLSEX {
        dwSize: core::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES
            | ICC_BAR_CLASSES
            | ICC_STANDARD_CLASSES
            | ICC_TREEVIEW_CLASSES
            | ICC_TAB_CLASSES
            | ICC_PROGRESS_CLASS,
    };
    unsafe {
        let _ = InitCommonControlsEx(&icc);
    }
}
