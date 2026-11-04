//! The two uninvited dialogs — Download Complete and NDM
//! Update — as native windows. Both are driven by core events
//! (DownloadComplete / UpdateAvailable / UpdateProgress /
//! UpdateInstalled), never by a menu, so they live outside the
//! modal builder in dialogs.rs: they must appear without
//! blocking the event loop, yield to real dialogs, and (for a
//! mandatory update) refuse dismissal. Field sets and rules
//! mirror the web UI's #cmodal / #umodal (ui/index.html,
//! app.js showNextComplete / closeComplete / showUpdate /
//! onUpdateProgress) one for one.

use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::dialogs;
use crate::sink::{self, AppEvent, Ctx};
use crate::fmt_size;

// ---------------------------------------------------------------------------
// Download Complete
// ---------------------------------------------------------------------------

thread_local! {
    /// Completions waiting for their dialog, in arrival order.
    static COMPLETE_QUEUE: RefCell<Vec<serde_json::Value>> = RefCell::new(Vec::new());
    /// The dialog on screen right now, if any.
    static COMPLETE_OPEN: RefCell<Option<Rc<CompleteDlg>>> = RefCell::new(None);
    /// Where a pumped dialog attaches (the main window) and the
    /// core handle its buttons call — captured on the first
    /// completion so dialogs.rs' modal-close hook can pump
    /// without knowing either.
    static COMPLETE_ANCHOR: RefCell<Option<(gtk::ApplicationWindow, Ctx)>> = RefCell::new(None);
}

struct CompleteDlg {
    window: gtk::Window,
    dont_show: gtk::CheckButton,
    /// Set by the first finish_complete call; re-entries (the
    /// delete-event raised by window.close()) step aside.
    closed: Cell<bool>,
}

/// A download finished (core already applied its
/// completion_show_dialog gate before emitting — core/src/lib.rs
/// run_download's finish path). Queue it and pump.
pub fn on_download_complete(parent: &gtk::ApplicationWindow, ctx: Ctx, v: &serde_json::Value) {
    COMPLETE_ANCHOR.with(|a| *a.borrow_mut() = Some((parent.clone(), ctx)));
    COMPLETE_QUEUE.with(|q| q.borrow_mut().push(v.clone()));
    pump_complete();
}

/// Show the next queued completion, if the way is clear: no
/// completion dialog open already (later finishes are not
/// announced again — app.js), and no builder dialog up
/// (completions yield; dialogs.rs calls this back on close).
/// Several queued at once fold into ONE summary (app.js F-26).
pub fn pump_complete() {
    if COMPLETE_OPEN.with(|o| o.borrow().is_some()) || dialogs::modal_open() {
        return;
    }
    let batch: Vec<serde_json::Value> =
        COMPLETE_QUEUE.with(|q| q.borrow_mut().drain(..).collect());
    if batch.is_empty() {
        return;
    }
    let Some((parent, ctx)) = COMPLETE_ANCHOR.with(|a| a.borrow().clone()) else {
        return;
    };
    if batch.len() > 1 {
        show_complete_summary(&parent, ctx, &batch);
    } else {
        show_complete_one(&parent, ctx, &batch[0]);
    }
}

/// A completion whose entry has since been deleted must not
/// surface (app.js R3): the main window's EntryRemoved handler
/// purges the id from the waiting queue through here.
pub fn forget_complete(id: &str) {
    COMPLETE_QUEUE.with(|q| {
        q.borrow_mut()
            .retain(|p| p.get("id").and_then(|x| x.as_str()) != Some(id))
    });
}

fn complete_box() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    b.set_margin_start(12);
    b.set_margin_end(12);
    b.set_margin_top(12);
    b.set_margin_bottom(12);
    b
}

fn note_label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_line_wrap(true);
    // WordChar, not Word: a filename or path is ONE unbroken
    // word, and plain word wrap reports that whole word as the
    // label's minimum width — a 187-char name stretched this
    // dialog to 1308px (audit-233 B-2). Breaking over-long words
    // at character boundaries keeps the cap below effective.
    l.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
    // A wrapping label still reports its UNWRAPPED line as its
    // natural width, and the window grows to fit it — one signed
    // CDN address (thousands of chars) blew this dialog wider
    // than the screen (2.3.1 field bug). Capping the width in
    // characters bounds the natural width, so the wrap actually
    // happens inside the dialog's own width.
    l.set_max_width_chars(64);
    l
}

fn complete_window(parent: &gtk::ApplicationWindow) -> (gtk::Window, gtk::Box, gtk::CheckButton) {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_transient_for(Some(parent));
    window.set_destroy_with_parent(true);
    window.set_title("Download complete");
    window.set_default_size(460, -1);
    let body = complete_box();
    let dont_show = gtk::CheckButton::with_label("Don't show this dialog again");
    (window, body, dont_show)
}

/// The shared close path (app.js closeComplete): apply the
/// don't-show-again choice to the global setting, optionally
/// open, then free the slot and pump the queue.
fn finish_complete(
    ctx: &Ctx,
    dlg: &Rc<CompleteDlg>,
    open: Option<(&str, bool)>, // (entry id, folder?)
    from_delete: bool,
) {
    if dlg.closed.get() {
        return;
    }
    dlg.closed.set(true);
    if dlg.dont_show.is_active() {
        set_show_complete_dialog(ctx, false);
    }
    if let Some((id, folder)) = open {
        if !id.is_empty() {
            let _ = sink::open_path(ctx.clone(), id.to_string(), folder);
        }
    }
    COMPLETE_OPEN.with(|o| *o.borrow_mut() = None);
    if !from_delete {
        // The delete-event this raises sees `closed` set and
        // lets the destruction through.
        dlg.window.close();
    }
    pump_complete();
}

fn wire_complete(
    ctx: Ctx,
    window: gtk::Window,
    dont_show: gtk::CheckButton,
    open_id: Option<String>,
    buttons: gtk::Box,
) -> Rc<CompleteDlg> {
    let dlg = Rc::new(CompleteDlg {
        window: window.clone(),
        dont_show,
        closed: Cell::new(false),
    });
    COMPLETE_OPEN.with(|o| *o.borrow_mut() = Some(dlg.clone()));
    if let Some(id) = &open_id {
        let open_btn = gtk::Button::with_label("Open");
        let folder_btn = gtk::Button::with_label("Open folder");
        buttons.pack_start(&open_btn, false, false, 0);
        buttons.pack_start(&folder_btn, false, false, 0);
        {
            let (d, c, id) = (dlg.clone(), ctx.clone(), id.clone());
            open_btn.connect_clicked(move |_| finish_complete(&c, &d, Some((&id, false)), false));
        }
        {
            let (d, c, id) = (dlg.clone(), ctx.clone(), id.clone());
            folder_btn.connect_clicked(move |_| finish_complete(&c, &d, Some((&id, true)), false));
        }
    }
    let close_btn = gtk::Button::with_label("Close");
    buttons.pack_end(&close_btn, false, false, 0);
    {
        let (d, c) = (dlg.clone(), ctx.clone());
        close_btn.connect_clicked(move |_| finish_complete(&c, &d, None, false));
    }
    {
        // window close / Esc behave as Close (app.js)
        let (d, c) = (dlg.clone(), ctx.clone());
        window.connect_delete_event(move |_, _| {
            finish_complete(&c, &d, None, true);
            gtk::glib::Propagation::Proceed
        });
        let (d, c) = (dlg.clone(), ctx);
        window.connect_key_press_event(move |_, e| {
            if e.keyval() == gtk::gdk::keys::constants::Escape {
                finish_complete(&c, &d, None, false);
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
    }
    dlg
}

fn show_complete_one(parent: &gtk::ApplicationWindow, ctx: Ctx, p: &serde_json::Value) {
    let (window, body, dont_show) = complete_window(parent);
    let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("Download");
    let title = gtk::Label::new(None);
    title.set_markup(&format!(
        "<b>{}</b>",
        gtk::glib::markup_escape_text(name)
    ));
    title.set_xalign(0.0);
    title.set_line_wrap(true);
    title.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
    title.set_max_width_chars(64);
    body.pack_start(&title, false, false, 0);
    let url = p.get("url").and_then(|x| x.as_str()).unwrap_or("");
    if !url.is_empty() {
        body.pack_start(&note_label(&format!("Address: {}", url)), false, false, 0);
    }
    let path = p.get("path").and_then(|x| x.as_str()).unwrap_or("");
    body.pack_start(
        &note_label(&format!("Saved to: {}", path)),
        false,
        false,
        0,
    );
    let total = p.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
    if total > 0 {
        body.pack_start(
            &note_label(&format!(
                "Downloaded {} ({} bytes)",
                fmt_size(total),
                total
            )),
            false,
            false,
            0,
        );
    }
    body.pack_start(&dont_show, false, false, 0);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    body.pack_start(&buttons, false, false, 0);
    window.add(&body);
    let id = p
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let dlg = wire_complete(ctx, window, dont_show, Some(id), buttons);
    dlg.window.show_all();
    dlg.window.present();
}

fn show_complete_summary(parent: &gtk::ApplicationWindow, ctx: Ctx, batch: &[serde_json::Value]) {
    let (window, body, dont_show) = complete_window(parent);
    let title = gtk::Label::new(None);
    title.set_markup(&format!("<b>{} downloads completed</b>", batch.len()));
    title.set_xalign(0.0);
    title.set_line_wrap(true);
    title.set_max_width_chars(64);
    body.pack_start(&title, false, false, 0);
    let mut names: Vec<String> = batch
        .iter()
        .map(|p| {
            p.get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("Download")
                .to_string()
        })
        .collect();
    let more = names.len().saturating_sub(12);
    names.truncate(12);
    if more > 0 {
        names.push(format!("…and {} more", more));
    }
    body.pack_start(&note_label(&names.join("\n")), false, false, 0);
    let total: u64 = batch
        .iter()
        .map(|p| p.get("total").and_then(|x| x.as_u64()).unwrap_or(0))
        .sum();
    if total > 0 {
        body.pack_start(
            &note_label(&format!("Downloaded {} in total", fmt_size(total))),
            false,
            false,
            0,
        );
    }
    body.pack_start(&dont_show, false, false, 0);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    body.pack_start(&buttons, false, false, 0);
    window.add(&body);
    // a summary has no single file to open — Close only (app.js)
    let dlg = wire_complete(ctx, window, dont_show, None, buttons);
    dlg.window.show_all();
    dlg.window.present();
}

/// The don't-show-again write: the same global setting the web
/// UI saves (settings.show_complete_dialog), through core's
/// own set_settings with every other field round-tripped.
fn set_show_complete_dialog(ctx: &Ctx, on: bool) {
    let mut v = serde_json::to_value(sink::get_settings(ctx.clone()))
        .unwrap_or(serde_json::Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("show_complete_dialog".to_string(), serde_json::json!(on));
    }
    if let Ok(s) = serde_json::from_value::<sink::Settings>(v) {
        let _ = sink::set_settings(ctx.clone(), s);
    }
}

// ---------------------------------------------------------------------------
// NDM Update
// ---------------------------------------------------------------------------

thread_local! {
    static UPDATE_DLG: RefCell<Option<Rc<UpdateDlg>>> = RefCell::new(None);
}

struct UpdateDlg {
    window: gtk::Window,
    parent: gtk::ApplicationWindow,
    title: gtk::Label,
    notes: gtk::Label,
    bar_wrap: gtk::Box,
    bar: gtk::ProgressBar,
    bar_text: gtk::Label,
    btn_update: gtk::Button,
    btn_restart: gtk::Button,
    btn_later: gtk::Button,
    mandatory: Cell<bool>,
    installed: Cell<bool>,
    installing: Cell<bool>,
}

/// Show (or refresh) the update offer — app.js showUpdate.
/// Mandatory mode: no Later, no close button, Esc and the
/// window's own close are refused until the update is in.
pub fn show_update(parent: &gtk::ApplicationWindow, ctx: Ctx, info: &serde_json::Value) {
    if info.get("status").and_then(|x| x.as_str()) != Some("available") {
        return;
    }
    let dlg = UPDATE_DLG.with(|u| u.borrow().clone());
    let dlg = match dlg {
        Some(d) => d,
        None => {
            let d = build_update(parent, ctx);
            UPDATE_DLG.with(|u| *u.borrow_mut() = Some(d.clone()));
            d
        }
    };
    let version = info.get("version").and_then(|x| x.as_str()).unwrap_or("");
    let notes = info.get("notes").and_then(|x| x.as_str()).unwrap_or("");
    let mandatory = info
        .get("mandatory")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    dlg.title
        .set_text(&format!("NDM {} is available", version));
    dlg.notes.set_text(&if mandatory {
        format!(
            "This update is required — downloads pause until it is installed. {}",
            notes
        )
    } else {
        notes.to_string()
    });
    dlg.mandatory.set(mandatory);
    dlg.installed.set(false);
    dlg.installing.set(false);
    dlg.bar_wrap.set_visible(false);
    dlg.btn_update.set_visible(true);
    dlg.btn_restart.set_visible(false);
    dlg.btn_later.set_visible(!mandatory);
    dlg.window.set_deletable(!mandatory);
    dlg.window.show_all();
    dlg.bar_wrap.set_visible(false);
    dlg.btn_restart.set_visible(false);
    dlg.btn_later.set_visible(!mandatory);
    dlg.window.present();
}

fn build_update(parent: &gtk::ApplicationWindow, ctx: Ctx) -> Rc<UpdateDlg> {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_transient_for(Some(parent));
    window.set_destroy_with_parent(true);
    window.set_title("NDM Update");
    window.set_default_size(460, -1);

    let body = complete_box();
    let title = gtk::Label::new(None);
    title.set_xalign(0.0);
    title.set_line_wrap(true);
    let notes = note_label("");
    // Release notes can run long (the live feed's do): the web
    // UI caps its notes block, so the native one scrolls inside
    // a fixed-height area instead of growing the window.
    let notes_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();
    notes_scroll.set_size_request(-1, 150);
    notes_scroll.add(&notes);
    let bar = gtk::ProgressBar::new();
    let bar_text = note_label("");
    let bar_wrap = gtk::Box::new(gtk::Orientation::Vertical, 4);
    bar_wrap.pack_start(&bar, false, false, 0);
    bar_wrap.pack_start(&bar_text, false, false, 0);
    let btn_update = gtk::Button::with_label("Update Now");
    let btn_restart = gtk::Button::with_label("Restart NDM");
    let btn_later = gtk::Button::with_label("Later");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.pack_start(&btn_update, false, false, 0);
    buttons.pack_start(&btn_restart, false, false, 0);
    buttons.pack_end(&btn_later, false, false, 0);
    body.pack_start(&title, false, false, 0);
    body.pack_start(&notes_scroll, false, false, 0);
    body.pack_start(&bar_wrap, false, false, 0);
    body.pack_start(&buttons, false, false, 0);
    window.add(&body);

    let dlg = Rc::new(UpdateDlg {
        window: window.clone(),
        parent: parent.clone(),
        title,
        notes,
        bar_wrap,
        bar,
        bar_text,
        btn_update: btn_update.clone(),
        btn_restart: btn_restart.clone(),
        btn_later: btn_later.clone(),
        mandatory: Cell::new(false),
        installed: Cell::new(false),
        installing: Cell::new(false),
    });
    {
        let (d, c) = (dlg.clone(), ctx.clone());
        btn_update.connect_clicked(move |_| {
            if d.installing.get() {
                return;
            }
            d.installing.set(true);
            d.btn_update.set_visible(false);
            d.bar_wrap.set_visible(true);
            d.bar_text.set_text("Downloading the update…");
            // install_update revalidates against the feed, then
            // core's own worker runs the install and narrates
            // through UpdateProgress; a hard refusal returns
            // here and is re-emitted as the error phase.
            let c2 = c.clone();
            std::thread::spawn(move || {
                if let Err(e) =
                    sink::rt::block_on(sink::install_update(c2.clone()))
                {
                    let _ = c2.emit(AppEvent::UpdateProgress(serde_json::json!({
                        "phase": "error", "message": e
                    })));
                }
            });
        });
    }
    {
        let d = dlg.clone();
        btn_later.connect_clicked(move |_| d.window.hide());
    }
    {
        let d = dlg.clone();
        btn_restart.connect_clicked(move |_| {
            // relaunch this shell, then close the main window —
            // its destroy quits the old process (main.rs wiring).
            // NDM_RESTART_WAIT tells the new process to wait for
            // this one's instance lock instead of mistaking the
            // shutdown window for a running instance.
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe)
                    .env("NDM_RESTART_WAIT", "1")
                    .spawn();
            }
            d.parent.close();
        });
    }
    {
        let d = dlg.clone();
        window.connect_delete_event(move |_, _| {
            // a mandatory update cannot be dismissed — not by
            // the close button (hidden), not by the WM, not by
            // Esc — until it is installed (app.js)
            if d.mandatory.get() && !d.installed.get() {
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
    }
    dlg
}

/// Core's install narration (app.js onUpdateProgress).
pub fn on_update_progress(v: &serde_json::Value) {
    let Some(dlg) = UPDATE_DLG.with(|u| u.borrow().clone()) else {
        return;
    };
    let mb = |b: u64| format!("{:.1}", b as f64 / 1_048_576.0);
    match v.get("phase").and_then(|x| x.as_str()).unwrap_or("") {
        "downloading" => {
            let done = v.get("downloaded").and_then(|x| x.as_u64()).unwrap_or(0);
            let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
            dlg.bar_wrap.set_visible(true);
            dlg.bar_text.set_text(&if total > 0 {
                format!(
                    "Downloading the update — {} of {} MB",
                    mb(done),
                    mb(total)
                )
            } else {
                format!("Downloading the update — {} MB", mb(done))
            });
            if total > 0 {
                dlg.bar
                    .set_fraction((done as f64 / total as f64).min(1.0));
            }
        }
        "installing" => {
            let msg = v
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("Installing — your password may be asked by the system…");
            dlg.bar_wrap.set_visible(true);
            dlg.bar_text.set_text(msg);
            dlg.bar.set_fraction(1.0);
        }
        "installed" => mark_installed(&dlg, "Update installed."),
        "opened" => mark_installed(
            &dlg,
            "The installer opened — finish the installation there, then restart NDM.",
        ),
        "error" => {
            let msg = v
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown error");
            dlg.installing.set(false);
            dlg.bar_wrap.set_visible(true);
            dlg.bar_text.set_text(&format!("Update failed: {}", msg));
            dlg.btn_update.set_visible(true);
        }
        _ => {}
    }
}

/// The UpdateInstalled event — the same end state as the
/// "installed" progress phase.
pub fn on_update_installed() {
    if let Some(dlg) = UPDATE_DLG.with(|u| u.borrow().clone()) {
        mark_installed(&dlg, "Update installed.");
    }
}

fn mark_installed(dlg: &Rc<UpdateDlg>, text: &str) {
    dlg.installing.set(false);
    dlg.installed.set(true);
    dlg.bar_wrap.set_visible(true);
    dlg.bar_text.set_text(text);
    dlg.btn_update.set_visible(false);
    dlg.btn_later.set_visible(false);
    dlg.btn_restart.set_visible(true);
    // a mandatory dialog unlocks once installed (app.js)
    dlg.window.set_deletable(true);
}
