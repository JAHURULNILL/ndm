//! Scheduler dialog (ui/app.js smodal / openScheduler) — one
//! modeless window over Settings.queues: per-queue schedule
//! window, exit/power-on-finish, per-queue simultaneous cap, the
//! "Files in this queue" staged reorder (applied on Save through
//! core `move_in_queue`, app.js F-13), queue create/delete, and
//! Start/Stop Queue Now through `scheduler_start_now` /
//! `scheduler_stop_now` — whose errors (update latch, quota) are
//! shown verbatim, never as a success (app.js W4 Q-1).
//!
//! Switching queues, creating or deleting with unsaved edits
//! asks first (the app's discard confirm). Save closes the
//! dialog like Options (F-12); a failed order save keeps it open.
//!
//! Runtime on a real Windows is UNVERIFIED from this machine,
//! like the rest of this shell.

use crate::winui::dialog as dlg;
use crate::winui::listview::{ColumnSpec, ListView};
use crate::winui::{pcw, send, set_window_text, wide, window_text};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW,
    RegisterClassW, SetForegroundWindow, ShowWindow, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, IDC_ARROW, SW_RESTORE, SW_SHOW, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE,
    WM_DESTROY, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_OVERLAPPED,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

const ID_QUEUE: i32 = 8301;
const ID_ENABLED: i32 = 8302;
const ID_START: i32 = 8303;
const ID_STOP: i32 = 8304;
const ID_EXIT: i32 = 8305;
const ID_POWER: i32 = 8306;
const ID_MAX: i32 = 8307;
const ID_FILES: i32 = 8308;
const ID_UP: i32 = 8309;
const ID_DOWN: i32 = 8310;
const ID_NEW_NAME: i32 = 8311;
const ID_NEW: i32 = 8312;
const ID_DELETE: i32 = 8313;
const ID_START_NOW: i32 = 8314;
const ID_STOP_NOW: i32 = 8315;
const ID_STATUS: i32 = 8316;
const ID_SAVE: i32 = 8317;
const ID_CANCEL: i32 = 8318;
const ID_DAYS: i32 = 8319; // 8319..=8325, Monday first
const ID_ONE_TIME: i32 = 8326;
const ID_SYNC: i32 = 8327;
const ID_EXPIRED: i32 = 8328;

const CBN_SELCHANGE_VAL: u32 = 1;
const WIN_W: i32 = 660;
const WIN_H: i32 = 660;
const POWER: [(&str, &str); 6] = [
    ("", "Use the Options power setting"),
    ("shutdown", "Shut down"),
    ("restart", "Restart"),
    ("sleep", "Sleep"),
    ("hibernate", "Hibernate"),
    ("logoff", "Log off"),
];

struct SchedDlg {
    queue: isize,
    enabled: isize,
    start: isize,
    stop: isize,
    days: [isize; 7],
    one_time: isize,
    expired: isize,
    sync: isize,
    exit: isize,
    power: isize,
    max: isize,
    files: isize,
    new_name: isize,
    status: isize,
    btn_delete: isize,
    queues: Vec<serde_json::Value>,
    sel: String,
    dirty: bool,
    filling: bool,
    staged_order: Vec<String>,
}

static DLG: Mutex<Option<SchedDlg>> = Mutex::new(None);
static WIN_RAW: AtomicIsize = AtomicIsize::new(0);

/// The live window handle, for the main loop's dialog-message
/// routing (IsDialogMessageW): a modeless dialog gets Tab
/// navigation and Esc semantics only when its messages pass
/// through it.
pub fn dialog_hwnd() -> Option<HWND> {
    let r = WIN_RAW.load(Ordering::SeqCst);
    if r == 0 { None } else { Some(h(r)) }
}

fn h(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}
fn raw(hwnd: HWND) -> isize {
    hwnd.0 as isize
}
fn js<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}
fn jb(v: &serde_json::Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}
fn ju(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
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
        let class_buf = wide("NdmSchedulerDialog");
        let title_buf = wide("Scheduler");
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
        let class_buf = wide("NdmSchedulerDialog");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(sched_wndproc),
            hInstance: crate::winui::instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((5 + 1) as *mut core::ffi::c_void),
            lpszClassName: pcw(&class_buf),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

fn set_status(d: &SchedDlg, text: &str) {
    unsafe { set_window_text(h(d.status), text) };
}

fn status_text(status: &str) -> &'static str {
    match status {
        "finished" => "Complete",
        "downloading" => "Downloading",
        "paused" => "Paused",
        "queued" => "Queued",
        "error" => "Error",
        "starting" => "Starting",
        "merging" => "Merging",
        "cancelled" => "Cancelled",
        _ => "—",
    }
}

fn cfg_of(d: &SchedDlg) -> serde_json::Value {
    d.queues
        .iter()
        .find(|q| js(q, "name") == d.sel)
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({
                "name": d.sel, "use_schedule": false, "start": "22:00", "stop": "06:00",
                "exit_when_done": false, "power_when_done": "", "max_simultaneous": 0,
                "days": [true, true, true, true, true, true, true], "one_time": "", "sync": false
            })
        })
}

fn fill_queue_combo(d: &SchedDlg) {
    unsafe {
        send(h(d.queue), 0x014B, WPARAM(0), LPARAM(0)); // CB_RESETCONTENT
        for q in &d.queues {
            let mut buf = wide(js(q, "name"));
            send(h(d.queue), 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
        }
        let idx = d.queues.iter().position(|q| js(q, "name") == d.sel).unwrap_or(0);
        send(h(d.queue), 0x014E, WPARAM(idx), LPARAM(0));
    }
}

fn fill_fields() {
    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_mut() else { return };
    d.filling = true;
    let cfg = cfg_of(d);
    unsafe {
        dlg::set_checked(h(d.enabled), jb(&cfg, "use_schedule"));
        set_window_text(h(d.start), js(&cfg, "start"));
        set_window_text(h(d.stop), js(&cfg, "stop"));
        let days = cfg.get("days").and_then(|x| x.as_array());
        for (i, raw_day) in d.days.iter().enumerate() {
            let on = days
                .and_then(|a| a.get(i))
                .and_then(|x| x.as_bool())
                .unwrap_or(true);
            dlg::set_checked(h(*raw_day), on);
        }
        let one_time = js(&cfg, "one_time");
        set_window_text(h(d.one_time), one_time);
        // a one-time date in the past can never fire again — say so
        let today = ndm_core::today_parts().1;
        set_window_text(
            h(d.expired),
            if !one_time.is_empty() && one_time < today.as_str() {
                "expired"
            } else {
                ""
            },
        );
        dlg::set_checked(h(d.sync), jb(&cfg, "sync"));
        dlg::set_checked(h(d.exit), jb(&cfg, "exit_when_done"));
        let pidx = POWER.iter().position(|(v, _)| *v == js(&cfg, "power_when_done")).unwrap_or(0);
        send(h(d.power), 0x014E, WPARAM(pidx), LPARAM(0));
        set_window_text(h(d.max), &ju(&cfg, "max_simultaneous").to_string());
        dlg::set_enabled(h(d.btn_delete), d.sel != ndm_core::main_queue_name());
    }
    d.staged_order = crate::cache_snapshot()
        .iter()
        .filter(|v| v.queue == d.sel)
        .map(|v| v.id.clone())
        .collect();
    d.dirty = false;
    d.filling = false;
    drop(guard);
    render_files();
}

fn render_files() {
    let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_mut() else { return };
    let cache = crate::cache_snapshot();
    let mut mine: Vec<&crate::sink::ProgressView> =
        cache.iter().filter(|v| v.queue == d.sel).collect();
    let pos: std::collections::HashMap<&str, usize> = d
        .staged_order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    mine.sort_by_key(|v| pos.get(v.id.as_str()).copied().unwrap_or(usize::MAX));
    d.staged_order = mine.iter().map(|v| v.id.clone()).collect();
    let lv = ListView { hwnd: h(d.files) };
    lv.clear();
    if mine.is_empty() {
        lv.insert_row(0, &["No downloads in this queue".to_string(), String::new()]);
    } else {
        for (i, v) in mine.iter().enumerate() {
            let name = if v.name.is_empty() { v.url.clone() } else { v.name.clone() };
            lv.insert_row(i as i32, &[name, status_text(&v.status).to_string()]);
        }
    }
}

/// Save the working queue list (create/delete paths save at
/// once, exactly like app.js's schedNew / schedDelete).
fn persist_queues(d: &SchedDlg) -> Result<(), String> {
    persist_queue_list(&d.queues)
}

fn persist_queue_list(queues: &[serde_json::Value]) -> Result<(), String> {
    let mut out = crate::settings_value();
    out["queues"] = serde_json::Value::Array(queues.to_vec());
    crate::save_settings_value(out)
}

fn discard_guard(hwnd: HWND) -> bool {
    let dirty = DLG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|d| d.dirty)
        .unwrap_or(false);
    if !dirty {
        return true;
    }
    let sel = DLG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|d| d.sel.clone())
        .unwrap_or_default();
    dlg::show_confirm(
        hwnd,
        "Discard changes",
        &format!("Discard the unsaved changes for \"{sel}\"?"),
        "Discard",
    )
}

fn switch_queue(hwnd: HWND, target: String) {
    if !discard_guard(hwnd) {
        // hold the combo on the old queue while/after asking
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_ref() {
            fill_queue_combo(d);
        }
        return;
    }
    {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_mut() {
            d.sel = target;
        }
    }
    fill_fields();
}

fn on_command(hwnd: HWND, id: i32, code: u32) {
    match id {
        ID_QUEUE => {
            if code == CBN_SELCHANGE_VAL {
                let target = {
                    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(d) = guard.as_ref() else { return };
                    if d.filling {
                        return;
                    }
                    let idx = unsafe { send(h(d.queue), 0x0147, WPARAM(0), LPARAM(0)) };
                    d.queues.get(idx.max(0) as usize).map(|q| js(q, "name").to_string())
                };
                if let Some(t) = target {
                    let cur = DLG.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|d| d.sel.clone());
                    if cur.as_deref() != Some(t.as_str()) {
                        switch_queue(hwnd, t);
                    }
                }
            }
        }
        ID_ENABLED | ID_START | ID_STOP | ID_EXIT | ID_POWER | ID_MAX | ID_ONE_TIME | ID_SYNC => {
            let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(d) = guard.as_mut() {
                if !d.filling {
                    d.dirty = true;
                }
            }
        }
        x if (ID_DAYS..ID_DAYS + 7).contains(&x) => {
            let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(d) = guard.as_mut() {
                if !d.filling {
                    d.dirty = true;
                }
            }
        }
        ID_UP | ID_DOWN => {
            let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            let Some(d) = guard.as_mut() else { return };
            let lv = ListView { hwnd: h(d.files) };
            let Some(row) = lv.selected_rows().into_iter().next() else { return };
            let row = row as usize;
            let j = if id == ID_UP {
                row.wrapping_sub(1)
            } else {
                row + 1
            };
            if row < d.staged_order.len() && j < d.staged_order.len() {
                d.staged_order.swap(row, j);
                d.dirty = true;
                drop(guard);
                render_files();
                let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = guard.as_mut() {
                    let lv = ListView { hwnd: h(d.files) };
                    lv.set_selected(j as i32, true);
                }
            }
        }
        ID_NEW => {
            if !discard_guard(hwnd) {
                return;
            }
            let name = {
                let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                let Some(d) = guard.as_ref() else { return };
                unsafe { window_text(h(d.new_name)) }.trim().to_string()
            };
            let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            let Some(d) = guard.as_mut() else { return };
            if name.is_empty() {
                set_status(d, "Type a name for the new queue first");
                return;
            }
            if d.queues.iter().any(|q| js(q, "name").eq_ignore_ascii_case(&name)) {
                set_status(d, "A queue with that name already exists");
                return;
            }
            d.queues.push(serde_json::json!({
                "name": name, "use_schedule": false, "start": "22:00", "stop": "06:00",
                "exit_when_done": false, "power_when_done": "", "max_simultaneous": 0,
                "days": [true, true, true, true, true, true, true], "one_time": "", "sync": false
            }));
            match persist_queues(d) {
                Ok(()) => {
                    d.sel = name.clone();
                    unsafe { set_window_text(h(d.new_name), "") };
                    fill_queue_combo(d);
                    drop(guard);
                    fill_fields();
                    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_ref() {
                        set_status(d, &format!("Queue \"{name}\" created"));
                    }
                    crate::post_refresh();
                }
                Err(e) => set_status(d, &format!("Could not save the new queue: {e}")),
            }
        }
        ID_DELETE => {
            let sel = {
                let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                guard.as_ref().map(|d| d.sel.clone()).unwrap_or_default()
            };
            if sel.is_empty() || sel == ndm_core::main_queue_name() {
                return;
            }
            if !discard_guard(hwnd) {
                return;
            }
            if !dlg::show_confirm(
                hwnd,
                "Delete queue",
                &format!("Delete the queue \"{sel}\"?\nIts downloads move to the Main download queue."),
                "Delete queue",
            ) {
                return;
            }
            let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            let Some(d) = guard.as_mut() else { return };
            d.queues.retain(|q| js(q, "name") != sel);
            match persist_queues(d) {
                Ok(()) => {
                    d.sel = ndm_core::main_queue_name();
                    fill_queue_combo(d);
                    drop(guard);
                    fill_fields();
                    // a main-window filter on the vanished queue
                    // falls back to All Downloads (app.js)
                    if crate::current_filter() == format!("queue:{sel}") {
                        *crate::FILTER.lock().unwrap_or_else(|e| e.into_inner()) =
                            "All Downloads".to_string();
                        crate::rebuild_display();
                    }
                    crate::set_status(1, &format!("Queue \"{sel}\" deleted"));
                    crate::post_refresh();
                }
                Err(e) => set_status(d, &format!("Could not delete the queue: {e}")),
            }
        }
        ID_START_NOW => {
            let Some(c) = crate::ctx() else { return };
            let sel = DLG.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|d| d.sel.clone()).unwrap_or_default();
            match ndm_core::scheduler_start_now(c, Some(sel.clone())) {
                Ok(_) => {
                    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_ref() {
                        set_status(d, &format!("Queue \"{sel}\" started"));
                    }
                }
                Err(e) => {
                    let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(d) = guard.as_ref() {
                        set_status(d, &format!("Could not start the queue: {e}"));
                    }
                }
            }
            crate::post_refresh();
        }
        ID_STOP_NOW => {
            let Some(c) = crate::ctx() else { return };
            let sel = DLG.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|d| d.sel.clone()).unwrap_or_default();
            ndm_core::scheduler_stop_now(c, Some(sel.clone()));
            let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(d) = guard.as_ref() {
                set_status(d, &format!("Queue \"{sel}\" paused"));
            }
            crate::post_refresh();
        }
        ID_SAVE => save(hwnd),
        ID_CANCEL => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        _ => {}
    }
}

fn save(hwnd: HWND) {
    let Some(c) = crate::ctx() else { return };
    // 1) the edited queue's fields into the working list, then the
    // whole settings object through core (sanitize mirrors the
    // legacy global scheduler fields from Main itself)
    let (sel, staged, new_queues, capped) = {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = guard.as_ref() else { return };
        // An unreadable max refuses the save in words (the
        // audit-233 pattern, same as the time fields below);
        // empty keeps its meaning of 0 = use the Options value.
        // A value over the cap is clamped but NAMED afterwards,
        // not silently cut.
        let max_text = unsafe { window_text(h(d.max)) };
        let max: u64 = match max_text.trim().parse::<u64>() {
            Ok(v) => v,
            Err(_) if max_text.trim().is_empty() => 0,
            Err(_) => {
                set_status(d, "That max-simultaneous value could not be read — nothing was saved.");
                return;
            }
        };
        let capped = max > 10;
        let start_s = unsafe { window_text(h(d.start)) }.trim().to_string();
        let stop_s = unsafe { window_text(h(d.stop)) }.trim().to_string();
        let one_time_s = unsafe { window_text(h(d.one_time)) }.trim().to_string();
        // F4 parity with shell-gtk (audit-233): an invalid time
        // or date must refuse the save in words — core's
        // sanitizer used to replace "99:99" with a default
        // silently, destroying the value the user had typed.
        let hhmm_ok = |s: &str| {
            s.is_empty() || !ndm_core::normalize_hhmm(s, String::new()).is_empty()
        };
        if !hhmm_ok(&start_s) || !hhmm_ok(&stop_s) {
            set_status(d, "That start/stop time could not be read (use HH:MM, e.g. 21:30) — nothing was saved.");
            return;
        }
        if !one_time_s.is_empty() && !ndm_core::valid_ymd(&one_time_s) {
            set_status(d, "That one-time date could not be read (use YYYY-MM-DD) — nothing was saved.");
            return;
        }
        let power_s = {
            let idx = unsafe { send(h(d.power), 0x0147, WPARAM(0), LPARAM(0)) };
            POWER.get(idx.max(0) as usize).map(|(v, _)| *v).unwrap_or("").to_string()
        };
        let edited = serde_json::json!({
            "name": d.sel,
            "use_schedule": dlg::is_checked(h(d.enabled)),
            "start": start_s,
            "stop": stop_s,
            "days": d.days.iter().map(|day| dlg::is_checked(h(*day))).collect::<Vec<_>>(),
            "one_time": one_time_s,
            "sync": dlg::is_checked(h(d.sync)),
            "exit_when_done": dlg::is_checked(h(d.exit)),
            "power_when_done": power_s,
            "max_simultaneous": max.min(10),
        });
        // Clone-mutate-swap (audit-233): the dialog's working
        // list is only replaced AFTER the save succeeds — a
        // failed persist used to leave the edited queue in
        // d.queues while core kept the old one, and a later
        // create/delete save would silently persist the edit
        // the user was told had failed.
        let mut new_queues = d.queues.clone();
        for q in new_queues.iter_mut() {
            if js(q, "name") == d.sel {
                *q = edited.clone();
            }
        }
        (d.sel.clone(), d.staged_order.clone(), new_queues, capped)
    };
    if let Err(e) = persist_queue_list(&new_queues) {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_ref() {
            set_status(d, &format!("Could not save the scheduler: {e}"));
        }
        return;
    }
    {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_mut() {
            d.queues = new_queues;
        }
    }
    // 2) the staged file order, via move_in_queue walks (app.js's
    // exact algorithm over the backend's current order)
    let mut cur: Vec<String> = ndm_core::list_downloads(c.clone())
        .iter()
        .map(crate::sink::ProgressView::from)
        .filter(|v| v.queue == sel && staged.contains(&v.id))
        .map(|v| v.id.clone())
        .collect();
    let target: Vec<String> = staged.into_iter().filter(|id| cur.contains(id)).collect();
    for i in 0..target.len() {
        if cur[i] == target[i] {
            continue;
        }
        let j = cur.iter().skip(i).position(|x| *x == target[i]).map(|p| p + i).unwrap_or(i);
        for _ in i..j {
            if let Err(e) = ndm_core::move_in_queue(c.clone(), target[i].clone(), true) {
                let guard = DLG.lock().unwrap_or_else(|e2| e2.into_inner());
                if let Some(d) = guard.as_ref() {
                    set_status(d, &format!("Could not save the file order: {e}"));
                }
                return;
            }
        }
        let item = cur.remove(j);
        cur.insert(i, item);
    }
    {
        let mut guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_mut() {
            d.dirty = false;
        }
    }
    crate::post_refresh();
    if capped {
        crate::set_status(1, "Scheduler saved — max simultaneous was capped at 10");
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn build(hwnd: HWND) {
    let settings = crate::settings_value();
    let queues = settings
        .get("queues")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let sel = crate::active_queue();
    let sel = if queues.iter().any(|q| js(q, "name") == sel) {
        sel
    } else {
        ndm_core::main_queue_name()
    };
    unsafe {
        let label = |text: &str, x: i32, y: i32, w: i32| {
            dlg::create_child(hwnd, "STATIC", text, WS_CHILD | WS_VISIBLE, x, y, w, 20, 0)
        };
        let edit = |id: i32, x: i32, y: i32, w: i32| {
            dlg::create_child(
                hwnd,
                "EDIT",
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | WINDOW_STYLE(0x0080),
                x,
                y,
                w,
                23,
                id,
            )
        };
        let check = |id: i32, text: &str, y: i32| {
            dlg::create_child(hwnd, "BUTTON", text, WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0002), 16, y, 500, 22, id)
        };
        label("Queue", 16, 17, 60);
        let queue = dlg::create_child(hwnd, "COMBOBOX", "", WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0003), 80, 14, 320, 120, ID_QUEUE);
        let enabled = check(ID_ENABLED, "Enable scheduler for this queue", 46);
        label("Start downloads at", 16, 77, 180);
        let start = edit(ID_START, 200, 74, 90);
        label("Stop downloads at", 330, 77, 180);
        let stop = edit(ID_STOP, 474, 74, 90);
        let exit = check(ID_EXIT, "Exit NDM when this queue finishes", 104);
        label("When this queue finishes, turn off computer", 16, 135, 300);
        let power = dlg::create_child(hwnd, "COMBOBOX", "", WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0003), 320, 132, 300, 120, ID_POWER);
        for (_, l) in POWER {
            let mut buf = wide(l);
            send(power, 0x0143, WPARAM(0), LPARAM(buf.as_mut_ptr() as isize));
        }
        label("Max simultaneous downloads in this queue (0 = use the Options value)", 16, 163, 470);
        let max = edit(ID_MAX, 490, 160, 60);
        label("On these days", 16, 193, 110);
        let mut days = [0isize; 7];
        for (i, name) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].iter().enumerate() {
            days[i] = raw(dlg::create_child(hwnd, "BUTTON", name, WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0002), 130 + i as i32 * 72, 190, 68, 22, ID_DAYS + i as i32));
        }
        label("Or one-time date", 16, 221, 130);
        let one_time = edit(ID_ONE_TIME, 150, 218, 120);
        let expired = dlg::create_child(hwnd, "STATIC", "", WS_CHILD | WS_VISIBLE, 278, 221, 80, 20, ID_EXPIRED);
        label("(yyyy-mm-dd; empty = the weekly days above)", 360, 221, 280);
        let sync = dlg::create_child(hwnd, "BUTTON", "Synchronize — re-download finished files when they change on the server", WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0002), 16, 244, 600, 22, ID_SYNC);
        label("Files in this queue — queued downloads start in this order:", 16, 272, 560);
        let files = ListView::create(
            hwnd,
            ID_FILES,
            &[ColumnSpec { label: "File Name", width: 380 }, ColumnSpec { label: "Status", width: 120 }],
        )
        .map(|lv| {
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(lv.hwnd, 16, 296, 520, 150, true);
            lv.hwnd
        })
        .unwrap_or_default();
        let up = dlg::create_child(hwnd, "BUTTON", "↑", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 544, 296, 44, 28, ID_UP);
        let down = dlg::create_child(hwnd, "BUTTON", "↓", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 544, 332, 44, 28, ID_DOWN);
        let _ = (up, down);
        let new_name = edit(ID_NEW_NAME, 16, 454, 300);
        dlg::create_child(hwnd, "BUTTON", "New queue", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 330, 454, 110, 26, ID_NEW);
        let btn_delete = dlg::create_child(hwnd, "BUTTON", "Delete queue", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 448, 454, 110, 26, ID_DELETE);
        dlg::create_child(hwnd, "BUTTON", "▶ Start Queue Now", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 16, 490, 170, 28, ID_START_NOW);
        dlg::create_child(hwnd, "BUTTON", "⏸ Stop Queue Now", WS_CHILD | WS_VISIBLE | WS_TABSTOP, 194, 490, 170, 28, ID_STOP_NOW);
        let status = dlg::create_child(hwnd, "STATIC", "", WS_CHILD | WS_VISIBLE, 16, 528, 620, 20, ID_STATUS);
        dlg::create_child(hwnd, "BUTTON", "Save", WS_CHILD | WS_VISIBLE | WS_TABSTOP, WIN_W - 208, WIN_H - 44, 92, 28, ID_SAVE);
        dlg::create_child(hwnd, "BUTTON", "Cancel", WS_CHILD | WS_VISIBLE | WS_TABSTOP, WIN_W - 108, WIN_H - 44, 92, 28, ID_CANCEL);
        let state = SchedDlg {
            queue: raw(queue),
            enabled: raw(enabled),
            start: raw(start),
            stop: raw(stop),
            days,
            one_time: raw(one_time),
            expired: raw(expired),
            sync: raw(sync),
            exit: raw(exit),
            power: raw(power),
            max: raw(max),
            files: raw(files),
            new_name: raw(new_name),
            status: raw(status),
            btn_delete: raw(btn_delete),
            queues,
            sel,
            dirty: false,
            filling: false,
            staged_order: Vec::new(),
        };
        *DLG.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
    }
    {
        let guard = DLG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_ref() {
            fill_queue_combo(d);
        }
    }
    fill_fields();
}

unsafe extern "system" fn sched_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
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
