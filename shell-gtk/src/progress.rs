//! Per-download progress dialog — PROGRESS-DIALOG-SPEC (LOCKED),
//! the native GTK window: one GtkWindow per entry, reopening
//! raises the existing one. Tab 1 "Download status" (six fields
//! in spec order, plain bar, Resume/Pause/Cancel per the §1.4
//! matrix, collapsible details: G5 strip + connections table +
//! the NDM properties group below it), Tab 2 "Speed Limiter"
//! (§2.2/G9: KB/s integer, apply-on-change via set_entry_limit),
//! Tab 3 "Options on completion" (§2.3/G2: independent checkboxes
//! via set_completion_actions + set_show_complete_override; no
//! hang-up-modem — deliberate omission).
//!
//! Live data arrives through `update`, called by the main
//! event dispatcher for every AppEvent::Progress payload; a
//! dialog applies only payloads for its own entry id. Per-event
//! cost: text sets only for changed values, the connections
//! store is rebuilt only when its rows change, and the strip
//! repaints only when blocks/conns change — no full rebuilds.
//!
//! Mirrors the Tauri dialog's established semantics where the
//! spec leaves the shape to the existing code (app.js
//! renderDetail / syncLimiterPanel / syncCompletionPanel):
//! video detection, the finished-entry "Done" Info rows (N1),
//! and the show-complete tri-state resolution.

use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::sink::{self, Ctx, ProgressView};
use crate::{fmt_rate, fmt_size, fmt_size_f};

/// Settings snapshot a dialog needs at open time.
pub struct OpenInfo {
    pub queues: Vec<String>,
    pub global_limit: u64,
    pub global_show_complete: bool,
}

// One dialog per entry id (spec §1.3). Main thread only.
thread_local! {
    static REGISTRY: RefCell<HashMap<String, Rc<ProgDlg>>> = RefCell::new(HashMap::new());
}

#[derive(Default, Clone, PartialEq)]
struct StripData {
    blocks: String,
    finished: bool,
    total: u64,
    conns_state: Vec<u8>,
    conns_partial: Vec<u64>,
    conns_block: Vec<i64>,
}

/// Last-applied values; every widget write is gated on these so
/// a 200 ms tick that changes nothing writes nothing.
#[derive(Default)]
struct Cache {
    title: String,
    status: String,
    size: String,
    downloaded: String,
    rate: String,
    left: String,
    resume_cap: String,
    fraction: f64,
    buttons: (bool, bool, bool),
    strip: StripData,
    strip_h: i32,
    rows: Vec<[String; 3]>,
    video: Option<bool>,
    save_to: String,
    referer: String,
    desc: String,
    queue: String,
    limit_bps: u64,
    completion_key: String,
}

struct ProgDlg {
    id: String,
    ctx: Ctx,
    global_limit: u64,
    global_show_complete: bool,
    window: gtk::Window,
    f_status: gtk::Label,
    f_size: gtk::Label,
    f_done: gtk::Label,
    f_rate: gtk::Label,
    f_left: gtk::Label,
    f_resume: gtk::Label,
    bar: gtk::ProgressBar,
    btn_resume: gtk::Button,
    btn_pause: gtk::Button,
    btn_cancel: gtk::Button,
    details: gtk::Box,
    caption: gtk::Label,
    strip: gtk::DrawingArea,
    strip_data: Rc<RefCell<StripData>>,
    table_scroll: gtk::ScrolledWindow,
    table_store: gtk::ListStore,
    save_to: gtk::Label,
    referer: gtk::Label,
    desc: gtk::Entry,
    queue: gtk::ComboBoxText,
    lim_on: gtk::CheckButton,
    lim_val: gtk::Entry,
    lim_note: gtk::Label,
    c_show: gtk::CheckButton,
    c_open_file: gtk::CheckButton,
    c_open_folder: gtk::CheckButton,
    c_exit: gtk::CheckButton,
    c_power: gtk::CheckButton,
    power_choice: gtk::ComboBoxText,
    cache: RefCell<Cache>,
    /// The last payload applied (handlers read the entry's real
    /// action set / limit from it, as app.js reads its items map).
    last: RefCell<ProgressView>,
    syncing: Cell<bool>,
    desc_editing: Cell<bool>,
    lim_editing: Cell<bool>,
    lim_dirty: Cell<bool>,
}

fn is_video(d: &ProgressView) -> bool {
    d.resume_capability == "Yes (yt-dlp resume)"
}

/// §3 status-text table; error shows the error text itself,
/// starting shows core's truthful sub-status when it carries one.
fn dialog_status_text(d: &ProgressView) -> String {
    match d.status.as_str() {
        "queued" => "Queued".to_string(),
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
        "downloading" => "Receiving data…".to_string(),
        "merging" => "Merging…".to_string(),
        "paused" => "Paused".to_string(),
        "finished" => "Finished".to_string(),
        "error" => {
            if d.error.is_empty() {
                "Error".to_string()
            } else {
                sink::display_error(&d.error)
            }
        }
        "cancelled" => "Cancelled".to_string(),
        s => {
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

/// §1.1 title: floor percent (integer division floors), one
/// space, filename; filename alone while total is unknown and
/// nothing is downloaded. Applied only when the string changes.
fn title_for(d: &ProgressView) -> String {
    if d.status == "finished" {
        return format!("100% {}", d.name);
    }
    if d.total > 0 {
        format!("{}% {}", (d.done_bytes * 100 / d.total).min(100), d.name)
    } else {
        d.name.clone()
    }
}

/// §3 time-left shape: the list formatter with a zero seconds
/// part dropped ("17 min", not "17 min 0 sec").
fn fmt_left_dialog(secs: f64) -> String {
    let sec = secs.max(0.0).round() as u64;
    if sec < 60 {
        return format!("{} sec", sec);
    }
    let m = sec / 60;
    let s = sec % 60;
    if m < 60 {
        if s == 0 {
            format!("{} min", m)
        } else {
            format!("{} min {} sec", m, s)
        }
    } else {
        format!("{} hour(s) {} min", m / 60, m % 60)
    }
}

/// §1.4 enablement matrix: (Resume, Pause, Cancel).
fn enablement(status: &str) -> (bool, bool, bool) {
    match status {
        "downloading" => (false, true, true),
        "merging" => (false, false, false),
        // Pause stays live while the job is still resolving
        // ("starting") or recovering from a stall ("resuming"):
        // core's pause parks the job in both phases (the stop
        // flag discards the in-flight resolve), and a greyed
        // Pause here read as "pause is broken" in the field
        // (2.3.1 bug 4).
        "starting" | "resuming" => (false, true, true),
        "queued" | "paused" => (true, false, true),
        // Error/cancelled: nothing is running to cancel, but
        // Cancel must still dismiss the dialog — greying it left
        // the window X as the only way out, the same trap the
        // finished arm below was fixed for (audit-233 B-1).
        "error" | "cancelled" => (true, false, true),
        // Finished: nothing left to pause or resume, but Cancel
        // must still dismiss the dialog — greying it left the
        // window X as the only way out (2.3.1 gate finding).
        "finished" => (false, false, true),
        _ => (false, false, false),
    }
}

/// §2.2 prefill, KB/s: the entry's limit, else the global, else 512.
fn limiter_prefill_kb(d: &ProgressView, global_limit: u64) -> u64 {
    let bps = if d.speed_limit > 0 {
        d.speed_limit
    } else if global_limit > 0 {
        global_limit
    } else {
        512 * 1024
    };
    (bps / 1024).max(1)
}

fn info_text(state: u8) -> &'static str {
    match state {
        1 => "Send GET…",
        2 => "Receiving data…",
        _ => "Waiting…",
    }
}

/// G5 strip paint: contiguous cells start→end, wrapping to a
/// second row only above 512 cells; '1' full fill, '2'
/// fractional fill from the owning connection's partial bytes,
/// '0' empty track; a start tick under each active connection's
/// current block. Same calm palette as the list's block map.
fn paint_strip(cr: &gtk::cairo::Context, w: f64, data: &StripData) {
    if w <= 0.0 {
        return;
    }
    let blocks = &data.blocks;
    let n = if blocks.is_empty() {
        if data.finished {
            128
        } else {
            return;
        }
    } else {
        blocks.chars().count().min(1024)
    };
    let rows = if n > 512 { 2 } else { 1 };
    let cols = (n + rows - 1) / rows;
    let cw = w / cols as f64;
    let ch = 16.0;
    let gap = (cw * 0.18).min(2.0);
    let cell_bytes = if data.total > 0 {
        (data.total as f64 / n as f64).max(1.0)
    } else {
        1024.0 * 1024.0
    };
    let frac_for = |cell: usize| -> f64 {
        for (i, b) in data.conns_block.iter().enumerate() {
            if *b == cell as i64 {
                let p = data.conns_partial.get(i).copied().unwrap_or(0);
                return if p > 0 {
                    (p as f64 / cell_bytes).min(1.0)
                } else {
                    0.0
                };
            }
        }
        0.0
    };
    let chars: Vec<char> = blocks.chars().collect();
    for i in 0..n {
        let x = (i % cols) as f64 * cw;
        let y = (i / cols) as f64 * (ch + 2.0);
        let c = if data.finished {
            '1'
        } else {
            chars.get(i).copied().unwrap_or('0')
        };
        // empty track first, then the fill over it
        cr.set_source_rgb(0.85, 0.89, 0.93);
        cr.rectangle(x + gap / 2.0, y, (cw - gap).max(1.0), ch - gap);
        let _ = cr.fill();
        match c {
            '1' => {
                cr.set_source_rgb(0.18, 0.62, 0.31);
                cr.rectangle(x + gap / 2.0, y, (cw - gap).max(1.0), ch - gap);
                let _ = cr.fill();
            }
            '2' => {
                let f = frac_for(i).max(0.08);
                cr.set_source_rgb(0.14, 0.34, 0.65);
                cr.rectangle(
                    x + gap / 2.0,
                    y,
                    ((cw - gap).max(1.0) * f).max(1.0),
                    ch - gap,
                );
                let _ = cr.fill();
            }
            _ => {}
        }
    }
    // start ticks under active connections' current blocks
    cr.set_source_rgb(0.18, 0.62, 0.31);
    for (i, b) in data.conns_block.iter().enumerate() {
        let b = *b;
        if b < 0 || b as usize >= n {
            continue;
        }
        let partial = data.conns_partial.get(i).copied().unwrap_or(0);
        if partial == 0 && data.conns_state.get(i).copied().unwrap_or(0) != 2 {
            continue;
        }
        let b = b as usize;
        let x = (b % cols) as f64 * cw + cw / 2.0;
        let y = (b / cols + 1) as f64 * (ch + 2.0) - 4.0;
        cr.move_to(x, y);
        cr.line_to(x - 3.0, y + 3.0);
        cr.line_to(x + 3.0, y + 3.0);
        cr.close_path();
        let _ = cr.fill();
    }
}

fn set_if_changed(label: &gtk::Label, cache: &mut String, value: String) {
    if *cache != value {
        label.set_text(&value);
        *cache = value;
    }
}

impl ProgDlg {
    fn apply(&self, d: &ProgressView) {
        *self.last.borrow_mut() = d.clone();
        let mut c = self.cache.borrow_mut();

        let title = title_for(d);
        if c.title != title {
            self.window.set_title(&title);
            c.title = title;
        }
        set_if_changed(&self.f_status, &mut c.status, dialog_status_text(d));
        set_if_changed(
            &self.f_size,
            &mut c.size,
            if d.total > 0 || d.status == "finished" {
                fmt_size(d.total)
            } else {
                "Unknown".to_string()
            },
        );
        set_if_changed(
            &self.f_done,
            &mut c.downloaded,
            if d.total > 0 {
                format!(
                    "{} ({:.2}%)",
                    fmt_size(d.done_bytes),
                    (100.0 * d.done_bytes as f64 / d.total as f64).min(100.0)
                )
            } else {
                format!("{} (size unknown)", fmt_size(d.done_bytes))
            },
        );
        set_if_changed(
            &self.f_rate,
            &mut c.rate,
            if d.status == "downloading" && d.speed_bps > 0.0 {
                format!("{}/sec", fmt_size_f(d.speed_bps))
            } else {
                "—".to_string()
            },
        );
        set_if_changed(
            &self.f_left,
            &mut c.left,
            if d.status == "downloading" && d.total > 0 && d.speed_bps > 0.0 {
                // saturating: a plain-download payload can report
                // done > total (core does not clamp it there); a
                // bare subtraction wraps in release and panics in
                // debug builds.
                fmt_left_dialog(d.total.saturating_sub(d.done_bytes) as f64 / d.speed_bps)
            } else {
                "—".to_string()
            },
        );
        set_if_changed(
            &self.f_resume,
            &mut c.resume_cap,
            if is_video(d) || d.resume_capability.starts_with("Yes") {
                "Yes".to_string()
            } else if d.resume_capability.is_empty() {
                "—".to_string()
            } else {
                "No".to_string()
            },
        );

        // plain bar, no % text; indeterminate while total unknown
        if d.status == "downloading" && d.total == 0 {
            self.bar.pulse();
        } else {
            let frac = if d.total > 0 {
                (d.done_bytes as f64 / d.total as f64).clamp(0.0, 1.0)
            } else if d.status == "finished" {
                1.0
            } else {
                0.0
            };
            if (c.fraction - frac).abs() > f64::EPSILON {
                self.bar.set_fraction(frac);
                c.fraction = frac;
            }
        }

        let en = enablement(&d.status);
        if c.buttons != en {
            self.btn_resume.set_sensitive(en.0);
            self.btn_pause.set_sensitive(en.1);
            self.btn_cancel.set_sensitive(en.2);
            c.buttons = en;
        }

        // G8: video — caption drops "by connections", table hidden
        let video = is_video(d);
        if c.video != Some(video) {
            self.caption.set_text(if video {
                "Download progress"
            } else {
                "Start positions and download progress by connections"
            });
            self.table_scroll.set_visible(!video);
            c.video = Some(video);
        }

        // strip: repaint only when its data actually changed
        let strip = StripData {
            blocks: d.blocks.clone(),
            finished: d.status == "finished",
            total: d.total,
            conns_state: d.conns_state.clone(),
            conns_partial: d.conns_partial.clone(),
            conns_block: d.conns_block.clone(),
        };
        if c.strip != strip {
            let h = if strip.blocks.chars().count() > 512 {
                42
            } else {
                24
            };
            if c.strip_h != h {
                self.strip.set_size_request(-1, h);
                c.strip_h = h;
            }
            *self.strip_data.borrow_mut() = strip.clone();
            c.strip = strip;
            self.strip.queue_draw();
        }

        // connections table (§3 filter; finished rows read "Done" — N1)
        if !video {
            let rows: Vec<[String; 3]> = d
                .conns_detail
                .iter()
                .enumerate()
                .filter(|(i, b)| {
                    **b > 0 || (d.status == "downloading" && *i < d.conns)
                })
                .map(|(i, b)| {
                    [
                        format!("{}", i + 1),
                        fmt_size(*b),
                        // row Info follows the ENTRY's state, not
                        // the connection's last live code: finished
                        // rows read "Done" (N1) and a paused entry's
                        // rows must not claim "Receiving data…" (m3)
                        if d.status == "finished" {
                            "Done".to_string()
                        } else if d.status == "paused" {
                            "Paused".to_string()
                        } else {
                            info_text(d.conns_state.get(i).copied().unwrap_or(0)).to_string()
                        },
                    ]
                })
                .collect();
            if c.rows != rows {
                self.table_store.clear();
                for r in &rows {
                    let iter = self.table_store.append();
                    self.table_store.set(
                        &iter,
                        &[
                            (0, &r[0] as &dyn gtk::glib::ToValue),
                            (1, &r[1] as &dyn gtk::glib::ToValue),
                            (2, &r[2] as &dyn gtk::glib::ToValue),
                        ],
                    );
                }
                // m1: the viewport grows with the row count (up
                // to ten rows) instead of pinning every table at
                // three visible rows — the rest used to hide
                // behind a scrollbar nobody thinks to scroll, so
                // the rows never summed to the dialog's total
                let h = (30 + rows.len() as i32 * 26).clamp(104, 290);
                self.table_scroll.set_size_request(-1, h);
                c.rows = rows;
            }
        }

        // properties group (below the table, §1.1 deviation ruling)
        set_if_changed(
            &self.save_to,
            &mut c.save_to,
            if d.save_to.is_empty() {
                "—".to_string()
            } else {
                d.save_to.clone()
            },
        );
        set_if_changed(
            &self.referer,
            &mut c.referer,
            if d.referer.is_empty() {
                "—".to_string()
            } else {
                d.referer.clone()
            },
        );
        if !self.desc_editing.get() && c.desc != d.description {
            self.desc.set_text(&d.description);
            c.desc = d.description.clone();
        }
        if c.queue != d.queue {
            self.syncing.set(true);
            set_combo_text(&self.queue, &d.queue);
            self.syncing.set(false);
            c.queue = d.queue.clone();
        }

        self.sync_limiter(d, &mut c);
        self.sync_completion(d, &mut c);
    }

    /// §2.2/G9 sync: never rewrite the field while the user is
    /// editing it or has an un-applied change in flight.
    fn sync_limiter(&self, d: &ProgressView, c: &mut Cache) {
        if self.global_limit > 0 {
            let note = format!(
                "A global limit of {} also applies (Options) — the lower of the two wins.",
                fmt_rate(self.global_limit as f64)
            );
            if !self.lim_note.is_visible() || self.lim_note.text() != note {
                self.lim_note.set_text(&note);
                self.lim_note.set_visible(true);
            }
        }
        if self.lim_dirty.get() || self.lim_editing.get() {
            c.limit_bps = d.speed_limit;
            return;
        }
        if c.limit_bps != d.speed_limit || self.lim_on.is_active() != (d.speed_limit > 0) {
            self.syncing.set(true);
            self.lim_on.set_active(d.speed_limit > 0);
            self.lim_val
                .set_text(&limiter_prefill_kb(d, self.global_limit).to_string());
            self.lim_val.set_sensitive(d.speed_limit > 0);
            self.syncing.set(false);
            c.limit_bps = d.speed_limit;
        }
    }

    fn apply_limit(&self) {
        self.lim_dirty.set(false);
        let on = self.lim_on.is_active();
        self.lim_val.set_sensitive(on);
        // A non-numeric value used to fall back to 512 silently —
        // the user typed one number and another took effect
        // (audit-233). Refuse it in words and keep the live limit.
        let raw = self.lim_val.text().trim().to_string();
        // An empty field takes the same refusal path: it used to
        // fall back to 512 silently, exactly the substitution the
        // non-numeric branch below exists to prevent.
        let kb: u64 = {
            match raw.parse::<u64>() {
                Ok(v) => v.max(1),
                Err(_) => {
                    self.lim_note.set_text(
                        "That speed could not be read as a number — the current limit still applies.",
                    );
                    // Put the live value back in the field so the
                    // state is evident even after the next sync
                    // rewrites the note (global-limit message).
                    self.lim_val.set_text(
                        &limiter_prefill_kb(&self.last.borrow(), self.global_limit).to_string(),
                    );
                    return;
                }
            }
        };
        self.lim_note.set_text("");
        let bps = if on { kb * 1024 } else { 0 };
        if bps == self.last.borrow().speed_limit {
            return;
        }
        if sink::set_entry_limit(self.ctx.clone(), self.id.clone(), bps).is_ok() {
            self.last.borrow_mut().speed_limit = bps;
            self.cache.borrow_mut().limit_bps = bps;
        }
    }

    /// §2.3 sync: checkboxes follow the entry's real action set
    /// and the show-complete tri-state (override ?? set membership
    /// ?? global), exactly core's completion_show_dialog order.
    fn sync_completion(&self, d: &ProgressView, c: &mut Cache) {
        let key = format!(
            "{:?}|{:?}",
            d.completion_actions, d.show_complete_override
        );
        if c.completion_key == key {
            return;
        }
        c.completion_key = key;
        let has = |a: &str| d.completion_actions.iter().any(|x| x == a);
        let show = match d.show_complete_override {
            Some(o) => o,
            None => has("show_complete") || self.global_show_complete,
        };
        self.syncing.set(true);
        self.c_show.set_active(show);
        self.c_open_file.set_active(has("open_file"));
        self.c_open_folder.set_active(has("open_folder"));
        self.c_exit.set_active(has("exit_app"));
        let power = if has("restart_pc") {
            Some(1)
        } else if has("turn_off_pc") {
            Some(0)
        } else {
            None
        };
        self.c_power.set_active(power.is_some());
        self.power_choice.set_active(Some(power.unwrap_or(0)));
        self.power_choice.set_sensitive(power.is_some());
        self.syncing.set(false);
    }

    /// One completion checkbox changed: rewrite the whole set
    /// from the last payload's set with only that slot changed
    /// (app.js toggleCompletionAction's shape). The show-complete
    /// box is NOT part of the set — it writes the override.
    fn push_completion(&self, mutate: impl Fn(&mut Vec<String>)) {
        let mut actions = self.last.borrow().completion_actions.clone();
        mutate(&mut actions);
        if sink::set_completion_actions(self.ctx.clone(), self.id.clone(), actions.clone())
            .is_ok()
        {
            self.last.borrow_mut().completion_actions = actions;
            self.cache.borrow_mut().completion_key.clear();
        }
    }
}

fn set_combo_text(combo: &gtk::ComboBoxText, text: &str) {
    if combo.active_text().map(|t| t.to_string()).as_deref() == Some(text) {
        return;
    }
    if let Some(model) = combo.model() {
        let mut i = 0;
        while let Some(iter) = model.iter_nth_child(None, i) {
            let v: String = model.value(&iter, 0).get().unwrap_or_default();
            if v == text {
                combo.set_active(Some(i as u32));
                return;
            }
            i += 1;
        }
    }
    combo.set_active(Some(0));
}

fn label_row(grid: &gtk::Grid, row: i32, name: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(name));
    l.set_xalign(0.0);
    let v = gtk::Label::new(None);
    v.set_xalign(0.0);
    v.set_hexpand(true);
    // Values wrap instead of clipping: the Status row carries
    // the full error text, and a long one used to run off the
    // dialog's edge unreadable (QA-220 m4).
    v.set_line_wrap(true);
    v.set_max_width_chars(72);
    grid.attach(&l, 0, row, 1, 1);
    grid.attach(&v, 1, row, 1, 1);
    v
}

thread_local! {
    /// Entries whose progress dialog the user has closed.
    /// A background reveal (a retry, a queue promotion, any
    /// later start of the same entry) must not re-open or yank
    /// the dialog forward after that — QA-220 m3: dialogs
    /// re-surfaced on every transition and stacked in front,
    /// seizing the work area. An explicit open (double-click,
    /// "Show progress") clears the mark.
    static USER_CLOSED: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
}

/// Open the dialog for `view`'s entry, or raise the existing one.
/// G7: with `minimized` set (the reveal payload's view), the
/// dialog is created with its full live state but lowered —
/// iconified instead of presented. `explicit` marks a user's
/// own open (double-click / "Show progress"): it always
/// presents and clears the user-closed mark. A non-explicit
/// reveal for a user-closed entry does nothing at all, and
/// for an already-open dialog it only refreshes the contents
/// — never yanks the window forward.
pub fn open_or_raise(
    parent: &gtk::ApplicationWindow,
    ctx: Ctx,
    view: &ProgressView,
    info: OpenInfo,
    minimized: bool,
    explicit: bool,
) {
    if explicit {
        USER_CLOSED.with(|s| s.borrow_mut().remove(&view.id));
    } else if USER_CLOSED.with(|s| s.borrow().contains(&view.id)) {
        return;
    }
    let existing = REGISTRY.with(|r| r.borrow().get(&view.id).cloned());
    if let Some(dlg) = existing {
        dlg.apply(view);
        if minimized {
            lower(&dlg);
        } else if explicit {
            dlg.window.set_transient_for(Some(parent));
            dlg.window.present();
        }
        return;
    }
    let dlg = build(parent, ctx, view, info, !minimized);
    REGISTRY.with(|r| r.borrow_mut().insert(view.id.clone(), dlg.clone()));
    dlg.apply(view);
    dlg.window.show_all();
    // show_all revives widgets apply() may have hidden (video table)
    dlg.cache.borrow_mut().video = None;
    let view2 = dlg.last.borrow().clone();
    dlg.apply(&view2);
    if minimized {
        lower(&dlg);
    } else {
        dlg.window.present();
    }
}

/// Lower a dialog into the taskbar (G7 "minimized"). A window
/// that is transient for the main window cannot be minimized
/// on its own — window managers minimize the family together
/// (or refuse outright) — so the lowered dialog drops its
/// transient link; the next raise restores it (open_or_raise).
fn lower(dlg: &ProgDlg) {
    dlg.window.set_transient_for(Option::<&gtk::Window>::None);
    dlg.window.iconify();
}

/// Route one live payload to its dialog, if one is open.
pub fn update(view: &ProgressView) {
    let dlg = REGISTRY.with(|r| r.borrow().get(&view.id).cloned());
    if let Some(dlg) = dlg {
        dlg.apply(view);
    }
}

/// Close and forget the dialog for `id` (entry removed / Cancel).
/// `window.close()` runs the delete-event handler, which drops
/// the registry entry itself.
pub fn close(id: &str) {
    let dlg = REGISTRY.with(|r| r.borrow().get(id).cloned());
    if let Some(dlg) = dlg {
        dlg.window.close();
    }
}

fn build(
    parent: &gtk::ApplicationWindow,
    ctx: Ctx,
    view: &ProgressView,
    info: OpenInfo,
    transient: bool,
) -> Rc<ProgDlg> {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    // A dialog that opens minimized carries no transient link
    // from the start (see lower()): the link is a map-time
    // property for the window manager, so it must be decided
    // here, not patched after the window appears.
    if transient {
        window.set_transient_for(Some(parent));
        window.set_destroy_with_parent(true);
    }
    window.set_title(&view.name);
    window.set_default_size(620, -1);

    // ---- Tab 1: Download status ----
    let url = gtk::Label::new(Some(&view.url));
    url.set_xalign(0.0);
    url.set_ellipsize(gtk::pango::EllipsizeMode::End);
    url.set_tooltip_text(Some(&view.url));
    url.set_selectable(true);

    let grid = gtk::Grid::builder()
        .row_spacing(4)
        .column_spacing(12)
        .build();
    let f_status = label_row(&grid, 0, "Status");
    let f_size = label_row(&grid, 1, "File size");
    let f_done = label_row(&grid, 2, "Downloaded");
    let f_rate = label_row(&grid, 3, "Transfer rate");
    let f_left = label_row(&grid, 4, "Time left");
    let f_resume = label_row(&grid, 5, "Resume capability");

    let bar = gtk::ProgressBar::new();
    bar.set_pulse_step(0.08);

    let btn_details = gtk::Button::with_label("<< Hide details");
    let btn_resume = gtk::Button::with_label("Resume");
    let btn_pause = gtk::Button::with_label("Pause");
    let btn_cancel = gtk::Button::with_label("Cancel");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.pack_start(&btn_details, false, false, 0);
    buttons.pack_end(&btn_cancel, false, false, 0);
    buttons.pack_end(&btn_pause, false, false, 0);
    buttons.pack_end(&btn_resume, false, false, 0);

    // details region (collapsible, default expanded)
    let caption = gtk::Label::new(Some(
        "Start positions and download progress by connections",
    ));
    caption.set_xalign(0.0);
    let strip_data = Rc::new(RefCell::new(StripData::default()));
    let strip = gtk::DrawingArea::new();
    strip.set_size_request(-1, 24);
    {
        let data = strip_data.clone();
        strip.connect_draw(move |area, cr| {
            let w = area.allocated_width() as f64;
            paint_strip(cr, w, &data.borrow());
            gtk::glib::Propagation::Proceed
        });
    }
    let table_store = gtk::ListStore::new(&[
        gtk::glib::types::Type::STRING,
        gtk::glib::types::Type::STRING,
        gtk::glib::types::Type::STRING,
    ]);
    let table = gtk::TreeView::with_model(&table_store);
    table.set_headers_visible(true);
    for (title, col) in [("N.", 0), ("Downloaded", 1), ("Info", 2)] {
        let column = gtk::TreeViewColumn::new();
        column.set_title(title);
        let cell = gtk::CellRendererText::new();
        gtk::prelude::CellLayoutExt::pack_start(&column, &cell, true);
        gtk::prelude::CellLayoutExt::add_attribute(&column, &cell, "text", col);
        table.append_column(&column);
    }
    let table_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .shadow_type(gtk::ShadowType::In)
        .build();
    table_scroll.add(&table);
    table_scroll.set_size_request(-1, 104);

    // NDM properties group — BELOW the table (§1.1 deviation ruling)
    let props = gtk::Grid::builder()
        .row_spacing(4)
        .column_spacing(12)
        .build();
    let save_to = label_row(&props, 0, "Save to");
    save_to.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let referer = label_row(&props, 1, "Referer");
    referer.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let desc_label = gtk::Label::new(Some("Description"));
    desc_label.set_xalign(0.0);
    let desc = gtk::Entry::new();
    desc.set_hexpand(true);
    props.attach(&desc_label, 0, 2, 1, 1);
    props.attach(&desc, 1, 2, 1, 1);
    let queue_label = gtk::Label::new(Some("Queue"));
    queue_label.set_xalign(0.0);
    let queue = gtk::ComboBoxText::new();
    for q in &info.queues {
        queue.append_text(q);
    }
    props.attach(&queue_label, 0, 3, 1, 1);
    props.attach(&queue, 1, 3, 1, 1);

    let details = gtk::Box::new(gtk::Orientation::Vertical, 6);
    details.pack_start(&caption, false, false, 0);
    details.pack_start(&strip, false, false, 0);
    details.pack_start(&table_scroll, false, false, 0);
    details.pack_start(&gtk::Separator::new(gtk::Orientation::Horizontal), false, false, 0);
    details.pack_start(&props, false, false, 0);

    let tab1 = gtk::Box::new(gtk::Orientation::Vertical, 6);
    tab1.set_margin_start(10);
    tab1.set_margin_end(10);
    tab1.set_margin_top(10);
    tab1.set_margin_bottom(10);
    tab1.pack_start(&url, false, false, 0);
    tab1.pack_start(&grid, false, false, 0);
    tab1.pack_start(&bar, false, false, 0);
    tab1.pack_start(&buttons, false, false, 0);
    tab1.pack_start(&details, false, false, 0);

    // ---- Tab 2: Speed Limiter ----
    let lim_on = gtk::CheckButton::with_label("Use Speed Limiter for this download");
    let lim_val = gtk::Entry::new();
    lim_val.set_width_chars(8);
    lim_val.set_input_purpose(gtk::InputPurpose::Digits);
    let lim_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    lim_row.pack_start(&gtk::Label::new(Some("Maximum download speed:")), false, false, 0);
    lim_row.pack_start(&lim_val, false, false, 0);
    lim_row.pack_start(&gtk::Label::new(Some("KB/s")), false, false, 0);
    let lim_note = gtk::Label::new(None);
    lim_note.set_xalign(0.0);
    lim_note.set_line_wrap(true);
    let tab2 = gtk::Box::new(gtk::Orientation::Vertical, 6);
    tab2.set_margin_start(10);
    tab2.set_margin_end(10);
    tab2.set_margin_top(10);
    tab2.set_margin_bottom(10);
    tab2.pack_start(&lim_on, false, false, 0);
    tab2.pack_start(&lim_row, false, false, 0);
    tab2.pack_start(&lim_note, false, false, 0);
    // §2.2 prefill at open (sync_limiter keeps it current after)
    lim_on.set_active(view.speed_limit > 0);
    lim_val.set_text(&limiter_prefill_kb(view, info.global_limit).to_string());
    lim_val.set_sensitive(view.speed_limit > 0);
    if info.global_limit > 0 {
        lim_note.set_text(&format!(
            "A global limit of {} also applies (Options) — the lower of the two wins.",
            fmt_rate(info.global_limit as f64)
        ));
    }

    // ---- Tab 3: Options on completion ----
    let c_show = gtk::CheckButton::with_label("Show \"Download complete\" dialog");
    let c_open_file = gtk::CheckButton::with_label("Open the file when done");
    let c_open_folder = gtk::CheckButton::with_label("Open the folder when done");
    let c_exit = gtk::CheckButton::with_label("Exit NDM when done");
    let c_power = gtk::CheckButton::with_label("Turn off the computer when done");
    let power_choice = gtk::ComboBoxText::new();
    power_choice.append_text("Shut down");
    power_choice.append_text("Restart");
    power_choice.set_active(Some(0));
    let power_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    power_row.pack_start(&c_power, false, false, 0);
    power_row.pack_start(&power_choice, false, false, 0);
    let tab3 = gtk::Box::new(gtk::Orientation::Vertical, 6);
    tab3.set_margin_start(10);
    tab3.set_margin_end(10);
    tab3.set_margin_top(10);
    tab3.set_margin_bottom(10);
    tab3.pack_start(&c_show, false, false, 0);
    tab3.pack_start(&c_open_file, false, false, 0);
    tab3.pack_start(&c_open_folder, false, false, 0);
    tab3.pack_start(&c_exit, false, false, 0);
    tab3.pack_start(&power_row, false, false, 0);

    let notebook = gtk::Notebook::new();
    notebook.append_page(&tab1, Some(&gtk::Label::new(Some("Download status"))));
    notebook.append_page(&tab2, Some(&gtk::Label::new(Some("Speed Limiter"))));
    notebook.append_page(
        &tab3,
        Some(&gtk::Label::new(Some("Options on completion"))),
    );
    window.add(&notebook);

    let dlg = Rc::new(ProgDlg {
        id: view.id.clone(),
        ctx,
        global_limit: info.global_limit,
        global_show_complete: info.global_show_complete,
        window: window.clone(),
        f_status,
        f_size,
        f_done,
        f_rate,
        f_left,
        f_resume,
        bar,
        btn_resume: btn_resume.clone(),
        btn_pause: btn_pause.clone(),
        btn_cancel: btn_cancel.clone(),
        details: details.clone(),
        caption,
        strip,
        strip_data,
        table_scroll,
        table_store,
        save_to,
        referer,
        desc: desc.clone(),
        queue: queue.clone(),
        lim_on: lim_on.clone(),
        lim_val: lim_val.clone(),
        lim_note,
        c_show: c_show.clone(),
        c_open_file: c_open_file.clone(),
        c_open_folder: c_open_folder.clone(),
        c_exit: c_exit.clone(),
        c_power: c_power.clone(),
        power_choice: power_choice.clone(),
        cache: RefCell::new(Cache::default()),
        last: RefCell::new(view.clone()),
        syncing: Cell::new(false),
        desc_editing: Cell::new(false),
        lim_editing: Cell::new(false),
        lim_dirty: Cell::new(false),
    });

    // ---- handlers ----
    {
        let d = dlg.clone();
        window.connect_delete_event(move |_, _| {
            REGISTRY.with(|r| r.borrow_mut().remove(&d.id));
            // however it closed — X, Cancel, the entry leaving —
            // a background reveal must not resurrect it (m3)
            USER_CLOSED.with(|s| s.borrow_mut().insert(d.id.clone()));
            gtk::glib::Propagation::Proceed
        });
    }
    {
        let d = dlg.clone();
        btn_resume.connect_clicked(move |_| {
            // a resume is a download start — it can probe, so it
            // runs on a worker, never on the UI thread
            let (ctx, id) = (d.ctx.clone(), d.id.clone());
            std::thread::spawn(move || {
                let _ = sink::rt::block_on(sink::resume_download(ctx, id));
            });
        });
    }
    {
        let d = dlg.clone();
        btn_pause.connect_clicked(move |_| {
            let _ = sink::pause_download(d.ctx.clone(), d.id.clone());
        });
    }
    {
        // G3: Cancel = core pause + close; entry + partial remain.
        // A finished entry has nothing to pause — there Cancel is
        // a pure close (its enablement is finished-only, above).
        let d = dlg.clone();
        btn_cancel.connect_clicked(move |_| {
            if d.last.borrow().status != "finished" {
                let _ = sink::pause_download(d.ctx.clone(), d.id.clone());
            }
            close(&d.id);
        });
    }
    {
        let d = dlg.clone();
        btn_details.connect_clicked(move |btn| {
            let show = !d.details.is_visible();
            d.details.set_visible(show);
            btn.set_label(if show {
                "<< Hide details"
            } else {
                "Show details >>"
            });
            let w = d.window.allocated_width();
            d.window.resize(w.max(620), 1);
        });
    }
    {
        let d = dlg.clone();
        desc.connect_focus_in_event(move |_, _| {
            d.desc_editing.set(true);
            gtk::glib::Propagation::Proceed
        });
    }
    {
        let d = dlg.clone();
        desc.connect_focus_out_event(move |e, _| {
            d.desc_editing.set(false);
            commit_description(&d, &e.text());
            gtk::glib::Propagation::Proceed
        });
    }
    {
        let d = dlg.clone();
        desc.connect_activate(move |e| {
            commit_description(&d, &e.text());
        });
    }
    {
        let d = dlg.clone();
        queue.connect_changed(move |combo| {
            if d.syncing.get() {
                return;
            }
            if let Some(text) = combo.active_text() {
                let text = text.to_string();
                if text != d.last.borrow().queue
                    && sink::set_entry_queue(d.ctx.clone(), d.id.clone(), text.clone()).is_ok()
                {
                    d.last.borrow_mut().queue = text.clone();
                    d.cache.borrow_mut().queue = text;
                }
            }
        });
    }
    {
        let d = dlg.clone();
        lim_on.connect_toggled(move |_| {
            if d.syncing.get() {
                return;
            }
            d.lim_dirty.set(true);
            d.apply_limit();
        });
    }
    {
        let d = dlg.clone();
        lim_val.connect_focus_in_event(move |_, _| {
            d.lim_editing.set(true);
            gtk::glib::Propagation::Proceed
        });
    }
    {
        let d = dlg.clone();
        lim_val.connect_focus_out_event(move |_, _| {
            d.lim_editing.set(false);
            if d.lim_dirty.get() {
                d.apply_limit();
            }
            gtk::glib::Propagation::Proceed
        });
    }
    {
        let d = dlg.clone();
        lim_val.connect_changed(move |_| {
            if !d.syncing.get() {
                d.lim_dirty.set(true);
            }
        });
    }
    {
        let d = dlg.clone();
        lim_val.connect_activate(move |_| {
            if d.lim_on.is_active() {
                d.apply_limit();
            }
        });
    }
    {
        let d = dlg.clone();
        c_show.connect_toggled(move |btn| {
            if d.syncing.get() {
                return;
            }
            let on = btn.is_active();
            if sink::set_show_complete_override(d.ctx.clone(), d.id.clone(), Some(on)).is_ok() {
                d.last.borrow_mut().show_complete_override = Some(on);
                d.cache.borrow_mut().completion_key.clear();
            }
        });
    }
    for (check, action) in [
        (&c_open_file, "open_file"),
        (&c_open_folder, "open_folder"),
        (&c_exit, "exit_app"),
    ] {
        let d = dlg.clone();
        check.connect_toggled(move |btn| {
            if d.syncing.get() {
                return;
            }
            let on = btn.is_active();
            d.push_completion(|actions| {
                actions.retain(|a| a != action);
                if on {
                    actions.push(action.to_string());
                }
            });
        });
    }
    {
        let d = dlg.clone();
        c_power.connect_toggled(move |btn| {
            if d.syncing.get() {
                return;
            }
            let on = btn.is_active();
            d.power_choice.set_sensitive(on);
            let choice = power_action(&d.power_choice);
            d.push_completion(|actions| {
                actions.retain(|a| a != "turn_off_pc" && a != "restart_pc");
                if on {
                    actions.push(choice.to_string());
                }
            });
        });
    }
    {
        let d = dlg.clone();
        power_choice.connect_changed(move |combo| {
            if d.syncing.get() || !d.c_power.is_active() {
                return;
            }
            let choice = power_action(combo);
            d.push_completion(|actions| {
                actions.retain(|a| a != "turn_off_pc" && a != "restart_pc");
                actions.push(choice.to_string());
            });
        });
    }
    dlg
}

fn power_action(combo: &gtk::ComboBoxText) -> &'static str {
    if combo.active() == Some(1) {
        "restart_pc"
    } else {
        "turn_off_pc"
    }
}

fn commit_description(d: &ProgDlg, text: &str) {
    let text = text.trim().to_string();
    if text == d.last.borrow().description {
        return;
    }
    if sink::set_entry_description(d.ctx.clone(), d.id.clone(), text.clone()).is_ok() {
        d.last.borrow_mut().description = text.clone();
        d.cache.borrow_mut().desc = text;
    }
}
