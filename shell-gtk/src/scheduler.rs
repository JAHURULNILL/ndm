//! Scheduler dialog (app.js smodal) — per-queue schedule/caps
//! over Settings.queues. Field edits are staged; queue switches
//! guard unsaved edits with a discard confirm; New/Delete queue
//! save immediately (app.js); the file order is staged and only
//! reaches core (move_in_queue) on Save (app.js F-13). Start /
//! Stop Now call core directly and core's own answer is shown —
//! an error verbatim, never a success the start did not earn.

use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::form::*;
use crate::sink;
use crate::{rebuild_category_tree, refresh_list, settings_json, Ui};

const MAIN_Q: &str = "Main download queue";

struct Sched {
    ui: Rc<Ui>,
    sel: RefCell<String>,
    dirty: Cell<bool>,
    guard: Cell<bool>,
    base: RefCell<serde_json::Value>,
    queue_combo: gtk::ComboBoxText,
    enabled: gtk::CheckButton,
    start: gtk::Entry,
    stop: gtk::Entry,
    days: Vec<gtk::CheckButton>,
    one_time: gtk::Entry,
    expired: gtk::Label,
    sync: gtk::CheckButton,
    exit_done: gtk::CheckButton,
    power: gtk::ComboBoxText,
    max: gtk::SpinButton,
    files: gtk::ListStore,
    files_view: gtk::TreeView,
    order: RefCell<Vec<String>>,
    new_name: gtk::Entry,
    btn_delete: gtk::Button,
    status: gtk::Label,
}

fn queue_cfg(base: &serde_json::Value, name: &str) -> serde_json::Value {
    base.get("queues")
        .and_then(|x| x.as_array())
        .and_then(|qs| qs.iter().find(|q| js(q, "name") == name).cloned())
        .unwrap_or_else(|| {
            serde_json::json!({
                "name": name, "use_schedule": false, "start": "22:00", "stop": "06:00",
                "exit_when_done": false, "power_when_done": "", "max_simultaneous": 0,
                "days": [true, true, true, true, true, true, true], "one_time": "", "sync": false
            })
        })
}

fn queue_names_of(base: &serde_json::Value) -> Vec<String> {
    let names: Vec<String> = base
        .get("queues")
        .and_then(|x| x.as_array())
        .map(|qs| qs.iter().map(|q| js(q, "name")).filter(|n| !n.is_empty()).collect())
        .unwrap_or_default();
    if names.is_empty() {
        vec![MAIN_Q.to_string()]
    } else {
        names
    }
}

fn fill_fields(s: &Rc<Sched>) {
    let base = s.base.borrow().clone();
    let cfg = queue_cfg(&base, &s.sel.borrow());
    s.guard.set(true);
    s.enabled.set_active(jb(&cfg, "use_schedule", false));
    s.start.set_text(&js(&cfg, "start"));
    s.stop.set_text(&js(&cfg, "stop"));
    let days = cfg.get("days").and_then(|x| x.as_array());
    for (i, d) in s.days.iter().enumerate() {
        d.set_active(
            days.and_then(|a| a.get(i)).and_then(|x| x.as_bool()).unwrap_or(true),
        );
    }
    let one_time = js(&cfg, "one_time");
    s.one_time.set_text(&one_time);
    // a one-time date in the past can never fire again — say so
    let today = sink::today_parts().1;
    s.expired.set_text(if !one_time.is_empty() && one_time.as_str() < today.as_str() {
        "expired"
    } else {
        ""
    });
    s.sync.set_active(jb(&cfg, "sync", false));
    s.exit_done.set_active(jb(&cfg, "exit_when_done", false));
    s.power.set_active_id(Some(js(&cfg, "power_when_done").as_str()));
    s.max.set_value(ju(&cfg, "max_simultaneous") as f64);
    s.btn_delete.set_sensitive(s.sel.borrow().as_str() != MAIN_Q);
    s.guard.set(false);
    s.dirty.set(false);
    // staged file order starts from the queue's current order
    let sel = s.sel.borrow().clone();
    let staged: Vec<String> = s
        .ui
        .order
        .borrow()
        .iter()
        .filter(|id| {
            s.ui.entries
                .borrow()
                .get(*id)
                .map(|d| d.queue == sel)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    *s.order.borrow_mut() = staged;
    render_files(s);
}

fn render_files(s: &Rc<Sched>) {
    s.files.clear();
    let entries = s.ui.entries.borrow();
    if s.order.borrow().is_empty() {
        let it = s.files.append();
        s.files.set(
            &it,
            &[
                (0, &"No downloads in this queue" as &dyn gtk::glib::ToValue),
                (1, &"" as &dyn gtk::glib::ToValue),
            ],
        );
        return;
    }
    for id in s.order.borrow().iter() {
        let (name, status) = entries
            .get(id)
            .map(|d| {
                (
                    if d.name.is_empty() { d.url.clone() } else { d.name.clone() },
                    d.status.clone(),
                )
            })
            .unwrap_or_else(|| (id.clone(), String::new()));
        let it = s.files.append();
        s.files.set(
            &it,
            &[
                (0, &name as &dyn gtk::glib::ToValue),
                (1, &status as &dyn gtk::glib::ToValue),
            ],
        );
    }
}

fn refill_combo(s: &Rc<Sched>) {
    let names = queue_names_of(&s.base.borrow());
    s.guard.set(true);
    s.queue_combo.remove_all();
    for n in &names {
        s.queue_combo.append(Some(n), n);
    }
    let sel = s.sel.borrow().clone();
    s.queue_combo.set_active_id(Some(sel.as_str()));
    s.guard.set(false);
}

fn save_base(s: &Rc<Sched>) -> Result<(), String> {
    let settings: sink::Settings =
        serde_json::from_value(s.base.borrow().clone()).map_err(|e| e.to_string())?;
    sink::set_settings(s.ui.ctx.clone(), settings)?;
    *s.base.borrow_mut() = settings_json(&s.ui);
    Ok(())
}

pub fn show(ui: &Rc<Ui>) {
    let dlg = dialog(&ui.window, "Scheduler", "Save", 620, 640);
    let content = dlg.content_area();
    let g = grid();
    // The form's natural height is ~770 px — taller than a
    // 720p screen (QA-220 m6). It rides in a scrolled window
    // capped well under that: the dialog opens at most ~640 px
    // tall and the lower rows scroll into reach.
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .max_content_height(520)
        .build();
    scroll.add(&g);
    content.pack_start(&scroll, true, true, 0);
    let mut y = 0;
    let queue_combo = gtk::ComboBoxText::new();
    y = row(&g, y, "Queue", &queue_combo);
    let enabled = check("Enable scheduler for this queue", false);
    y = check_row(&g, y, &enabled);
    let start = entry("22:00");
    y = row(&g, y, "Start downloads at", &start);
    let stop = entry("06:00");
    y = row(&g, y, "Stop downloads at", &stop);
    let days_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let mut days = Vec::new();
    for d in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] {
        let c = check(d, true);
        days_box.add(&c);
        days.push(c);
    }
    y = row(&g, y, "On these days", &days_box);
    let one_time = entry("");
    one_time.set_placeholder_text(Some("yyyy-mm-dd — one single day; empty = weekly days above"));
    let expired = gtk::Label::new(None);
    expired.set_halign(gtk::Align::Start);
    {
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bx.pack_start(&one_time, true, true, 0);
        bx.add(&expired);
        y = row(&g, y, "Or one-time date", &bx);
    }
    let sync = check("Synchronize — re-download finished files when they change on the server", false);
    y = check_row(&g, y, &sync);
    let exit_done = check("Exit NDM when this queue finishes", false);
    y = check_row(&g, y, &exit_done);
    let power = combo(
        &[
            ("", "Use the Options power setting"),
            ("shutdown", "Shut down"),
            ("restart", "Restart"),
            ("sleep", "Sleep"),
            ("hibernate", "Hibernate"),
            ("logoff", "Log off"),
        ],
        "",
    );
    y = row(&g, y, "When this queue finishes, turn off computer", &power);
    let max = spin(0.0, 10.0, 0.0, 0);
    y = row(&g, y, "Max simultaneous downloads in this queue (0 = use the Options value)", &max);
    y = note(&g, y, "Files in this queue — queued downloads start in this order:", true);
    let files = gtk::ListStore::new(&[
        gtk::glib::types::Type::STRING,
        gtk::glib::types::Type::STRING,
    ]);
    let files_view = gtk::TreeView::with_model(&files);
    for (t, c) in [("File", 0), ("Status", 1)] {
        let col = gtk::TreeViewColumn::new();
        col.set_title(t);
        let cell = gtk::CellRendererText::new();
        gtk::prelude::CellLayoutExt::pack_start(&col, &cell, true);
        gtk::prelude::CellLayoutExt::add_attribute(&col, &cell, "text", c);
        files_view.append_column(&col);
    }
    let files_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .shadow_type(gtk::ShadowType::In)
        .build();
    files_scroll.set_size_request(-1, 120);
    files_scroll.add(&files_view);
    g.attach(&files_scroll, 0, y, 2, 1);
    y += 1;
    let move_btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_up = gtk::Button::with_label("↑ Start earlier");
    let btn_down = gtk::Button::with_label("↓ Start later");
    move_btns.add(&btn_up);
    move_btns.add(&btn_down);
    g.attach(&move_btns, 0, y, 2, 1);
    y += 1;
    let new_name = entry("");
    new_name.set_placeholder_text(Some("New queue name"));
    let btn_new = gtk::Button::with_label("New queue");
    let btn_delete = gtk::Button::with_label("Delete queue");
    {
        let l = gtk::Label::new(Some(""));
        g.attach(&l, 0, y, 1, 1);
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bx.pack_start(&new_name, true, true, 0);
        bx.add(&btn_new);
        bx.add(&btn_delete);
        g.attach(&bx, 1, y, 1, 1);
        y += 1;
    }
    let now_btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_start = gtk::Button::with_label("▶ Start Queue Now");
    let btn_stop = gtk::Button::with_label("⏸ Stop Queue Now");
    now_btns.add(&btn_start);
    now_btns.add(&btn_stop);
    g.attach(&now_btns, 0, y, 2, 1);
    y += 1;
    let status = status_label();
    g.attach(&status, 0, y, 2, 1);
    content.show_all();

    let s = Rc::new(Sched {
        ui: ui.clone(),
        sel: RefCell::new(crate::pick_queue_name(ui)),
        dirty: Cell::new(false),
        guard: Cell::new(false),
        base: RefCell::new(settings_json(ui)),
        queue_combo,
        enabled,
        start,
        stop,
        days,
        one_time,
        expired,
        sync,
        exit_done,
        power,
        max,
        files,
        files_view: files_view.clone(),
        order: RefCell::new(Vec::new()),
        new_name,
        btn_delete: btn_delete.clone(),
        status,
    });
    refill_combo(&s);
    fill_fields(&s);

    // dirty tracking on every field
    {
        let mark = {
            let s = s.clone();
            move || {
                if !s.guard.get() {
                    s.dirty.set(true);
                }
            }
        };
        let m = mark.clone();
        s.enabled.connect_toggled(move |_| m());
        let m = mark.clone();
        s.start.connect_changed(move |_| m());
        let m = mark.clone();
        s.stop.connect_changed(move |_| m());
        for d in &s.days {
            let m = mark.clone();
            d.connect_toggled(move |_| m());
        }
        let m = mark.clone();
        s.one_time.connect_changed(move |_| m());
        let m = mark.clone();
        s.sync.connect_toggled(move |_| m());
        let m = mark.clone();
        s.exit_done.connect_toggled(move |_| m());
        let m = mark.clone();
        s.power.connect_changed(move |_| m());
        let m = mark.clone();
        s.max.connect_value_changed(move |_| m());
    }
    {
        // queue switch with the discard guard (app.js)
        let s2 = s.clone();
        s.queue_combo.connect_changed(move |c| {
            if s2.guard.get() {
                return;
            }
            let Some(target) = c.active_id().map(|x| x.to_string()) else {
                return;
            };
            if target == *s2.sel.borrow() {
                return;
            }
            if s2.dirty.get() {
                let old = s2.sel.borrow().clone();
                s2.guard.set(true);
                s2.queue_combo.set_active_id(Some(old.as_str()));
                s2.guard.set(false);
                let ok = crate::dialogs::show_confirm(
                    &s2.ui.window,
                    "Discard changes",
                    &format!("Discard the unsaved changes for \"{old}\"?"),
                    "Discard",
                );
                if !ok {
                    return;
                }
                s2.guard.set(true);
                s2.queue_combo.set_active_id(Some(target.as_str()));
                s2.guard.set(false);
            }
            *s2.sel.borrow_mut() = target;
            fill_fields(&s2);
        });
    }
    {
        let s2 = s.clone();
        btn_up.connect_clicked(move |_| move_file(&s2, true));
    }
    {
        let s2 = s.clone();
        btn_down.connect_clicked(move |_| move_file(&s2, false));
    }
    {
        let s2 = s.clone();
        btn_new.connect_clicked(move |_| {
            let name = s2.new_name.text().trim().to_string();
            if name.is_empty() {
                s2.status.set_text("Type a name for the new queue first");
                return;
            }
            if queue_names_of(&s2.base.borrow())
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&name))
            {
                s2.status.set_text("A queue with that name already exists");
                return;
            }
            {
                let mut base = s2.base.borrow_mut();
                if let Some(qs) = base.get_mut("queues").and_then(|x| x.as_array_mut()) {
                    qs.push(serde_json::json!({
                        "name": name, "use_schedule": false, "start": "22:00",
                        "stop": "06:00", "exit_when_done": false,
                        "power_when_done": "", "max_simultaneous": 0,
                        "days": [true, true, true, true, true, true, true],
                        "one_time": "", "sync": false
                    }));
                }
            }
            match save_base(&s2) {
                Ok(()) => {
                    *s2.sel.borrow_mut() = name.clone();
                    s2.new_name.set_text("");
                    refill_combo(&s2);
                    fill_fields(&s2);
                    rebuild_category_tree(&s2.ui);
                    s2.status.set_text(&format!("Queue \"{name}\" created"));
                }
                Err(e) => s2.status.set_text(&format!("Could not create the queue: {e}")),
            }
        });
    }
    {
        let s2 = s.clone();
        btn_delete.connect_clicked(move |_| {
            let gone = s2.sel.borrow().clone();
            if gone == MAIN_Q {
                return;
            }
            if !crate::dialogs::show_confirm(
                &s2.ui.window,
                "Delete queue",
                &format!(
                    "Delete the queue \"{gone}\"?\nIts downloads move to the Main download queue."
                ),
                "Delete queue",
            ) {
                return;
            }
            {
                let mut base = s2.base.borrow_mut();
                if let Some(qs) = base.get_mut("queues").and_then(|x| x.as_array_mut()) {
                    qs.retain(|q| js(q, "name") != gone);
                }
            }
            match save_base(&s2) {
                Ok(()) => {
                    *s2.sel.borrow_mut() = MAIN_Q.to_string();
                    refill_combo(&s2);
                    fill_fields(&s2);
                    rebuild_category_tree(&s2.ui);
                    refresh_list(&s2.ui);
                    s2.status.set_text(&format!("Queue \"{gone}\" deleted"));
                }
                Err(e) => s2.status.set_text(&format!("Could not delete the queue: {e}")),
            }
        });
    }
    {
        let s2 = s.clone();
        btn_start.connect_clicked(move |_| {
            let name = s2.sel.borrow().clone();
            match sink::scheduler_start_now(s2.ui.ctx.clone(), Some(name)) {
                Ok(msg) => s2.status.set_text(&msg),
                Err(e) => s2.status.set_text(&e), // core's answer, verbatim
            }
        });
    }
    {
        let s2 = s.clone();
        btn_stop.connect_clicked(move |_| {
            let name = s2.sel.borrow().clone();
            sink::scheduler_stop_now(s2.ui.ctx.clone(), Some(name.clone()));
            s2.status.set_text(&format!("Queue \"{name}\" stopped"));
        });
    }

    loop {
        if run(&dlg) != gtk::ResponseType::Ok {
            break;
        }
        match save(&s) {
            Ok(()) => {
                refresh_list(ui);
                rebuild_category_tree(ui);
                break;
            }
            Err(e) => s.status.set_text(&e),
        }
    }
    dlg.hide();
}

fn selected_file(s: &Rc<Sched>) -> Option<usize> {
    let (_, iter) = s.files_view.selection().selected()?;
    let path = s.files.path(&iter)?;
    path.indices().first().copied().map(|i| i as usize)
}

fn move_file(s: &Rc<Sched>, up: bool) {
    let Some(i) = selected_file(s) else { return };
    let mut order = s.order.borrow_mut();
    let j = if up {
        if i == 0 {
            return;
        }
        i - 1
    } else {
        if i + 1 >= order.len() {
            return;
        }
        i + 1
    };
    order.swap(i, j);
    drop(order);
    s.dirty.set(true);
    render_files(s);
    if let Some(iter) = s.files.iter_nth_child(None, j as i32) {
        s.files_view.selection().select_iter(&iter);
    }
}

fn save(s: &Rc<Sched>) -> Result<(), String> {
    let sel = s.sel.borrow().clone();
    let start_t = s.start.text().trim().to_string();
    let stop_t = s.stop.text().trim().to_string();
    {
        let mut base = s.base.borrow_mut();
        if let Some(qs) = base.get_mut("queues").and_then(|x| x.as_array_mut()) {
            for q in qs.iter_mut() {
                if js(q, "name") == sel {
                    if let Some(o) = q.as_object_mut() {
                        o.insert("use_schedule".into(), serde_json::json!(s.enabled.is_active()));
                        o.insert(
                            "start".into(),
                            serde_json::json!(if start_t.is_empty() { "22:00" } else { &start_t }),
                        );
                        o.insert(
                            "stop".into(),
                            serde_json::json!(if stop_t.is_empty() { "06:00" } else { &stop_t }),
                        );
                        o.insert(
                            "days".into(),
                            serde_json::json!(s.days.iter().map(|d| d.is_active()).collect::<Vec<_>>()),
                        );
                        o.insert(
                            "one_time".into(),
                            serde_json::json!(s.one_time.text().trim().to_string()),
                        );
                        o.insert("sync".into(), serde_json::json!(s.sync.is_active()));
                        o.insert("exit_when_done".into(), serde_json::json!(s.exit_done.is_active()));
                        o.insert("power_when_done".into(), serde_json::json!(combo_text(&s.power)));
                        o.insert(
                            "max_simultaneous".into(),
                            serde_json::json!(s.max.value_as_int().clamp(0, 10)),
                        );
                    }
                }
            }
        }
    }
    save_base(s).map_err(|e| format!("Could not save the schedule: {e}"))?;
    // apply the staged order (app.js sSave walk)
    let target = s.order.borrow().clone();
    if !target.is_empty() {
        let views = sink::list_downloads(s.ui.ctx.clone());
        let mut cur: Vec<String> = views
            .iter()
            .map(|p| sink::ProgressView::from(p))
            .filter(|d| d.queue == sel && target.contains(&d.id))
            .map(|d| d.id)
            .collect();
        let target: Vec<String> = target.into_iter().filter(|id| cur.contains(id)).collect();
        for i in 0..target.len() {
            if cur[i] == target[i] {
                continue;
            }
            let j = cur.iter().skip(i).position(|x| *x == target[i]).map(|p| p + i);
            let Some(j) = j else { continue };
            for _ in i..j {
                sink::move_in_queue(s.ui.ctx.clone(), target[i].clone(), true)
                    .map_err(|e| format!("Could not save the file order: {e}"))?;
            }
            let id = cur.remove(j);
            cur.insert(i, id);
        }
    }
    s.dirty.set(false);
    Ok(())
}
