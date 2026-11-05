//! Video picker (app.js vmodal) — probe on a worker thread,
//! stale probes dropped by sequence, format rows with the
//! probe's own badge/size/note, preselect from
//! settings.video_quality, category-dir routing with a
//! user-edit override, the Later choice preserved, duplicate →
//! confirm → force, Download-all, and the ensure_components
//! gate (the probe waits invisibly for the engine, app.js
//! gateComponents). One window; reopening raises it.

use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::form::*;
use crate::sink::{self, Ctx};
use crate::{fmt_size, refresh_list, set_notice, settings_json, Ui};

thread_local! {
    static DLG: RefCell<Option<Rc<VideoDlg>>> = RefCell::new(None);
}

struct VideoDlg {
    window: gtk::Window,
    ui: Rc<Ui>,
    ctx: Ctx,
    url: gtk::Entry,
    dir: gtk::Entry,
    queue: gtk::ComboBoxText,
    desc: gtk::Entry,
    title: gtk::Entry,
    meta: gtk::Label,
    status: gtk::Label,
    options_box: gtk::Box,
    btn_start: gtk::Button,
    btn_later: gtk::Button,
    btn_all: gtk::Button,
    info: RefCell<Option<serde_json::Value>>,
    radios: RefCell<Vec<gtk::RadioButton>>,
    seq: Cell<u64>,
    dir_touched: Cell<bool>,
    dir_guard: Cell<bool>,
    ready: Cell<bool>,
    waiting: Cell<bool>,
    cookies: RefCell<Option<String>>,
    /// The page the cookie token was captured for. The token may
    /// ride a probe/add ONLY for that exact page — it used to
    /// ride whatever URL sat in the box (audit-233: a captured
    /// facebook.com session sent to an arbitrary probe URL).
    cookies_url: RefCell<String>,
    token_consumed: Cell<bool>,
}

/// The captured-session token, when the URL it would ride with
/// is the page it was captured on; otherwise None.
fn session_cookies(d: &VideoDlg, current_url: &str) -> Option<String> {
    let tok = d.cookies.borrow().clone()?;
    if !tok.is_empty() && d.cookies_url.borrow().as_str() == current_url {
        Some(tok)
    } else {
        None
    }
}

/// app.js routedDirFor: the category's own folder, else default.
fn routed_dir(ui: &Ui, cat: &str) -> String {
    let s = settings_json(ui);
    s.get("category_dirs")
        .and_then(|d| d.get(cat))
        .and_then(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .map(|x| x.to_string())
        .unwrap_or_else(|| js(&s, "default_dir"))
}

/// app.js pickQualityIndex, verbatim rules.
fn pick_quality_index(options: &[serde_json::Value], quality: &str) -> usize {
    if quality == "best" || quality.is_empty() {
        return 0;
    }
    if quality == "audio" {
        return options
            .iter()
            .position(|o| jb(o, "audio", false))
            .unwrap_or(0);
    }
    let target: i64 = quality.parse().unwrap_or(0);
    if let Some(i) = options.iter().position(|o| {
        !jb(o, "audio", false) && js(o, "label").starts_with(quality)
    }) {
        return i;
    }
    options
        .iter()
        .position(|o| {
            let h = ju(o, "height") as i64;
            let h = if h > 0 { h } else { js(o, "label").parse().unwrap_or(0) };
            !jb(o, "audio", false) && h > 0 && h <= target
        })
        .unwrap_or(0)
}

fn fmt_duration(sec: u64) -> String {
    if sec == 0 {
        return String::new();
    }
    let (h, m, s) = (sec / 3600, (sec % 3600) / 60, sec % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn show(ui: Rc<Ui>, url: Option<String>, cookies: Option<String>) {
    if let Some(d) = DLG.with(|d| d.borrow().clone()) {
        d.window.present();
        if let Some(u) = &url {
            // Reopening for another entry REPLACES the captured
            // session: the old code kept entry A's token, so A's
            // session rode B's probe and B's fresh token was
            // ignored entirely (audit-233). An unconsumed replaced
            // token is discarded, not left behind in core.
            if !d.token_consumed.get() {
                if let Some(old) = d.cookies.borrow().clone() {
                    if !old.is_empty() && Some(&old) != cookies.as_ref() {
                        sink::discard_cookies(d.ctx.clone(), old);
                    }
                }
            }
            *d.cookies.borrow_mut() = cookies;
            *d.cookies_url.borrow_mut() = u.clone();
            d.token_consumed.set(false);
            d.url.set_text(u);
            start_fetch(&d);
        }
        return;
    }
    let ctx = ui.ctx.clone();
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_transient_for(Some(&ui.window));
    window.set_destroy_with_parent(true);
    window.set_title("Download Video");
    window.set_default_size(640, 560);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    let g = grid();
    g.set_margin_start(0);
    g.set_margin_end(0);
    g.set_margin_top(0);
    g.set_margin_bottom(0);
    let url_e = entry(url.as_deref().unwrap_or(""));
    url_e.set_placeholder_text(Some("https://www.youtube.com/watch?v=…"));
    let mut y = row(&g, 0, "Video page address", &url_e);
    let s = settings_json(&ui);
    let dir_e = entry(&routed_dir(&ui, "Video"));
    y = row(&g, y, "Save to", &dir_e);
    let queues = crate::queue_names(&ui);
    let queue = combo(
        &queues.iter().map(|q| (q.as_str(), q.as_str())).collect::<Vec<_>>(),
        &crate::pick_queue_name(&ui),
    );
    y = row(&g, y, "Queue", &queue);
    let desc = entry("");
    desc.set_placeholder_text(Some("(optional note)"));
    row(&g, y, "Description", &desc);
    body.pack_start(&g, false, false, 0);

    let fetch_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_fetch = gtk::Button::with_label("Fetch Info");
    let status = status_label();
    fetch_row.pack_start(&btn_fetch, false, false, 0);
    fetch_row.pack_start(&status, true, true, 0);
    body.pack_start(&fetch_row, false, false, 0);

    let title = entry("");
    title.set_placeholder_text(Some("Video title"));
    body.pack_start(&title, false, false, 0);
    let meta = gtk::Label::new(None);
    meta.set_halign(gtk::Align::Start);
    body.pack_start(&meta, false, false, 0);
    let options_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .shadow_type(gtk::ShadowType::In)
        .build();
    scroll.set_size_request(-1, 190);
    scroll.add(&options_box);
    body.pack_start(&scroll, true, true, 0);

    let btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_start = gtk::Button::with_label("⬇ Start Download");
    let btn_all = gtk::Button::with_label("Download all");
    let btn_later = gtk::Button::with_label("Download Later");
    let btn_cancel = gtk::Button::with_label("Cancel");
    btns.pack_start(&btn_start, false, false, 0);
    btns.pack_start(&btn_all, false, false, 0);
    btns.pack_start(&btn_later, false, false, 0);
    btns.pack_end(&btn_cancel, false, false, 0);
    body.pack_start(&btns, false, false, 0);
    window.add(&body);

    let ready = sink::components_status(ctx.clone())
        .get("ready")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let dlg = Rc::new(VideoDlg {
        window: window.clone(),
        ui: ui.clone(),
        ctx,
        url: url_e.clone(),
        dir: dir_e.clone(),
        queue,
        desc,
        title,
        meta,
        status,
        options_box,
        btn_start: btn_start.clone(),
        btn_later: btn_later.clone(),
        btn_all: btn_all.clone(),
        info: RefCell::new(None),
        radios: RefCell::new(Vec::new()),
        seq: Cell::new(0),
        dir_touched: Cell::new(false),
        dir_guard: Cell::new(false),
        ready: Cell::new(ready),
        waiting: Cell::new(false),
        cookies: RefCell::new(cookies),
        cookies_url: RefCell::new(url.clone().unwrap_or_default()),
        token_consumed: Cell::new(false),
    });
    DLG.with(|d| *d.borrow_mut() = Some(dlg.clone()));
    set_start_enabled(&dlg, false);
    let _ = s;

    {
        let d = dlg.clone();
        btn_fetch.connect_clicked(move |_| start_fetch(&d));
    }
    {
        let d = dlg.clone();
        url_e.connect_activate(move |_| start_fetch(&d));
    }
    {
        // editing the address invalidates any retained probe
        let d = dlg.clone();
        url_e.connect_changed(move |_| {
            d.seq.set(d.seq.get() + 1);
            if d.info.borrow().is_some() {
                *d.info.borrow_mut() = None;
                set_start_enabled(&d, false);
                d.status.set_text("");
            }
        });
    }
    {
        let d = dlg.clone();
        dir_e.connect_changed(move |_| {
            if !d.dir_guard.get() {
                d.dir_touched.set(true);
            }
        });
    }
    {
        let d = dlg.clone();
        btn_start.connect_clicked(move |_| submit(&d, false));
    }
    {
        let d = dlg.clone();
        btn_later.connect_clicked(move |_| submit(&d, true));
    }
    {
        let d = dlg.clone();
        btn_all.connect_clicked(move |_| submit_all(&d));
    }
    {
        let d = dlg.clone();
        btn_cancel.connect_clicked(move |_| close(&d));
    }
    {
        let d = dlg.clone();
        window.connect_delete_event(move |_, _| {
            discard_if_abandoned(&d);
            DLG.with(|x| *x.borrow_mut() = None);
            // Slice 3: this picker was the screen's one capture
            // dialog; queued offers and captures get their
            // turn now.
            crate::pump_file_offers();
            crate::pump_capture_queue();
            gtk::glib::Propagation::Proceed
        });
    }
    window.show_all();
    window.present();
    if url.is_some() {
        start_fetch(&dlg);
    }
}

/// Slice 3: is the picker on screen right now? The capture
/// arbitration in main.rs queues competing video/grab captures
/// while it is.
pub fn is_open() -> bool {
    DLG.with(|d| {
        d.borrow()
            .as_ref()
            .map(|d| d.window.is_visible())
            .unwrap_or(false)
    })
}

fn discard_if_abandoned(d: &Rc<VideoDlg>) {
    if !d.token_consumed.get() {
        if let Some(tok) = d.cookies.borrow().clone() {
            if !tok.is_empty() {
                sink::discard_cookies(d.ctx.clone(), tok);
            }
        }
    }
}

fn close(d: &Rc<VideoDlg>) {
    discard_if_abandoned(d);
    DLG.with(|x| *x.borrow_mut() = None);
    d.window.close();
}

fn set_start_enabled(d: &Rc<VideoDlg>, on: bool) {
    d.btn_start.set_sensitive(on);
    d.btn_later.set_sensitive(on);
    d.btn_all.set_sensitive(on);
}

fn set_routed_dir(d: &Rc<VideoDlg>, cat: &str) {
    if d.dir_touched.get() {
        return;
    }
    d.dir_guard.set(true);
    d.dir.set_text(&routed_dir(&d.ui, cat));
    d.dir_guard.set(false);
}

fn start_fetch(d: &Rc<VideoDlg>) {
    let url = d.url.text().trim().to_string();
    if url.is_empty() {
        d.url.grab_focus();
        return;
    }
    if !d.ready.get() {
        // gateComponents: make sure the silent fetch is running
        // and wait invisibly — the dialog just looks like a slow
        // page read; on_components_progress re-runs the probe.
        let st = sink::components_status(d.ctx.clone());
        if st.get("ready").and_then(|x| x.as_bool()).unwrap_or(false) {
            d.ready.set(true);
        } else {
            sink::ensure_components(d.ctx.clone());
            d.waiting.set(true);
            d.status.set_text("Reading video qualities & file sizes…");
            return;
        }
    }
    d.status.set_text("Reading video qualities & file sizes…");
    set_start_enabled(d, false);
    *d.info.borrow_mut() = None;
    let seq = d.seq.get() + 1;
    d.seq.set(seq);
    let (tx, rx) = gtk::glib::MainContext::channel::<(u64, Result<serde_json::Value, String>)>(
        gtk::glib::Priority::DEFAULT,
    );
    let ctx = d.ctx.clone();
    let cookies = session_cookies(d, &url);
    std::thread::spawn(move || {
        let r = sink::rt::block_on(sink::probe_video(ctx, url, cookies))
            .map(|info| sink::to_json(&info));
        let _ = tx.send((seq, r));
    });
    let d2 = d.clone();
    rx.attach(None, move |(got_seq, r)| {
        if got_seq != d2.seq.get() {
            return gtk::glib::ControlFlow::Continue; // stale probe
        }
        match r {
            Ok(info) => apply_info(&d2, info),
            Err(e) => {
                d2.status.set_text(&e);
            }
        }
        gtk::glib::ControlFlow::Break
    });
}

/// Core's component narration: the waiting probe runs when the
/// engine lands; a gave-up fetch reads as a probe failure.
pub fn on_components_progress(v: &serde_json::Value) {
    let Some(d) = DLG.with(|x| x.borrow().clone()) else {
        return;
    };
    match v.get("phase").and_then(|x| x.as_str()) {
        Some("done") => {
            d.ready.set(true);
            if d.waiting.get() {
                d.waiting.set(false);
                start_fetch(&d);
            }
        }
        Some("error") => {
            if d.waiting.get() {
                d.waiting.set(false);
                d.status
                    .set_text("Could not read this page — check the connection, then try again.");
            }
        }
        _ => {}
    }
}

fn apply_info(d: &Rc<VideoDlg>, info: serde_json::Value) {
    let options = info
        .get("options")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if options.is_empty() {
        d.status.set_text("No downloadable formats found for this page");
        return;
    }
    d.title.set_text(&js(&info, "title"));
    let dur = fmt_duration(ju(&info, "duration"));
    let meta = [
        js(&info, "uploader"),
        dur.clone(),
        format!("{} quality options", options.len()),
    ]
    .into_iter()
    .filter(|x| !x.is_empty())
    .collect::<Vec<_>>()
    .join(" • ");
    d.meta.set_text(&meta);
    // clear old rows
    for child in d.options_box.children() {
        d.options_box.remove(&child);
    }
    d.radios.borrow_mut().clear();
    let quality = js(&settings_json(&d.ui), "video_quality");
    let preselect = pick_quality_index(&options, &quality);
    let mut first: Option<gtk::RadioButton> = None;
    for (i, o) in options.iter().enumerate() {
        // badge: the probe's own ext, else the selector's [ext=…],
        // never an invented container (app.js N-09)
        let sel = js(o, "selector");
        let sel_ext = sel
            .split("[ext=")
            .nth(1)
            .and_then(|r| r.split(']').next())
            .unwrap_or("")
            .to_string();
        let badge = {
            let e = js(o, "ext");
            if !e.is_empty() {
                e
            } else if !sel_ext.is_empty() {
                sel_ext
            } else if jb(o, "audio", false) {
                "Audio".to_string()
            } else {
                "MP4".to_string()
            }
        }
        .to_uppercase();
        let approx = jb(o, "size_approx", false);
        let mut size = js(o, "size_text");
        if size.is_empty() {
            size = if ju(o, "size_bytes") > 0 {
                fmt_size(ju(o, "size_bytes"))
            } else if sel == "best" {
                "Auto".to_string()
            } else {
                "Stream".to_string()
            };
        }
        if approx && size != "Auto" && size != "Stream" && !size.starts_with('~') {
            size = format!("~{size}");
        }
        let note = js(o, "note")
            .split('•')
            .filter(|p| {
                let l = p.to_lowercase();
                !(l.contains("mb") || l.contains("gb") || l.contains("kb"))
            })
            .collect::<Vec<_>>()
            .join("•")
            .trim()
            .to_string();
        let text = format!(
            "[{}]  {}{}{}",
            badge,
            js(o, "label"),
            if note.is_empty() { String::new() } else { format!("  ·  {note}") },
            format!("   —   {size}")
        );
        let radio = match &first {
            None => {
                let r = gtk::RadioButton::with_label(&text);
                first = Some(r.clone());
                r
            }
            Some(f) => gtk::RadioButton::with_label_from_widget(f, &text),
        };
        radio.set_active(i == preselect);
        {
            let d2 = d.clone();
            let audio = jb(o, "audio", false);
            radio.connect_toggled(move |r| {
                if r.is_active() {
                    set_routed_dir(&d2, if audio { "Music" } else { "Video" });
                }
            });
        }
        {
            let d2 = d.clone();
            let radio2 = radio.clone();
            radio.connect_button_press_event(move |_, e| {
                if e.button() == 1 && e.event_type() == gtk::gdk::EventType::DoubleButtonPress {
                    radio2.set_active(true);
                    submit(&d2, false);
                    return gtk::glib::Propagation::Stop;
                }
                gtk::glib::Propagation::Proceed
            });
        }
        d.options_box.pack_start(&radio, false, false, 0);
        d.radios.borrow_mut().push(radio);
    }
    d.options_box.show_all();
    *d.info.borrow_mut() = Some(info);
    d.status.set_text("");
    set_start_enabled(d, true);
}

fn selected_index(d: &Rc<VideoDlg>) -> Option<usize> {
    d.radios
        .borrow()
        .iter()
        .position(|r| r.is_active())
        .or(Some(0))
}

/// Everything one video add needs, snapshotted from the
/// widgets on the GTK thread so the add itself can run on a
/// worker — add_video starts a download (probe, session), and
/// on this thread a slow host froze the picker behind Download.
#[derive(Clone)]
struct VideoAddArgs {
    url: String,
    dir: String,
    selector: String,
    audio: bool,
    title: String,
    cookies: Option<String>,
    later: bool,
    size_bytes: u64,
    duration: f64,
    queue: Option<String>,
    description: Option<String>,
}

fn add_args(d: &Rc<VideoDlg>, opt: &serde_json::Value, later: bool) -> VideoAddArgs {
    let info = d.info.borrow().clone().unwrap_or(serde_json::Value::Null);
    let dir = {
        let t = d.dir.text().trim().to_string();
        if t.is_empty() { "~/Downloads".to_string() } else { t }
    };
    let title = {
        let t = d.title.text().trim().to_string();
        if t.is_empty() { js(&info, "title") } else { t }
    };
    VideoAddArgs {
        url: d.url.text().trim().to_string(),
        dir,
        selector: js(opt, "selector"),
        audio: jb(opt, "audio", false),
        title,
        cookies: session_cookies(d, d.url.text().trim()),
        later,
        size_bytes: ju(opt, "size_bytes"),
        duration: ju(&info, "duration") as f64,
        queue: Some(combo_text(&d.queue)),
        description: {
            let t = d.desc.text().trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        },
    }
}

fn run_video_add(ctx: sink::Ctx, a: VideoAddArgs, force: bool) -> Result<String, String> {
    sink::rt::block_on(sink::add_video(
        ctx,
        a.url,
        a.dir,
        a.selector,
        a.audio,
        a.title,
        a.cookies,
        Some(a.later),
        Some(a.size_bytes),
        Some(a.duration),
        Some(force),
        a.queue,
        a.description,
        Some(true), // the shown folder is final (W193-3)
    ))
}

fn submit(d: &Rc<VideoDlg>, later: bool) {
    let Some(info) = d.info.borrow().clone() else { return };
    let options = info
        .get("options")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let Some(idx) = selected_index(d) else { return };
    let Some(opt) = options.get(idx).cloned() else { return };
    set_start_enabled(d, false);
    let args = Rc::new(add_args(d, &opt, later));
    spawn_video_add(d, args, false, true);
}

/// Run one video add on a worker thread and settle its verdict
/// on the GTK thread. A duplicate gets the "download again as
/// a new copy?" confirm here, and the force retry re-enters
/// with the same snapshotted arguments.
fn spawn_video_add(d: &Rc<VideoDlg>, args: Rc<VideoAddArgs>, force: bool, may_confirm: bool) {
    let (tx, rx) = gtk::glib::MainContext::channel::<Result<String, String>>(
        gtk::glib::Priority::DEFAULT,
    );
    let ctx = d.ctx.clone();
    let a = (*args).clone();
    std::thread::spawn(move || {
        let _ = tx.send(run_video_add(ctx, a, force));
    });
    let d2 = d.clone();
    rx.attach(None, move |result| {
        match result {
            Err(e) if may_confirm && e.starts_with("duplicate:") => {
                if crate::dialogs::show_confirm(
                    &d2.ui.window,
                    "Download again",
                    "This video quality is already in the download list.\nDownload it again as a new copy?",
                    "Download again",
                ) {
                    spawn_video_add(&d2, args.clone(), true, false);
                } else {
                    // User kept the existing entry: close WITHOUT
                    // settle's "Video added" — nothing was added,
                    // and an empty/wrong notice in its place was
                    // an audit-233 minor.
                    let ui = d2.ui.clone();
                    close(&d2);
                    refresh_list(&ui);
                    set_notice(&ui, "Kept the existing download");
                }
            }
            other => settle_video_add(&d2, other),
        }
        gtk::glib::ControlFlow::Break
    });
}

fn settle_video_add(d: &Rc<VideoDlg>, result: Result<String, String>) {
    match result {
        Ok(_) => {
            if d.cookies.borrow().is_some() {
                d.token_consumed.set(true);
            }
            let ui = d.ui.clone();
            close(d);
            refresh_list(&ui);
            set_notice(&ui, "Video added");
        }
        Err(e) => {
            d.status.set_text(&e);
            set_start_enabled(d, true);
        }
    }
}

fn submit_all(d: &Rc<VideoDlg>) {
    let Some(info) = d.info.borrow().clone() else { return };
    let options = info
        .get("options")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if options.is_empty() {
        return;
    }
    // app.js: the dialog closes first, then every quality is
    // added; ones already in the list are skipped, not duplicated.
    // The arguments are snapshotted from the widgets before the
    // close; the adds themselves run on a worker thread.
    if d.cookies.borrow().is_some() {
        d.token_consumed.set(true);
    }
    let batch: Vec<VideoAddArgs> = options.iter().map(|opt| add_args(d, opt, false)).collect();
    let ui = d.ui.clone();
    let ctx = d.ctx.clone();
    close(d);
    let (tx, rx) = gtk::glib::MainContext::channel::<(usize, usize)>(
        gtk::glib::Priority::DEFAULT,
    );
    std::thread::spawn(move || {
        let (mut sent, mut skipped) = (0usize, 0usize);
        for a in batch {
            match run_video_add(ctx.clone(), a, false) {
                Ok(_) => sent += 1,
                Err(_) => skipped += 1,
            }
        }
        let _ = tx.send((sent, skipped));
    });
    rx.attach(None, move |(sent, skipped)| {
        refresh_list(&ui);
        set_notice(
            &ui,
            &format!(
                "Download all: {} qualities added{}",
                sent,
                if skipped > 0 {
                    format!(", {} skipped", skipped)
                } else {
                    String::new()
                }
            ),
        );
        gtk::glib::ControlFlow::Break
    });
}
