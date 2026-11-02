//! Small widget helpers for the hand-built modal dialogs
//! (Options, Scheduler, Video, Grabber, Columns, Toolbar) —
//! the rows the field-table builder in dialogs.rs cannot
//! express (tabs, staged lists, radio groups). One construction
//! style everywhere: label-left grids, 6/12 spacing, bounded
//! dialog sizes (the wave-1 runaway-width lesson).

use gtk::prelude::*;

/// A modal dialog with an OK-style primary button + Cancel.
/// Sizes are always explicit and bounded.
pub fn dialog(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    ok_label: &str,
    width: i32,
    height: i32,
) -> gtk::Dialog {
    let d = gtk::Dialog::with_buttons(
        Some(title),
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[(ok_label, gtk::ResponseType::Ok), ("Cancel", gtk::ResponseType::Cancel)],
    );
    d.set_default_size(width, height);
    d.set_default_response(gtk::ResponseType::Ok);
    d
}

/// Run a hand-built dialog modally (with the modal-depth book-
/// keeping Download Complete respects). Returns the response.
pub fn run(d: &gtk::Dialog) -> gtk::ResponseType {
    crate::dialogs::modal_enter();
    let r = d.run();
    crate::dialogs::modal_exit();
    r
}

pub fn grid() -> gtk::Grid {
    gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(12)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build()
}

/// Attach `label` + widget as one grid row; returns the row.
/// Labels wrap at a bounded width — an unwrapped long label
/// stretched dialogs past their set size (wave-1 lesson).
pub fn row(g: &gtk::Grid, y: i32, label: &str, w: &impl IsA<gtk::Widget>) -> i32 {
    let l = gtk::Label::new(Some(label));
    l.set_halign(gtk::Align::Start);
    l.set_valign(gtk::Align::Center);
    l.set_line_wrap(true);
    l.set_max_width_chars(44);
    l.set_width_chars(18);
    g.attach(&l, 0, y, 1, 1);
    g.attach(w, 1, y, 1, 1);
    y + 1
}

/// A full-width note line (wraps, bounded — never stretches the
/// dialog the way an unwrapped URL line did in wave 1).
pub fn note(g: &gtk::Grid, y: i32, text: &str, bold: bool) -> i32 {
    let l = gtk::Label::new(None);
    if bold {
        l.set_markup(&format!(
            "<b>{}</b>",
            gtk::glib::markup_escape_text(text)
        ));
    } else {
        l.set_text(text);
    }
    l.set_halign(gtk::Align::Start);
    l.set_line_wrap(true);
    l.set_max_width_chars(72);
    g.attach(&l, 0, y, 2, 1);
    y + 1
}

pub fn entry(initial: &str) -> gtk::Entry {
    let e = gtk::Entry::new();
    e.set_text(initial);
    e.set_hexpand(true);
    e.set_activates_default(true);
    e
}

pub fn secret(initial: &str) -> gtk::Entry {
    let e = entry(initial);
    e.set_visibility(false);
    e.set_input_purpose(gtk::InputPurpose::Password);
    e
}

pub fn check(label: &str, on: bool) -> gtk::CheckButton {
    let c = gtk::CheckButton::with_label(label);
    c.set_active(on);
    c
}

/// A widget row spanning the grid (checks, radios, buttons).
pub fn check_row(g: &gtk::Grid, y: i32, c: &impl IsA<gtk::Widget>) -> i32 {
    g.attach(c, 0, y, 2, 1);
    y + 1
}

pub fn spin(min: f64, max: f64, val: f64, digits: u32) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(min, max, if digits > 0 { 0.5 } else { 1.0 });
    s.set_digits(digits);
    s.set_value(val);
    s.set_activates_default(true);
    s
}

/// Combo over (value, label) pairs; `initial` selects by value.
pub fn combo(pairs: &[(&str, &str)], initial: &str) -> gtk::ComboBoxText {
    let c = gtk::ComboBoxText::new();
    for (v, l) in pairs {
        c.append(Some(v), l);
    }
    c.set_active_id(Some(if pairs.iter().any(|(v, _)| *v == initial) {
        initial
    } else {
        pairs.first().map(|(v, _)| *v).unwrap_or("")
    }));
    c
}

pub fn combo_text(c: &gtk::ComboBoxText) -> String {
    c.active_id().map(|s| s.to_string()).unwrap_or_default()
}

pub fn status_label() -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_halign(gtk::Align::Start);
    l.set_line_wrap(true);
    l.set_max_width_chars(72);
    l
}

/// Read a JSON string field with a fallback.
pub fn js(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

pub fn jb(v: &serde_json::Value, key: &str, dflt: bool) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(dflt)
}

pub fn ju(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// Split a comma/space separated extension list the way app.js
/// does (trim, strip leading dot, lowercase, dedupe, no blanks).
pub fn parse_exts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in text.split(|c: char| c == ',' || c.is_whitespace()) {
        let e = part.trim().trim_start_matches('.').to_ascii_lowercase();
        if !e.is_empty() && !out.contains(&e) {
            out.push(e);
        }
    }
    out
}
