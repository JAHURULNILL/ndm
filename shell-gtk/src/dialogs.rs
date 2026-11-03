//! Dialog builder — the GTK twin of the Windows shell's dialog
//! helper (NATIVE-PLAN.md §2/§4: "a thin in-house helper layer,
//! written once and reused by every dialog").
//!
//! A dialog is a field table: each row is a label plus one widget
//! (entry / password entry / combo / checkbutton / read-only
//! value). `run_dialog` builds a modal dialog from the table,
//! runs it, and returns the typed values on OK — `None` on
//! cancel/close. Phase 5 builds the 17 parity dialogs
//! (NATIVE-PLAN.md §1.6) on this one helper instead of
//! hand-rolling GTK per dialog.

use gtk::prelude::*;
use std::cell::Cell;
use std::collections::HashMap;

thread_local! {
    /// Depth of builder dialogs currently on screen. The
    /// Download Complete dialog reads this through
    /// `modal_open()`: completions yield while a real dialog
    /// is up (app.js showNextComplete's rule) and are pumped
    /// when the last one closes (see the end of `run_dialog`).
    static MODAL_DEPTH: Cell<u32> = Cell::new(0);
}

/// True while any builder dialog is running.
pub fn modal_open() -> bool {
    MODAL_DEPTH.with(|d| d.get() > 0)
}

/// Mark a hand-built modal dialog (the tabbed / wizard dialogs
/// in options.rs, scheduler.rs, …) as open/closed, so Download
/// Complete yields to it exactly as it yields to builder
/// dialogs. Callers pair the calls; `modal_exit` pumps the
/// completion queue when the last modal closes, like
/// `run_dialog` does.
pub fn modal_enter() {
    MODAL_DEPTH.with(|d| d.set(d.get() + 1));
}

pub fn modal_exit() {
    MODAL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    if !modal_open() {
        crate::modals::pump_complete();
        crate::pump_file_offers();
    }
}

/// One row of a dialog's field table.
pub struct FieldSpec {
    /// Key the returned value is stored under.
    pub id: &'static str,
    /// Row label shown left of the widget.
    pub label: &'static str,
    pub kind: FieldKind,
    /// Initial text (entry / combo selection / read-only value).
    pub initial: String,
    /// Initial state for `FieldKind::Check`.
    pub initial_bool: bool,
}

pub enum FieldKind {
    /// Single-line text entry.
    Entry,
    /// Drop-down with the given choices.
    Combo(Vec<String>),
    /// Checkbutton; the label sits on the button itself.
    Check,
    /// Read-only value text (About-style rows, info panes).
    ReadOnly,
    /// Multi-line text area (the Import paste box).
    TextArea,
}

impl FieldSpec {
    pub fn entry(id: &'static str, label: &'static str, initial: &str) -> Self {
        FieldSpec {
            id,
            label,
            kind: FieldKind::Entry,
            initial: initial.to_string(),
            initial_bool: false,
        }
    }

    pub fn combo(
        id: &'static str,
        label: &'static str,
        choices: Vec<String>,
        initial: &str,
    ) -> Self {
        FieldSpec {
            id,
            label,
            kind: FieldKind::Combo(choices),
            initial: initial.to_string(),
            initial_bool: false,
        }
    }

    pub fn check(id: &'static str, label: &'static str, initial_bool: bool) -> Self {
        FieldSpec {
            id,
            label,
            kind: FieldKind::Check,
            initial: String::new(),
            initial_bool,
        }
    }

    pub fn readonly(id: &'static str, label: &'static str, value: &str) -> Self {
        FieldSpec {
            id,
            label,
            kind: FieldKind::ReadOnly,
            initial: value.to_string(),
            initial_bool: false,
        }
    }

    pub fn textarea(id: &'static str, label: &'static str, initial: &str) -> Self {
        FieldSpec {
            id,
            label,
            kind: FieldKind::TextArea,
            initial: initial.to_string(),
            initial_bool: false,
        }
    }
}

/// A value collected from a dialog row.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    Text(String),
    Bool(bool),
}

impl FieldValue {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            FieldValue::Text(s) => Some(s),
            FieldValue::Bool(_) => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            FieldValue::Bool(b) => Some(*b),
            FieldValue::Text(_) => None,
        }
    }
}

/// Widgets built for one `run_dialog` call, kept so the values
/// can be read back when the user confirms.
enum RowWidget {
    Entry(gtk::Entry),
    Combo(gtk::ComboBoxText),
    Check(gtk::CheckButton),
    ReadOnly,
    Text(gtk::TextView),
}

/// Build and run a modal dialog from a field table.
///
/// Returns `Some(values)` when the user presses the OK button,
/// `None` on Cancel / window close. The dialog is transient for
/// `parent` and modal, matching the web UI's one-modal-layer rule
/// (app.js N-08): callers open at most one at a time.
pub fn run_dialog(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    fields: &[FieldSpec],
    ok_label: &str,
) -> Option<HashMap<String, FieldValue>> {
    run_dialog_impl(parent, title, fields, ok_label, true)
}

/// A read-only information dialog (About, Keyboard Shortcuts):
/// one dismiss button only. The shared builder's Cancel twin
/// made no sense there — Cancel and OK did the same thing,
/// which reads as an unfinished dialog (2.3.1 consistency fix).
pub fn run_info_dialog(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    fields: &[FieldSpec],
    ok_label: &str,
) -> Option<HashMap<String, FieldValue>> {
    run_dialog_impl(parent, title, fields, ok_label, false)
}

fn run_dialog_impl(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    fields: &[FieldSpec],
    ok_label: &str,
    with_cancel: bool,
) -> Option<HashMap<String, FieldValue>> {
    let buttons: Vec<(&str, gtk::ResponseType)> = if with_cancel {
        vec![
            (ok_label, gtk::ResponseType::Ok),
            ("Cancel", gtk::ResponseType::Cancel),
        ]
    } else {
        vec![(ok_label, gtk::ResponseType::Ok)]
    };
    let dialog = gtk::Dialog::with_buttons(
        Some(title),
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &buttons
            .iter()
            .map(|(l, r)| (*l, *r))
            .collect::<Vec<_>>()[..],
    );
    dialog.set_default_response(gtk::ResponseType::Ok);

    let grid = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(12)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();

    let mut rows: Vec<(&'static str, RowWidget)> = Vec::new();
    // The first editable field takes the focus when the dialog
    // opens (the Add URL dialog opens ready to type/paste).
    let mut first_focus: Option<gtk::Widget> = None;
    for (i, f) in fields.iter().enumerate() {
        let row = i as i32;
        match &f.kind {
            FieldKind::Check => {
                let check = gtk::CheckButton::with_label(f.label);
                check.set_active(f.initial_bool);
                grid.attach(&check, 0, row, 2, 1);
                rows.push((f.id, RowWidget::Check(check)));
            }
            FieldKind::ReadOnly => {
                let label = gtk::Label::new(Some(f.label));
                label.set_halign(gtk::Align::Start);
                label.set_valign(gtk::Align::Baseline);
                let value = gtk::Label::new(Some(&f.initial));
                value.set_halign(gtk::Align::Start);
                value.set_valign(gtk::Align::Baseline);
                value.set_selectable(true);
                // Wrap long values (URLs, explanatory notes):
                // unwrapped, one long line stretched the Refresh
                // Address dialog to 1908 px.
                value.set_line_wrap(true);
                value.set_max_width_chars(72);
                grid.attach(&label, 0, row, 1, 1);
                grid.attach(&value, 1, row, 1, 1);
                rows.push((f.id, RowWidget::ReadOnly));
            }
            FieldKind::Entry => {
                let label = gtk::Label::new(Some(f.label));
                label.set_halign(gtk::Align::Start);
                label.set_valign(gtk::Align::Baseline);
                let entry = gtk::Entry::new();
                entry.set_text(&f.initial);
                entry.set_hexpand(true);
                entry.set_valign(gtk::Align::Baseline);
                // Enter in any entry confirms the dialog.
                entry.set_activates_default(true);
                if first_focus.is_none() {
                    first_focus = Some(entry.clone().upcast::<gtk::Widget>());
                }
                grid.attach(&label, 0, row, 1, 1);
                grid.attach(&entry, 1, row, 1, 1);
                rows.push((f.id, RowWidget::Entry(entry)));
            }
            FieldKind::TextArea => {
                let label = gtk::Label::new(Some(f.label));
                label.set_halign(gtk::Align::Start);
                label.set_valign(gtk::Align::Start);
                let view = gtk::TextView::new();
                view.set_wrap_mode(gtk::WrapMode::WordChar);
                view.buffer().unwrap().set_text(&f.initial);
                let scroll = gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .vscrollbar_policy(gtk::PolicyType::Automatic)
                    .shadow_type(gtk::ShadowType::In)
                    .build();
                scroll.set_size_request(460, 150);
                scroll.add(&view);
                grid.attach(&label, 0, row, 1, 1);
                grid.attach(&scroll, 1, row, 1, 1);
                rows.push((f.id, RowWidget::Text(view)));
            }
            FieldKind::Combo(choices) => {
                let label = gtk::Label::new(Some(f.label));
                label.set_halign(gtk::Align::Start);
                label.set_valign(gtk::Align::Baseline);
                let combo = gtk::ComboBoxText::new();
                combo.set_valign(gtk::Align::Baseline);
                for c in choices {
                    combo.append_text(c);
                }
                if !f.initial.is_empty() {
                    combo.set_active_id(Some(&f.initial));
                    // set_active_id matches the row *id*, which for
                    // append_text rows is the text itself only when
                    // set; fall back to a text scan.
                    if combo.active().is_none() {
                        for (idx, c) in choices.iter().enumerate() {
                            if *c == f.initial {
                                combo.set_active(Some(idx as u32));
                                break;
                            }
                        }
                    }
                } else if !choices.is_empty() {
                    combo.set_active(Some(0));
                }
                grid.attach(&label, 0, row, 1, 1);
                grid.attach(&combo, 1, row, 1, 1);
                rows.push((f.id, RowWidget::Combo(combo)));
            }
        }
    }

    let content = dialog.content_area();
    content.add(&grid);
    dialog.show_all();
    if let Some(w) = &first_focus {
        w.grab_focus();
    }

    MODAL_DEPTH.with(|d| d.set(d.get() + 1));
    let response = dialog.run();
    MODAL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    let result = if response == gtk::ResponseType::Ok {
        let mut values = HashMap::new();
        for (id, widget) in &rows {
            let value = match widget {
                RowWidget::Entry(e) => FieldValue::Text(e.text().to_string()),
                RowWidget::Combo(c) => {
                    FieldValue::Text(c.active_text().map(|s| s.to_string()).unwrap_or_default())
                }
                RowWidget::Check(c) => FieldValue::Bool(c.is_active()),
                RowWidget::Text(v) => {
                    let b = v.buffer().unwrap();
                    FieldValue::Text(
                        b.text(&b.start_iter(), &b.end_iter(), false)
                            .map(|s| s.to_string())
                            .unwrap_or_default(),
                    )
                }
                RowWidget::ReadOnly => continue,
            };
            values.insert((*id).to_string(), value);
        }
        Some(values)
    } else {
        None
    };
    dialog.hide();
    // A completion that landed while this dialog was up gets
    // its own dialog now (app.js: every modal-close path calls
    // showNextComplete back) — and so does a file offer that
    // queued behind this dialog (pumpFileLinks' same rule).
    if !modal_open() {
        crate::modals::pump_complete();
        crate::pump_file_offers();
    }
    result
}

/// The About dialog's field table — the builder's worked example,
/// mirroring the web UI's About modal (ui/index.html `#amodal`):
/// product name, version, video-engine line and the honest
/// components note, all read-only rows.
pub fn about_field_table() -> Vec<FieldSpec> {
    vec![
        FieldSpec::readonly("name", "Name", "Nill Download Manager"),
        FieldSpec::readonly(
            "version",
            "Version",
            &format!("{} (native GTK shell)", env!("CARGO_PKG_VERSION")),
        ),
        FieldSpec::readonly(
            "engine",
            "Video engine",
            "yt-dlp / deno — downloaded on first use, verified by SHA-256",
        ),
        FieldSpec::readonly(
            "about",
            "About",
            "Fast, resumable downloads with browser integration\n\
             for files and videos from 1,700+ sites.",
        ),
    ]
}

/// Show the About dialog (the worked example for `run_dialog`).
pub fn show_about(parent: &impl IsA<gtk::Window>) {
    let _ = run_info_dialog(parent, "About NDM", &about_field_table(), "OK");
}

/// The generic confirmation (app.js showConfirm / #cfmodal):
/// a message and one affirmative button; Cancel, the window
/// close and Esc all answer false. Resolves true only on the
/// affirmative button.
pub fn show_confirm(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    msg: &str,
    yes_label: &str,
) -> bool {
    let fields = [FieldSpec::readonly("msg", "", msg)];
    run_dialog(parent, title, &fields, yes_label).is_some()
}

/// The generic text prompt (app.js showPrompt / #pmodal): a
/// message plus one input, prefilled with `initial`. Some(text)
/// on OK (Enter confirms — the builder's entries activate the
/// default), None on Cancel / close / Esc.
pub fn show_prompt(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    msg: &str,
    initial: &str,
) -> Option<String> {
    let mut fields = Vec::new();
    if !msg.is_empty() {
        fields.push(FieldSpec::readonly("msg", "", msg));
    }
    fields.push(FieldSpec::entry("value", "", initial));
    let values = run_dialog(parent, title, &fields, "OK")?;
    values
        .get("value")
        .and_then(|v| v.as_text())
        .map(|s| s.to_string())
}
