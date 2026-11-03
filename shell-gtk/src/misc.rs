//! The smaller dialogs of tranche 2 — Columns chooser and
//! Customize Toolbar, both over the shell-local prefs store
//! (prefs.rs), mirroring app.js's colmodal / tbmodal.

use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::form::*;
use crate::prefs;
use crate::{apply_columns, apply_toolbar, Ui};

// ---- Columns (app.js openColumnChooser) --------------------------

pub fn show_columns(ui: &Rc<Ui>) {
    let dlg = dialog(&ui.window, "Choose Columns", "OK", 460, 520);
    let content = dlg.content_area();
    let top = crate::form::grid();
    content.pack_start(&top, false, false, 0);
    let mut y = 0;
    y = note(&top, y, "Choose which columns the download list shows, and in what order. The file name column always stays.", true);
    let _ = y;
    let rows_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    rows_box.set_margin_start(10);
    rows_box.set_margin_end(10);
    content.pack_start(&rows_box, true, true, 0);
    let status = status_label();
    content.pack_end(&status, false, false, 0);
    let reset = gtk::Button::with_label("Reset to defaults");
    content.pack_end(&reset, false, false, 0);
    content.show_all();

    let edit: Rc<RefCell<Vec<prefs::ColumnCfg>>> =
        Rc::new(RefCell::new(ui.col_cfg.borrow().clone()));
    build_column_rows(&rows_box, &edit);
    {
        let edit = edit.clone();
        reset.connect_clicked(move |_| {
            *edit.borrow_mut() = prefs::default_columns();
            build_column_rows(&rows_box, &edit);
        });
    }
    if run(&dlg) == gtk::ResponseType::Ok {
        let cfg = edit.borrow().clone();
        prefs::save_columns(&cfg);
        *ui.col_cfg.borrow_mut() = cfg;
        apply_columns(ui);
    }
    dlg.hide();
}

fn build_column_rows(rows_box: &gtk::Box, edit: &Rc<RefCell<Vec<prefs::ColumnCfg>>>) {
    for child in rows_box.children() {
        rows_box.remove(&child);
    }
    let items = edit.borrow().clone();
    for (i, c) in items.iter().enumerate() {
        let (_, title, _, _) = prefs::COLUMN_DEFS
            .iter()
            .find(|(id, _, _, _)| *id == c.id)
            .copied()
            .unwrap_or(("name", "File Name", 0, false));
        let rowb = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let chk = gtk::CheckButton::with_label(title);
        chk.set_active(c.visible);
        if c.id == "name" {
            chk.set_sensitive(false);
        }
        rowb.pack_start(&chk, false, false, 0);
        let width = spin(0.0, 800.0, c.width as f64, 0);
        width.set_tooltip_text(Some("Column width in pixels (0 = automatic)"));
        rowb.pack_end(&width, false, false, 0);
        let wlabel = gtk::Label::new(Some("Width"));
        rowb.pack_end(&wlabel, false, false, 0);
        let up = gtk::Button::with_label("↑");
        let down = gtk::Button::with_label("↓");
        up.set_sensitive(i > 0);
        down.set_sensitive(i + 1 < items.len());
        rowb.pack_end(&down, false, false, 0);
        rowb.pack_end(&up, false, false, 0);
        rows_box.add(&rowb);
        {
            let edit = edit.clone();
            let id = c.id.clone();
            chk.connect_toggled(move |b| {
                if let Some(slot) = edit.borrow_mut().iter_mut().find(|x| x.id == id) {
                    slot.visible = b.is_active();
                }
            });
        }
        {
            let edit = edit.clone();
            let id = c.id.clone();
            width.connect_value_changed(move |s| {
                if let Some(slot) = edit.borrow_mut().iter_mut().find(|x| x.id == id) {
                    slot.width = s.value_as_int();
                }
            });
        }
        {
            let edit = edit.clone();
            let rows_box = rows_box.clone();
            up.connect_clicked(move |_| {
                let mut v = edit.borrow_mut();
                if i > 0 && i < v.len() {
                    v.swap(i, i - 1);
                }
                drop(v);
                build_column_rows(&rows_box, &edit);
            });
        }
        {
            let edit = edit.clone();
            let rows_box = rows_box.clone();
            down.connect_clicked(move |_| {
                let mut v = edit.borrow_mut();
                if i + 1 < v.len() {
                    v.swap(i, i + 1);
                }
                drop(v);
                build_column_rows(&rows_box, &edit);
            });
        }
    }
    rows_box.show_all();
}

// ---- Add Batch (app.js bmodal / bAdd) ----------------------------
//
// Parse/count semantics exactly as app.js (W193-15, N-07): every
// pasted line counts; lines starting with "http" are links, the
// rest are reported, never silently dropped; per-line failures
// reopen the dialog with only the failed + non-link lines left,
// each named with its reason (duplicates in plain language).

pub fn show_batch(ui: &Rc<Ui>) {
    batch_dialog(ui, String::new(), String::new(), None);
}

/// One round of the batch dialog. Validation failures reopen
/// it in place (the loop); add outcomes settle in batch_finish,
/// which reopens it here carrying only the failed lines.
fn batch_dialog(ui: &Rc<Ui>, mut text: String, mut status: String, dir_in: Option<String>) {
    use crate::dialogs::{run_dialog, FieldSpec, FieldValue};
    let dir0 = crate::form::js(&crate::settings_json(ui), "default_dir");
    let mut dir = dir_in.unwrap_or_else(|| dir0.clone());
    loop {
        let mut fields = Vec::new();
        if !status.is_empty() {
            fields.push(FieldSpec::readonly("status", "", &status));
        }
        fields.push(FieldSpec::textarea("links", "One download link per line", &text));
        fields.push(FieldSpec::entry("dir", "Save to folder", &dir));
        let Some(values) = run_dialog(&ui.window, "Add Batch Download", &fields, "Add All")
        else {
            return;
        };
        let get = |id: &str| match values.get(id) {
            Some(FieldValue::Text(s)) => s.clone(),
            _ => String::new(),
        };
        text = get("links");
        dir = get("dir");
        let raw: Vec<String> = text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let urls: Vec<&String> = raw.iter().filter(|l| crate::is_download_url(l)).collect();
        let invalid: Vec<&String> =
            raw.iter().filter(|l| !crate::is_download_url(l)).collect();
        if raw.is_empty() {
            status = "Paste the links first — one link per line (http, https or ftp).".to_string();
            continue;
        }
        if urls.is_empty() {
            status = format!(
                "No valid links found — {} line{} not a link. One link per line (http, https or ftp).",
                invalid.len(),
                if invalid.len() == 1 { " is" } else { "s are" }
            );
            text = raw.join("\n");
            continue;
        }
        let dir_use = if dir.trim().is_empty() { dir0.clone() } else { dir.trim().to_string() };
        // The adds run on a worker thread: each add can probe
        // its server, and a list of slow links used to freeze
        // the window once per line. batch_finish settles the
        // round when the results land.
        let (tx, rx) = gtk::glib::MainContext::channel::<Vec<(String, Result<String, String>)>>(
            gtk::glib::Priority::DEFAULT,
        );
        let ctx = ui.ctx.clone();
        let batch: Vec<String> = urls.iter().map(|u| (*u).clone()).collect();
        std::thread::spawn(move || {
            let mut results = Vec::new();
            for url in batch {
                let r = crate::sink::rt::block_on(crate::sink::add_download(
                    ctx.clone(),
                    url.clone(),
                    dir_use.clone(),
                    Some(false),
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
                    None,
                ));
                results.push((url, r));
            }
            let _ = tx.send(results);
        });
        let ui2 = ui.clone();
        let invalid: Vec<String> = invalid.iter().map(|s| (*s).clone()).collect();
        let mut payload = Some((raw.clone(), invalid, dir.clone()));
        rx.attach(None, move |results| {
            if let Some((r, i, dd)) = payload.take() {
                batch_finish(&ui2, r, i, results, dd);
            }
            gtk::glib::ControlFlow::Break
        });
        return;
    }
}

/// Settle one batch round on the GTK thread: all-clean closes
/// the flow with the summary notice; failures reopen the
/// dialog carrying only the failed + non-link lines, each
/// named with its reason (the loop's original tail, verbatim).
fn batch_finish(
    ui: &Rc<Ui>,
    raw: Vec<String>,
    invalid: Vec<String>,
    results: Vec<(String, Result<String, String>)>,
    dir: String,
) {
    let total_links = results.len();
    let mut ok = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    for (url, r) in results {
        match r {
            Ok(_) => ok += 1,
            Err(e) => {
                let reason = if e.starts_with("duplicate:") {
                    "already in the download list".to_string()
                } else if e.chars().count() > 140 {
                    format!("{}…", e.chars().take(140).collect::<String>())
                } else {
                    e
                };
                failed.push((url, reason));
            }
        }
    }
    if failed.is_empty() && invalid.is_empty() {
        crate::refresh_list(ui);
        crate::set_notice(ui, &format!("Added {} of {} downloads", ok, total_links));
        return;
    }
    let failed_urls: Vec<&str> = failed.iter().map(|(u, _)| u.as_str()).collect();
    let text = raw
        .iter()
        .filter(|l| failed_urls.contains(&l.as_str()) || !crate::is_download_url(l))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let mut s = format!("Added {} of {} lines", ok, raw.len());
    if !failed.is_empty() {
        s += &format!(" — {} failed:\n", failed.len());
        for (u, r) in failed.iter().take(12) {
            s += &format!("• {} — {}\n", u, r);
        }
        if failed.len() > 12 {
            s += &format!("…and {} more", failed.len() - 12);
        }
    }
    if !invalid.is_empty() {
        s += &format!(
            "\n{} line{} not a link and could not be added.",
            invalid.len(),
            if invalid.len() == 1 { " is" } else { "s are" }
        );
    }
    let status = s.trim_end().to_string();
    batch_dialog(ui, text, status, Some(dir));
}

// ---- Shortcuts (app.js kmodal, static list verbatim) ------------

pub fn show_shortcuts(ui: &Rc<Ui>) {
    use crate::dialogs::FieldSpec;
    let fields = [
        FieldSpec::readonly("k1", "Ctrl+N", "Add New Download"),
        FieldSpec::readonly("k2", "Ctrl+Shift+V", "Add Video"),
        FieldSpec::readonly("k3", "F5", "Refresh List"),
        FieldSpec::readonly("k4", "Esc", "Close Dialog / Menu"),
        FieldSpec::readonly("k5", "Ctrl+Q", "Exit NDM"),
        FieldSpec::readonly("k6", "Ctrl+A", "Select All Downloads"),
    ];
    crate::dialogs::run_info_dialog(&ui.window, "Keyboard Shortcuts", &fields, "OK");
}

// ---- Customize Toolbar (app.js openToolbarCfg) -------------------

pub fn show_toolbar_cfg(ui: &Rc<Ui>) {
    let dlg = dialog(&ui.window, "Customize Toolbar", "OK", 420, 480);
    let content = dlg.content_area();
    let g = crate::form::grid();
    content.pack_start(&g, true, true, 0);
    let mut y = 0;
    y = note(&g, y, "Show or hide toolbar buttons, and choose the button size.", true);
    let size = combo(&[("large", "Large buttons"), ("small", "Small buttons")], &prefs::load_toolbar().size);
    y = row(&g, y, "Button size", &size);
    let cur = prefs::load_toolbar();
    let mut checks: Vec<(String, gtk::CheckButton)> = Vec::new();
    for (id, label) in prefs::TOOLBAR_DEFS {
        let chk = check(label, !cur.hidden.iter().any(|h| h == id));
        y = check_row(&g, y, &chk);
        checks.push((id.to_string(), chk));
    }
    content.show_all();
    if run(&dlg) == gtk::ResponseType::Ok {
        let hidden: Vec<String> = checks
            .iter()
            .filter(|(_, b)| !b.is_active())
            .map(|(id, _)| id.clone())
            .collect();
        prefs::save_toolbar(&prefs::ToolbarCfg {
            hidden,
            size: combo_text(&size),
        });
        apply_toolbar(ui);
    }
    dlg.hide();
}
