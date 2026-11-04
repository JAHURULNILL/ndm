//! Dialog builder — the Phase-3 force multiplier (NATIVE-PLAN §4
//! risk 1: "the winui dialog-builder helpers in the first week").
//!
//! A modal dialog is generated from a field/control table: each
//! `FieldSpec` row becomes label + control (STATIC / EDIT /
//! COMBOBOX / checkbox BUTTON) laid out on a fixed grid, plus an
//! OK/Cancel button row. `show_modal` runs its own message loop
//! with the parent disabled (true modality) and returns typed
//! values — callers never touch HWNDs.
//!
//! Implementation note: children are created programmatically on a
//! plain top-level window rather than via a DLGTEMPLATE resource.
//! That keeps the table the single source of truth (no resource
//! compiler step in CI) while using the same standard controls a
//! template would instantiate.

use super::{instance, pcw, send, wide};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    IsDialogMessageW, LoadCursorW, RegisterClassW, ShowWindow, TranslateMessage, BM_GETCHECK,
    BM_SETCHECK,
    BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CBS_DROPDOWNLIST,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, DM_SETDEFID, ES_AUTOHSCROLL, HMENU, IDC_ARROW, MSG,
    SW_SHOW, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

pub const ID_OK: i32 = 9001;
pub const ID_CANCEL: i32 = 9002;
pub const ID_EXTRA: i32 = 9003;
const FIELD_ID_BASE: i32 = 9100;

/// Modal dialogs currently open (show_modal is re-entrant through
/// its own message pump, so a depth count, not a flag). Other
/// surfaces that must yield to a user dialog — the Download
/// Complete pump — read this.
static MODAL_DEPTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn any_modal_open() -> bool {
    MODAL_DEPTH.load(std::sync::atomic::Ordering::SeqCst) > 0
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    /// Static text row (the field's label is the whole row).
    Label,
    /// Label + single-line edit.
    Edit,
    /// Label + multi-line edit (the Import dialog's paste area);
    /// reads back as Text, without the single-line read's cap.
    Multiline,
    /// Label + drop-down list; `options` on the spec.
    Combo,
    /// Checkbox whose text is the label.
    Checkbox,
}

#[derive(Clone, Debug)]
pub struct FieldSpec {
    pub label: &'static str,
    pub kind: FieldKind,
    pub initial: String,
    pub options: Vec<&'static str>,
    pub selected: usize,
    pub checked: bool,
}

impl FieldSpec {
    pub fn label(text: &'static str) -> Self {
        Self { label: text, kind: FieldKind::Label, initial: String::new(), options: Vec::new(), selected: 0, checked: false }
    }
    pub fn edit(label: &'static str, initial: impl Into<String>) -> Self {
        Self { label, kind: FieldKind::Edit, initial: initial.into(), options: Vec::new(), selected: 0, checked: false }
    }
    pub fn multiline(label: &'static str, initial: impl Into<String>) -> Self {
        Self { label, kind: FieldKind::Multiline, initial: initial.into(), options: Vec::new(), selected: 0, checked: false }
    }
    pub fn combo(label: &'static str, options: Vec<&'static str>, selected: usize) -> Self {
        Self { label, kind: FieldKind::Combo, initial: String::new(), options, selected, checked: false }
    }
    pub fn checkbox(label: &'static str, checked: bool) -> Self {
        Self { label, kind: FieldKind::Checkbox, initial: String::new(), options: Vec::new(), selected: 0, checked }
    }
}

/// A typed value read back from one field row, in table order.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    Label,
    Text(String),
    Choice { index: usize, text: String },
    Checked(bool),
}

#[derive(Clone, Debug)]
pub struct DialogSpec {
    pub title: &'static str,
    pub width: i32,
    /// 0 = derive from the field count.
    pub height: i32,
    pub fields: Vec<FieldSpec>,
    pub ok_label: &'static str,
    /// None = no Cancel button (About-style dialogs).
    pub cancel_label: Option<&'static str>,
    /// Optional third button, shown left of OK (the Download
    /// Complete dialog's "Open folder" beside "Open"/"Close").
    /// Clicking it finishes with `extra: true` in the outcome.
    pub extra_label: Option<&'static str>,
}

impl DialogSpec {
    pub fn new(title: &'static str, fields: Vec<FieldSpec>) -> Self {
        Self {
            title,
            width: 460,
            height: 0,
            fields,
            ok_label: "OK",
            cancel_label: Some("Cancel"),
            extra_label: None,
        }
    }
    fn derived_height(&self) -> i32 {
        if self.height > 0 {
            self.height
        } else {
            // rows (a multiline row is taller) + button row +
            // title/caption allowance
            self.fields.iter().map(|f| row_height(f, self.width)).sum::<i32>() + 96
        }
    }
}

/// Layout height of one field row.
fn row_height(field: &FieldSpec, width: i32) -> i32 {
    match field.kind {
        FieldKind::Multiline => 118,
        // A Label row is body text in a fixed-height STATIC: one
        // hard-coded line silently CLIPS any body that wraps —
        // the Delete confirmation's "the files on disk will be
        // deleted too" line vanished exactly that way
        // (audit-233). Size the row to the wrapped line count
        // (conservative ~6px/char + word-wrap headroom; a little
        // extra height is harmless, a clipped warning is not).
        FieldKind::Label => label_lines(field.label, width - 40) * 16 + 14,
        _ => 30,
    }
}

fn label_lines(text: &str, text_width: i32) -> i32 {
    let cpl = ((text_width / 6).max(20)) as usize;
    let chars = text.chars().count();
    let effective = chars + chars / 7;
    (((effective + cpl - 1) / cpl) as i32).max(1)
}

#[derive(Clone, Debug)]
pub struct DialogOutcome {
    pub accepted: bool,
    /// True when the spec's extra button (ID_EXTRA) was clicked.
    pub extra: bool,
    pub values: Vec<FieldValue>,
}

// ----------------------------------------------------------------
// Modal plumbing. A modal runs its own message loop, and messages
// for OTHER windows still dispatch inside it — a background event
// (a download completing) can therefore open a second modal while
// the first is up. A single shared state slot would be clobbered
// by the inner dialog, orphaning the outer one mid-loop
// (audit-233, suspected → proven by construction). The state is
// a STACK keyed by dialog hwnd; every operation names its dialog.
// HWNDs are stored as isize so the state is Send.
// ----------------------------------------------------------------
struct DialogState {
    hwnd: isize,
    fields: Vec<(FieldKind, isize, Vec<String>)>, // kind, hwnd, combo options
    accepted: bool,
    extra: bool,
    finished: bool,
    outcome: Option<DialogOutcome>,
}

static DIALOG_STATE: Mutex<Vec<DialogState>> = Mutex::new(Vec::new());

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

/// Read a control's FULL text, length first (a pasted import
/// list outgrows any fixed buffer; the shared window_text now
/// uses this same approach).
unsafe fn full_window_text(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    let n = GetWindowTextW(hwnd, &mut buf);
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe fn collect_values(dialog: HWND) -> Vec<FieldValue> {
    let guard = DIALOG_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.iter().find(|s| s.hwnd == dialog.0 as isize) else { return Vec::new() };
    state
        .fields
        .iter()
        .map(|(kind, raw, options)| {
            let hwnd = hwnd_of(*raw);
            match kind {
                FieldKind::Label => FieldValue::Label,
                FieldKind::Edit => FieldValue::Text(full_window_text(hwnd)),
                FieldKind::Multiline => FieldValue::Text(full_window_text(hwnd)),
                FieldKind::Combo => {
                    let idx = send(hwnd, CB_GETCURSEL, WPARAM(0), LPARAM(0));
                    let index = if idx < 0 { 0 } else { idx as usize };
                    let text = options.get(index).cloned().unwrap_or_default();
                    FieldValue::Choice { index, text }
                }
                FieldKind::Checkbox => {
                    let checked = send(hwnd, BM_GETCHECK, WPARAM(0), LPARAM(0)) == 1;
                    FieldValue::Checked(checked)
                }
            }
        })
        .collect()
}

unsafe fn finish_dialog(hwnd: HWND, accepted: bool, extra: bool) {
    let values = collect_values(hwnd);
    {
        let mut guard = DIALOG_STATE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = guard.iter_mut().find(|s| s.hwnd == hwnd.0 as isize) {
            state.accepted = accepted;
            state.extra = extra;
            state.finished = true;
            state.outcome = Some(DialogOutcome { accepted, extra, values });
        }
    }
    let _ = DestroyWindow(hwnd);
}

unsafe extern "system" fn dlg_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (w.0 & 0xffff) as i32;
            // IDOK (1) / IDCANCEL (2): what IsDialogMessageW sends
            // for Enter / Esc when no button consumed them first —
            // they mean the same as this dialog's OK / Cancel.
            if id == 1 {
                finish_dialog(hwnd, true, false);
                return LRESULT(0);
            }
            if id == 2 {
                finish_dialog(hwnd, false, false);
                return LRESULT(0);
            }
            if id == ID_OK {
                finish_dialog(hwnd, true, false);
                return LRESULT(0);
            }
            if id == ID_EXTRA {
                finish_dialog(hwnd, false, true);
                return LRESULT(0);
            }
            if id == ID_CANCEL {
                finish_dialog(hwnd, false, false);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_CLOSE => {
            finish_dialog(hwnd, false, false);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

fn register_dialog_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let class_buf = wide("NdmWinUiDialog");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dlg_proc),
            hInstance: instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

/// Create one child control on a window built by hand (the modal
/// builder above, or a modeless window such as the progress
/// dialog). Public so modeless windows share the one creation
/// path instead of re-wrapping CreateWindowExW per window.
pub unsafe fn create_child(
    parent: HWND,
    class: &str,
    text: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
) -> HWND {
    let class_buf = wide(class);
    let text_buf = wide(text);
    CreateWindowExW(
        Default::default(),
        pcw(&class_buf),
        pcw(&text_buf),
        style,
        x,
        y,
        w,
        h,
        Some(parent),
        Some(HMENU(id as *mut core::ffi::c_void)),
        Some(instance()),
        None,
    )
    .unwrap_or_default()
}

/// Show a modal dialog built from `spec`; returns typed values.
/// Must be called on the UI thread.
pub fn show_modal(parent: HWND, spec: &DialogSpec) -> DialogOutcome {
    register_dialog_class();
    MODAL_DEPTH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let outcome = show_modal_inner(parent, spec);
    MODAL_DEPTH.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    outcome
}

/// Parents currently disabled by an open modal, as a multiset:
/// nested modals over the SAME parent each hold one entry. The
/// epilogue used to re-enable the parent unconditionally, so
/// closing an inner modal re-enabled the window while an outer
/// modal was still open over it.
static PARENT_DISABLES: Mutex<Vec<isize>> = Mutex::new(Vec::new());

fn disable_parent(parent: HWND) {
    let mut guard = PARENT_DISABLES.lock().unwrap_or_else(|e| e.into_inner());
    let first = !guard.contains(&(parent.0 as isize));
    guard.push(parent.0 as isize);
    if first {
        unsafe {
            let _ = EnableWindow(parent, false);
        }
    }
}

fn enable_parent(parent: HWND) {
    let still_held = {
        let mut guard = PARENT_DISABLES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = guard.iter().position(|p| *p == parent.0 as isize) {
            guard.remove(pos);
        }
        guard.contains(&(parent.0 as isize))
    };
    if !still_held {
        unsafe {
            let _ = EnableWindow(parent, true);
        }
    }
}

fn show_modal_inner(parent: HWND, spec: &DialogSpec) -> DialogOutcome {
    unsafe {
        let class_buf = wide("NdmWinUiDialog");
        let title_buf = wide(spec.title);
        let height = spec.derived_height();
        let hwnd = match CreateWindowExW(
            Default::default(),
            pcw(&class_buf),
            pcw(&title_buf),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            spec.width,
            height,
            Some(parent),
            None,
            Some(instance()),
            None,
        ) {
            Ok(h) => h,
            Err(_) => return DialogOutcome { accepted: false, extra: false, values: Vec::new() },
        };

        // Build the field rows on the fixed grid.
        let margin = 12;
        let label_w = 120;
        let control_x = margin + label_w + 8;
        let control_w = (spec.width - control_x - margin - 16).max(80);
        let mut handles: Vec<(FieldKind, isize, Vec<String>)> = Vec::new();
        let mut y = margin;
        for (i, field) in spec.fields.iter().enumerate() {
            let id = FIELD_ID_BASE + i as i32;
            let this_y = y;
            let row_h = row_height(field, spec.width);
            y += row_h;
            let y = this_y;
            match field.kind {
                FieldKind::Label => {
                    let h = create_child(hwnd, "STATIC", field.label, WS_CHILD | WS_VISIBLE, margin, y + 3, spec.width - 2 * margin - 16, row_h - 10, id);
                    handles.push((field.kind.clone(), h.0 as isize, Vec::new()));
                }
                FieldKind::Edit => {
                    create_child(hwnd, "STATIC", field.label, WS_CHILD | WS_VISIBLE, margin, y + 3, label_w, 20, 0);
                    let style = WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32);
                    let h = create_child(hwnd, "EDIT", &field.initial, style, control_x, y, control_w, 23, id);
                    handles.push((field.kind.clone(), h.0 as isize, Vec::new()));
                }
                FieldKind::Multiline => {
                    // The label sits above a full-width edit (the
                    // Import dialog's shape in the web UI).
                    create_child(hwnd, "STATIC", field.label, WS_CHILD | WS_VISIBLE, margin, y + 3, spec.width - 2 * margin - 16, 20, 0);
                    let style = WS_CHILD
                        | WS_VISIBLE
                        | WS_BORDER
                        | WS_TABSTOP
                        | WINDOW_STYLE((ES_AUTOHSCROLL | 0x0004 | 0x0001) as u32) // ES_MULTILINE | ES_AUTOVSCROLL
                        | WINDOW_STYLE(0x0010_0000); // WS_VSCROLL
                    let h = create_child(hwnd, "EDIT", &field.initial, style, margin, y + 24, spec.width - 2 * margin - 16, 88, id);
                    handles.push((field.kind.clone(), h.0 as isize, Vec::new()));
                }
                FieldKind::Combo => {
                    create_child(hwnd, "STATIC", field.label, WS_CHILD | WS_VISIBLE, margin, y + 3, label_w, 20, 0);
                    let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(CBS_DROPDOWNLIST as u32);
                    let h = create_child(hwnd, "COMBOBOX", "", style, control_x, y, control_w, 120, id);
                    for opt in &field.options {
                        let mut buf = wide(opt);
                        send(h, CB_ADDSTRING, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
                    }
                    send(h, CB_SETCURSEL, WPARAM(field.selected), LPARAM(0));
                    let options: Vec<String> = field.options.iter().map(|s| s.to_string()).collect();
                    handles.push((field.kind.clone(), h.0 as isize, options));
                }
                FieldKind::Checkbox => {
                    let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32);
                    let h = create_child(hwnd, "BUTTON", field.label, style, margin, y, spec.width - 2 * margin - 16, 23, id);
                    if field.checked {
                        send(h, BM_SETCHECK, WPARAM(1), LPARAM(0));
                    }
                    handles.push((field.kind.clone(), h.0 as isize, Vec::new()));
                }
            }
        }

        // Button row, right-aligned, in display order: the extra
        // button (if any), OK, Cancel (if any).
        let by = height - 64;
        let bw = 88;
        let mut btns: Vec<(i32, &str)> = Vec::new();
        if let Some(extra) = spec.extra_label {
            btns.push((ID_EXTRA, extra));
        }
        btns.push((ID_OK, spec.ok_label));
        if let Some(cancel) = spec.cancel_label {
            btns.push((ID_CANCEL, cancel));
        }
        let total_w = btns.len() as i32 * bw + (btns.len() as i32 - 1) * 8;
        let mut bx = spec.width - margin - 16 - total_w;
        for (id, label) in btns {
            // OK is the default push button: Enter activates it
            // (with DM_SETDEFID below, IsDialogMessageW routes
            // Enter to it instead of dropping the key).
            let style = if id == ID_OK {
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32)
            } else {
                WS_CHILD | WS_VISIBLE | WS_TABSTOP
            };
            create_child(hwnd, "BUTTON", label, style, bx, by, bw, 26, id);
            bx += bw + 8;
        }
        send(hwnd, DM_SETDEFID, WPARAM(ID_OK as usize), LPARAM(0));

        {
            let mut guard = DIALOG_STATE.lock().unwrap_or_else(|e| e.into_inner());
            guard.push(DialogState {
                hwnd: hwnd.0 as isize,
                fields: handles,
                accepted: false,
                extra: false,
                finished: false,
                outcome: None,
            });
        }

        // Modality: disable the parent for the duration of the loop.
        disable_parent(parent);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg = MSG::default();
        loop {
            let done = {
                let guard = DIALOG_STATE.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .iter()
                    .find(|s| s.hwnd == hwnd.0 as isize)
                    .map(|s| s.finished)
                    .unwrap_or(true)
            };
            if done {
                break;
            }
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                // WM_QUIT while modal: re-post so the outer loop exits too.
                windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
                break;
            }
            // Dialog keyboard navigation (Tab order, mnemonics,
            // arrow keys in groups) — without IsDialogMessageW
            // the modal's controls were mouse-only (audit-233).
            if IsDialogMessageW(hwnd, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        enable_parent(parent);

        let outcome = {
            let mut guard = DIALOG_STATE.lock().unwrap_or_else(|e| e.into_inner());
            match guard.iter().position(|s| s.hwnd == hwnd.0 as isize) {
                Some(pos) => guard.remove(pos).outcome,
                None => None,
            }
        };
        match outcome {
            Some(outcome) => outcome,
            None => DialogOutcome { accepted: false, extra: false, values: Vec::new() },
        }
    }
}

/// Checkbox state helpers for hand-built windows (the modal
/// builder reads its own fields through `collect_values`; a
/// modeless window needs the same two messages as functions).
pub fn set_checked(hwnd: HWND, on: bool) {
    unsafe {
        send(hwnd, BM_SETCHECK, WPARAM(if on { 1 } else { 0 }), LPARAM(0));
    }
}

pub fn is_checked(hwnd: HWND) -> bool {
    unsafe { send(hwnd, BM_GETCHECK, WPARAM(0), LPARAM(0)) == 1 }
}

/// Enable or grey any child control (EnableWindow wrapper).
pub fn set_enabled(hwnd: HWND, on: bool) {
    unsafe {
        let _ = EnableWindow(hwnd, on);
    }
}

/// The generic Confirm (the web UI's cfmodal / showConfirm):
/// message + a primary button + Cancel; ×/Cancel settle false.
pub fn show_confirm(parent: HWND, title: &str, msg: &str, yes_label: &str) -> bool {
    let title_s: &'static str = Box::leak(title.to_string().into_boxed_str());
    let msg_s: &'static str = Box::leak(msg.to_string().into_boxed_str());
    let yes_s: &'static str = Box::leak(yes_label.to_string().into_boxed_str());
    let spec = DialogSpec {
        title: title_s,
        width: 460,
        height: 0,
        fields: vec![FieldSpec::label(msg_s)],
        ok_label: yes_s,
        cancel_label: Some("Cancel"),
        extra_label: None,
    };
    show_modal(parent, &spec).accepted
}

/// The generic Prompt (the web UI's pmodal / showPrompt): title +
/// message + one text field; OK returns the entered text,
/// Cancel/× returns None.
pub fn show_prompt(parent: HWND, title: &str, msg: &str, initial: &str) -> Option<String> {
    let title_s: &'static str = Box::leak(title.to_string().into_boxed_str());
    let msg_s: &'static str = Box::leak(msg.to_string().into_boxed_str());
    let spec = DialogSpec::new(
        title_s,
        vec![FieldSpec::label(msg_s), FieldSpec::edit("", initial.to_string())],
    );
    let outcome = show_modal(parent, &spec);
    if !outcome.accepted {
        return None;
    }
    match outcome.values.get(1) {
        Some(FieldValue::Text(t)) => Some(t.clone()),
        _ => None,
    }
}

/// Worked example: the About dialog, built entirely with the dialog
/// builder (labels + a single OK button) — the same content the web
/// UI's `amodal` shows (ui/index.html): app name, version, engine
/// line and the about note.
pub fn show_about(parent: HWND, version: &str, engine: &str) -> DialogOutcome {
    let version_line: &'static str = Box::leak(format!("Version {version}").into_boxed_str());
    let engine_line: &'static str = Box::leak(format!("Video engine: {engine}").into_boxed_str());
    let spec = DialogSpec {
        title: "About NDM",
        width: 440,
        height: 0,
        fields: vec![
            FieldSpec::label("Nill Download Manager"),
            FieldSpec::label(version_line),
            FieldSpec::label(engine_line),
            FieldSpec::label("Fast, resumable downloads with browser integration"),
            FieldSpec::label("for files and videos from 1,700+ sites."),
        ],
        ok_label: "OK",
        cancel_label: None,
        extra_label: None,
    };
    show_modal(parent, &spec)
}
