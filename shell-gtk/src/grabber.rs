//! Site Grabber wizard (app.js gmodal) — five steps over
//! core's grab_explore + Settings.grab_projects: start page /
//! saved projects / templates, save-to, explore filters, file
//! filters, results. Selection is by result INDEX and survives
//! filtering (F-25); the Download button carries the true
//! count; partial failure keeps the wizard open with the
//! reason in plain language. One window; reopening raises it.

use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use crate::form::*;
use crate::sink::{self, Ctx};
use crate::{fmt_size, refresh_list, set_notice, settings_json, Ui};

thread_local! {
    static DLG: RefCell<Option<Rc<GrabDlg>>> = RefCell::new(None);
}

const STEP_TITLES: [&str; 5] = [
    "Site Grabber — Step 1: Set start page",
    "Site Grabber — Step 2: Save files to",
    "Site Grabber — Step 3: Set explorer filters",
    "Site Grabber — Step 4: Set file filters",
    "Site Grabber — Exploration results",
];

/// (key, label, include, exclude, depth_same, whole) — app.js
/// GRAB_TEMPLATES, verbatim.
const TEMPLATES: [(&str, &str, &str, &str, i64, bool); 7] = [
    ("custom", "Custom settings", "", "", -1, false),
    ("pictures", "All pictures of the web site", "png, jpg, jpeg, gif, bmp, webp, svg, ico, tif, tiff", "", 10, true),
    ("audio", "Audio files from the start page and one next level", "mp3, m4a, m4b, flac, wav, ogg, opus, aac, wma", "", 1, false),
    ("video", "All video files of the web site", "mp4, mkv, webm, avi, mov, flv, wmv, m4v, ts", "", 10, true),
    ("all_except_pages", "All files of the web site except web pages and images", "", "html, htm, php, asp, aspx, png, jpg, jpeg, gif, bmp, webp, svg", 10, true),
    ("all_files", "The whole web site — all files", "", "", 10, true),
    ("complete_page", "Complete start page with all pictures, styles and scripts (offline browsing)", "html, htm, js, css, png, jpg, jpeg, gif, svg, webp, ico, woff, woff2, ttf", "", 0, false),
];

/// app.js GRAB_PRESETS, verbatim.
fn preset_exts(key: &str) -> Option<&'static str> {
    Some(match key {
        "images" => "gif, jpg, jpeg, jpe, bmp, png, webp, svg, ico, tif, tiff",
        "video" => "mp4, mkv, webm, avi, mov, flv, wmv, m4v, ts",
        "audio" => "mp3, m4a, m4b, flac, wav, ogg, opus, aac, wma",
        "pdf" => "pdf",
        "exe" => "exe, msi, deb, rpm, dmg, pkg, appimage, apk, bin, run",
        "compressed" => "zip, rar, 7z, tar, gz, bz2, xz, zst, tgz",
        "page_assets" => "html, htm, js, css, png, jpg, jpeg, gif, svg, ico",
        _ => return None,
    })
}

struct GrabDlg {
    window: gtk::Window,
    ui: Rc<Ui>,
    ctx: Ctx,
    title: gtk::Label,
    book: gtk::Notebook,
    btn_back: gtk::Button,
    btn_next: gtk::Button,
    btn_fetch: gtk::Button,
    btn_download: gtk::Button,
    // step 1
    proj_name: gtk::Entry,
    proj_sel: gtk::ComboBoxText,
    url: gtk::Entry,
    template: gtk::ComboBoxText,
    // step 2
    dir: gtk::Entry,
    subfolders: gtk::CheckButton,
    add_queue: gtk::CheckButton,
    convert: gtk::CheckButton,
    queue_sel: gtk::ComboBoxText,
    // step 3
    mode_whole: gtk::RadioButton,
    depth_same: gtk::SpinButton,
    depth_other: gtk::SpinButton,
    no_parent: gtk::CheckButton,
    // step 4
    incl: gtk::Entry,
    excl: gtk::Entry,
    same_site: gtk::CheckButton,
    min_kb: gtk::SpinButton,
    max_mb: gtk::SpinButton,
    // step 5
    status: gtk::Label,
    kinds_box: gtk::Box,
    filter: gtk::Entry,
    count: gtk::Label,
    store: gtk::ListStore,
    items: RefCell<Vec<serde_json::Value>>,
    unchecked: RefCell<HashSet<usize>>,
    kinds_off: RefCell<HashSet<String>>,
    exploring: Cell<bool>,
}

fn exts_of(text: &str) -> Vec<String> {
    // app.js: split on commas/whitespace, strip a leading dot,
    // lowercase, drop empties.
    text.split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// app.js grabWizardState — the project's full field set.
fn wizard_state(d: &GrabDlg) -> serde_json::Value {
    serde_json::json!({
        "name": d.proj_name.text().trim().to_string(),
        "start_url": d.url.text().trim().to_string(),
        "template": combo_text(&d.template),
        "save_dir": d.dir.text().trim().to_string(),
        "use_subfolders": d.subfolders.is_active(),
        "add_as_queue": d.add_queue.is_active(),
        "convert_links": d.convert.is_active(),
        "queue": combo_text(&d.queue_sel),
        "depth_same": (d.depth_same.value_as_int()).clamp(0, 10),
        "depth_other": (d.depth_other.value_as_int()).clamp(0, 10),
        "whole_site": d.mode_whole.is_active(),
        "no_parent": d.no_parent.is_active(),
        "same_site_files_only": d.same_site.is_active(),
        "include_exts": exts_of(&d.incl.text()),
        "exclude_exts": exts_of(&d.excl.text()),
        "min_kb": (d.min_kb.value_as_int()).max(0),
        "max_mb": (d.max_mb.value_as_int()).max(0),
    })
}

fn save_projects(ui: &Ui, projects: serde_json::Value) -> Result<(), String> {
    let mut base = settings_json(ui);
    base["grab_projects"] = projects;
    let settings: sink::Settings =
        serde_json::from_value(base).map_err(|e| e.to_string())?;
    sink::set_settings(ui.ctx.clone(), settings)
}

fn fill_projects(d: &Rc<GrabDlg>) {
    d.proj_sel.remove_all();
    d.proj_sel.append(Some(""), "(no saved project selected)");
    if let Some(list) = settings_json(&d.ui)
        .get("grab_projects")
        .and_then(|x| x.as_array())
    {
        for p in list {
            let name = js(p, "name");
            if !name.is_empty() {
                d.proj_sel.append(Some(&name), &name);
            }
        }
    }
    d.proj_sel.set_active(Some(0));
}

/// The queue dropdown's rows: (no queue) + the settings' queue
/// names — mirrors fill_projects.
fn fill_queues(d: &GrabDlg) {
    d.queue_sel.remove_all();
    d.queue_sel.append(Some(""), "(no queue)");
    if let Some(list) = settings_json(&d.ui)
        .get("queues")
        .and_then(|x| x.as_array())
    {
        for q in list {
            let name = js(q, "name");
            if !name.is_empty() {
                d.queue_sel.append(Some(&name), &name);
            }
        }
    }
    d.queue_sel.set_active(Some(0));
}

fn apply_project(d: &GrabDlg, p: &serde_json::Value) {
    d.proj_name.set_text(&js(p, "name"));
    d.url.set_text(&js(p, "start_url"));
    let t = js(p, "template");
    d.template
        .set_active_id(Some(if t.is_empty() { "custom" } else { t.as_str() }));
    let dir = js(p, "save_dir");
    let dir = if dir.is_empty() {
        js(&settings_json(&d.ui), "default_dir")
    } else {
        dir
    };
    d.dir.set_text(&dir);
    d.subfolders.set_active(jb(p, "use_subfolders", false));
    d.add_queue.set_active(jb(p, "add_as_queue", false));
    // a project saved by an older build has no convert_links:
    // it migrates OFF (core's serde default); the wizard's own
    // checkbox is what turns it on
    d.convert.set_active(jb(p, "convert_links", false));
    fill_queues(d);
    let q = js(p, "queue");
    if !q.is_empty() {
        d.queue_sel.set_active_id(Some(&q));
    }
    d.depth_same
        .set_value(p.get("depth_same").and_then(|x| x.as_i64()).unwrap_or(1) as f64);
    d.depth_other
        .set_value(p.get("depth_other").and_then(|x| x.as_i64()).unwrap_or(0) as f64);
    d.mode_whole.set_active(jb(p, "whole_site", false));
    d.no_parent.set_active(jb(p, "no_parent", false));
    d.same_site.set_active(jb(p, "same_site_files_only", true));
    let join = |k: &str| {
        p.get(k)
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    d.incl.set_text(&join("include_exts"));
    d.excl.set_text(&join("exclude_exts"));
    d.min_kb
        .set_value(p.get("min_kb").and_then(|x| x.as_i64()).unwrap_or(0) as f64);
    d.max_mb
        .set_value(p.get("max_mb").and_then(|x| x.as_i64()).unwrap_or(0) as f64);
}

/// (host, path segments) of an http(s)/file URL — just enough
/// of URL parsing for the subfolder rule; anything odd yields
/// empty segments and the base dir wins.
fn url_parts(u: &str) -> (String, Vec<String>, bool) {
    let rest = u.split("://").nth(1).unwrap_or("");
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
    (host.to_string(), segs, path.ends_with('/'))
}

/// app.js grabDirFor: the file's folders below the start
/// page's own folder, appended to the save dir.
fn grab_dir_for(base: &str, start_url: &str, item_url: &str) -> String {
    let (host, mut parts, _) = url_parts(item_url);
    if parts.is_empty() {
        return base.to_string();
    }
    parts.pop(); // the file name itself
    let (shost, ssegs, strailing) = url_parts(start_url);
    if shost == host {
        let mut root = ssegs;
        if !strailing && !root.is_empty() {
            root.pop(); // the start page itself
        }
        if root.iter().enumerate().all(|(i, seg)| parts.get(i) == Some(seg)) {
            parts.drain(..root.len());
        }
    }
    if parts.is_empty() {
        base.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), parts.join("/"))
    }
}

fn visible(d: &GrabDlg, it: &serde_json::Value) -> bool {
    if d.kinds_off.borrow().contains(&js(it, "kind")) {
        return false;
    }
    let q = d.filter.text().trim().to_lowercase();
    q.is_empty()
        || format!("{} {}", js(it, "name"), js(it, "url"))
            .to_lowercase()
            .contains(&q)
}

fn update_count(d: &GrabDlg) {
    let items = d.items.borrow();
    let sel: Vec<usize> = (0..items.len())
        .filter(|i| !d.unchecked.borrow().contains(i))
        .collect();
    let vis = sel.iter().filter(|i| visible(d, &items[**i])).count();
    d.count.set_text(&if vis == sel.len() {
        format!("{} selected", sel.len())
    } else {
        format!("{} selected ({} in current view)", sel.len(), vis)
    });
    d.btn_download
        .set_label(&format!("Download Selected ({})", sel.len()));
    d.btn_download.set_sensitive(!sel.is_empty());
}

fn render_kinds(d: &Rc<GrabDlg>) {
    for child in d.kinds_box.children() {
        d.kinds_box.remove(&child);
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for it in d.items.borrow().iter() {
        let k = js(it, "kind");
        if let Some(slot) = counts.iter_mut().find(|(n, _)| *n == k) {
            slot.1 += 1;
        } else {
            counts.push((k, 1));
        }
    }
    for (kind, n) in counts {
        let chk = gtk::CheckButton::with_label(&format!("{kind} ({n})"));
        chk.set_active(!d.kinds_off.borrow().contains(&kind));
        {
            let d = d.clone();
            let kind = kind.clone();
            chk.connect_toggled(move |b| {
                if b.is_active() {
                    d.kinds_off.borrow_mut().remove(&kind);
                } else {
                    d.kinds_off.borrow_mut().insert(kind.clone());
                }
                render_list(&d);
            });
        }
        d.kinds_box.add(&chk);
    }
    d.kinds_box.show_all();
}

fn render_list(d: &Rc<GrabDlg>) {
    d.store.clear();
    for (i, it) in d.items.borrow().iter().enumerate() {
        if !visible(d, it) {
            continue;
        }
        let size = it
            .get("size")
            .and_then(|x| x.as_u64())
            .map(fmt_size)
            .unwrap_or_else(|| "—".to_string());
        let iter = d.store.append();
        d.store.set(
            &iter,
            &[
                (0, &!d.unchecked.borrow().contains(&i) as &dyn gtk::glib::ToValue),
                (1, &js(it, "name") as &dyn gtk::glib::ToValue),
                (2, &js(it, "label") as &dyn gtk::glib::ToValue),
                (3, &js(it, "kind") as &dyn gtk::glib::ToValue),
                (4, &size as &dyn gtk::glib::ToValue),
                (5, &(i as u32) as &dyn gtk::glib::ToValue),
            ],
        );
    }
    update_count(d);
}

/// Chrome for the current step. NEVER sets the notebook page
/// itself: it runs FROM the switch-page handler, and calling
/// set_page there recurses until the stack overflows (proven
/// live — the wizard died on the first Next click).
fn set_step(d: &GrabDlg, n: u32) {
    d.window.set_title(STEP_TITLES[n as usize]);
    d.title.set_text(STEP_TITLES[n as usize]);
    d.title.set_markup(&format!("<b>{}</b>", STEP_TITLES[n as usize]));
    d.btn_back.set_sensitive(n > 0);
    d.btn_next.set_visible(n < 3);
    d.btn_fetch.set_visible(n == 3);
    d.btn_download.set_visible(n == 4);
}

fn fetch(d: &Rc<GrabDlg>) {
    if d.exploring.get() {
        return;
    }
    let p = wizard_state(d);
    let start_url = js(&p, "start_url");
    if start_url.is_empty() {
        d.book.set_page(0);
        d.url.grab_focus();
        return;
    }
    d.book.set_page(4);
    let args = serde_json::json!({
        "url": start_url,
        "depth_same": p["depth_same"], "depth_other": p["depth_other"],
        "whole_site": p["whole_site"], "no_parent": p["no_parent"],
        "same_site_files_only": p["same_site_files_only"],
        "include_exts": p["include_exts"], "exclude_exts": p["exclude_exts"],
        "min_kb": p["min_kb"], "max_mb": p["max_mb"],
    });
    let Ok(args) = sink::grab_args(args) else {
        d.status.set_text("The exploration settings could not be read");
        return;
    };
    d.exploring.set(true);
    d.btn_fetch.set_sensitive(false);
    d.status.set_text("Exploring…");
    let (tx, rx) = gtk::glib::MainContext::channel::<Result<serde_json::Value, String>>(
        gtk::glib::Priority::DEFAULT,
    );
    {
        let ctx = d.ctx.clone();
        std::thread::spawn(move || {
            let r = sink::rt::block_on(sink::grab_explore(ctx, args)).map(|dto| sink::to_json(&dto));
            let _ = tx.send(r);
        });
    }
    let d2 = d.clone();
    rx.attach(None, move |r| {
        d2.exploring.set(false);
        d2.btn_fetch.set_sensitive(true);
        match r {
            Ok(res) => {
                let items = res
                    .get("items")
                    .and_then(|x| x.as_array())
                    .cloned()
                    .unwrap_or_default();
                let pages = res.get("pages_visited").and_then(|x| x.as_u64()).unwrap_or(0);
                let title = js(&res, "title");
                let n = items.len();
                *d2.items.borrow_mut() = items;
                d2.unchecked.borrow_mut().clear();
                d2.kinds_off.borrow_mut().clear();
                render_kinds(&d2);
                render_list(&d2);
                d2.status.set_text(&format!(
                    "Explored {} page{} — found {} files on \"{}\"",
                    pages,
                    if pages == 1 { "" } else { "s" },
                    n,
                    if title.is_empty() { js(&wizard_state(&d2), "start_url") } else { title }
                ));
            }
            Err(e) => d2.status.set_text(&e),
        }
        gtk::glib::ControlFlow::Break
    });
}

fn download_selected(d: &Rc<GrabDlg>) {
    let p = wizard_state(d);
    let base = js(&p, "save_dir");
    let base = if base.is_empty() { "~/Downloads".to_string() } else { base };
    let start_url = js(&p, "start_url");
    let chosen: Vec<serde_json::Value> = d
        .items
        .borrow()
        .iter()
        .enumerate()
        .filter(|(i, _)| !d.unchecked.borrow().contains(i))
        .map(|(_, it)| it.clone())
        .collect();
    if chosen.is_empty() {
        return;
    }
    d.btn_download.set_sensitive(false);
    // F1: with conversion on, the start page itself joins the run
    // (exploration crawls pages but returns only files) and the
    // whole run is registered for the post-completion rewrite
    let convert = jb(&p, "convert_links", false);
    let add_as_queue = jb(&p, "add_as_queue", false);
    if convert {
        let mut urls: Vec<String> = chosen.iter().map(|it| js(it, "url")).collect();
        urls.push(start_url.clone());
        sink::grab_register_run(urls);
    }
    // Snapshot the (url, dir) pairs here; the adds themselves
    // run on a worker thread — each can probe its server, and a
    // long grab list used to freeze the wizard once per file.
    let jobs: Vec<(String, String)> = chosen
        .iter()
        .map(|it| {
            let dir = if jb(&p, "use_subfolders", false) {
                grab_dir_for(&base, &start_url, &js(it, "url"))
            } else {
                base.clone()
            };
            (js(it, "url"), dir)
        })
        .collect();
    let total = jobs.len();
    let (tx, rx) = gtk::glib::MainContext::channel::<(usize, String)>(
        gtk::glib::Priority::DEFAULT,
    );
    let ctx = d.ctx.clone();
    std::thread::spawn(move || {
        if convert {
            let _ = sink::rt::block_on(sink::add_download(
                ctx.clone(),
                start_url,
                base,
                Some(add_as_queue),
                Some("Grabber".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ));
        }
        let mut failed = 0usize;
        let mut first_err = String::new();
        for (url, dir) in jobs {
            let r = sink::rt::block_on(sink::add_download(
                ctx.clone(),
                url,
                dir,
                Some(add_as_queue),
                Some("Grabber".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ));
            if let Err(e) = r {
                failed += 1;
                if first_err.is_empty() {
                    first_err = e;
                }
            }
        }
        let _ = tx.send((failed, first_err));
    });
    let d2 = d.clone();
    rx.attach(None, move |(failed, first_err)| {
        if failed > 0 {
            // plain language, wizard stays open (app.js)
            let why = if first_err.starts_with("duplicate") {
                "already in the download list".to_string()
            } else if first_err.chars().count() > 90 {
                format!("{}…", first_err.chars().take(90).collect::<String>())
            } else {
                first_err
            };
            d2.status.set_text(&format!(
                "{} of {} could not be added — {}",
                failed,
                total,
                why
            ));
            d2.btn_download.set_sensitive(true);
        } else {
            refresh_list(&d2.ui);
            d2.window.hide();
        }
        gtk::glib::ControlFlow::Break
    });
}

pub fn show(ui: Rc<Ui>, prefill: Option<String>, auto_explore: bool) {
    if let Some(d) = DLG.with(|c| c.borrow().clone()) {
        reset(&d);
        if let Some(u) = prefill {
            d.url.set_text(&u);
            if auto_explore {
                fetch(&d);
            }
        }
        d.window.present();
        return;
    }
    let window = gtk::Window::builder()
        .title(STEP_TITLES[0])
        .default_width(680)
        .default_height(600)
        .transient_for(&ui.window)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    window.add(&root);
    let title = gtk::Label::new(Some(STEP_TITLES[0]));
    title.set_halign(gtk::Align::Start);
    title.set_markup(&format!("<b>{}</b>", STEP_TITLES[0]));
    root.add(&title);
    let book = gtk::Notebook::new();
    book.set_show_tabs(true);
    root.pack_start(&book, true, true, 0);

    // ---- step 1: start page --------------------------------------
    let (g1, mut y) = (grid(), 0);
    let proj_name = entry("");
    proj_name.set_placeholder_text(Some("project_1"));
    y = row(&g1, y, "Grabber project name", &proj_name);
    let proj_sel = gtk::ComboBoxText::new();
    y = row(&g1, y, "Saved projects", &proj_sel);
    let proj_btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_load = gtk::Button::with_label("Load");
    let btn_psave = gtk::Button::with_label("Save project");
    let btn_pdel = gtk::Button::with_label("Delete");
    proj_btns.add(&btn_load);
    proj_btns.add(&btn_psave);
    proj_btns.add(&btn_pdel);
    g1.attach(&proj_btns, 1, y, 1, 1);
    y += 1;
    let url = entry("");
    url.set_placeholder_text(Some("https://example.com/page.html"));
    y = row(&g1, y, "Start page / address", &url);
    let template = combo(&TEMPLATES.map(|t| (t.0, t.1)), "custom");
    y = row(&g1, y, "Project template", &template);
    y = note(&g1, y, "Downloads from a site with a saved login (Options → Site Logins) use that login automatically.", false);
    let _ = y;
    book.append_page(&g1, Some(&gtk::Label::new(Some("1 · Start page"))));

    // ---- step 2: save files --------------------------------------
    let (g2, mut y) = (grid(), 0);
    let dir = entry("");
    y = row(&g2, y, "Save downloaded files to", &dir);
    let subfolders = check("Use the original relative subfolders", false);
    y = check_row(&g2, y, &subfolders);
    let add_queue = check("Add the files as a queue (Download Later) instead of starting them at once", false);
    y = check_row(&g2, y, &add_queue);
    // F1: on for projects the wizard creates; a loaded project
    // restores its own saved value (older projects: off)
    let convert = check("Convert links in the downloaded pages for offline browsing", true);
    y = check_row(&g2, y, &convert);
    // F2: the queue whose schedule re-runs this project
    let queue_sel = gtk::ComboBoxText::new();
    y = row(&g2, y, "Re-run on this queue's schedule", &queue_sel);
    let _ = y;
    book.append_page(&g2, Some(&gtk::Label::new(Some("2 · Save files"))));

    // ---- step 3: explore ------------------------------------------
    let (g3, mut y) = (grid(), 0);
    let mode_whole = gtk::RadioButton::with_label("Explore the whole site");
    y = check_row(&g3, y, &mode_whole);
    let mode_levels = gtk::RadioButton::with_label_from_widget(
        &mode_whole,
        "Explore the specified number of link levels:",
    );
    mode_levels.set_active(true);
    y = check_row(&g3, y, &mode_levels);
    let depth_same = spin(0.0, 10.0, 1.0, 0);
    y = row(&g3, y, "Levels within the start site", &depth_same);
    let depth_other = spin(0.0, 10.0, 0.0, 0);
    y = row(&g3, y, "Levels on other sites", &depth_other);
    let no_parent = check("Don't explore parent directories", false);
    y = check_row(&g3, y, &no_parent);
    y = note(&g3, y, "Exploration follows page links in static HTML. Pages built entirely by JavaScript are the browser extension's territory — the same split IDM uses.", false);
    let _ = y;
    book.append_page(&g3, Some(&gtk::Label::new(Some("3 · Explore"))));

    // ---- step 4: file filters --------------------------------------
    let (g4, mut y) = (grid(), 0);
    let preset = combo(
        &[
            ("", "All file types"),
            ("images", "Images files"),
            ("video", "Video files"),
            ("audio", "Audio files"),
            ("pdf", "PDF document files"),
            ("exe", "Executable files"),
            ("compressed", "Compressed files"),
            ("page_assets", "Complete start page (assets)"),
        ],
        "",
    );
    y = row(&g4, y, "Download the following files (filter preset)", &preset);
    let incl = entry("");
    incl.set_placeholder_text(Some("e.g. zip, pdf, mp4"));
    y = row(&g4, y, "…or list extensions yourself (comma separated)", &incl);
    let excl = entry("");
    excl.set_placeholder_text(Some("e.g. exe, dmg"));
    y = row(&g4, y, "Don't download these extensions", &excl);
    let same_site = check("Search files on this site only", true);
    y = check_row(&g4, y, &same_site);
    let min_kb = spin(0.0, 1_000_000.0, 0.0, 0);
    y = row(&g4, y, "Download if file size is not less than (KB)", &min_kb);
    let max_mb = spin(0.0, 100_000.0, 0.0, 0);
    y = row(&g4, y, "…and not more than (MB, 0 = no limit)", &max_mb);
    let _ = y;
    book.append_page(&g4, Some(&gtk::Label::new(Some("4 · File filters"))));

    // ---- step 5: results --------------------------------------------
    let g5 = gtk::Box::new(gtk::Orientation::Vertical, 6);
    g5.set_margin_start(10);
    g5.set_margin_end(10);
    g5.set_margin_top(8);
    let status = status_label();
    g5.add(&status);
    let kinds_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    g5.add(&kinds_box);
    let filter = entry("");
    filter.set_placeholder_text(Some("Filter by name or URL…"));
    g5.add(&filter);
    let selrow = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_all = gtk::Button::with_label("Select All");
    let btn_none = gtk::Button::with_label("Select None");
    let count = gtk::Label::new(Some("0 selected"));
    selrow.add(&btn_all);
    selrow.add(&btn_none);
    selrow.add(&count);
    g5.add(&selrow);
    let store = gtk::ListStore::new(&[
        gtk::glib::types::Type::BOOL,
        gtk::glib::types::Type::STRING,
        gtk::glib::types::Type::STRING,
        gtk::glib::types::Type::STRING,
        gtk::glib::types::Type::STRING,
        gtk::glib::types::Type::U32,
    ]);
    let view = gtk::TreeView::with_model(&store);
    let toggle = gtk::CellRendererToggle::new();
    {
        toggle.set_activatable(true);
        let col = gtk::TreeViewColumn::new();
        gtk::prelude::CellLayoutExt::pack_start(&col, &toggle, false);
        gtk::prelude::CellLayoutExt::add_attribute(&col, &toggle, "active", 0);
        view.append_column(&col);
        for (t, c, expand) in [("File", 1, true), ("Label", 2, false), ("Kind", 3, false), ("Size", 4, false)] {
            let col = gtk::TreeViewColumn::new();
            col.set_title(t);
            col.set_expand(expand);
            let cell = gtk::CellRendererText::new();
            if expand {
                cell.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
            }
            gtk::prelude::CellLayoutExt::pack_start(&col, &cell, true);
            gtk::prelude::CellLayoutExt::add_attribute(&col, &cell, "text", c);
            view.append_column(&col);
        }
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .shadow_type(gtk::ShadowType::In)
        .build();
    scroll.add(&view);
    g5.pack_start(&scroll, true, true, 0);
    book.append_page(&g5, Some(&gtk::Label::new(Some("Results"))));

    // ---- footer buttons ----------------------------------------------
    let btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    btns.set_halign(gtk::Align::End);
    let btn_back = gtk::Button::with_label("‹ Back");
    let btn_next = gtk::Button::with_label("Next ›");
    let btn_fetch = gtk::Button::with_label("Explore");
    let btn_download = gtk::Button::with_label("Download Selected (0)");
    let btn_cancel = gtk::Button::with_label("Cancel");
    btns.add(&btn_back);
    btns.add(&btn_next);
    btns.add(&btn_fetch);
    btns.add(&btn_download);
    btns.add(&btn_cancel);
    root.add(&btns);
    window.show_all();

    let d = Rc::new(GrabDlg {
        window: window.clone(),
        ui: ui.clone(),
        ctx: ui.ctx.clone(),
        title,
        book: book.clone(),
        btn_back: btn_back.clone(),
        btn_next: btn_next.clone(),
        btn_fetch: btn_fetch.clone(),
        btn_download: btn_download.clone(),
        proj_name,
        proj_sel,
        url,
        template,
        dir,
        subfolders,
        add_queue,
        convert,
        queue_sel,
        mode_whole,
        depth_same,
        depth_other,
        no_parent,
        incl,
        excl,
        same_site,
        min_kb,
        max_mb,
        status,
        kinds_box,
        filter,
        count,
        store,
        items: RefCell::new(Vec::new()),
        unchecked: RefCell::new(HashSet::new()),
        kinds_off: RefCell::new(HashSet::new()),
        exploring: Cell::new(false),
    });
    DLG.with(|c| *c.borrow_mut() = Some(d.clone()));

    {
        // selection by result index (F-25): flip the index set,
        // patch the row in place, recount — no full re-render.
        let d = d.clone();
        toggle.connect_toggled(move |_, path| {
            if let Some(iter) = d.store.iter(&path) {
                let idx = d.store.value(&iter, 5).get::<u32>().unwrap_or(0) as usize;
                let now_checked = if d.unchecked.borrow().contains(&idx) {
                    d.unchecked.borrow_mut().remove(&idx);
                    true
                } else {
                    d.unchecked.borrow_mut().insert(idx);
                    false
                };
                d.store
                    .set_value(&iter, 0, &now_checked.to_value());
                update_count(&d);
            }
        });
    }
    {
        let d = d.clone();
        book.connect_switch_page(move |_, _, page| set_step(&d, page));
    }
    {
        let d = d.clone();
        btn_back.connect_clicked(move |_| {
            let p = d.book.current_page().unwrap_or(0);
            if p > 0 {
                d.book.set_page(p as i32 - 1);
            }
        });
    }
    {
        let d = d.clone();
        btn_next.connect_clicked(move |_| {
            let p = d.book.current_page().unwrap_or(0);
            if p == 0 && d.url.text().trim().is_empty() {
                d.url.grab_focus();
                return;
            }
            if p < 3 {
                d.book.set_page(p as i32 + 1);
            }
        });
    }
    {
        let d = d.clone();
        btn_fetch.connect_clicked(move |_| fetch(&d));
    }
    {
        let d = d.clone();
        btn_download.connect_clicked(move |_| download_selected(&d));
    }
    {
        let w = window.clone();
        btn_cancel.connect_clicked(move |_| w.hide());
    }
    {
        let d = d.clone();
        window.connect_delete_event(move |w, _| {
            w.hide();
            gtk::glib::Propagation::Stop
        });
        let _ = &d;
    }
    {
        // template → pre-fill filters/depth/mode (app.js change handler)
        let d2 = d.clone();
        d.template.connect_changed(move |c| {
            let d = &d2;
            let key = combo_text(c);
            let Some(t) = TEMPLATES.iter().find(|t| t.0 == key) else {
                return;
            };
            if !t.2.is_empty() {
                d.incl.set_text(t.2);
            }
            if !t.3.is_empty() {
                d.excl.set_text(t.3);
            }
            d.mode_whole.set_active(t.5);
            if t.4 >= 0 {
                d.depth_same.set_value(t.4 as f64);
            }
        });
    }
    {
        let d = d.clone();
        preset.connect_changed(move |c| {
            if let Some(exts) = preset_exts(&combo_text(c)) {
                d.incl.set_text(exts);
            }
        });
    }
    {
        let d2 = d.clone();
        d.filter.connect_changed(move |_| render_list(&d2));
    }
    {
        let d = d.clone();
        btn_all.connect_clicked(move |_| {
            d.unchecked.borrow_mut().clear();
            render_list(&d);
        });
    }
    {
        let d = d.clone();
        btn_none.connect_clicked(move |_| {
            let n = d.items.borrow().len();
            *d.unchecked.borrow_mut() = (0..n).collect();
            render_list(&d);
        });
    }
    {
        let d = d.clone();
        btn_load.connect_clicked(move |_| {
            let name = combo_text(&d.proj_sel);
            if let Some(p) = settings_json(&d.ui)
                .get("grab_projects")
                .and_then(|x| x.as_array())
                .and_then(|list| list.iter().find(|p| js(p, "name") == name).cloned())
            {
                apply_project(&d, &p);
                set_notice(&d.ui, &format!("Project \"{name}\" loaded"));
            }
        });
    }
    {
        let d = d.clone();
        btn_psave.connect_clicked(move |_| {
            let p = wizard_state(&d);
            if js(&p, "name").is_empty() {
                d.proj_name.grab_focus();
                set_notice(&d.ui, "Give the project a name first");
                return;
            }
            if js(&p, "start_url").is_empty() {
                d.url.grab_focus();
                set_notice(&d.ui, "Give the project a start page first");
                return;
            }
            let name = js(&p, "name");
            let mut list: Vec<serde_json::Value> = settings_json(&d.ui)
                .get("grab_projects")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            list.retain(|x| !js(x, "name").eq_ignore_ascii_case(&name));
            list.push(p);
            match save_projects(&d.ui, serde_json::Value::Array(list)) {
                Ok(()) => {
                    fill_projects(&d);
                    d.proj_sel.set_active_id(Some(&name));
                    set_notice(&d.ui, &format!("Grabber project \"{name}\" saved"));
                }
                Err(e) => set_notice(&d.ui, &format!("Could not save the project: {e}")),
            }
        });
    }
    {
        let d = d.clone();
        btn_pdel.connect_clicked(move |_| {
            let name = combo_text(&d.proj_sel);
            if name.is_empty() {
                return;
            }
            let mut list: Vec<serde_json::Value> = settings_json(&d.ui)
                .get("grab_projects")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            list.retain(|x| js(x, "name") != name);
            if save_projects(&d.ui, serde_json::Value::Array(list)).is_ok() {
                fill_projects(&d);
                set_notice(&d.ui, &format!("Project \"{name}\" deleted"));
            }
        });
    }

    reset(&d);
    if let Some(u) = prefill {
        d.url.set_text(&u);
        if auto_explore {
            fetch(&d);
        }
    }
    window.present();
}

/// app.js openGrabber: fresh state every open.
fn reset(d: &Rc<GrabDlg>) {
    d.items.borrow_mut().clear();
    d.unchecked.borrow_mut().clear();
    d.kinds_off.borrow_mut().clear();
    d.url.set_text("");
    d.proj_name.set_text("");
    let def = js(&settings_json(&d.ui), "default_dir");
    d.dir.set_text(if def.is_empty() { "~/Downloads" } else { &def });
    d.filter.set_text("");
    render_kinds(d);
    render_list(d);
    d.status.set_text("The exploration results will appear here");
    fill_projects(d);
    fill_queues(d);
    set_step(d, 0);
    d.book.set_page(0);
}
