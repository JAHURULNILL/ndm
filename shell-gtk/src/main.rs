//! ndm-shell-gtk — NDM's native Linux shell (GTK 3, gtk-rs).
//!
//! The main window (1120×720, min 860×540 — the §1.1 spec the
//! Tauri window carries today), the app's real toolbar /
//! category / column / menu surfaces read from the web UI
//! (`ui/index.html`, `ui/app.js`), the block map as a custom
//! cell renderer, and the `EventSink` bridge that replaces
//! Tauri `emit`/`listen`.
//!
//! FUNCTIONAL WIRING (this stage): `main` bootstraps ndm-core
//! in the exact order the Tauri shell's setup() does
//! (AppState::new → Ctx::new → init_paths → startup sweeps →
//! ext notice → the background entry points), the list is
//! filled from core's `list_downloads` and kept live through
//! the sink, the category tree filters the list with the web
//! UI's semantics, and the toolbar / menus / row context menu
//! call the core commands of the same name. Actions with no
//! honest core path in this shell yet report themselves in
//! the status bar instead of pretending to work — the same
//! no-fake-fallback rule the web UI follows (app.js F-2). The
//! per-download progress dialog (progress.rs) opens from a row
//! double-click, the row menu's "Show progress", and core's
//! ProgressDialogReveal.

mod blockmap;
mod dialogs;
mod form;
mod gtksink;
mod icons;
mod modals;
mod basket;
mod grabber;
mod misc;
mod options;
mod prefs;
mod progress;
mod scheduler;
mod sink;
mod video;

use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gtksink::{GtkEventSink, UiMessage};
use sink::{AppEvent, AppState, Ctx, ProgressView};

// ---------------------------------------------------------------------------
// Download-list model columns.
//
// The visible TreeView columns are the web UI's real column set
// in its real order (app.js COLUMN_DEFS): File Name, Q, Size,
// Status, Time left, Transfer rate, Last Try Date, Description,
// Date Added, Save To — plus the block-map column the plan (§3)
// assigns to a custom cell renderer, appended after the real set
// so their order is untouched. The remaining model columns are
// bookkeeping the renderers/handlers need (entry id, raw bytes).
// ---------------------------------------------------------------------------
const COL_ID: u32 = 0; // String — entry id (not a visible column)
const COL_NAME: u32 = 1; // String — File Name
const COL_Q: u32 = 2; // String — Q (queue marker)
const COL_SIZE: u32 = 3; // String — Size (formatted)
const COL_STATUS: u32 = 4; // String — Status (formatted)
const COL_TIMELEFT: u32 = 5; // String — Time left (formatted)
const COL_RATE: u32 = 6; // String — Transfer rate (formatted)
const COL_LASTTRY: u32 = 7; // String — Last Try Date (formatted)
const COL_DESC: u32 = 8; // String — Description
const COL_ADDED: u32 = 9; // String — Date Added (formatted)
const COL_SAVETO: u32 = 10; // String — Save To
const COL_BLOCKS: u32 = 11; // String — 128-char block map (renderer)
const COL_FRACTION: u32 = 12; // f64 — done/total (renderer fallback)
const COL_DONE: u32 = 13; // u64 — raw done bytes
const COL_TOTAL: u32 = 14; // u64 — raw total bytes
const COL_SPEED: u32 = 15; // f64 — raw speed B/s
const COL_ADDED_RAW: u32 = 16; // String — ISO added stamp (hidden sort key)
const COL_STATUS_RAW: u32 = 17; // String — raw status word (hidden sort key)
const COL_ICON: u32 = 18; // Pixbuf — per-type row glyph (item 17)
const NCOLS: usize = 19;

// Category-tree model columns: the row label plus the filter
// key the selection applies (app.js visibleItems semantics:
// "All Downloads", a category name, "Unfinished", "Finished",
// "Grabber", or "queue:<name>"; "" = not selectable, the
// Queues header row).
const CAT_COL_LABEL: u32 = 0;
const CAT_COL_FILTER: u32 = 1;

// ---------------------------------------------------------------------------
// Formatting — mirrors of the web UI's list formatters (app.js):
// sizes always 2 decimals, rates in "/sec", humanised time-left,
// IDM date shape. One format on every surface (app.js W-6).
// ---------------------------------------------------------------------------
const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

fn fmt_size_f(mut v: f64) -> String {
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", v, UNITS[i])
}

fn fmt_size(b: u64) -> String {
    fmt_size_f(b as f64)
}

fn fmt_rate(bps: f64) -> String {
    if !(bps > 0.0) {
        return String::new();
    }
    if bps < 1024.0 {
        return format!("{:.2} KB/sec", bps / 1024.0);
    }
    format!("{}/sec", fmt_size_f(bps))
}

fn fmt_left(secs: f64) -> String {
    let sec = secs.max(0.0).round() as u64;
    if sec < 60 {
        return format!("{} sec", sec);
    }
    let m = sec / 60;
    let s = sec % 60;
    if m < 60 {
        return format!("{} min {} sec", m, s);
    }
    format!("{} hour(s) {} min", m / 60, m % 60)
}

const IDM_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "YYYY-MM-DD HH:MM:SS" → "Mon DD HH:MM:SS YYYY" (app.js
/// fmtIdmDate); unparseable strings pass through unchanged.
fn fmt_idm_date(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() < 16 {
        return s.to_string();
    }
    let (year, rest) = s.split_at(4);
    let mut parts = rest.split(|c| c == '-' || c == ' ' || c == 'T' || c == ':');
    let _ = parts.next(); // leading empty from the '-' split
    let (Some(month), Some(day), Some(hh), Some(mm)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return s.to_string();
    };
    let Ok(m) = month.parse::<usize>() else {
        return s.to_string();
    };
    if !(1..=12).contains(&m) {
        return s.to_string();
    }
    let ss = parts.next().unwrap_or("");
    if ss.is_empty() {
        format!("{} {} {}:{} {}", IDM_MONTHS[m - 1], day, hh, mm, year)
    } else {
        format!("{} {} {}:{}:{} {}", IDM_MONTHS[m - 1], day, hh, mm, ss, year)
    }
}

fn pct_of(total: u64, done: u64) -> f64 {
    if total > 0 {
        (100.0 * done as f64 / total as f64).min(100.0)
    } else {
        0.0
    }
}

/// The list's Status cell text (app.js cellHtml 'status'):
/// "Complete" / "Error" / "Queued" / "Paused" / phase labels,
/// else the bare 2-decimal percentage. (QA-220 m5: a paused
/// row used to fall through to the percentage, so nothing in
/// the list said the download was paused.)
fn status_text(d: &ProgressView) -> String {
    match d.status.as_str() {
        "finished" => "Complete".to_string(),
        "error" => "Error".to_string(),
        "queued" => "Queued".to_string(),
        "paused" => "Paused".to_string(),
        "starting" => {
            if d.detail.trim().is_empty() {
                "Starting…".to_string()
            } else {
                d.detail.clone()
            }
        }
        "resuming" => {
            if d.detail.trim().is_empty() {
                "Resuming…".to_string()
            } else {
                d.detail.clone()
            }
        }
        "merging" => "Merging…".to_string(),
        _ if d.total > 0 => format!("{:.2}%", pct_of(d.total, d.done_bytes)),
        s => {
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UI state shared by the builders, the actions and the event
// handler. Everything here is touched only on the GTK main
// thread (core work goes through `ctx` / the core runtime).
// ---------------------------------------------------------------------------
struct Ui {
    window: gtk::ApplicationWindow,
    store: gtk::ListStore,
    /// Status bar sections (plan §1/§2 spike: 3 parts) —
    /// [0] list counts, [1] aggregate transfer rate,
    /// [2] current notice / readiness.
    status: [gtk::Label; 3],
    /// ETA credibility samples per entry (app.js noteEtaSample):
    /// id → (last done_bytes seen, distinct-sample count).
    eta_samples: RefCell<HashMap<String, (u64, u32)>>,
    /// The core handle every action calls through.
    ctx: Ctx,
    /// The shell's mirror of the core list (id → last payload
    /// view), in core list order via `order`. Filters, Delete
    /// Completed and the counts read this, exactly as the web
    /// UI reads its `items` map.
    entries: RefCell<HashMap<String, ProgressView>>,
    order: RefCell<Vec<String>>,
    /// The active category-tree filter key (see CAT_COL_FILTER).
    filter: RefCell<String>,
    list_view: gtk::TreeView,
    cat_store: gtk::TreeStore,
    cat_view: gtk::TreeView,
    /// The queue the Start/Stop Queue toolbar buttons act on
    /// (app.js queuePick: the picked queue, else the first).
    queue_pick: RefCell<String>,
    /// The toolbar widget + its buttons by app.js TOOLBAR_DEFS
    /// id — the Customize Toolbar dialog shows/hides through
    /// this map (prefs `ndm-toolbar-v1`).
    toolbar: RefCell<Option<gtk::Toolbar>>,
    toolbar_buttons: RefCell<HashMap<String, gtk::ToolButton>>,
    /// The main list's column configuration (prefs
    /// `ndm-columns-v2`), applied by populate_columns.
    col_cfg: RefCell<Vec<prefs::ColumnCfg>>,
    /// The View menu's Drop Basket check item — the basket
    /// window's own close button keeps it in step (basket.rs).
    basket_check: RefCell<Option<gtk::CheckMenuItem>>,
    /// The category sidebar's scrolled container — the View
    /// menu's Categories Sidebar toggle shows/hides it (2.3.1:
    /// the toggle used to be scaffold with no handler at all).
    cat_pane: RefCell<Option<gtk::ScrolledWindow>>,
}

/// ETA text for a payload, "~"-marked until credible — the same
/// rule as app.js fmtEta (≥256 KB downloaded AND ≥3 distinct
/// samples), empty whenever no honest estimate exists.
fn eta_text(ui: &Ui, d: &ProgressView) -> String {
    if d.status != "downloading" || d.total == 0 || !(d.speed_bps > 0.0) {
        if d.status != "downloading" {
            ui.eta_samples.borrow_mut().remove(&d.id);
        }
        return String::new();
    }
    let remaining = d.total.saturating_sub(d.done_bytes);
    if remaining == 0 {
        return String::new();
    }
    let mut samples = ui.eta_samples.borrow_mut();
    let entry = samples.entry(d.id.clone()).or_insert((d.done_bytes, 1));
    if entry.0 != d.done_bytes {
        entry.0 = d.done_bytes;
        entry.1 += 1;
    }
    let credible = d.done_bytes >= 256 * 1024 && entry.1 >= 3;
    let text = fmt_left(remaining as f64 / d.speed_bps);
    if credible {
        text
    } else {
        format!("~{}", text)
    }
}

/// Locate an entry's row by its stable id, as a TreePath.
///
/// The path — never the iter — crosses this function's boundary:
/// a GtkTreeIter that was advanced by `iter_next` and then
/// returned by value can silently point at row 0 again (the
/// mutation goes through a shared reference, and the returned
/// copy is free to be materialized from the pre-advance value;
/// proven with a minimal repro during the 2.1.0 audit fixes —
/// M1). Callers re-acquire the iter from the path at the point
/// of use, which is always exact.
fn find_path(store: &gtk::ListStore, id: &str) -> Option<gtk::TreePath> {
    let mut iter = store.iter_first()?;
    loop {
        let row_id: String = store
            .value(&iter, COL_ID as i32)
            .get()
            .unwrap_or_default();
        if row_id == id {
            return store.path(&iter);
        }
        if !store.iter_next(&mut iter) {
            return None;
        }
    }
}

/// The iter for an entry's row, freshly acquired from its path
/// (see find_path for why the iter itself is never carried).
fn iter_for(store: &gtk::ListStore, id: &str) -> Option<gtk::TreeIter> {
    find_path(store, id).and_then(|p| store.iter(&p))
}

/// Write one payload into its list row, creating the row on first
/// sight. This is the native replacement for the web UI's
/// progress-event row update (render/updateRowCells).
fn apply_progress(ui: &Ui, d: &ProgressView) {
    let left = eta_text(ui, d);
    let iter = match iter_for(&ui.store, &d.id) {
        Some(it) => it,
        None => ui.store.append(),
    };
    // IDM shows its queue glyph here; text stand-in until Phase 5 icons
    let q: String = if matches!(d.status.as_str(), "downloading" | "paused" | "queued") {
        "●".to_string()
    } else {
        String::new()
    };
    let size = if d.total > 0 || d.status == "finished" {
        fmt_size(d.total)
    } else {
        "Unknown".to_string()
    };
    let fraction = if d.total > 0 {
        (d.done_bytes as f64 / d.total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ui.store.set(
        &iter,
        &[
            (COL_ID, &d.id as &dyn gtk::glib::ToValue),
            (COL_NAME, &d.name as &dyn gtk::glib::ToValue),
            (COL_Q, &q as &dyn gtk::glib::ToValue),
            (COL_SIZE, &size as &dyn gtk::glib::ToValue),
            (COL_STATUS, &status_text(d) as &dyn gtk::glib::ToValue),
            (COL_TIMELEFT, &left as &dyn gtk::glib::ToValue),
            (
                COL_RATE,
                &(if d.status == "downloading" {
                    fmt_rate(d.speed_bps)
                } else {
                    String::new()
                }) as &dyn gtk::glib::ToValue,
            ),
            (
                COL_LASTTRY,
                &fmt_idm_date(if d.last_try.is_empty() {
                    &d.added
                } else {
                    &d.last_try
                }) as &dyn gtk::glib::ToValue,
            ),
            (COL_DESC, &d.description as &dyn gtk::glib::ToValue),
            (COL_ADDED, &fmt_idm_date(&d.added) as &dyn gtk::glib::ToValue),
            (COL_SAVETO, &d.save_to as &dyn gtk::glib::ToValue),
            (COL_BLOCKS, &d.blocks as &dyn gtk::glib::ToValue),
            (COL_FRACTION, &fraction as &dyn gtk::glib::ToValue),
            (COL_DONE, &d.done_bytes as &dyn gtk::glib::ToValue),
            (COL_TOTAL, &d.total as &dyn gtk::glib::ToValue),
            (COL_SPEED, &d.speed_bps as &dyn gtk::glib::ToValue),
            (COL_ADDED_RAW, &d.added as &dyn gtk::glib::ToValue),
            (COL_STATUS_RAW, &d.status as &dyn gtk::glib::ToValue),
            (
                COL_ICON,
                &icons::pixbuf(
                    icons::row_icon_name(&d.category, &d.name),
                    16,
                    ui.window.scale_factor(),
                ) as &dyn gtk::glib::ToValue,
            ),
        ],
    );
    refresh_status_counts(ui);
}

/// Sections [0]/[1] of the status bar follow the list itself:
/// row count and the summed live rate of downloading rows.
fn refresh_status_counts(ui: &Ui) {
    let mut count = 0u32;
    let mut active = 0u32;
    let mut speed = 0.0f64;
    if let Some(mut iter) = ui.store.iter_first() {
        loop {
            count += 1;
            let sp: f64 = ui
                .store
                .value(&iter, COL_SPEED as i32)
                .get()
                .unwrap_or_default();
            if sp > 0.0 {
                active += 1;
                speed += sp;
            }
            if !ui.store.iter_next(&mut iter) {
                break;
            }
        }
    }
    ui.status[0].set_text(&if count == 0 {
        "No downloads".to_string()
    } else {
        format!("{} download{}", count, if count == 1 { "" } else { "s" })
    });
    ui.status[1].set_text(&if active > 0 {
        format!("Total: {}", fmt_rate(speed))
    } else {
        String::new()
    });
}

fn set_notice(ui: &Ui, text: &str) {
    ui.status[2].set_text(text);
}

/// An init_paths failure from main(): the window does not exist
/// yet when it happens, so the error waits here for build_ui to
/// put it in the status bar — the Err arm's comment has always
/// promised that surface; this slot is what keeps the promise.
static INIT_PATHS_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

// ---------------------------------------------------------------------------
// List data — the mirror, the filter, full refreshes.
// ---------------------------------------------------------------------------

/// The web UI's filter semantics (app.js visibleItems), verbatim:
/// Finished → status finished; Unfinished → status not finished;
/// "queue:<name>" → entry queue equals the name; "All Downloads"
/// → everything; anything else → entry category equals the key
/// (this is also how the "Grabber projects" row, key "Grabber",
/// filters).
fn entry_visible(filter: &str, d: &ProgressView) -> bool {
    if filter == "All Downloads" {
        true
    } else if filter == "Finished" {
        d.status == "finished"
    } else if filter == "Unfinished" {
        d.status != "finished"
    } else if let Some(q) = filter.strip_prefix("queue:") {
        d.queue == q
    } else {
        d.category == filter
    }
}

/// Rebuild the mirror and the visible rows from core's list.
/// Deliberately ONLY for startup, a category-filter change, an
/// explicit Refresh, and the completion of a user action (add /
/// queue reorder) — never per core event: Progress,
/// DownloadComplete and EntryRemoved all update single rows
/// incrementally, so the per-event cost stays O(1) in rows.
fn refresh_list(ui: &Ui) {
    let views: Vec<ProgressView> = sink::list_downloads(ui.ctx.clone())
        .iter()
        .map(ProgressView::from)
        .collect();
    {
        let mut entries = ui.entries.borrow_mut();
        let mut order = ui.order.borrow_mut();
        entries.clear();
        order.clear();
        for v in &views {
            entries.insert(v.id.clone(), v.clone());
            order.push(v.id.clone());
        }
    }
    ui.store.clear();
    let filter = ui.filter.borrow().clone();
    for v in &views {
        if entry_visible(&filter, v) {
            apply_progress(ui, v);
        }
    }
    refresh_status_counts(ui);
    refresh_toolbar_states(ui);
}

/// One live payload: update the mirror, then the row — creating
/// it when the entry belongs to the current filter, removing it
/// when it no longer does.
fn upsert_entry(ui: &Ui, d: &ProgressView) {
    {
        let mut entries = ui.entries.borrow_mut();
        if !entries.contains_key(&d.id) {
            ui.order.borrow_mut().push(d.id.clone());
        }
        entries.insert(d.id.clone(), d.clone());
    }
    if entry_visible(&ui.filter.borrow(), d) {
        apply_progress(ui, d);
    } else if let Some(mut iter) = iter_for(&ui.store, &d.id) {
        ui.store.remove(&mut iter);
        ui.eta_samples.borrow_mut().remove(&d.id);
        refresh_status_counts(ui);
    }
    refresh_toolbar_states(ui);
}

fn remove_entry_local(ui: &Ui, id: &str) {
    ui.entries.borrow_mut().remove(id);
    ui.order.borrow_mut().retain(|x| x != id);
    ui.eta_samples.borrow_mut().remove(id);
    if let Some(mut iter) = iter_for(&ui.store, id) {
        ui.store.remove(&mut iter);
    }
    refresh_status_counts(ui);
    refresh_toolbar_states(ui);
}

/// The ids the list selection currently names, in row order.
fn selected_ids(ui: &Ui) -> Vec<String> {
    let (paths, model) = ui.list_view.selection().selected_rows();
    let mut ids = Vec::new();
    for path in paths {
        if let Some(iter) = model.iter(&path) {
            let id: String = model.value(&iter, COL_ID as i32).get().unwrap_or_default();
            if !id.is_empty() {
                ids.push(id);
            }
        }
    }
    ids
}

fn select_id(ui: &Ui, id: &str) {
    if let Some(path) = find_path(&ui.store, id) {
        let sel = ui.list_view.selection();
        sel.unselect_all();
        sel.select_path(&path);
        ui.list_view.scroll_to_cell(Some(&path), None::<&gtk::TreeViewColumn>, false, 0.0, 0.0);
    }
}

thread_local! {
    /// Last applied toolbar sensitivity bitmask — progress
    /// events recompute states at event rate, and the buttons
    /// are only touched when the mask actually changes. Starts
    /// at an impossible value so the first recompute always
    /// lands (an empty list must grey the buttons at startup).
    static TOOLBAR_MASK: Cell<u8> = const { Cell::new(0xFF) };
}

/// IDM toolbar logic: the command buttons mirror the
/// selection and the list's state instead of sitting lit at
/// all times. Nothing selected ⇒ Resume/Stop/Delete off; a
/// stoppable selection (downloading/starting/merging/queued)
/// lights Stop; a selection with anything resumable lights
/// Resume (paused, error, cancelled, queued — but never a
/// finished entry: there is nothing left to resume, and a lit
/// Resume on a complete download reads as a live action);
/// Stop All only while something is downloading;
/// Delete Completed only while a finished row exists. Add
/// URL / Options / Scheduler / Grabber / queue buttons are
/// state-independent and stay sensitive.
pub(crate) fn refresh_toolbar_states(ui: &Ui) {
    let sel = selected_ids(ui);
    let entries = ui.entries.borrow();
    let mut any_stoppable = false;
    let mut any_resumable = false;
    for id in &sel {
        if let Some(d) = entries.get(id) {
            if matches!(
                d.status.as_str(),
                "downloading" | "starting" | "merging" | "queued"
            ) {
                any_stoppable = true;
            }
            if !matches!(
                d.status.as_str(),
                "downloading" | "starting" | "merging" | "finished"
            ) {
                any_resumable = true;
            }
        }
    }
    let any_downloading = entries
        .values()
        .any(|d| matches!(d.status.as_str(), "downloading" | "starting" | "resuming"));
    let any_finished = entries.values().any(|d| d.status == "finished");
    let states: [(&str, bool); 5] = [
        ("btnResume", any_resumable),
        ("btnStop", any_stoppable),
        ("btnStopAll", any_downloading),
        ("btnDelete", !sel.is_empty()),
        ("btnDelDone", any_finished),
    ];
    let mask: u8 = states
        .iter()
        .enumerate()
        .fold(0u8, |m, (i, (_, on))| if *on { m | (1 << i) } else { m });
    let changed = TOOLBAR_MASK.with(|c| {
        let old = c.get();
        c.set(mask);
        old != mask
    });
    if !changed {
        return;
    }
    let buttons = ui.toolbar_buttons.borrow();
    for (id, on) in states {
        if let Some(b) = buttons.get(id) {
            b.set_sensitive(on);
        }
    }
}

/// The file a finished entry drags out as (WS-J drag-out).
/// `save_to` is core's out_path: the file itself for plain
/// downloads; for video entries it is the folder and the file is
/// the entry's own name inside it (or stem + a media extension) —
/// the same resolution core's open_path applies. None when no
/// real file is on disk.
fn drag_out_file(d: &ProgressView) -> Option<PathBuf> {
    let p = PathBuf::from(&d.save_to);
    if p.is_file() {
        return Some(p);
    }
    if !p.is_dir() || d.name.is_empty() || d.name.contains('/') {
        return None;
    }
    let direct = p.join(&d.name);
    if direct.is_file() {
        return Some(direct);
    }
    let stem = d.name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&d.name);
    ["mp4", "mkv", "webm", "m4a", "mp3"]
        .iter()
        .map(|ext| p.join(format!("{stem}.{ext}")))
        .find(|c| c.is_file())
}

/// file:// URIs for a drag carrying the given entries: every
/// FINISHED one whose file exists, in the given order.
/// Empty = no drag may start.
fn drag_out_uris(ui: &Ui, ids: &[String]) -> Vec<String> {
    let entries = ui.entries.borrow();
    ids.iter()
        .filter_map(|id| entries.get(id))
        .filter(|d| d.status == "finished")
        .filter_map(drag_out_file)
        .map(|p| gtk::gio::File::for_path(&p).uri().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Settings accessors. Core's `Settings` fields are private to
// the core crate, so the shell reads them through the struct's
// own Serialize shape — the same JSON the web UI consumes.
// ---------------------------------------------------------------------------
fn settings_json(ui: &Ui) -> serde_json::Value {
    serde_json::to_value(sink::get_settings(ui.ctx.clone())).unwrap_or(serde_json::Value::Null)
}

fn queue_names(ui: &Ui) -> Vec<String> {
    let s = settings_json(ui);
    let names: Vec<String> = s
        .get("queues")
        .and_then(|q| q.as_array())
        .map(|qs| {
            qs.iter()
                .filter_map(|q| q.get("name").and_then(|n| n.as_str()).map(|n| n.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        vec!["Main download queue".to_string()]
    } else {
        names
    }
}

/// app.js pickQueueName: the picked queue while it still
/// exists, else the first queue.
fn pick_queue_name(ui: &Ui) -> String {
    let names = queue_names(ui);
    let pick = ui.queue_pick.borrow().clone();
    if names.iter().any(|n| n == &pick) {
        pick
    } else {
        names.into_iter().next().unwrap_or_else(|| "Main download queue".to_string())
    }
}

fn category_names(ui: &Ui) -> Vec<String> {
    let state = ui.ctx.state();
    let s = sink::current_settings(state);
    sink::category_names_in_use(state, &s)
}

// ---------------------------------------------------------------------------
// Actions — every one calls the ndm-core command of the same
// name the web UI invokes (src-tauri wrappers show the shapes).
// Async core commands run on core's own runtime via block_on
// from this (non-runtime) thread — the same pattern core's
// incoming-line handler uses for resume_download.
// ---------------------------------------------------------------------------

/// A yes/no question in the app's own dialog (the web UI's
/// showConfirm equivalent, dialogs::show_confirm).
fn confirm_dialog(ui: &Ui, title: &str, text: &str, ok_label: &str) -> bool {
    dialogs::show_confirm(&ui.window, title, text, ok_label)
}

/// Write one settings change through core's set_settings,
/// round-tripping every other field from the current settings
/// (the same full-struct save the web UI's saveSettings does).
fn update_settings(ui: &Ui, mutate: impl Fn(&mut serde_json::Map<String, serde_json::Value>)) {
    let mut v = settings_json(ui);
    if let Some(obj) = v.as_object_mut() {
        mutate(obj);
    }
    if let Ok(s) = serde_json::from_value::<sink::Settings>(v) {
        let _ = sink::set_settings(ui.ctx.clone(), s);
    }
}

/// Add URL — the web UI's Download File Info flow (app.js
/// openFileDialog / runFileProbe / submitAdd): ask for the
/// address, probe it through core's probe_file (which fills
/// size / resume support / detected category / the folder the
/// file will actually land in), let the user confirm or adjust
/// folder, category, queue, name and description, then call
/// core's add_download with dir_explicit — the dialog's folder
/// is the final word, exactly as in submitAdd. A duplicate
/// answer from core gets the web UI's "download again as a new
/// copy?" confirm and the force retry.
/// The schemes the add paths accept: the engine downloads
/// over HTTP, HTTPS and FTP (2.2.0's FTP support was real, but
/// the shells' http(s)-only validation kept every FTP link out
/// — QA finding B1).
pub(crate) fn is_download_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("ftp://")
}

fn open_add_dialog(ui: &Rc<Ui>) {
    open_file_dialog(ui, None);
}

/// The offer variant (app.js openFileDialog with a payload —
/// an extension catch, a basket / clipboard / drop hand-off):
/// the address stage is skipped and the flow opens on the
/// probed File Info, prefilled with the offered address and
/// the browser's suggested name (the title's basename); the
/// probe and the add both carry the captured session token
/// and the page the file came from, the dialog's title names
/// the source exactly as app.js titles it, and cancelling
/// discards the token, so a session the user did not use
/// never lingers on disk (app.js closeFileDialog).
fn open_file_dialog(ui: &Rc<Ui>, offer: Option<&serde_json::Value>) {
    open_file_dialog_flow(ui, offer, Rc::new(|| {}));
}

thread_local! {
    /// One add flow at a time. A flow spans its address stage,
    /// the (now asynchronous) probe and the add itself; while
    /// one is in flight, incoming file offers wait in their
    /// queue (pump_file_offers checks this) and a second manual
    /// Add is turned away — without the gate, two File Info
    /// dialogs could stack once the probe stopped freezing the
    /// window.
    static ADD_FLOW_ACTIVE: Cell<bool> = Cell::new(false);
}

/// What one add flow carries from its address stage to its
/// settled add, snapshotted off the offer (or the typed
/// address) before any worker thread is spawned.
struct AddFlow {
    url: String,
    cookies: Option<String>,
    referer: Option<String>,
    /// The offered file title ("" for a typed address).
    title: String,
    /// The offered source tag ("" for a typed address).
    source: String,
}

/// The File Info dialog's answers, snapshotted for the add —
/// the add itself runs on a worker thread.
struct AddArgs {
    dir: String,
    later: bool,
    category: Option<String>,
    name: Option<String>,
    queue: Option<String>,
    description: Option<String>,
    total_arg: Option<u64>,
}

fn open_file_dialog_flow(
    ui: &Rc<Ui>,
    offer: Option<&serde_json::Value>,
    on_done: Rc<dyn Fn()>,
) {
    if ADD_FLOW_ACTIVE.with(|f| f.get()) {
        set_notice(ui, "Finish the current add first");
        on_done();
        return;
    }
    let offer_str = |key: &str| {
        offer
            .and_then(|v| v.get(key))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let cookies: Option<String> = match offer_str("cookies") {
        c if c.is_empty() => None,
        c => Some(c),
    };
    let referer: Option<String> = match offer_str("pageUrl") {
        r if r.is_empty() => None,
        r => Some(r),
    };
    let url = if offer.is_some() {
        offer_str("url").trim().to_string()
    } else {
        let url_fields = [dialogs::FieldSpec::entry("url", "Download address", "")];
        let Some(values) =
            dialogs::run_dialog(&ui.window, "Add New Download", &url_fields, "Continue")
        else {
            on_done();
            return;
        };
        values
            .get("url")
            .and_then(|v| v.as_text())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    if url.is_empty() {
        if offer.is_none() {
            set_notice(ui, "Enter a download address first");
        }
        on_done();
        return;
    }
    if !is_download_url(&url) {
        set_notice(ui, "The address must start with http://, https:// or ftp://");
        on_done();
        return;
    }

    // Probe (core probe_file) on a worker thread. Run on this
    // thread it froze the whole window behind any slow or
    // redirecting server — proven on the user's own laptop
    // with a real GitHub release URL (paste → Continue →
    // "not responding" → force quit). The File Info dialog
    // opens from the channel callback when the probe lands.
    ADD_FLOW_ACTIVE.with(|f| f.set(true));
    set_notice(ui, "Checking the address…");
    let flow = Rc::new(AddFlow {
        title: offer_str("title"),
        source: offer_str("source"),
        url,
        cookies,
        referer,
    });
    let (tx, rx) = gtk::glib::MainContext::channel::<Result<serde_json::Value, String>>(
        gtk::glib::Priority::DEFAULT,
    );
    {
        let ctx = ui.ctx.clone();
        let (u2, c2, r2) = (flow.url.clone(), flow.cookies.clone(), flow.referer.clone());
        std::thread::spawn(move || {
            let _ = tx.send(sink::rt::block_on(sink::probe_file(ctx, u2, c2, r2, None, None)));
        });
    }
    let ui2 = ui.clone();
    rx.attach(None, move |probe| {
        show_file_info(&ui2, &flow, probe, on_done.clone());
        gtk::glib::ControlFlow::Break
    });
}

/// The File Info stage of the add flow: runs on the GTK thread
/// from the probe's channel callback, with the probe's verdict
/// in hand. A failed probe does not block the add — the web UI
/// shows "—" and still lets the user start — but the failure
/// is shown in the dialog, not hidden.
fn show_file_info(
    ui: &Rc<Ui>,
    flow: &Rc<AddFlow>,
    probe: Result<serde_json::Value, String>,
    on_done: Rc<dyn Fn()>,
) {
    show_file_info_inner(ui, flow, probe, on_done, None)
}

/// A failed Start re-presents the card with the user's edits
/// intact and the failure shown in it (audit-233: the dialog
/// used to close on any Start error, discarding every field
/// the user had just typed — the Options F-4 discipline says a
/// failed save keeps the dialog). `retry` carries the exact
/// arguments of the failed attempt plus its error.
fn show_file_info_inner(
    ui: &Rc<Ui>,
    flow: &Rc<AddFlow>,
    probe: Result<serde_json::Value, String>,
    on_done: Rc<dyn Fn()>,
    retry: Option<(Rc<AddArgs>, String)>,
) {
    let url = flow.url.clone();
    let (total, size_text, resume_text, probed_category, probed_dir, suggested, probe_note) =
        match &probe {
            Ok(info) => {
                let total = info.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
                let size = if total > 0 {
                    fmt_size(total)
                } else {
                    "Unknown".to_string()
                };
                let resume = match info.get("resumable").and_then(|x| x.as_bool()) {
                    Some(true) => "Yes".to_string(),
                    Some(false) => "No".to_string(),
                    None => "—".to_string(),
                };
                (
                    total,
                    size,
                    resume,
                    info.get("category")
                        .and_then(|x| x.as_str())
                        .unwrap_or("General")
                        .to_string(),
                    info.get("dir")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    info.get("suggested_name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    String::new(),
                )
            }
            Err(e) => (
                0,
                "—".to_string(),
                "—".to_string(),
                "General".to_string(),
                String::new(),
                String::new(),
                format!(
                    "Could not check this address: {}",
                    sink::display_error(&e)
                ),
            ),
        };

    // A retry re-presents the failed attempt's own values.
    let (total, probed_category, probe_note) = match &retry {
        Some((args, err)) => (
            args.total_arg.unwrap_or(total),
            args.category.clone().unwrap_or(probed_category),
            format!("Could not add the download: {}", sink::display_error(err)),
        ),
        None => (total, probed_category, probe_note),
    };
    let s = settings_json(ui);
    let default_dir = s
        .get("default_dir")
        .and_then(|x| x.as_str())
        .unwrap_or("~/Downloads")
        .to_string();
    let dir_initial = match &retry {
        Some((args, _)) => args.dir.clone(),
        // The stored setting keeps its "~/" form; the field shows
        // the folder the download will really use (a probe success
        // already hands back the resolved absolute path — the
        // failed-probe fallback must match it, not show the raw
        // setting string).
        None if probed_dir.is_empty() => {
            if let Some(rest) = default_dir.strip_prefix("~/") {
                match std::env::var_os("HOME") {
                    Some(h) => format!("{}/{}", h.to_string_lossy(), rest),
                    None => default_dir,
                }
            } else {
                default_dir
            }
        }
        None => probed_dir,
    };
    // app.js: the offered title's basename is the name; the
    // probe's suggested name only fills a name left empty.
    let name_initial = {
        let title = &flow.title;
        let base = title
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let from_title = if base.is_empty() { suggested.clone() } else { base };
        match &retry {
            Some((args, _)) => args.name.clone().unwrap_or(from_title),
            None => from_title,
        }
    };

    let info_title = match flow.source.as_str() {
        "clipboard" => "Download link was found in clipboard. Do you want to download it?",
        "basket" => "Link dropped on the basket \u{2014} download it?",
        "drop" => "Link dropped \u{2014} download it?",
        _ => "Download File Info",
    };

    // M1 (the user's standing rule): a file add gets the same
    // card treatment the video picker has \u{2014} a header naming
    // the file, its address under it, and the probe facts in
    // an inset panel \u{2014} instead of the plain form grid this
    // step used to be. Every field below is the same field
    // with the same value semantics as before: address, file
    // size, resume support, save-to, category, queue, file
    // name, description, Download later.
    let dlg = form::dialog(&ui.window, info_title, "Start Download", 640, -1);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(14);
    body.set_margin_end(14);
    body.set_margin_top(12);
    body.set_margin_bottom(6);

    let header = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let title_lbl = gtk::Label::new(None);
    title_lbl.set_markup(&format!(
        "<span size=\"large\"><b>{}</b></span>",
        gtk::glib::markup_escape_text(if name_initial.is_empty() {
            "New download"
        } else {
            &name_initial
        })
    ));
    title_lbl.set_halign(gtk::Align::Start);
    title_lbl.set_line_wrap(true);
    // WordChar: a long filename is one unbroken word; plain word
    // wrap made this card 1637px wide on a 1400px screen
    // (audit-233 B-2 — same fix as modals::note_label).
    title_lbl.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
    title_lbl.set_max_width_chars(72);
    header.pack_start(&title_lbl, false, false, 0);
    let addr_lbl = gtk::Label::new(Some(&url));
    addr_lbl.set_halign(gtk::Align::Start);
    addr_lbl.set_line_wrap(true);
    addr_lbl.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
    addr_lbl.set_max_width_chars(72);
    addr_lbl.set_selectable(true);
    header.pack_start(&addr_lbl, false, false, 0);
    body.pack_start(&header, false, false, 0);

    let card = gtk::Frame::new(None);
    card.set_shadow_type(gtk::ShadowType::In);
    let facts = gtk::Grid::builder()
        .row_spacing(4)
        .column_spacing(12)
        .margin_start(10)
        .margin_end(10)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    let mut fy = 0;
    for (label, value) in [("File size", &size_text), ("Resume support", &resume_text)] {
        let l = gtk::Label::new(Some(label));
        l.set_halign(gtk::Align::Start);
        let v = gtk::Label::new(Some(value.as_str()));
        v.set_halign(gtk::Align::Start);
        v.set_hexpand(true);
        facts.attach(&l, 0, fy, 1, 1);
        facts.attach(&v, 1, fy, 1, 1);
        fy += 1;
    }
    if !probe_note.is_empty() {
        let n = gtk::Label::new(Some(&probe_note));
        n.set_halign(gtk::Align::Start);
        n.set_line_wrap(true);
        n.set_max_width_chars(64);
        facts.attach(&n, 0, fy, 2, 1);
    }
    card.add(&facts);
    body.pack_start(&card, false, false, 0);

    let g = gtk::Grid::builder().row_spacing(6).column_spacing(12).build();
    let dir_e = form::entry(&dir_initial);
    let mut y = form::row(&g, 0, "Save to", &dir_e);
    let cat_names: Vec<String> = category_names(ui);
    let category_c = form::combo(
        &cat_names
            .iter()
            .map(|n| (n.as_str(), n.as_str()))
            .collect::<Vec<_>>(),
        &probed_category,
    );
    y = form::row(&g, y, "Category", &category_c);
    let queue_list: Vec<String> = queue_names(ui);
    let queue_c = form::combo(
        &queue_list
            .iter()
            .map(|n| (n.as_str(), n.as_str()))
            .collect::<Vec<_>>(),
        &match &retry {
            Some((args, _)) => args.queue.clone().unwrap_or_else(|| pick_queue_name(ui)),
            None => pick_queue_name(ui),
        },
    );
    y = form::row(&g, y, "Queue", &queue_c);
    let name_e = form::entry(&name_initial);
    y = form::row(&g, y, "File name", &name_e);
    let desc_e = form::entry(match &retry {
        Some((args, _)) => args.description.as_deref().unwrap_or(""),
        None => "",
    });
    desc_e.set_placeholder_text(Some("(optional note)"));
    y = form::row(&g, y, "Description", &desc_e);
    let later_c = form::check(
        "Download later",
        matches!(&retry, Some((args, _)) if args.later),
    );
    form::check_row(&g, y, &later_c);
    body.pack_start(&g, false, false, 0);
    dlg.content_area().pack_start(&body, false, false, 0);
    dlg.show_all();
    name_e.grab_focus();

    if form::run(&dlg) != gtk::ResponseType::Ok {
        // Cancel / close: the captured session is discarded,
        // never left behind for a download nobody confirmed.
        if let Some(token) = &flow.cookies {
            sink::discard_cookies(ui.ctx.clone(), token.clone());
        }
        dlg.close();
        finish_add_flow(&on_done);
        return;
    }
    let dir = {
        let d = dir_e.text().trim().to_string();
        if d.is_empty() { dir_initial.clone() } else { d }
    };
    let category = {
        let c = form::combo_text(&category_c);
        if c.is_empty() { None } else { Some(c) }
    };
    let queue = {
        let q = form::combo_text(&queue_c);
        if q.is_empty() { None } else { Some(q) }
    };
    let name = {
        let n = name_e.text().trim().to_string();
        if n.is_empty() { None } else { Some(n) }
    };
    let description = {
        let d = desc_e.text().trim().to_string();
        if d.is_empty() { None } else { Some(d) }
    };
    let later = later_c.is_active();
    dlg.close();
    let total_arg = if total > 0 { Some(total) } else { None };

    let before: Rc<std::collections::HashSet<String>> =
        Rc::new(ui.entries.borrow().keys().cloned().collect());
    let args = Rc::new(AddArgs {
        dir,
        later,
        category,
        name,
        queue,
        description,
        total_arg,
    });
    spawn_add(ui, flow, &args, false, before, on_done, true);
}

/// Run one add_download on a worker thread and settle its
/// verdict back on the GTK thread. A duplicate answer gets the
/// web UI's "download again as a new copy?" confirm (W193-17)
/// and the force retry — confirm and retry both live here, on
/// the GTK side of the channel, exactly as the synchronous
/// flow sequenced them.
fn spawn_add(
    ui: &Rc<Ui>,
    flow: &Rc<AddFlow>,
    args: &Rc<AddArgs>,
    force: bool,
    before: Rc<std::collections::HashSet<String>>,
    on_done: Rc<dyn Fn()>,
    may_confirm: bool,
) {
    let (tx, rx) = gtk::glib::MainContext::channel::<Result<String, String>>(
        gtk::glib::Priority::DEFAULT,
    );
    {
        let ctx = ui.ctx.clone();
        let (url, cookies, referer) =
            (flow.url.clone(), flow.cookies.clone(), flow.referer.clone());
        // owned field copies cross to the worker — never the Rc
        let (dir, later, category, name, queue, description, total_arg) = (
            args.dir.clone(),
            args.later,
            args.category.clone(),
            args.name.clone(),
            args.queue.clone(),
            args.description.clone(),
            args.total_arg,
        );
        std::thread::spawn(move || {
            let _ = tx.send(sink::rt::block_on(sink::add_download(
                ctx,
                url,
                dir,
                Some(later),
                category,
                name,
                cookies,
                referer,
                Some(force),
                queue,
                None,
                None,
                description,
                total_arg,
                Some(true),
            )));
        });
    }
    let ui2 = ui.clone();
    let flow2 = flow.clone();
    let args2 = args.clone();
    rx.attach(None, move |result| {
        match result {
            Err(e) if may_confirm && e.starts_with("duplicate:") => {
                // IDM asks what to do about a duplicate link — so
                // does the web UI, and so does this shell.
                if confirm_dialog(
                    &ui2,
                    "Download again",
                    "This file is already in the download list.\nDownload it again as a new copy?",
                    "Download again",
                ) {
                    spawn_add(&ui2, &flow2, &args2, true, before.clone(), on_done.clone(), false);
                } else {
                    // the user chose to keep the existing entry —
                    // done, not an error
                    finish_add_flow(&on_done);
                }
            }
            result => settle_add(&ui2, &flow2, &args2, result, before.as_ref(), &on_done),
        }
        gtk::glib::ControlFlow::Break
    });
}

/// Land a settled add on the GTK thread: refresh the list,
/// select the just-added entry (IDM behaviour, app.js F-07) or
/// narrate the failure — then close the flow out.
fn settle_add(
    ui: &Rc<Ui>,
    flow: &Rc<AddFlow>,
    args: &Rc<AddArgs>,
    result: Result<String, String>,
    before: &std::collections::HashSet<String>,
    on_done: &Rc<dyn Fn()>,
) {
    match result {
        Ok(_) => {
            refresh_list(ui);
            let fresh = ui
                .order
                .borrow()
                .iter()
                .find(|id| !before.contains(*id))
                .cloned();
            if let Some(id) = fresh {
                select_id(ui, &id);
            }
            // the selection-changed recompute can race the
            // status landing in the entry map; settle the
            // toolbar once more with the final state
            refresh_toolbar_states(ui);
            set_notice(ui, "Download added");
            finish_add_flow(on_done);
        }
        Err(e) => {
            // Re-present the card with the user's edits and the
            // failure inside it; the flow stays open (no finish)
            // so Cancel/Start behave exactly as the first time.
            // The probe is not re-run — the facts it gathered
            // (size, resume) are carried in a synthetic result.
            let probe = Ok(serde_json::json!({
                "total": args.total_arg.unwrap_or(0),
                "resumable": serde_json::Value::Null,
                "category": args.category.clone().unwrap_or_else(|| "General".to_string()),
                "dir": args.dir.clone(),
                "suggested_name": args.name.clone().unwrap_or_default(),
            }));
            show_file_info_inner(ui, flow, probe, on_done.clone(), Some((args.clone(), e)));
        }
    }
}

/// Close an add flow out: release the one-at-a-time gate and
/// run the caller's continuation (the file-offer pump's next
/// presentation, on the offer path).
fn finish_add_flow(on_done: &Rc<dyn Fn()>) {
    ADD_FLOW_ACTIVE.with(|f| f.set(false));
    on_done();
}

// ---------------------------------------------------------------------------
// Incoming file offers — app.js queueFileLink / pumpFileLinks:
// presented one at a time in the File Info dialog above, never
// stacked over a dialog the user is already working in. They
// wait in this queue while any modal is up (the wave-1 yield
// discipline — the same modal depth the Download Complete
// dialog yields to, modals.rs) and are pumped when the last
// modal closes (dialogs.rs calls pump_file_offers back from
// its close hooks, exactly as it pumps completions) and when
// a presented offer's own dialog finishes. A repeat of the
// address already presented or already waiting is dropped.
// ---------------------------------------------------------------------------

thread_local! {
    /// Offers waiting for their dialog, in arrival order.
    static FILE_OFFER_QUEUE: RefCell<Vec<serde_json::Value>> = RefCell::new(Vec::new());
    /// The address of the offer on screen right now.
    static FILE_OFFER_PRESENTED: RefCell<Option<String>> = RefCell::new(None);
    /// The Ui the pump runs against — captured on the first
    /// offer (modals.rs' COMPLETE_ANCHOR pattern), so the
    /// modal-close hooks in dialogs.rs can pump without
    /// holding a Ui themselves.
    static FILE_OFFER_ANCHOR: RefCell<Option<Rc<Ui>>> = RefCell::new(None);
}

thread_local! {
    /// Slice 3 (2.3.6): video/grab captures that arrived while
    /// another capture-born dialog was on screen, in arrival
    /// order — (kind, url, cookies). File offers have always
    /// queued this way; video and grab captures used to open
    /// their windows immediately, so a single page action could
    /// stack two or three dialogs at once (field complaint #4).
    static CAPTURE_QUEUE: RefCell<std::collections::VecDeque<(&'static str, String, Option<String>)>> =
        RefCell::new(std::collections::VecDeque::new());
    /// The Ui the capture pump runs against (same anchor
    /// pattern as FILE_OFFER_ANCHOR).
    static CAPTURE_ANCHOR: RefCell<Option<Rc<Ui>>> = RefCell::new(None);
}

/// Is a capture-born dialog on screen right now — a modal, a
/// presented file offer, the video picker, the grabber wizard?
/// While one is up, further video/grab captures queue instead
/// of opening on top of it.
fn capture_dialog_visible() -> bool {
    dialogs::modal_open()
        || ADD_FLOW_ACTIVE.with(|f| f.get())
        || FILE_OFFER_PRESENTED.with(|p| p.borrow().is_some())
        || video::is_open()
        || grabber::is_open()
}

fn queue_capture(ui: &Rc<Ui>, kind: &'static str, url: String, cookies: Option<String>) {
    CAPTURE_ANCHOR.with(|a| *a.borrow_mut() = Some(ui.clone()));
    CAPTURE_QUEUE.with(|q| q.borrow_mut().push_back((kind, url, cookies)));
}

/// Present the next queued video/grab capture, if the way is
/// clear. Called from every dialog-clearing seam (modal_exit,
/// run_dialog's end, the file-offer settle hook, the video
/// picker's close and the grabber's hides), exactly like
/// pump_file_offers.
pub fn pump_capture_queue() {
    if capture_dialog_visible() {
        return;
    }
    let Some(ui) = CAPTURE_ANCHOR.with(|a| a.borrow().clone()) else {
        return;
    };
    let next = CAPTURE_QUEUE.with(|q| q.borrow_mut().pop_front());
    let Some((kind, url, cookies)) = next else {
        return;
    };
    if kind == "video" {
        video::show(ui, Some(url), cookies);
    } else {
        grabber::show(ui, Some(url), true);
    }
}

fn queue_incoming_file(ui: &Rc<Ui>, payload: serde_json::Value) {
    FILE_OFFER_ANCHOR.with(|a| *a.borrow_mut() = Some(ui.clone()));
    let url = payload
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if !is_download_url(&url) {
        set_notice(ui, "Incoming file without an address");
        return;
    }
    let presented =
        FILE_OFFER_PRESENTED.with(|p| p.borrow().as_deref() == Some(url.as_str()));
    let queued = FILE_OFFER_QUEUE.with(|q| {
        q.borrow()
            .iter()
            .any(|v| v.get("url").and_then(|x| x.as_str()) == Some(url.as_str()))
    });
    if presented || queued {
        return; // the same address is already on its way in
    }
    FILE_OFFER_QUEUE.with(|q| q.borrow_mut().push(payload));
    pump_file_offers();
}

/// Present the next waiting offer, if the way is clear: no
/// modal dialog up, no add flow in flight and no offer
/// already on screen.
pub fn pump_file_offers() {
    if dialogs::modal_open()
        || ADD_FLOW_ACTIVE.with(|f| f.get())
        || FILE_OFFER_PRESENTED.with(|p| p.borrow().is_some())
        || video::is_open()
        || grabber::is_open()
    {
        return;
    }
    let Some(ui) = FILE_OFFER_ANCHOR.with(|a| a.borrow().clone()) else {
        return;
    };
    let next = FILE_OFFER_QUEUE.with(|q| {
        if q.borrow().is_empty() {
            None
        } else {
            Some(q.borrow_mut().remove(0))
        }
    });
    let Some(next) = next else { return };
    let url = next
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    FILE_OFFER_PRESENTED.with(|p| *p.borrow_mut() = Some(url));
    // The flow is asynchronous (probe + add on worker threads):
    // this presentation ends when the flow settles, and only
    // then does the next waiting offer get its turn (app.js:
    // every modal-close path calls pumpFileLinks back).
    let on_done = Rc::new(move || {
        FILE_OFFER_PRESENTED.with(|p| *p.borrow_mut() = None);
        pump_file_offers();
        pump_capture_queue();
    });
    open_file_dialog_flow(&ui, Some(&next), on_done);
}

/// Resume the given entries. A resume is a download START
/// (the scheduler/quota gate, a revalidation probe), so the
/// calls run on a worker thread — on this thread a slow
/// server froze the window behind the click. The first
/// failure, if any, is narrated when the batch lands.
fn resume_ids(ui: &Rc<Ui>, ids: &[String]) {
    if ids.is_empty() {
        set_notice(ui, "Select a download first");
        return;
    }
    let (tx, rx) = gtk::glib::MainContext::channel::<Vec<String>>(
        gtk::glib::Priority::DEFAULT,
    );
    let ctx = ui.ctx.clone();
    let ids: Vec<String> = ids.to_vec();
    std::thread::spawn(move || {
        let mut errors = Vec::new();
        for id in &ids {
            let r = sink::rt::block_on(sink::resume_download(ctx.clone(), id.clone()));
            if let Err(e) = r {
                errors.push(e);
            }
        }
        let _ = tx.send(errors);
    });
    let ui2 = ui.clone();
    rx.attach(None, move |errors| {
        if let Some(e) = errors.into_iter().next() {
            set_notice(&ui2, &format!("Could not resume: {}", e));
        }
        gtk::glib::ControlFlow::Break
    });
}

fn pause_ids(ui: &Rc<Ui>, ids: &[String]) {
    if ids.is_empty() {
        set_notice(ui, "Select a download first");
        return;
    }
    for id in ids {
        if let Err(e) = sink::pause_download(ui.ctx.clone(), id.clone()) {
            set_notice(ui, &format!("Could not stop: {}", e));
            return;
        }
    }
}

/// The toolbar's Stop All and the Downloads menu's Pause All
/// share the web UI's shape: pause every live entry, keep going
/// past individual failures. "Live" includes the resolve
/// ("starting") and stall-recovery ("resuming") phases — core
/// parks the job in both, so leaving them out made Stop All
/// silently skip downloads that had not reached steady state
/// (audit-233).
fn pause_all_downloading(ui: &Rc<Ui>) {
    let ids: Vec<String> = ui
        .order
        .borrow()
        .iter()
        .filter(|id| {
            ui.entries
                .borrow()
                .get(*id)
                .map(|d| matches!(d.status.as_str(), "downloading" | "starting" | "resuming"))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if ids.is_empty() {
        set_notice(ui, "Nothing is downloading");
        return;
    }
    let mut n = 0;
    for id in &ids {
        if sink::pause_download(ui.ctx.clone(), id.clone()).is_ok() {
            n += 1;
        }
    }
    set_notice(ui, &format!("Stopped {} download{}", n, if n == 1 { "" } else { "s" }));
}

/// Delete with the web UI's confirm behaviour (app.js
/// openDeleteConfirm + its confirm handler): ONE dialog for the
/// whole batch, one "also delete the file" checkbox (applies to
/// every status), then core's bulk remove_downloads — per-id
/// results decide what actually leaves the list, and failures
/// are reported, never hidden.
fn delete_ids(ui: &Rc<Ui>, ids: Vec<String>) {
    let ids: Vec<String> = ids
        .into_iter()
        .filter(|id| ui.entries.borrow().contains_key(id))
        .collect();
    if ids.is_empty() {
        set_notice(ui, "Select a download first");
        return;
    }
    let (title_text, file_label) = if ids.len() == 1 {
        let name = ui
            .entries
            .borrow()
            .get(&ids[0])
            .map(|d| {
                if d.name.is_empty() {
                    "download".to_string()
                } else {
                    d.name.clone()
                }
            })
            .unwrap_or_else(|| "download".to_string());
        (
            format!("Delete \"{}\" from the list?", name),
            "Also delete the file from the disk (including a partial download)",
        )
    } else {
        (
            format!("Delete {} downloads from the list?", ids.len()),
            "Also delete the files from the disk (including partial downloads)",
        )
    };
    let fields = [
        dialogs::FieldSpec::readonly("name", "", &title_text),
        dialogs::FieldSpec::check("file", file_label, false),
    ];
    let Some(values) = dialogs::run_dialog(&ui.window, "Delete", &fields, "Delete") else {
        return;
    };
    let delete_file = values.get("file").and_then(|v| v.as_bool()).unwrap_or(false);
    run_delete(ui, ids, delete_file);
}

fn run_delete(ui: &Rc<Ui>, ids: Vec<String>, delete_file: bool) {
    // The removal runs on a worker thread: core settles the
    // entry's engine before touching files, and a wedged
    // download (retry-spinning against a dead server) takes
    // seconds to settle. On this thread that wait froze the
    // window — and a force-quit inside it leaked the partial
    // file the user had asked to delete (QA-220 M2). The
    // per-id results land back here and are handled exactly
    // as the synchronous version handled them.
    let (tx, rx) = gtk::glib::MainContext::channel::<Vec<serde_json::Value>>(
        gtk::glib::Priority::DEFAULT,
    );
    let ctx = ui.ctx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(sink::remove_downloads(ctx, ids, Some(delete_file)));
    });
    let ui2 = ui.clone();
    rx.attach(None, move |results| {
        settle_delete(&ui2, &results, delete_file);
        gtk::glib::ControlFlow::Break
    });
}

fn settle_delete(ui: &Rc<Ui>, results: &[serde_json::Value], delete_file: bool) {
    let mut removed = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut failed_ids: Vec<String> = Vec::new();
    for r in results {
        let id = r.get("id").and_then(|x| x.as_str()).unwrap_or("");
        if r.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
            removed += 1;
            remove_entry_local(ui, id);
        } else {
            failed_ids.push(id.to_string());
            let name = ui
                .entries
                .borrow()
                .get(id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| id.to_string());
            let err = r
                .get("error")
                .and_then(|x| x.as_str())
                .unwrap_or("could not be deleted");
            failures.push(format!("\"{}\": {}", name, err));
        }
    }
    // the EntryRemoved events fire for these rows too;
    // remove_entry_local above is idempotent, so event and
    // result paths converge without a full-list rebuild
    if failures.len() == 1 {
        set_notice(ui, &failures[0]);
    } else if !failures.is_empty() {
        set_notice(
            ui,
            &format!(
                "{} downloads could not be deleted — {}",
                failures.len(),
                failures[0]
            ),
        );
    } else if removed > 0 {
        set_notice(
            ui,
            &format!(
                "Deleted {} download{} from the list",
                removed,
                if removed == 1 { "" } else { "s" }
            ),
        );
    }
    // 2.3.1 (item 11): no traps, ever. When the file deletion
    // failed, the entry stayed in the list — offer the IDM way
    // out IN the dialog flow: report the file problem and let
    // the user remove the entry from the list anyway (the file
    // itself then stays on disk, which the dialog says plainly).
    if delete_file && !failed_ids.is_empty() {
        let text = if failed_ids.len() == 1 {
            format!(
                "{}\n\nRemove it from the list anyway? The file stays on disk.",
                failures[0]
            )
        } else {
            format!(
                "{} downloads could not delete their files.\n\n{}Remove them from the list anyway? The files stay on disk.",
                failed_ids.len(),
                failures[0]
            )
        };
        let fields = [dialogs::FieldSpec::readonly("why", "", &text)];
        if dialogs::run_dialog(&ui.window, "File could not be deleted", &fields, "Remove anyway")
            .is_some()
        {
            run_delete(ui, failed_ids, false);
        }
    }
}

/// Delete Completed (app.js deleteCompleted): every finished
/// entry, no confirm dialog, files stay on disk, one bulk call.
fn delete_completed(ui: &Rc<Ui>) {
    let ids: Vec<String> = ui
        .order
        .borrow()
        .iter()
        .filter(|id| {
            ui.entries
                .borrow()
                .get(*id)
                .map(|d| d.status == "finished")
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if ids.is_empty() {
        set_notice(ui, "No completed downloads");
        return;
    }
    run_delete(ui, ids, false);
}

fn start_queue_now(ui: &Rc<Ui>) {
    let name = pick_queue_name(ui);
    match sink::scheduler_start_now(ui.ctx.clone(), Some(name.clone())) {
        Ok(_) => set_notice(ui, &format!("Queue \"{}\" started", name)),
        Err(e) => set_notice(ui, &format!("Could not start the queue: {}", e)),
    }
}

fn stop_queue_now(ui: &Rc<Ui>) {
    let name = pick_queue_name(ui);
    sink::scheduler_stop_now(ui.ctx.clone(), Some(name.clone()));
    set_notice(ui, &format!("Queue \"{}\" stopped", name));
}

thread_local! {
    /// A manual update check is in flight. The check now runs
    /// on a worker thread (below); a second click while one is
    /// running must not start a duplicate — the running check's
    /// verdict serves both.
    static UPDATE_CHECK_RUNNING: Cell<bool> = Cell::new(false);
}

/// Help → Check for Updates (app.js manualUpdateCheck): the
/// verdict stays on screen — an offer opens the Update dialog,
/// "current" and failures land in the status bar. The check
/// itself is core's check_update, and it runs on a WORKER
/// thread: it used to block_on here, and on a slow or blackholed
/// network the feed fetch froze the whole window for minutes
/// (the user's "not responding" at launch). The result rides
/// the main-context channel back, the same pattern as the
/// video probe (video.rs); the automatic launch check and the
/// Win32 shell's manual check were already threaded this way.
fn manual_update_check(ui: &Rc<Ui>) {
    if UPDATE_CHECK_RUNNING.with(|r| r.get()) {
        return;
    }
    UPDATE_CHECK_RUNNING.with(|r| r.set(true));
    set_notice(ui, "Checking for updates…");
    let (tx, rx) = gtk::glib::MainContext::channel::<Result<serde_json::Value, String>>(
        gtk::glib::Priority::DEFAULT,
    );
    let ctx = ui.ctx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(sink::rt::block_on(sink::check_update(ctx)));
    });
    let ui2 = ui.clone();
    rx.attach(None, move |r| {
        UPDATE_CHECK_RUNNING.with(|r| r.set(false));
        match r {
            Ok(v) if v.get("status").and_then(|x| x.as_str()) == Some("available") => {
                modals::show_update(&ui2.window, ui2.ctx.clone(), &v);
            }
            Ok(v) if v.get("status").and_then(|x| x.as_str()) == Some("current") => {
                let ver = v.get("version").and_then(|x| x.as_str()).unwrap_or("");
                set_notice(&ui2, &format!("NDM is up to date ({})", ver));
            }
            _ => set_notice(&ui2, "Could not reach the update server — try again later"),
        }
        gtk::glib::ControlFlow::Break
    });
}

fn open_selected(ui: &Rc<Ui>, folder: bool) {
    let Some(id) = selected_ids(ui).into_iter().next() else {
        set_notice(ui, "Select a download first");
        return;
    };
    if let Err(e) = sink::open_path(ui.ctx.clone(), id, folder) {
        set_notice(ui, &format!("Could not open: {}", e));
    }
}

/// Refresh download address (IDM, app.js ctx menu): prompt for
/// the new address, then core's refresh_url decides whether the
/// entry resumes against it or restarts from zero.
fn refresh_address(ui: &Rc<Ui>, id: &str) {
    let current = ui
        .entries
        .borrow()
        .get(id)
        .map(|d| d.url.clone())
        .unwrap_or_default();
    // Mirror app.js openRefreshAddress (#rmodal): the current
    // address shown read-only, the same-file/different-file
    // note, and the new-address field prefilled with the
    // current URL.
    let fields = [
        dialogs::FieldSpec::readonly("cur", "Current address", &current),
        dialogs::FieldSpec::readonly(
            "note",
            "",
            "Download addresses often expire. Paste a fresh one for the same file: \
             if the new address serves the same file, the download continues from \
             its current progress; if it serves a different file, the download \
             starts over. Which of the two applies is decided when the download \
             next starts.",
        ),
        dialogs::FieldSpec::entry("url", "New URL for the same file", &current),
    ];
    let Some(values) = dialogs::run_dialog(&ui.window, "Refresh download address", &fields, "Refresh")
    else {
        return;
    };
    let new_url = values
        .get("url")
        .and_then(|v| v.as_text())
        .unwrap_or("")
        .trim()
        .to_string();
    if new_url.is_empty() {
        return;
    }
    // refresh_url probes the new address over the network; on
    // the UI thread a slow server froze the whole window for the
    // probe's duration. Same thread + channel pattern as the
    // other async commands (resume_ids above).
    let (tx, rx) = gtk::glib::MainContext::channel::<Result<(), String>>(
        gtk::glib::Priority::DEFAULT,
    );
    let ctx = ui.ctx.clone();
    let id = id.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(sink::refresh_url(ctx, id, new_url));
    });
    let ui2 = ui.clone();
    rx.attach(None, move |r| {
        match r {
            Ok(()) => set_notice(&ui2, "Address refreshed — the download stays paused"),
            Err(e) => set_notice(&ui2, &format!("Could not refresh the address: {}", e)),
        }
        gtk::glib::ControlFlow::Break
    });
}

/// Export the downloads list (app.js exportList): core's
/// export_downloads writes the JSON file itself (to
/// ~/Downloads/ndm-downloads-export.json) and returns its path;
/// the shell only narrates where it went.
fn export_downloads_action(ui: &Ui) {
    match sink::export_downloads(ui.ctx.clone()) {
        Ok(path) => set_notice(
            ui,
            &format!(
                "Exported the list to {path} — keep it or share it to move downloads between computers"
            ),
        ),
        Err(e) => set_notice(ui, &format!("Export failed: {e}")),
    }
}

/// Import a downloads list (app.js openImport/importFromText):
/// paste the exported JSON, then one core add_download per row,
/// always "later" so nothing starts on its own. Rows carry no
/// folder of their own in the export format — the folder comes
/// from save_to/path when present (a file path gives its
/// folder), else the default directory. Counts and messages
/// mirror app.js exactly.
fn import_downloads(ui: &Rc<Ui>) {
    let fields = [
        dialogs::FieldSpec::readonly(
            "head",
            "",
            "Paste an exported NDM downloads list (JSON)",
        ),
        dialogs::FieldSpec::textarea("json", "", ""),
    ];
    let Some(values) = dialogs::run_dialog(&ui.window, "Import downloads", &fields, "Import")
    else {
        return;
    };
    let text = values
        .get("json")
        .and_then(|v| v.as_text())
        .unwrap_or("")
        .to_string();
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
        set_notice(ui, "Import failed: That is not valid JSON.");
        return;
    };
    let items: Vec<&serde_json::Value> = match &parsed {
        serde_json::Value::Array(a) => a.iter().collect(),
        serde_json::Value::Object(_) => vec![&parsed],
        _ => Vec::new(),
    };
    if items.is_empty() {
        set_notice(ui, "Import failed: No downloads found in that list.");
        return;
    }
    let default_dir = settings_json(ui)
        .get("default_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let str_of = |v: &serde_json::Value, key: &str| {
        v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
    };
    // Snapshot every row's add arguments here, then run the
    // adds on a worker thread: each add can probe its server,
    // and a pasted list of slow links used to freeze the window
    // once per row. The counts and the closing notice are
    // unchanged, delivered when the batch lands.
    let mut rows: Vec<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<u64>,
    )> = Vec::new();
    for row in items {
        let url = str_of(row, "url").unwrap_or_default();
        let save_to = str_of(row, "save_to")
            .or_else(|| str_of(row, "path"))
            .unwrap_or_default();
        let dir = if save_to.is_empty() {
            default_dir.clone()
        } else {
            let last = save_to.rsplit('/').next().unwrap_or("");
            if last.contains('.') {
                save_to
                    .rsplit_once('/')
                    .map(|(p, _)| p.to_string())
                    .unwrap_or(save_to.clone())
            } else {
                save_to.clone()
            }
        };
        let total = row.get("total").and_then(|t| t.as_u64()).filter(|t| *t > 0);
        rows.push((
            url,
            dir,
            str_of(row, "category"),
            str_of(row, "name"),
            str_of(row, "queue"),
            total,
        ));
    }
    let (tx, rx) = gtk::glib::MainContext::channel::<(usize, usize)>(
        gtk::glib::Priority::DEFAULT,
    );
    let ctx = ui.ctx.clone();
    std::thread::spawn(move || {
        let (mut ok, mut err) = (0usize, 0usize);
        for (url, dir, category, name, queue, total) in rows {
            let added = sink::rt::block_on(sink::add_download(
                ctx.clone(),
                url,
                dir,
                Some(true),
                category,
                name,
                None,
                None,
                None,
                queue,
                None,
                None,
                None,
                total,
                None,
            ));
            if added.is_ok() {
                ok += 1;
            } else {
                err += 1;
            }
        }
        let _ = tx.send((ok, err));
    });
    let ui2 = ui.clone();
    rx.attach(None, move |(ok, err)| {
        refresh_list(&ui2);
        let msg = if ok > 0 && err > 0 {
            format!("Imported {ok} download(s), {err} failed — the rest were skipped")
        } else if ok > 0 {
            format!("Imported {ok} download(s) (paused — start them when ready)")
        } else {
            "Import failed: nothing could be imported from that list".to_string()
        };
        set_notice(&ui2, &msg);
        gtk::glib::ControlFlow::Break
    });
}

/// Move the entry to another queue (app.js ctx menu "Move to
/// queue", shown only when more than one queue exists).
fn move_to_queue(ui: &Rc<Ui>, id: &str, queue: &str) {
    let current = ui
        .entries
        .borrow()
        .get(id)
        .map(|d| d.queue.clone())
        .unwrap_or_default();
    if current == queue {
        return;
    }
    match sink::set_entry_queue(ui.ctx.clone(), id.to_string(), queue.to_string()) {
        Ok(()) => {
            // core emits a Progress payload for the entry, which
            // lands it in (or out of) the current filter through
            // the ordinary upsert path — no rebuild needed
            if let Some(d) = ui.entries.borrow_mut().get_mut(id) {
                d.queue = queue.to_string();
            }
            set_notice(ui, &format!("Moved to queue \"{}\"", queue));
        }
        Err(e) => set_notice(ui, &format!("Could not move the download: {}", e)),
    }
}

// ---------------------------------------------------------------------------
// Builders — menu bar, toolbar, category pane, list, status bar.
// Names and order are the web UI's (ui/index.html menubar +
// toolbar, app.js TOOLBAR_DEFS / COLUMN_DEFS / tree markup).
// ---------------------------------------------------------------------------

/// A menu action this shell cannot run yet reports itself in
/// the status bar rather than faking success (app.js F-2 rule).
fn stub_action(ui: &Rc<Ui>, label: &str) -> impl Fn() + 'static {
    let ui = ui.clone();
    let label = label.to_string();
    move || set_notice(&ui, &format!("{} — not yet available in the native shell", label))
}

/// Open (or raise) the per-download progress dialog for an
/// entry (PROGRESS-DIALOG-SPEC §1.3: row double-click, the row
/// menu's "Show progress", and core's ProgressDialogReveal).
/// Seeded from the shell's mirror; live payloads keep it current
/// through the dispatcher's progress routing. The user's own
/// opens are explicit (always present); core's reveal is not —
/// a dialog the user closed stays closed (QA-220 m3).
fn open_progress_dialog(ui: &Rc<Ui>, id: &str) {
    open_progress_dialog_view(ui, id, false, true);
}

/// G7: the "minimized" reveal — the dialog opens lowered
/// (iconified), its live state running from the start.
fn open_progress_dialog_lowered(ui: &Rc<Ui>, id: &str) {
    open_progress_dialog_view(ui, id, true, false);
}

/// Core's start reveal ("show" view): background, not explicit.
fn open_progress_dialog_reveal(ui: &Rc<Ui>, id: &str) {
    open_progress_dialog_view(ui, id, false, false);
}

fn open_progress_dialog_view(ui: &Rc<Ui>, id: &str, minimized: bool, explicit: bool) {
    open_progress_dialog_view_try(ui, id, minimized, explicit, true);
}

fn open_progress_dialog_view_try(
    ui: &Rc<Ui>,
    id: &str,
    minimized: bool,
    explicit: bool,
    may_retry: bool,
) {
    let view = ui.entries.borrow().get(id).cloned();
    let Some(view) = view else {
        if may_retry && !explicit {
            // Core's start-reveal can beat the shell mirror's
            // first payload for a just-created entry by one emit
            // cycle: the entry exists, the mirror just has not
            // ingested it yet. One short retry instead of a false
            // "no longer in the list" notice and a lost dialog
            // (the bug-9 dead-air failure in race form).
            let (u, id) = (ui.clone(), id.to_string());
            gtk::glib::timeout_add_local_once(
                std::time::Duration::from_millis(350),
                move || {
                    open_progress_dialog_view_try(&u, &id, minimized, false, false);
                },
            );
            return;
        }
        set_notice(ui, "That download is no longer in the list");
        return;
    };
    let s = settings_json(ui);
    let info = progress::OpenInfo {
        queues: queue_names(ui),
        global_limit: s
            .get("speed_limit_bps")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        global_show_complete: s
            .get("show_complete_dialog")
            .and_then(|x| x.as_bool())
            .unwrap_or(true),
    };
    progress::open_or_raise(&ui.window, ui.ctx.clone(), &view, info, minimized, explicit);
}

fn menu_item(label: &str) -> gtk::MenuItem {
    gtk::MenuItem::with_label(label)
}

fn build_menubar(ui: &Rc<Ui>) -> gtk::MenuBar {
    let bar = gtk::MenuBar::new();
    let add_menu = |bar: &gtk::MenuBar, title: &str, build: &dyn Fn(&gtk::Menu)| {
        let root = menu_item(title);
        let menu = gtk::Menu::new();
        build(&menu);
        root.set_submenu(Some(&menu));
        bar.add(&root);
    };
    let real_item = |menu: &gtk::Menu, label: &str, f: Box<dyn Fn()>| {
        let item = menu_item(label);
        item.connect_activate(move |_| f());
        menu.add(&item);
    };

    // Tasks — the web UI's menu; Batch / Grabber / Scheduler
    // destinations are not part of this stage and stay labelled
    // stubs; Import/Export are wired (item 7).
    add_menu(&bar, "Tasks", &|menu| {
        let u = ui.clone();
        real_item(
            menu,
            "Add Batch Download…",
            Box::new(move || misc::show_batch(&u)),
        );
        let u = ui.clone();
        real_item(
            menu,
            "Import Downloads…",
            Box::new(move || import_downloads(&u)),
        );
        let u = ui.clone();
        real_item(
            menu,
            "Export Downloads",
            Box::new(move || export_downloads_action(&u)),
        );
        menu.add(&gtk::SeparatorMenuItem::new());
        let u = ui.clone();
        real_item(
            menu,
            "Grabber",
            Box::new(move || grabber::show(u.clone(), None, false)),
        );
        let u = ui.clone();
        real_item(menu, "Scheduler", Box::new(move || scheduler::show(&u)));
    });

    // File — Add New Download is the real Add URL flow; Exit
    // closes the window, which exits the app (see build_ui's
    // destroy → quit wiring, mirroring the Tauri shell's
    // CloseRequested → exit(0)).
    add_menu(&bar, "File", &|menu| {
        let u = ui.clone();
        real_item(menu, "Add New Download…", Box::new(move || open_add_dialog(&u)));
        let u = ui.clone();
        real_item(
            menu,
            "Add Video…",
            Box::new(move || video::show(u.clone(), None, None)),
        );
        let u = ui.clone();
        real_item(
            menu,
            "Add Batch Download…",
            Box::new(move || misc::show_batch(&u)),
        );
        let u = ui.clone();
        real_item(
            menu,
            "Import Downloads…",
            Box::new(move || import_downloads(&u)),
        );
        let u = ui.clone();
        real_item(
            menu,
            "Export Downloads",
            Box::new(move || export_downloads_action(&u)),
        );
        menu.add(&gtk::SeparatorMenuItem::new());
        let u = ui.clone();
        real_item(
            menu,
            "Exit",
            Box::new(move || {
                u.window.close();
            }),
        );
    });

    // Downloads — every item wired to its core command.
    add_menu(&bar, "Downloads", &|menu| {
        let u = ui.clone();
        real_item(
            menu,
            "Start All",
            Box::new(move || {
                let n = sink::start_all(u.ctx.clone());
                set_notice(
                    &u,
                    &if n > 0 {
                        format!("Started {} download{}", n, if n == 1 { "" } else { "s" })
                    } else {
                        "Nothing to start".to_string()
                    },
                );
            }),
        );
        let u = ui.clone();
        real_item(menu, "Pause All", Box::new(move || pause_all_downloading(&u)));
        let u = ui.clone();
        real_item(menu, "Stop All", Box::new(move || pause_all_downloading(&u)));
        menu.add(&gtk::SeparatorMenuItem::new());
        let u = ui.clone();
        real_item(
            menu,
            "Select All",
            Box::new(move || {
                u.list_view.selection().select_all();
            }),
        );
        let u = ui.clone();
        real_item(
            menu,
            "Delete Selected",
            Box::new(move || {
                let ids = selected_ids(&u);
                delete_ids(&u, ids);
            }),
        );
        let u = ui.clone();
        real_item(menu, "Delete All Completed", Box::new(move || delete_completed(&u)));
        let u = ui.clone();
        real_item(
            menu,
            "Delete All…",
            Box::new(move || {
                // app.js deleteAllEntries: EVERY entry, through
                // the same one-dialog batch delete
                let ids = u.order.borrow().clone();
                delete_ids(&u, ids);
            }),
        );
        menu.add(&gtk::SeparatorMenuItem::new());
        let u = ui.clone();
        real_item(
            menu,
            "Grabber",
            Box::new(move || grabber::show(u.clone(), None, false)),
        );
        let u = ui.clone();
        real_item(menu, "Scheduler", Box::new(move || scheduler::show(&u)));
        let u = ui.clone();
        real_item(menu, "Options", Box::new(move || options::show(&u)));
    });

    // View — Dark Theme and Categories Sidebar are REAL
    // toggles (2.3.1: they shipped as scaffold check items
    // with no handlers — dead controls that lied to the user).
    // Both persist in ui-prefs and are applied at startup.
    {
        let root = menu_item("View");
        let menu = gtk::Menu::new();
        {
            let item = gtk::CheckMenuItem::with_label("Dark Theme");
            item.set_active(prefs::get("ndm-dark-theme")
                .and_then(|v| v.as_bool())
                .unwrap_or(false));
            item.connect_toggled(move |it| {
                let on = it.is_active();
                if let Some(settings) = gtk::Settings::default() {
                    settings.set_property("gtk-application-prefer-dark-theme", on);
                }
                prefs::set("ndm-dark-theme", serde_json::json!(on));
            });
            menu.add(&item);
        }
        {
            let item = gtk::CheckMenuItem::with_label("Categories Sidebar");
            item.set_active(prefs::get("ndm-sidebar")
                .and_then(|v| v.as_bool())
                .unwrap_or(true));
            let u = ui.clone();
            item.connect_toggled(move |it| {
                let on = it.is_active();
                if let Some(pane) = u.cat_pane.borrow().as_ref() {
                    pane.set_visible(on);
                }
                prefs::set("ndm-sidebar", serde_json::json!(on));
            });
            menu.add(&item);
        }
        {
            // Drop Basket is real: it owns the basket window's
            // visibility (basket.rs), starting hidden.
            let item = gtk::CheckMenuItem::with_label("Drop Basket");
            item.set_active(false);
            *ui.basket_check.borrow_mut() = Some(item.clone());
            let u = ui.clone();
            item.connect_toggled(move |it| basket::set_visible(&u, it.is_active()));
            menu.add(&item);
        }
        menu.add(&gtk::SeparatorMenuItem::new());
        for (label, key) in [
            ("Sort by Name", "name"),
            ("Sort by Date Added", "added"),
            ("Sort by Size", "size"),
            ("Sort by Status", "status"),
        ] {
            let u = ui.clone();
            real_item(&menu, label, Box::new(move || apply_menu_sort(&u, key)));
        }
        menu.add(&gtk::SeparatorMenuItem::new());
        let u = ui.clone();
        real_item(&menu, "Columns…", Box::new(move || misc::show_columns(&u)));
        let u = ui.clone();
        real_item(
            &menu,
            "Customize Toolbar…",
            Box::new(move || misc::show_toolbar_cfg(&u)),
        );
        menu.add(&gtk::SeparatorMenuItem::new());
        let u = ui.clone();
        real_item(
            &menu,
            "Refresh List",
            Box::new(move || {
                refresh_list(&u);
                rebuild_category_tree(&u);
            }),
        );
        root.set_submenu(Some(&menu));
        bar.add(&root);
    }

    // Help — About is the dialog builder's worked example and
    // is fully real already (dialogs::show_about).
    {
        let root = menu_item("Help");
        let menu = gtk::Menu::new();
        let u = ui.clone();
        real_item(
            &menu,
            "Check for Updates…",
            Box::new(move || manual_update_check(&u)),
        );
        let u = ui.clone();
        real_item(
            &menu,
            "Keyboard Shortcuts",
            Box::new(move || misc::show_shortcuts(&u)),
        );
        menu.add(&gtk::SeparatorMenuItem::new());
        let about = menu_item("About NDM");
        let ui2 = ui.clone();
        about.connect_activate(move |_| dialogs::show_about(&ui2.window));
        menu.add(&about);
        root.set_submenu(Some(&menu));
        bar.add(&root);
    }
    bar
}

/// The toolbar: IDM's set and order as the web UI carries them
/// (ui/index.html #toolbar / app.js TOOLBAR_DEFS) — Add URL,
/// Resume, Stop, Stop All, separator, Delete, Delete Completed,
/// Options, Scheduler, Start Queue, Stop Queue, Grabber. The
/// split buttons' queue-picker halves (▼) collapse into the
/// main button in this shell; Start/Stop Queue act on the
/// picked queue (see pick_queue_name).
fn build_toolbar(ui: &Rc<Ui>) -> gtk::Toolbar {
    let bar = gtk::Toolbar::new();
    bar.set_style(gtk::ToolbarStyle::Both);
    let register = |ui: &Rc<Ui>, id: &str, btn: &gtk::ToolButton| {
        ui.toolbar_buttons
            .borrow_mut()
            .insert(id.to_string(), btn.clone());
    };
    let scale = ui.window.scale_factor();
    let add_real = |id: &str, label: &str, icon: &str, f: Box<dyn Fn()>| {
        let btn = gtk::ToolButton::builder().label(label).build();
        if let Some(img) = icons::image(icon, 22, scale) {
            // apply_toolbar marks every button no_show_all (so the
            // state-restore show_all cannot resurrect a hidden
            // one) — which also means the window's show_all never
            // reaches this child. Show it explicitly, or the button
            // renders label-only (and, in icon-only size, nothing).
            img.show();
            btn.set_icon_widget(Some(&img));
        }
        register(ui, id, &btn);
        btn.connect_clicked(move |_| f());
        bar.add(&btn);
    };

    let u = ui.clone();
    add_real("btnAdd", "Add URL", "add", Box::new(move || open_add_dialog(&u)));
    let u = ui.clone();
    add_real(
        "btnResume",
        "Resume",
        "resume",
        Box::new(move || {
            let ids = selected_ids(&u);
            resume_ids(&u, &ids);
        }),
    );
    let u = ui.clone();
    add_real(
        "btnStop",
        "Stop",
        "stop",
        Box::new(move || {
            let ids = selected_ids(&u);
            pause_ids(&u, &ids);
        }),
    );
    let u = ui.clone();
    add_real("btnStopAll", "Stop All", "stop_all", Box::new(move || pause_all_downloading(&u)));
    bar.add(&gtk::SeparatorToolItem::new());
    let u = ui.clone();
    add_real(
        "btnDelete",
        "Delete",
        "delete",
        Box::new(move || {
            let ids = selected_ids(&u);
            delete_ids(&u, ids);
        }),
    );
    let u = ui.clone();
    add_real("btnDelDone", "Delete Completed", "delete_completed", Box::new(move || delete_completed(&u)));
    let u = ui.clone();
    add_real("btnOptions", "Options", "options", Box::new(move || options::show(&u)));
    let u = ui.clone();
    add_real("btnScheduler", "Scheduler", "scheduler", Box::new(move || scheduler::show(&u)));
    let u = ui.clone();
    add_real("btnStartQueue", "Start Queue", "queue_start", Box::new(move || start_queue_now(&u)));
    let u = ui.clone();
    add_real("btnStopQueue", "Stop Queue", "queue_stop", Box::new(move || stop_queue_now(&u)));
    let u = ui.clone();
    add_real(
        "btnGrabber",
        "Grabber",
        "grabber",
        Box::new(move || grabber::show(u.clone(), None, false)),
    );
    *ui.toolbar.borrow_mut() = Some(bar.clone());
    bar
}

/// Apply the toolbar config (show/hide + size) — app.js
/// applyToolbarCfg; called at startup and on dialog save.
pub(crate) fn apply_toolbar(ui: &Ui) {
    let cfg = prefs::load_toolbar();
    let buttons = ui.toolbar_buttons.borrow();
    for (id, _) in prefs::TOOLBAR_DEFS {
        if let Some(btn) = buttons.get(id) {
            // no_show_all: the window's show_all (state-restored
            // seam) must not resurrect a configured-hidden button.
            btn.set_no_show_all(true);
            btn.set_visible(!cfg.hidden.iter().any(|h| h == id));
        }
    }
    if let Some(bar) = ui.toolbar.borrow().as_ref() {
        if cfg.size == "small" {
            bar.set_style(gtk::ToolbarStyle::Icons);
            bar.set_icon_size(gtk::IconSize::SmallToolbar);
        } else {
            bar.set_style(gtk::ToolbarStyle::Both);
            bar.set_icon_size(gtk::IconSize::LargeToolbar);
        }
    }
}

/// The category pane: one GtkTreeView over a TreeStore whose
/// rows are (re)built by rebuild_category_tree from real core
/// state — the web UI's top-level rows (ui/index.html #catTree):
/// All Downloads (with the five file-type children plus any
/// custom categories in use), Unfinished, Finished, Grabber
/// projects, Queues (one child per configured queue).
fn build_category_pane() -> (gtk::TreeStore, gtk::TreeView) {
    let store = gtk::TreeStore::new(&[
        gtk::glib::types::Type::STRING, // CAT_COL_LABEL
        gtk::glib::types::Type::STRING, // CAT_COL_FILTER
    ]);
    let view = gtk::TreeView::with_model(&store);
    view.set_headers_visible(false);
    let col = gtk::TreeViewColumn::new();
    col.set_title("Categories");
    let cell = gtk::CellRendererText::new();
    gtk::prelude::CellLayoutExt::pack_start(&col, &cell, true);
    gtk::prelude::CellLayoutExt::add_attribute(&col, &cell, "text", CAT_COL_LABEL as i32);
    view.append_column(&col);
    (store, view)
}

/// (Re)seed the category tree from core state: built-in
/// category children, custom categories in use (app.js: names
/// from entries/settings beyond the built-ins, "General" and
/// "Grabber" excluded — they have no tree node / their own row),
/// and the configured queues under Queues.
fn rebuild_category_tree(ui: &Ui) {
    let store = &ui.cat_store;
    store.clear();
    let append = |parent: Option<&gtk::TreeIter>, label: &str, filter: &str| -> gtk::TreeIter {
        let iter = store.append(parent);
        store.set(
            &iter,
            &[
                (CAT_COL_LABEL, &label as &dyn gtk::glib::ToValue),
                (CAT_COL_FILTER, &filter as &dyn gtk::glib::ToValue),
            ],
        );
        iter
    };
    let all = append(None, "All Downloads", "All Downloads");
    for child in ["Compressed", "Documents", "Music", "Programs", "Video"] {
        append(Some(&all), child, child);
    }
    // custom categories in use join the All Downloads children,
    // as the web UI's tree rebuild does
    let builtin = [
        "General",
        "Compressed",
        "Documents",
        "Music",
        "Video",
        "Programs",
        "Grabber",
    ];
    for name in category_names(ui) {
        if !builtin.contains(&name.as_str()) {
            append(Some(&all), &name, &name);
        }
    }
    append(None, "Unfinished", "Unfinished");
    append(None, "Finished", "Finished");
    append(None, "Grabber projects", "Grabber");
    let queues = append(None, "Queues", "");
    for name in queue_names(ui) {
        append(Some(&queues), &name, &format!("queue:{}", name));
    }
    ui.cat_view.expand_row(&gtk::TreePath::new_first(), false);
    let _ = queues;
    // keep the current filter's row selected (first fill:
    // "All Downloads")
    let current = ui.filter.borrow().clone();
    select_category_row(ui, &current);
}

fn select_category_row(ui: &Ui, filter: &str) {
    fn walk(
        ui: &Ui,
        parent: Option<&gtk::TreeIter>,
        filter: &str,
    ) -> bool {
        let mut i = 0;
        loop {
            let Some(iter) = ui.cat_store.iter_nth_child(parent, i) else {
                return false;
            };
            let key: String = ui
                .cat_store
                .value(&iter, CAT_COL_FILTER as i32)
                .get()
                .unwrap_or_default();
            if key == filter {
                if let Some(path) = ui.cat_store.path(&iter) {
                    ui.cat_view.expand_to_path(&path);
                    ui.cat_view.selection().select_path(&path);
                }
                return true;
            }
            if walk(ui, Some(&iter), filter) {
                return true;
            }
            i += 1;
        }
    }
    walk(ui, None, filter);
}

/// The download list: GtkTreeView over a ListStore; visible
/// columns come from the column config (app.js COLUMN_DEFS /
/// `ndm-columns-v2`) + the block-map renderer column, which is
/// this shell's own and always appended last (see the
/// model-column table at the top of this file).
fn column_model(id: &str) -> u32 {
    match id {
        "name" => COL_NAME,
        "q" => COL_Q,
        "size" => COL_SIZE,
        "status" => COL_STATUS,
        "timeleft" => COL_TIMELEFT,
        "rate" => COL_RATE,
        "lasttry" => COL_LASTTRY,
        "desc" => COL_DESC,
        "added" => COL_ADDED,
        "saveto" => COL_SAVETO,
        _ => COL_NAME,
    }
}

/// The model column a "Sort by …" key sorts on — the same keys
/// the column headers use (see populate_columns).
fn sort_key_column(key: &str) -> u32 {
    match key {
        "size" => COL_TOTAL,
        "added" => COL_ADDED_RAW,
        "status" => COL_STATUS_RAW,
        _ => COL_NAME,
    }
}

/// View → Sort by …: sort the list by the key; choosing the
/// same key again flips the direction (the header-click
/// behaviour, mirrored in the menu). QA-220 m1: these were
/// "not yet available" stubs and the headers were inert.
fn apply_menu_sort(ui: &Rc<Ui>, key: &str) {
    let col = sort_key_column(key);
    let current = ui.store.sort_column_id();
    let order = match current {
        Some((gtk::SortColumn::Index(c), gtk::SortType::Ascending)) if c == col => {
            gtk::SortType::Descending
        }
        _ => gtk::SortType::Ascending,
    };
    ui.store
        .set_sort_column_id(gtk::SortColumn::Index(col), order);
    set_notice(
        ui,
        &format!(
            "Sorted by {} ({})",
            match key {
                "size" => "size",
                "added" => "date added",
                "status" => "status",
                _ => "name",
            },
            if order == gtk::SortType::Ascending { "ascending" } else { "descending" }
        ),
    );
}

thread_local! {
    /// The current File Name column, refreshed by every
    /// populate_columns — the flexible column the refit below
    /// resizes (see refit_name_column).
    static NAME_COLUMN: std::cell::RefCell<Option<gtk::TreeViewColumn>> =
        const { std::cell::RefCell::new(None) };
}

/// The name column is the list's flexible column: when the
/// fixed columns together are wider than the viewport, GTK
/// clips the LAST columns instead of shrinking any fixed one —
/// at ordinary window widths the Last Try Date lost its year
/// (field bug Y-4's root, which a wider date column alone could
/// never fix). On every allocation, hand the name column
/// exactly the width the others leave behind (its cell
/// ellipsizes), so every fixed column — dates included — is
/// always fully on screen; in a roomy window the name column
/// takes the slack, as its expand flag already intended.
fn refit_name_column(view: &gtk::TreeView) {
    let alloc = view.allocation().width();
    if alloc <= 0 {
        return;
    }
    let name_col = NAME_COLUMN.with(|c| c.borrow().clone());
    let Some(name_col) = name_col else { return };
    if !view.columns().iter().any(|c| c == &name_col) {
        return;
    }
    let others: i32 = view
        .columns()
        .iter()
        .filter(|c| *c != &name_col)
        .map(|c| c.width())
        .sum();
    let target = (alloc - others).clamp(140, 1600);
    if (name_col.width() - target).abs() > 2 {
        name_col.set_fixed_width(target);
    }
}

pub(crate) fn populate_columns(view: &gtk::TreeView, cfg: &[prefs::ColumnCfg]) {
    for col in view.columns() {
        view.remove_column(&col);
    }
    for c in cfg.iter().filter(|c| c.visible) {
        let (_, title, _, _) = prefs::COLUMN_DEFS
            .iter()
            .find(|(id, _, _, _)| *id == c.id)
            .copied()
            .unwrap_or(("name", "File Name", 0, false));
        let model_col = column_model(&c.id);
        let col = gtk::TreeViewColumn::new();
        col.set_title(title);
        col.set_resizable(true);
        // Sortable headers (QA-220 m1): Name sorts on its text,
        // Size on the raw byte count, Date Added on the ISO
        // stamp, Status on the raw status word — the ListStore's
        // own comparisons over the (hidden) key columns.
        let sort_col = match c.id.as_str() {
            "name" => Some(COL_NAME),
            "size" => Some(COL_TOTAL),
            "added" => Some(COL_ADDED_RAW),
            "status" => Some(COL_STATUS_RAW),
            _ => None,
        };
        if let Some(sc) = sort_col {
            col.set_sort_column_id(sc as i32);
            col.set_clickable(true);
        }
        if c.width > 0 {
            col.set_fixed_width(c.width);
            col.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        }
        if matches!(c.id.as_str(), "lasttry" | "added") {
            // A date column narrower than its default clips the
            // year (field bug Y-4); never let it shrink below the
            // width its text needs, even when the window is tight
            // and the expandable name column should be the one to
            // give.
            let def = prefs::COLUMN_DEFS
                .iter()
                .find(|(id, _, _, _)| *id == c.id)
                .map(|(_, _, w, _)| *w)
                .unwrap_or(0);
            if def > 0 {
                col.set_min_width(def);
            }
        }
        if c.width <= 0 && matches!(c.id.as_str(), "name" | "desc" | "saveto") {
            // Width 0 means "automatic", but an autosized column
            // holding unbounded text (a 187-char filename, a long
            // path) requisitions its full text width and drags
            // the whole main window wider — permanently, for the
            // session (audit-233 B-2). These three get a capped
            // fixed width instead (the name cell ellipsizes);
            // short-content columns keep true autosize.
            let def = prefs::COLUMN_DEFS
                .iter()
                .find(|(id, _, _, _)| *id == c.id)
                .map(|(_, _, w, _)| *w)
                .unwrap_or(0);
            col.set_fixed_width(if def > 0 { def } else { 380 });
            col.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            if c.id == "name" {
                col.set_expand(true);
            }
        }
        if c.id == "blocks" {
            // Block map — custom renderer (blockmap.rs), fed by
            // the model's blocks + fraction columns. A cfg entry
            // like the rest since audit-233 (hideable/movable in
            // the Columns chooser); width already applied above.
            if c.width <= 0 {
                col.set_fixed_width(160);
                col.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            }
            let cell = blockmap::BlockMapRenderer::new();
            gtk::prelude::CellLayoutExt::pack_start(&col, &cell, true);
            gtk::prelude::CellLayoutExt::add_attribute(&col, &cell, "blocks", COL_BLOCKS as i32);
            gtk::prelude::CellLayoutExt::add_attribute(&col, &cell, "fraction", COL_FRACTION as i32);
            view.append_column(&col);
            continue;
        }
        if model_col == COL_NAME {
            // Per-type row glyph (item 17), packed ahead of the
            // name text inside the same column — no model or
            // column-order change.
            let pix = gtk::CellRendererPixbuf::new();
            gtk::prelude::CellLayoutExt::pack_start(&col, &pix, false);
            gtk::prelude::CellLayoutExt::add_attribute(
                &col,
                &pix,
                "pixbuf",
                COL_ICON as i32,
            );
            pix.set_padding(2, 0);
        }
        let cell = gtk::CellRendererText::new();
        if model_col == COL_NAME {
            cell.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
        }
        gtk::prelude::CellLayoutExt::pack_start(&col, &cell, true);
        gtk::prelude::CellLayoutExt::add_attribute(&col, &cell, "text", model_col as i32);
        view.append_column(&col);
        if c.id == "name" {
            NAME_COLUMN.with(|slot| *slot.borrow_mut() = Some(col.clone()));
        }
    }
}

pub(crate) fn apply_columns(ui: &Ui) {
    populate_columns(&ui.list_view, &ui.col_cfg.borrow());
}

fn build_download_list(store: &gtk::ListStore, cfg: &[prefs::ColumnCfg]) -> gtk::TreeView {
    let view = gtk::TreeView::with_model(store);
    view.set_grid_lines(gtk::TreeViewGridLines::Horizontal);
    view.set_reorderable(false);
    let selection = view.selection();
    selection.set_mode(gtk::SelectionMode::Multiple);
    populate_columns(&view, cfg);
    view
}

/// The row context menu — the web UI's showCtxMenu shape
/// (app.js): header (entry name), Resume, Pause,
/// Properties / Status…, Refresh download address…, separator,
/// Open File, Open Folder, separator, Delete…, separator,
/// Select All, Delete All…. "Move to queue" appears between
/// Open Folder and Delete when more than one queue exists
/// (app.js: its own headed section), and — as in the web UI —
/// every item acts on the row the menu was opened for / the
/// current selection.
fn build_context_menu(ui: &Rc<Ui>, name: &str, id: Option<String>) -> gtk::Menu {
    let menu = gtk::Menu::new();
    let head = menu_item(name);
    head.set_sensitive(false);
    menu.add(&head);
    let real = |label: &str, f: Box<dyn Fn()>| {
        let item = menu_item(label);
        item.connect_activate(move |_| f());
        menu.add(&item);
    };
    // State gating for the row's own actions (audit-233: every
    // item used to be live in every state — Resume on a running
    // download, Pause on a finished one, Open File before any
    // file exists).
    let status = id
        .as_ref()
        .and_then(|i| ui.entries.borrow().get(i).map(|d| d.status.clone()))
        .unwrap_or_default();
    let gated = |label: &str, on: bool, f: Box<dyn Fn()>| {
        let item = menu_item(label);
        item.set_sensitive(on);
        item.connect_activate(move |_| f());
        menu.add(&item);
    };

    {
        // Resume/Pause follow the same target rule Delete
        // states below (app.js openDeleteConfirm's rule): the
        // whole multi-selection when the menu's row is part of
        // one, else just that row. They used to act on the
        // clicked row alone, silently ignoring a selection the
        // right-click had deliberately kept (audit-233).
        let (u, id) = (ui.clone(), id.clone());
        gated(
            "Resume",
            id.is_some()
                && !matches!(status.as_str(), "downloading" | "starting" | "merging" | "resuming"),
            Box::new(move || {
                let sel = selected_ids(&u);
                let ids: Vec<String> = match &id {
                    Some(i) if sel.len() > 1 && sel.iter().any(|x| x == i) => sel,
                    Some(i) => vec![i.clone()],
                    None => sel,
                };
                if !ids.is_empty() {
                    resume_ids(&u, &ids);
                }
            }),
        );
    }
    {
        let (u, id) = (ui.clone(), id.clone());
        gated(
            "Pause",
            id.is_some()
                && matches!(status.as_str(), "downloading" | "starting" | "resuming" | "queued"),
            Box::new(move || {
                let sel = selected_ids(&u);
                let ids: Vec<String> = match &id {
                    Some(i) if sel.len() > 1 && sel.iter().any(|x| x == i) => sel,
                    Some(i) => vec![i.clone()],
                    None => sel,
                };
                if !ids.is_empty() {
                    pause_ids(&u, &ids);
                }
            }),
        );
    }
    {
        // The per-download progress dialog (PROGRESS-DIALOG-SPEC
        // §1.3: the row menu's "Show progress").
        let item = menu_item("Show progress");
        item.set_sensitive(id.is_some());
        let (u, id) = (ui.clone(), id.clone());
        item.connect_activate(move |_| {
            if let Some(id) = &id {
                open_progress_dialog(&u, id);
            }
        });
        menu.add(&item);
    }
    {
        let (u, id) = (ui.clone(), id.clone());
        real(
            "Refresh download address…",
            Box::new(move || {
                if let Some(id) = &id {
                    refresh_address(&u, id);
                }
            }),
        );
    }
    menu.add(&gtk::SeparatorMenuItem::new());
    {
        let u = ui.clone();
        real("Open File", Box::new(move || open_selected(&u, false)));
    }
    {
        let u = ui.clone();
        real("Open Folder", Box::new(move || open_selected(&u, true)));
    }
    // "Move to queue" — only when more than one queue exists,
    // exactly as the web UI omits the section otherwise.
    let queues = queue_names(ui);
    if queues.len() > 1 {
        menu.add(&gtk::SeparatorMenuItem::new());
        let head = menu_item("Move to queue");
        head.set_sensitive(false);
        menu.add(&head);
        let current = id
            .as_ref()
            .and_then(|id| ui.entries.borrow().get(id).map(|d| d.queue.clone()))
            .unwrap_or_default();
        for qn in queues {
            let label = if qn == current {
                format!("✓ {}", qn)
            } else {
                qn.clone()
            };
            let (u, idc, q) = (ui.clone(), id.clone(), qn.clone());
            real(
                &label,
                Box::new(move || {
                    if let Some(id) = &idc {
                        move_to_queue(&u, id, &q);
                    }
                }),
            );
        }
    }
    menu.add(&gtk::SeparatorMenuItem::new());
    {
        let u = ui.clone();
        real(
            "Delete…",
            Box::new(move || {
                // the whole multi-selection when the menu's row
                // is part of one, else just that row (app.js
                // openDeleteConfirm's target rule)
                let ids = selected_ids(&u);
                delete_ids(&u, ids);
            }),
        );
    }
    menu.add(&gtk::SeparatorMenuItem::new());
    {
        let u = ui.clone();
        real(
            "Select All",
            Box::new(move || {
                u.list_view.selection().select_all();
            }),
        );
    }
    {
        let u = ui.clone();
        real(
            "Delete All…",
            Box::new(move || {
                let ids = u.order.borrow().clone();
                delete_ids(&u, ids);
            }),
        );
    }
    // Queue order within the entry's queue (core move_in_queue,
    // the Scheduler's reorder primitive) — a small real extra
    // the row menu is the natural home for.
    {
        let (u, id) = (ui.clone(), id.clone());
        real(
            "Move Up in Queue",
            Box::new(move || {
                if let Some(id) = &id {
                    let _ = sink::move_in_queue(u.ctx.clone(), id.clone(), true);
                    refresh_list(&u);
                }
            }),
        );
    }
    {
        let (u, id) = (ui.clone(), id.clone());
        real(
            "Move Down in Queue",
            Box::new(move || {
                if let Some(id) = &id {
                    let _ = sink::move_in_queue(u.ctx.clone(), id.clone(), false);
                    refresh_list(&u);
                }
            }),
        );
    }
    menu.show_all();
    menu
}

/// Status bar with 3 sections (see `Ui::status`).
fn build_statusbar(status: &[gtk::Label; 3]) -> gtk::Box {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    for (i, label) in status.iter().enumerate() {
        label.set_halign(gtk::Align::Start);
        label.set_margin_start(6);
        label.set_margin_end(6);
        let frame = gtk::Frame::new(None);
        frame.set_shadow_type(gtk::ShadowType::In);
        frame.add(label);
        if i == 2 {
            frame.set_hexpand(true);
        }
        bar.add(&frame);
    }
    bar
}

// ---------------------------------------------------------------------------
// Event dispatch — runs ONLY on the GTK main thread (the channel
// receiver is attached to the default main context). One match
// arm per AppEvent variant, deliberately without a wildcard:
// the enum is the contract (sink.rs), and a new core event must
// fail this compile until the shell handles it.
// ---------------------------------------------------------------------------
fn handle_app_event(ui: &Rc<Ui>, event: AppEvent) {
    match event {
        AppEvent::Progress(payload) => {
            // Real row update — the list's whole live surface.
            // Core's payload fields are private, so the row code
            // reads the decoded wire view (see sink.rs). The
            // same view feeds an open progress dialog for this
            // entry (progress.rs routes by id).
            let view = ProgressView::from(&payload);
            upsert_entry(ui, &view);
            progress::update(&view);
        }
        AppEvent::DownloadComplete(v) => {
            // One row update, nothing more: mark the mirror
            // entry finished and land the finish in its row
            // (cadence rule — no full-list rebuild per event;
            // the final Progress payload carries the same state
            // and the next user-initiated refresh resyncs all).
            // The Download Complete dialog itself is a later
            // stage's surface.
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                if let Some(d) = ui.entries.borrow_mut().get_mut(id) {
                    d.status = "finished".to_string();
                    if d.total > 0 {
                        d.done_bytes = d.total;
                    }
                    d.speed_bps = 0.0;
                }
                let still_visible = ui
                    .entries
                    .borrow()
                    .get(id)
                    .map(|d| entry_visible(&ui.filter.borrow(), d))
                    .unwrap_or(false);
                if still_visible {
                    if let Some(iter) = iter_for(&ui.store, id) {
                        ui.store.set(
                            &iter,
                            &[
                                (COL_STATUS, &"Complete" as &dyn gtk::glib::ToValue),
                                // The raw status is the Status
                                // column's SORT KEY: leaving it at
                                // "downloading" kept finished rows
                                // sorted as active until the next
                                // full refresh (audit-233).
                                (COL_STATUS_RAW, &"finished" as &dyn gtk::glib::ToValue),
                                (COL_Q, &"" as &dyn gtk::glib::ToValue),
                                (COL_FRACTION, &1.0f64 as &dyn gtk::glib::ToValue),
                                (COL_RATE, &"" as &dyn gtk::glib::ToValue),
                                (COL_TIMELEFT, &"" as &dyn gtk::glib::ToValue),
                                (COL_SPEED, &0.0f64 as &dyn gtk::glib::ToValue),
                            ],
                        );
                        refresh_status_counts(ui);
                    } else if let Some(d) = ui.entries.borrow().get(id).cloned() {
                        // The row was filtered out while it was
                        // downloading (the Finished filter was
                        // active): completion is what makes it
                        // visible, so insert it — updating alone
                        // left the Finished view without the row
                        // until a manual refresh.
                        apply_progress(ui, &d);
                        refresh_status_counts(ui);
                    }
                } else if let Some(mut iter) = iter_for(&ui.store, id) {
                    ui.store.remove(&mut iter);
                    refresh_status_counts(ui);
                }
            }
            // An open progress dialog for this entry shows the
            // final state and stays open (spec §1.3).
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                if let Some(d) = ui.entries.borrow().get(id).cloned() {
                    progress::update(&d);
                }
            }
            refresh_toolbar_states(ui);
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if name.is_empty() {
                set_notice(ui, "Download complete");
            } else {
                set_notice(ui, &format!("Download complete: {}", name));
            }
            // The Download Complete dialog: core emitted this
            // event only when its completion_show_dialog rule
            // passed (override → entry action → global), so the
            // shell simply shows it (modals.rs mirrors cmodal,
            // including yield-to-dialogs and the batch fold).
            modals::on_download_complete(&ui.window, ui.ctx.clone(), &v);
        }
        AppEvent::EntryRemoved(v) => {
            // Real: rows leave the list only through this event
            // (the web UI's listener contract, payload {id}).
            // One row removal per event — a bulk remove emits
            // one event per row, so this stays O(1) per event
            // instead of rebuilding the whole list N times.
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                progress::close(id);
                modals::forget_complete(id);
                remove_entry_local(ui, id);
            }
        }
        AppEvent::IncomingFile(v) => {
            // Browser-caught files and basket / clipboard /
            // drop hand-offs: the File Info dialog prefilled
            // with the offered payload, queued behind any open
            // modal exactly as app.js queues file links.
            queue_incoming_file(ui, v);
        }
        AppEvent::IncomingVideo(v) => {
            let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let cookies = v
                .get("cookies")
                .and_then(|x| x.as_str())
                .filter(|x| !x.is_empty())
                .map(|x| x.to_string());
            if url.starts_with("http://") || url.starts_with("https://") {
                if capture_dialog_visible() {
                    queue_capture(ui, "video", url, cookies);
                } else {
                    video::show(ui.clone(), Some(url), cookies);
                }
            } else {
                set_notice(ui, "Incoming video without an address");
            }
        }
        AppEvent::IncomingGrab(v) => {
            // The extension's grab-all-links: the wizard opens
            // with the page filled in and exploration running
            // (app.js pumpGrabLinks).
            let url = v
                .get("url")
                .and_then(|x| x.as_str())
                .filter(|x| !x.is_empty())
                .or_else(|| v.get("pageUrl").and_then(|x| x.as_str()))
                .unwrap_or("")
                .to_string();
            if url.starts_with("http://") || url.starts_with("https://") {
                if capture_dialog_visible() {
                    queue_capture(ui, "grab", url, None);
                } else {
                    grabber::show(ui.clone(), Some(url), true);
                }
            } else {
                set_notice(ui, "Incoming grab without an address");
            }
        }
        AppEvent::BasketNote(v) => {
            let text = v
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("Basket note");
            basket::on_basket_note(ui, text);
            set_notice(ui, text);
        }
        AppEvent::ComponentsProgress(v) => {
            // The video picker waits on this invisibly (its gate);
            // the status line only names a component in flight.
            video::on_components_progress(&v);
            if let Some(name) = v.get("name").and_then(|x| x.as_str()) {
                set_notice(ui, &format!("Setting up the video engine: {}", name));
            }
        }
        AppEvent::UpdateAvailable(v) => {
            // The Update dialog (modals.rs mirrors umodal,
            // mandatory mode included).
            if v.get("status").and_then(|x| x.as_str()) == Some("available") {
                modals::show_update(&ui.window, ui.ctx.clone(), &v);
            }
            set_notice(ui, "A new version of NDM is available");
        }
        AppEvent::UpdateProgress(v) => {
            modals::on_update_progress(&v);
        }
        AppEvent::UpdateInstalled(_) => {
            modals::on_update_installed();
            set_notice(ui, "Update installed — restart NDM to use it");
        }
        AppEvent::QuotaExceeded(_) => {
            set_notice(ui, "Download limit reached for this period");
        }
        AppEvent::QuotaCleared(_) => {
            set_notice(ui, "Download limit cleared");
        }
        AppEvent::ExitVetoed(v) => {
            // Core postponed an automatic exit because a
            // download is paused (maybe_exit_when_done) — the
            // shell's job is only to surface it.
            let paused = v.get("paused").and_then(|x| x.as_u64()).unwrap_or(0);
            if paused > 0 {
                set_notice(
                    ui,
                    &format!(
                        "Exit postponed: {} download{} paused",
                        paused,
                        if paused == 1 { "" } else { "s" }
                    ),
                );
            } else {
                set_notice(ui, "Exit postponed: a download is paused");
            }
        }
        AppEvent::PersistFailed(_) => {
            set_notice(ui, "Warning: the download list could not be saved");
        }
        AppEvent::PersistRecovered(_) => {
            set_notice(ui, "Download list saved");
        }
        AppEvent::DropFileIgnored(v) => {
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
            set_notice(ui, &format!("Not a web link: {}", name));
        }
        AppEvent::ExtUpdateNotice(_) => {
            set_notice(ui, "Browser extension update pending — restart the browser");
        }
        AppEvent::ProgressDialogReveal(v) => {
            // Core emits this on starts when the
            // progress_dialog_view setting is "show" or
            // "minimized" (§1.3/G7 — core's
            // maybe_reveal_progress_dialog is the gate) and the
            // payload carries the view. The shell's part: "show"
            // opens/raises the real dialog; "minimized" creates
            // it lowered (iconified), never presented.
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
            if !id.is_empty() {
                if v.get("view").and_then(|x| x.as_str()) == Some("minimized") {
                    open_progress_dialog_lowered(ui, id);
                } else {
                    open_progress_dialog_reveal(ui, id);
                }
            }
        }
    }
}

fn handle_ui_message(ui: &Rc<Ui>, msg: UiMessage) -> gtk::glib::ControlFlow {
    match msg {
        UiMessage::Event(event) => handle_app_event(ui, event),
        UiMessage::RaiseMainWindow => {
            ui.window.present();
        }
        UiMessage::ExitApp => {
            ui.window.close();
            return gtk::glib::ControlFlow::Break;
        }
    }
    gtk::glib::ControlFlow::Continue
}

// ---------------------------------------------------------------------------
// Startup show seam — NATIVE-PLAN.md §1.4: the Tauri shell shows
// the window on the WebView's first page-load `Finished`; the
// native replacement key is "state restored" — the journal is
// back and the first list fill has run. The native shell has
// no page-load gate, so the window shows as soon as build_ui
// has bootstrapped state and filled the list; a `--background`
// start stays hidden either way, as today.
// ---------------------------------------------------------------------------
fn on_state_restored(window: &gtk::ApplicationWindow, background: bool) {
    if background {
        window.hide();
    } else {
        window.show_all();
        window.present();
    }
}

/// The app-data dir on Linux — the Tauri app's own:
/// ($XDG_DATA_HOME or ~/.local/share)/com.ndm.downloadmanager
/// (the same computation the Tauri shell's single-instance
/// guard makes for its port).
pub(crate) fn app_data_dir() -> PathBuf {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local").join("share"))
            .unwrap_or_else(|| PathBuf::from(".local/share")),
    };
    base.join("com.ndm.downloadmanager")
}

/// The finish layer (item 17): one accent, calm hover/pressed
/// states on the toolbar, and a focus ring a keyboard user can
/// actually see. Deliberately shallow — layout, structure and the
/// distro theme's own styling are untouched; this only evens out
/// states the stock theme leaves flat.
fn install_finish_css() {
    const CSS: &str = "
toolbar { padding: 4px 6px; }
toolbar button { border-radius: 8px; padding: 6px 10px 4px 10px; }
toolbar button:hover { background-color: alpha(#2f7fd0, 0.10); }
toolbar button:active { background-color: alpha(#2f7fd0, 0.18); }
toolbar button:disabled { opacity: 0.45; }
button:focus, entry:focus, treeview:focus, check:focus, radio:focus {
  outline-color: alpha(#2f7fd0, 0.55);
  outline-style: solid;
  outline-width: 2px;
  outline-offset: -2px;
}
";
    let provider = gtk::CssProvider::new();
    if provider.load_from_data(CSS.as_bytes()).is_ok() {
        if let Some(screen) = gtk::gdk::Screen::default() {
            gtk::StyleContext::add_provider_for_screen(
                &screen,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    }
}

fn build_ui(app: &gtk::Application, ctx: &Ctx, receiver: &gtksink::ReceiverSlot) {
    install_finish_css();
    let store = gtk::ListStore::new(&[
        gtk::glib::types::Type::STRING, // COL_ID
        gtk::glib::types::Type::STRING, // COL_NAME
        gtk::glib::types::Type::STRING, // COL_Q
        gtk::glib::types::Type::STRING, // COL_SIZE
        gtk::glib::types::Type::STRING, // COL_STATUS
        gtk::glib::types::Type::STRING, // COL_TIMELEFT
        gtk::glib::types::Type::STRING, // COL_RATE
        gtk::glib::types::Type::STRING, // COL_LASTTRY
        gtk::glib::types::Type::STRING, // COL_DESC
        gtk::glib::types::Type::STRING, // COL_ADDED
        gtk::glib::types::Type::STRING, // COL_SAVETO
        gtk::glib::types::Type::STRING, // COL_BLOCKS
        gtk::glib::types::Type::F64,    // COL_FRACTION
        gtk::glib::types::Type::U64,    // COL_DONE
        gtk::glib::types::Type::U64,    // COL_TOTAL
        gtk::glib::types::Type::F64,    // COL_SPEED
        gtk::glib::types::Type::STRING, // COL_ADDED_RAW
        gtk::glib::types::Type::STRING, // COL_STATUS_RAW
        gtk::gdk_pixbuf::Pixbuf::static_type(), // COL_ICON
    ]);
    debug_assert_eq!(store.n_columns() as usize, NCOLS);

    // The remembered window size wins over the spec default
    // (audit-233: geometry was never remembered — every launch
    // reset to 1120x720 wherever the user had left it). Only the
    // size is kept; position is the window manager's business
    // and a stale one can strand the window off-screen.
    let (def_w, def_h) = prefs::get("ndm-window-size")
        .and_then(|v| {
            let a = v.as_array()?;
            let w = a.first()?.as_i64()? as i32;
            let h = a.get(1)?.as_i64()? as i32;
            if w >= 860 && h >= 540 && w <= 7680 && h <= 4320 {
                Some((w, h))
            } else {
                None
            }
        })
        .unwrap_or((1120, 720)); // §1.1 main-window spec
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Nill Download Manager")
        .default_width(def_w)
        .default_height(def_h)
        .build();
    window.set_size_request(860, 540); // §1.1 minimum size

    // Closing the main window IS the user leaving (Tauri shell,
    // N-12: CloseRequested on main → exit(0); there is no tray
    // and no close-to-tray mode). The window's destruction
    // therefore quits the application outright.
    {
        let app = app.clone();
        // Track the live size on every allocation: at destroy
        // time gtk_window_get_size already reports the builder
        // defaults (the GdkWindow is gone), which silently
        // saved 1120x720 whatever the user had resized to —
        // caught by the FIX-233 rig proof, not by reading.
        let live_size = Rc::new(std::cell::Cell::new((def_w, def_h)));
        let track = live_size.clone();
        window.connect_size_allocate(move |_, alloc| {
            track.set((alloc.width(), alloc.height()));
        });
        window.connect_destroy(move |_| {
            let (w, h) = live_size.get();
            prefs::set("ndm-window-size", serde_json::json!([w, h]));
            app.quit();
        });
    }

    let (cat_store, cat_view) = build_category_pane();
    let col_cfg = prefs::load_columns();
    let list = build_download_list(&store, &col_cfg);
    list.connect_size_allocate(|view, _alloc| refit_name_column(view));

    let ui = Rc::new(Ui {
        window: window.clone(),
        store: store.clone(),
        status: [gtk::Label::new(None), gtk::Label::new(None), gtk::Label::new(None)],
        eta_samples: RefCell::new(HashMap::new()),
        ctx: ctx.clone(),
        entries: RefCell::new(HashMap::new()),
        order: RefCell::new(Vec::new()),
        filter: RefCell::new("All Downloads".to_string()),
        list_view: list.clone(),
        cat_store: cat_store.clone(),
        cat_view: cat_view.clone(),
        queue_pick: RefCell::new("Main download queue".to_string()),
        toolbar: RefCell::new(None),
        toolbar_buttons: RefCell::new(HashMap::new()),
        col_cfg: RefCell::new(col_cfg),
        basket_check: RefCell::new(None),
        cat_pane: RefCell::new(None),
    });
    ui.status[0].set_text("No downloads");
    ui.status[2].set_text("Ready");

    let menubar = build_menubar(&ui);
    let toolbar = build_toolbar(&ui);
    apply_toolbar(&ui);

    // Keyboard shortcuts (app.js's global keydown): Ctrl+N,
    // Ctrl+Shift+V, F5, Ctrl+Q, Ctrl+A — inert while a text
    // field has focus, as in the web UI.
    {
        let u = ui.clone();
        window.connect_key_press_event(move |win, ev| {
            use gtk::gdk;
            let focused = win
                .focused_widget()
                .map(|w| {
                    w.is::<gtk::Entry>()
                        || w.is::<gtk::TextView>()
                        || w.is::<gtk::SpinButton>()
                })
                .unwrap_or(false);
            if focused {
                return gtk::glib::Propagation::Proceed;
            }
            let ctrl = ev.state().contains(gdk::ModifierType::CONTROL_MASK);
            let shift = ev.state().contains(gdk::ModifierType::SHIFT_MASK);
            let kv = ev.keyval();
            if ctrl && !shift && kv == gdk::keys::constants::n {
                open_add_dialog(&u);
            } else if ctrl && shift && kv == gdk::keys::constants::v {
                video::show(u.clone(), None, None);
            } else if kv == gdk::keys::constants::F5 {
                refresh_list(&u);
                rebuild_category_tree(&u);
            } else if ctrl && kv == gdk::keys::constants::q {
                win.close();
            } else if ctrl && kv == gdk::keys::constants::a {
                u.list_view.selection().select_all();
            } else {
                return gtk::glib::Propagation::Proceed;
            }
            gtk::glib::Propagation::Stop
        });
    }

    // Drag-out (WS-J): drag a FINISHED row out of the list into
    // a file manager / desktop and get the actual file — the
    // inverse of the Drop Basket. The entry set is snapshotted
    // at button-press (this GTK's TreeView collapses a
    // multi-selection to the pressed row on press, before
    // drag-begin runs — the snapshot is what keeps a group drag
    // a group drag). The gate is applied at drag-begin: no
    // finished entry with a file on disk in the set, no drag.
    {
        let u = ui.clone();
        let press_set: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let stash: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let targets = [gtk::TargetEntry::new(
            "text/uri-list",
            gtk::TargetFlags::OTHER_APP,
            0,
        )];
        list.drag_source_set(
            gtk::gdk::ModifierType::BUTTON1_MASK,
            &targets,
            gtk::gdk::DragAction::COPY,
        );
        {
            let u = u.clone();
            let press_set = press_set.clone();
            list.connect_button_press_event(move |view, ev| {
                if ev.button() == 1 {
                    let (x, y) = ev.position();
                    let mut set: Vec<String> = Vec::new();
                    if let Some((Some(path), _, _, _)) =
                        view.path_at_pos(x as i32, y as i32)
                    {
                        if let Some(iter) = u.store.iter(&path) {
                            let id: String = u
                                .store
                                .value(&iter, COL_ID as i32)
                                .get()
                                .unwrap_or_default();
                            let selected = selected_ids(&u);
                            if selected.iter().any(|s| s == &id) {
                                set = selected;
                            } else if !id.is_empty() {
                                set = vec![id];
                            }
                        }
                    }
                    *press_set.borrow_mut() = set;
                }
                gtk::glib::Propagation::Proceed
            });
        }
        let stash_begin = stash.clone();
        list.connect_drag_begin(move |_, ctx| {
            let uris = drag_out_uris(&u, &press_set.borrow());
            if uris.is_empty() {
                ctx.drag_abort(0); // GDK_CURRENT_TIME
            }
            *stash_begin.borrow_mut() = uris;
        });
        list.connect_drag_data_get(move |_, _, data, _, _| {
            let uris = stash.borrow();
            if !uris.is_empty() {
                let refs: Vec<&str> = uris.iter().map(|s| s.as_str()).collect();
                data.set_uris(&refs);
            }
        });
    }

    // Toolbar sensitivity follows the list selection (IDM
    // logic — see refresh_toolbar_states).
    {
        let u = ui.clone();
        ui.list_view.selection().connect_changed(move |_| {
            refresh_toolbar_states(&u);
        });
    }

    // Category selection filters the list (app.js selectTreeCat
    // → visibleItems). The Queues header row carries an empty
    // key and only expands.
    {
        let u = ui.clone();
        cat_view.selection().connect_changed(move |sel| {
            let Some((model, iter)) = sel.selected() else {
                return;
            };
            let key: String = model
                .value(&iter, CAT_COL_FILTER as i32)
                .get()
                .unwrap_or_default();
            if key.is_empty() {
                return;
            }
            if *u.filter.borrow() != key {
                *u.filter.borrow_mut() = key;
                refresh_list(&u);
            }
        });
    }

    // Right-click on the category pane: "New category…" — the
    // web UI's prompt caller (app.js inCatAdd → showPrompt): the
    // name persists as an empty category_exts slot, the same
    // place the Options editor stores its overrides, so it
    // survives restarts; an existing name just selects it.
    {
        let u = ui.clone();
        cat_view.connect_button_press_event(move |_, event| {
            if event.button() != 3 {
                return gtk::glib::Propagation::Proceed;
            }
            let menu = gtk::Menu::new();
            let item = menu_item("New category…");
            let u2 = u.clone();
            item.connect_activate(move |_| {
                let Some(raw) = dialogs::show_prompt(
                    &u2.window,
                    "New category",
                    "Name for the new category:",
                    "",
                ) else {
                    return;
                };
                let name = raw.trim().to_string();
                if name.is_empty() {
                    return;
                }
                if let Some(existing) = category_names(&u2)
                    .iter()
                    .find(|c| c.eq_ignore_ascii_case(&name))
                    .cloned()
                {
                    *u2.filter.borrow_mut() = existing.clone();
                    refresh_list(&u2);
                    select_category_row(&u2, &existing);
                    return;
                }
                update_settings(&u2, |obj| {
                    let exts = obj
                        .entry("category_exts")
                        .or_insert_with(|| serde_json::json!({}));
                    if let Some(m) = exts.as_object_mut() {
                        m.insert(name.clone(), serde_json::json!([]));
                    }
                });
                rebuild_category_tree(&u2);
                set_notice(&u2, &format!("Category \"{}\" created", name));
            });
            menu.add(&item);
            menu.show_all();
            menu.popup_at_pointer(Some(event));
            gtk::glib::Propagation::Stop
        });
    }

    // Right-click on a row: select it (when it is not already
    // part of the selection), then the app context menu.
    let ui_for_menu = ui.clone();
    list.connect_button_press_event(move |view, event| {
        if event.button() == 3 {
            let mut name = "Download".to_string();
            let mut row_id: Option<String> = None;
            let (x, y) = event.position();
            if let Some((Some(path), _, _, _)) = view.path_at_pos(x as i32, y as i32) {
                if let Some(model) = view.model() {
                    if let Some(iter) = model.iter(&path) {
                        let found: String = model
                            .value(&iter, COL_NAME as i32)
                            .get()
                            .unwrap_or_default();
                        if !found.is_empty() {
                            name = found;
                        }
                        let id: String = model
                            .value(&iter, COL_ID as i32)
                            .get()
                            .unwrap_or_default();
                        if !id.is_empty() {
                            row_id = Some(id);
                        }
                        // a right-clicked row outside the current
                        // selection becomes the selection (the
                        // web UI's row-menu behaviour)
                        let sel = view.selection();
                        if !sel.path_is_selected(&path) {
                            sel.unselect_all();
                            sel.select_path(&path);
                        }
                    }
                }
            }
            if name.chars().count() > 34 {
                name = format!("{}…", name.chars().take(34).collect::<String>());
            }
            let menu = build_context_menu(&ui_for_menu, &name, row_id);
            menu.popup_at_pointer(Some(event));
            return gtk::glib::Propagation::Stop;
        }
        gtk::glib::Propagation::Proceed
    });

    // Double-click a row opens the per-download progress
    // dialog (PROGRESS-DIALOG-SPEC §1.3, NDM's ruling).
    {
        let u = ui.clone();
        list.connect_row_activated(move |_, path, _| {
            let mut id = String::new();
            if let Some(model) = u.list_view.model() {
                if let Some(iter) = model.iter(path) {
                    id = model.value(&iter, COL_ID as i32).get().unwrap_or_default();
                }
            }
            if !id.is_empty() {
                open_progress_dialog(&u, &id);
            }
        });
    }

    let cat_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    cat_scroll.add(&cat_view);
    cat_scroll.set_size_request(200, -1);
    *ui.cat_pane.borrow_mut() = Some(cat_scroll.clone());
    // The persisted sidebar choice is applied at the END of
    // build_ui, after on_state_restored's show_all: hiding it
    // here via no_show_all would make show_all skip the whole
    // subtree and the tree inside would never be shown at all
    // (the pane would sit allocated but blank).
    // …and the persisted dark-theme choice (the Settings object
    // is process-wide, so this also covers later dialogs).
    if prefs::get("ndm-dark-theme")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        if let Some(settings) = gtk::Settings::default() {
            settings.set_property("gtk-application-prefer-dark-theme", true);
        }
    }
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();
    list_scroll.add(&list);

    let panes = gtk::Paned::new(gtk::Orientation::Horizontal);
    panes.pack1(&cat_scroll, false, false);
    panes.pack2(&list_scroll, true, false);
    panes.set_position(210);

    let statusbar = build_statusbar(&ui.status);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add(&menubar);
    root.add(&toolbar);
    root.pack_start(&panes, true, true, 0);
    root.add(&statusbar);
    window.add(&root);

    // Event bridge: core threads → this main thread.
    let ui_for_events = ui.clone();
    receiver.attach(move |msg| handle_ui_message(&ui_for_events, msg));

    // First fill from real core state: the category tree from
    // settings + entries, the list from list_downloads, and the
    // one-time startup notice (the web UI pulls the same slot
    // once on load).
    rebuild_category_tree(&ui);
    refresh_list(&ui);
    if let Some(notice) = sink::take_startup_notice(ui.ctx.clone()) {
        if !notice.trim().is_empty() {
            set_notice(&ui, &notice);
        }
    }
    // An init_paths failure outranks any startup notice: it means
    // the app is running against lost state.
    if let Ok(mut slot) = INIT_PATHS_ERROR.lock() {
        if let Some(err) = slot.take() {
            set_notice(&ui, &err);
        }
    }
    // Pull a latched update offer (app.js pulls get_pending_update
    // on load — the launch check's emit can predate this shell's
    // listener on a cold start).
    let pending = sink::get_pending_update(ui.ctx.clone());
    if pending.get("status").and_then(|x| x.as_str()) == Some("available") {
        modals::show_update(&ui.window, ui.ctx.clone(), &pending);
    }

    // State-restored seam (§1.4): init_paths ran in main()
    // before the app started and the first fill just ran, so
    // the window may show now.
    let background = std::env::args().any(|a| a == "--background");
    on_state_restored(&window, background);
    // The persisted Categories Sidebar choice, applied after
    // show_all has shown the whole tree (see cat_pane above).
    if !prefs::get("ndm-sidebar")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
    {
        if let Some(pane) = ui.cat_pane.borrow().as_ref() {
            pane.set_visible(false);
        }
    }
}

/// Single-instance guard, independent of GtkApplication's
/// D-Bus forwarding: an exclusive flock on <data dir>/instance.lock
/// held for the process's whole life (the returned File must stay
/// alive — dropping it releases the lock). Returns None when
/// another instance already holds it: that instance is asked to
/// raise its window through the incoming queue (the show_app line
/// its drain already honours) and this process exits before any
/// state bootstrap, so two instances can never hold divergent
/// copies of the journal and overwrite each other's entries
/// (field bug I-2: a paused download vanished from the list —
/// the second instance's view of the journal is the one
/// mechanism that fits, and a second process was observed
/// running alongside the first in the test rig). GtkApplication
/// forwarding stays as the fast path where the session bus
/// supports it; this guard is the floor that always holds.
/// flock releases on process death, so a crash leaves no stale
/// lock behind. Non-Unix and unexpected fs errors fail open —
/// the Windows shell carries its own guard, and a broken lock
/// file must never keep the app itself from starting.
#[cfg(unix)]
fn acquire_instance_guard(dir: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(dir.join("instance.lock"))
        .ok()?;
    let mut rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 && std::env::var_os("NDM_RESTART_WAIT").is_some() {
        // Update-restart handoff: the old process still holds the
        // lock while it shuts down, and it is EXPECTED to release
        // it momentarily. Without a wait, this process read the
        // held lock as "another instance is running", queued a
        // raise to the dying process and exited — the restart
        // silently left no app at all. Retry briefly; a genuinely
        // running instance (env unset, or still alive after the
        // wait) falls through to the normal path below.
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                break;
            }
        }
    }
    if rc == 0 {
        return Some(file);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        if let Ok(mut q) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("incoming.jsonl"))
        {
            use std::io::Write;
            let _ = q.write_all(b"{\"kind\":\"show_app\"}\n");
        }
        std::process::exit(0);
    }
    Some(file)
}

fn main() {
    // The single-instance floor runs before ANY state work:
    // a second process must not sweep, restore or persist
    // against the shared journal. The guard File lives in this
    // binding until main returns — i.e. for the app's whole run.
    #[cfg(unix)]
    let _instance_guard = acquire_instance_guard(&app_data_dir());
    // The one channel between core threads and the GTK thread.
    #[allow(deprecated)]
    let (sender, receiver) = gtk::glib::MainContext::channel::<UiMessage>(
        gtk::glib::Priority::DEFAULT,
    );
    let sink: Arc<dyn sink::EventSink> = Arc::new(GtkEventSink::new(sender));

    // Bootstrap — the Tauri shell's setup() order, verbatim:
    // AppState::new → Ctx::new(state, sink) → init_paths(dir) →
    // run_startup_sweeps → check_ext_notice → the background
    // entry points. The data dir is the Tauri app's own, so a
    // profile is shared, never forked.
    let state = Arc::new(AppState::new());
    let ctx = Ctx::new(state.clone(), sink);
    let dir = app_data_dir();
    match state.init_paths(&dir) {
        Ok(()) => {
            sink::run_startup_sweeps(&state);
            sink::check_ext_notice(&ctx, &state);
            sink::run_launch_update_check(ctx.clone());
            sink::run_update_recheck_loop(ctx.clone());
            sink::run_component_warmup(ctx.clone());
            sink::startup_resume(&ctx);
            sink::run_scheduler_loop(ctx.clone());
            sink::run_clipboard_watcher(ctx.clone());
            sink::run_snapshot_loop(ctx.clone());
            sink::run_incoming_loop(ctx.clone(), dir);
        }
        Err(e) => {
            // No journal, no honest app: say so on stderr and
            // still show the window with the failure in the
            // status bar rather than running against lost state.
            eprintln!("[ndm-shell-gtk] init_paths({}) failed: {}", dir.display(), e);
            if let Ok(mut slot) = INIT_PATHS_ERROR.lock() {
                *slot = Some(format!(
                    "NDM could not open its data folder ({}): {} — downloads are shown from memory only and nothing will be saved.",
                    dir.display(),
                    e
                ));
            }
        }
    }

    let app = gtk::Application::builder()
        .application_id("tech.zahurul.ndm")
        .build();
    // The native host cold-starts the app with --background
    // (browser capture while NDM is closed). GApplication runs
    // argv through GOption inside run() and exits on any option
    // it does not know, so the flag must be registered here —
    // without this, a cold-start spawn died with "Unknown option
    // --background" before activate ever fired and the queued
    // download sat untouched until the user opened the app by
    // hand (field failure, 4 Nov 2026). build_ui still reads
    // the flag from env::args; registration only admits it.
    app.add_main_option(
        "background",
        gtk::glib::Char(0),
        gtk::glib::OptionFlags::NONE,
        gtk::glib::OptionArg::None,
        "Start hidden; the browser capture queue is drained in the background",
        None,
    );

    let receiver = gtksink::ReceiverSlot::new(receiver);
    // GtkApplication is single-instance: launching NDM again
    // forwards an activate to THIS process. Building the UI a
    // second time produced a dead twin window — its ReceiverSlot
    // take() had already been spent on the first window, so it
    // never received a core event, and closing it quit the whole
    // app (audit-233). Build once; later activations just raise
    // the existing window.
    let built = std::cell::Cell::new(false);
    app.connect_activate(move |app| {
        if built.get() {
            if let Some(w) = app.windows().first() {
                w.present();
            }
            return;
        }
        built.set(true);
        build_ui(app, &ctx, &receiver);
    });
    app.run();
}
