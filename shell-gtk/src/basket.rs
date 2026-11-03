//! Drop Basket (ui/basket.html + basket.js) — a small
//! borderless always-on-top window the shell owns outright
//! (core has no set_basket_visible; the Tauri command was a
//! shell seam). Link drops go to core's basket_add (the Add
//! URL offer path); file drops go to handle_dropped_path
//! (on_basket = true), whose answer returns as a BasketNote
//! event. Notes clear after 2.6 s (basket.js timing).

use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::sink;
use crate::Ui;

thread_local! {
    static DLG: RefCell<Option<Rc<Basket>>> = RefCell::new(None);
}

struct Basket {
    window: gtk::Window,
    note: gtk::Label,
    note_src: RefCell<Option<gtk::glib::SourceId>>,
}

fn build(ui: &Rc<Ui>) -> Rc<Basket> {
    let window = gtk::Window::builder()
        .title("NDM Drop Basket")
        .default_width(232)
        .default_height(118)
        .decorated(false)
        .resizable(false)
        .skip_taskbar_hint(true)
        .type_hint(gtk::gdk::WindowTypeHint::Utility)
        .build();
    window.set_keep_above(true);
    let frame = gtk::EventBox::new();
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    vbox.set_margin_top(10);
    vbox.set_margin_bottom(10);
    frame.add(&vbox);
    window.add(&frame);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let title = gtk::Label::new(None);
    title.set_markup("<b>⬇ NDM Basket</b>");
    title.set_halign(gtk::Align::Start);
    head.pack_start(&title, true, true, 0);
    let close = gtk::Button::with_label("×");
    close.set_tooltip_text(Some("Hide the basket"));
    close.set_relief(gtk::ReliefStyle::None);
    head.add(&close);
    vbox.add(&head);
    let hint = gtk::Label::new(Some("Drop a download link here"));
    hint.set_halign(gtk::Align::Start);
    vbox.add(&hint);
    let note = gtk::Label::new(None);
    note.set_halign(gtk::Align::Start);
    note.set_line_wrap(true);
    vbox.add(&note);
    window.show_all();
    window.hide();

    let b = Rc::new(Basket {
        window: window.clone(),
        note,
        note_src: RefCell::new(None),
    });
    DLG.with(|c| *c.borrow_mut() = Some(b.clone()));

    {
        let ui = ui.clone();
        close.connect_clicked(move |_| set_visible(&ui, false));
    }
    {
        // drag the basket by its background (basket.js's
        // drag region): a press on the frame starts the WM move.
        let w = window.clone();
        frame.connect_button_press_event(move |_, ev| {
            if ev.button() == 1 {
                w.begin_move_drag(
                    ev.button() as i32,
                    ev.root().0 as i32,
                    ev.root().1 as i32,
                    ev.time(),
                );
            }
            gtk::glib::Propagation::Proceed
        });
    }
    // Drops: link text (uri-list / plain) and OS files.
    let targets = [
        gtk::TargetEntry::new("text/uri-list", gtk::TargetFlags::OTHER_APP, 0),
        gtk::TargetEntry::new("text/plain", gtk::TargetFlags::OTHER_APP, 1),
    ];
    window.drag_dest_set(gtk::DestDefaults::ALL, &targets, gtk::gdk::DragAction::COPY);
    {
        let ui = ui.clone();
        let b2 = b.clone();
        window.connect_drag_data_received(move |_, _, _, _, data, _, _| {
            on_drop(&ui, &b2, data);
        });
    }
    b
}

fn on_drop(ui: &Rc<Ui>, b: &Rc<Basket>, data: &gtk::SelectionData) {
    // OS files arrive as file:// URIs — core answers them with
    // a BasketNote (the reason, or "Sent to NDM ✓").
    let uris = data.uris();
    let files: Vec<String> = uris
        .iter()
        .map(|u| u.to_string())
        .filter(|u| u.starts_with("file://"))
        .collect();
    if !files.is_empty() {
        for u in &files {
            if let Some(path) = gtk::gio::File::for_uri(u).path() {
                sink::handle_dropped_path(&ui.ctx, &path, true);
            }
        }
        return;
    }
    // Link text (basket.js): the uri-list's first non-comment
    // line wins, else the plain text.
    let text = data
        .text()
        .map(|t| t.to_string())
        .unwrap_or_else(|| uris.first().map(|u| u.to_string()).unwrap_or_default());
    let candidate = text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("")
        .to_string();
    if crate::is_download_url(&candidate) {
        match sink::basket_add(ui.ctx.clone(), candidate) {
            Ok(()) => show_note(b, "Sent to NDM ✓"),
            Err(_) => show_note(b, "Could not send the link"),
        }
    } else {
        show_note(b, "That was not a web link");
    }
}

fn ensure(ui: &Rc<Ui>) -> Rc<Basket> {
    DLG.with(|c| c.borrow().clone()).unwrap_or_else(|| build(ui))
}

pub fn is_visible(ui: &Rc<Ui>) -> bool {
    let _ = ui;
    DLG.with(|c| {
        c.borrow()
            .as_ref()
            .map(|b| b.window.is_visible())
            .unwrap_or(false)
    })
}

pub fn set_visible(ui: &Rc<Ui>, visible: bool) {
    if visible {
        let b = ensure(ui);
        b.window.present();
    } else if let Some(b) = DLG.with(|c| c.borrow().clone()) {
        b.window.hide();
    }
    // keep the View-menu check in step, whichever side moved
    if let Some(item) = ui.basket_check.borrow().as_ref() {
        if item.is_active() != visible {
            item.set_active(visible);
        }
    }
}

/// A core BasketNote (e.g. the answer to a dropped file) shows
/// on the basket for basket.js's 2.6 s when it is up.
pub fn on_basket_note(ui: &Rc<Ui>, text: &str) {
    if is_visible(ui) {
        if let Some(b) = DLG.with(|c| c.borrow().clone()) {
            show_note(&b, text);
        }
    }
}

fn show_note(b: &Rc<Basket>, text: &str) {
    b.note.set_text(text);
    if let Some(src) = b.note_src.borrow_mut().take() {
        src.remove();
    }
    let b2 = b.clone();
    let src = gtk::glib::timeout_add_local(std::time::Duration::from_millis(2600), move || {
        b2.note.set_text("");
        *b2.note_src.borrow_mut() = None;
        gtk::glib::ControlFlow::Break
    });
    *b.note_src.borrow_mut() = Some(src);
}
