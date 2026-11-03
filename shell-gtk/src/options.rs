//! Options dialog (app.js omodal / openOptions + oSave) — five
//! tabs exactly as the web UI groups them: General / Connection
//! / Downloads / Proxy & Site Logins / Categories. Every field
//! is staged from core's Settings (read through the JSON seam)
//! and Save writes the whole object back through core's
//! set_settings — sanitize/side-effects stay core's. A failed
//! save keeps the dialog open and says so (app.js F-4).

use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::form::*;
use crate::sink;
use crate::{rebuild_category_tree, refresh_list, settings_json, Ui};

const CATS: [&str; 6] = ["Compressed", "Documents", "Music", "Video", "Programs", "General"];
const CATS_TYPES: [&str; 5] = ["Compressed", "Documents", "Music", "Video", "Programs"];
const BUILTINS: [&str; 7] = [
    "General", "Compressed", "Documents", "Music", "Video", "Programs", "Grabber",
];

/// app.js BUILTIN_CATEGORY_EXTS — the Categories editor compares
/// against these to decide what is worth storing.
pub fn builtin_exts(cat: &str) -> Vec<&'static str> {
    match cat {
        "Compressed" => vec!["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "tgz", "tbz2", "txz"],
        "Programs" => vec![
            "exe", "msi", "msix", "msixbundle", "deb", "rpm", "dmg", "pkg", "appimage", "apk",
            "bin", "run", "cab", "jar", "war", "vmdk", "qcow2", "vdi", "iso", "img", "flatpakref",
        ],
        "Video" => vec!["mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "m4v", "ts"],
        "Music" => vec!["mp3", "m4a", "m4b", "flac", "wav", "ogg", "opus", "aac", "wma"],
        "Documents" => vec![
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "csv", "rtf", "epub",
            "mobi",
        ],
        _ => vec![],
    }
}

struct Opt {
    dir: gtk::Entry,
    auto_resume: gtk::CheckButton,
    auto_update: gtk::CheckButton,
    clipboard: gtk::CheckButton,
    exit_done: gtk::CheckButton,
    video_q: gtk::ComboBoxText,
    conns: gtk::SpinButton,
    conn_exc: gtk::Entry,
    limit_on: gtk::CheckButton,
    limit_val: gtk::SpinButton,
    quota_mb: gtk::SpinButton,
    quota_hours: gtk::SpinButton,
    simul: gtk::SpinButton,
    complete_dlg: gtk::CheckButton,
    progress_view: gtk::ComboBoxText,
    sounds: gtk::CheckButton,
    power: gtk::ComboBoxText,
    power_force: gtk::CheckButton,
    completion_program: gtk::Entry,
    completion_args: gtk::Entry,
    proxy_modes: Vec<(String, gtk::RadioButton)>,
    proxy_pac: gtk::Entry,
    proxy_scheme: gtk::ComboBoxText,
    proxy_host: gtk::Entry,
    proxy_port: gtk::SpinButton,
    proxy_user: gtk::Entry,
    proxy_pass: gtk::Entry,
    proxy_http: gtk::CheckButton,
    proxy_https: gtk::CheckButton,
    proxy_ftp: gtk::CheckButton,
    proxy_browser: gtk::CheckButton,
    logins: Rc<RefCell<Vec<(String, String, String)>>>,
    login_store: gtk::ListStore,
    login_site: gtk::Entry,
    login_user: gtk::Entry,
    login_pass: gtk::Entry,
    cat_dirs: Vec<gtk::Entry>,
    cat_exts: Vec<gtk::Entry>,
    cat_sites: Vec<gtk::Entry>,
    cat_store: gtk::ListStore,
    cat_names: Rc<RefCell<Vec<String>>>,
    cat_status: gtk::Label,
    status: gtk::Label,
}

fn page_scroll() -> (gtk::ScrolledWindow, gtk::Grid) {
    let g = grid();
    let sc = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();
    sc.add(&g);
    (sc, g)
}

fn rebuild_logins(o: &Opt) {
    o.login_store.clear();
    for (site, login, _) in o.logins.borrow().iter() {
        let it = o.login_store.append();
        o.login_store.set(
            &it,
            &[
                (0, site as &dyn gtk::glib::ToValue),
                (1, login as &dyn gtk::glib::ToValue),
                (2, &"••••••" as &dyn gtk::glib::ToValue),
            ],
        );
    }
}

fn rebuild_cats(ui: &Rc<Ui>, o: &Opt) {
    let mut names = crate::category_names(ui);
    names.sort_by_key(|a| a.to_lowercase());
    o.cat_store.clear();
    for n in &names {
        let builtin = BUILTINS.iter().any(|b| b.eq_ignore_ascii_case(n));
        let label = if builtin { format!("{n} (built-in)") } else { n.clone() };
        let it = o.cat_store.append();
        o.cat_store.set(&it, &[(0, &label as &dyn gtk::glib::ToValue)]);
    }
    *o.cat_names.borrow_mut() = names;
}

pub fn show(ui: &Rc<Ui>) {
    let s = settings_json(ui);
    let dlg = dialog(&ui.window, "Options", "Save", 680, 620);
    let nb = gtk::Notebook::new();
    let content = dlg.content_area();
    content.pack_start(&nb, true, true, 0);

    // ---- General -------------------------------------------------
    let (sc, g) = page_scroll();
    let mut y = 0;
    let dir = entry(&js(&s, "default_dir"));
    y = row(&g, y, "Default download folder", &dir);
    let auto_resume = check("Auto-resume unfinished downloads on start", jb(&s, "auto_resume", false));
    y = check_row(&g, y, &auto_resume);
    let auto_update = check("Automatically check for updates", jb(&s, "auto_update", true));
    y = check_row(&g, y, &auto_update);
    let clipboard = check("Watch the clipboard for download links", jb(&s, "clipboard_monitor", false));
    y = check_row(&g, y, &clipboard);
    let exit_done = check("Exit NDM when all downloads finish", jb(&s, "exit_when_done", false));
    y = check_row(&g, y, &exit_done);
    let engine_text = {
        let st = sink::components_status(ui.ctx.clone());
        if st.get("ready").and_then(|x| x.as_bool()).unwrap_or(false) {
            "Ready (video engine installed)"
        } else {
            "Sets itself up automatically in the background"
        }
    };
    let engine = gtk::Label::new(Some(engine_text));
    engine.set_halign(gtk::Align::Start);
    y = row(&g, y, "Video engine", &engine);
    let video_q = combo(
        &[
            ("best", "Best quality"), ("2160", "2160p (4K)"), ("1440", "1440p"),
            ("1080", "1080p"), ("720", "720p"), ("480", "480p"), ("360", "360p"),
            ("240", "240p"), ("audio", "Audio only"),
        ],
        &js(&s, "video_quality"),
    );
    row(&g, y, "Default video quality", &video_q);
    nb.append_page(&sc, Some(&gtk::Label::new(Some("General"))));

    // ---- Connection ----------------------------------------------
    let (sc, g) = page_scroll();
    let mut y = 0;
    let conns = spin(1.0, 32.0, ju(&s, "max_connections").max(1) as f64, 0);
    y = row(&g, y, "Max connections per download", &conns);
    let exc = s
        .get("conn_exceptions")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let conn_exc = entry(&exc);
    conn_exc.set_placeholder_text(Some("Example: slow-server.com = 1, *.example.org = 2"));
    y = row(&g, y, "Per-server connections", &conn_exc);
    y = note(&g, y, "Servers listed here get fewer parallel connections than the global setting (IDM's connection exceptions). Separate entries with commas.", false);
    let limit_on = check("Limit download speed", ju(&s, "speed_limit_bps") > 0);
    y = check_row(&g, y, &limit_on);
    let limit_bps = ju(&s, "speed_limit_bps");
    let limit_val = spin(
        0.0,
        10000.0,
        if limit_bps > 0 {
            ((limit_bps as f64 / 1_048_576.0 * 1000.0).round()) / 1000.0
        } else {
            2.0
        },
        2,
    );
    y = row(&g, y, "Speed limit (MB/s)", &limit_val);
    let quota_mb = spin(0.0, 1_000_000.0, ju(&s, "quota_mb") as f64, 0);
    y = row(&g, y, "Download limit (MB per period, 0 = off)", &quota_mb);
    let quota_hours = spin(1.0, 168.0, ju(&s, "quota_hours").max(1) as f64, 0);
    row(&g, y, "Limit period (hours)", &quota_hours);
    nb.append_page(&sc, Some(&gtk::Label::new(Some("Connection"))));

    // ---- Downloads -------------------------------------------------
    let (sc, g) = page_scroll();
    let mut y = 0;
    let simul = spin(0.0, 10.0, ju(&s, "max_simultaneous") as f64, 0);
    y = row(&g, y, "Max simultaneous downloads", &simul);
    y = note(&g, y, "0 = unlimited simultaneous downloads.", false);
    let complete_dlg = check("Show the \"Download complete\" dialog", jb(&s, "show_complete_dialog", true));
    y = check_row(&g, y, &complete_dlg);
    let progress_view = combo(
        &[("show", "Show"), ("minimized", "Minimized"), ("never", "Don't show")],
        &js(&s, "progress_dialog_view"),
    );
    y = row(&g, y, "Download progress dialog", &progress_view);
    y = note(&g, y, "How the per-download progress dialog opens when a download starts in the app (downloads caught from the browser keep their in-page card).", false);
    let sounds = check("Play a sound when a download completes or fails", jb(&s, "sound_events", true));
    y = check_row(&g, y, &sounds);
    y = note(&g, y, "When all downloads finish", true);
    let power = combo(
        &[
            ("", "Do nothing"), ("shutdown", "Shut down"), ("restart", "Restart"),
            ("sleep", "Sleep"), ("hibernate", "Hibernate"), ("logoff", "Log off"),
        ],
        &js(&s, "power_when_done"),
    );
    y = row(&g, y, "Computer power action", &power);
    let power_force = check("Force running programs to close with the power action", jb(&s, "power_force", false));
    y = check_row(&g, y, &power_force);
    y = note(&g, y, "Run this program when a download completes", true);
    y = note(&g, y, "(for example, your antivirus scanner)", false);
    let completion_program = entry(&js(&s, "completion_program"));
    completion_program.set_placeholder_text(Some("Program path — empty means off"));
    y = row(&g, y, "Program", &completion_program);
    let completion_args = entry(&js(&s, "completion_args"));
    completion_args.set_placeholder_text(Some("e.g. --scan \"{file}\""));
    y = row(&g, y, "Arguments", &completion_args);
    note(&g, y, "{file} is replaced by the downloaded file's full path; if the arguments contain no {file}, the path is added at the end.", false);
    nb.append_page(&sc, Some(&gtk::Label::new(Some("Downloads"))));

    // ---- Proxy & Site Logins ---------------------------------------
    let (sc, g) = page_scroll();
    let mut y = 0;
    y = note(&g, y, "Proxy / Socks — how downloads reach the internet", true);
    let mode_now = {
        let m = js(&s, "proxy_mode");
        if !m.is_empty() {
            m
        } else if !js(&s, "proxy").is_empty() {
            "manual".to_string()
        } else {
            "none".to_string()
        }
    };
    let mut proxy_modes: Vec<(String, gtk::RadioButton)> = Vec::new();
    let mut first_radio: Option<gtk::RadioButton> = None;
    for (val, label) in [
        ("none", "No proxy — direct connection"),
        ("system", "Use system settings"),
        ("pac", "Use automatic configuration script (PAC)"),
        ("manual", "Manual proxy configuration"),
    ] {
        let r = match &first_radio {
            None => {
                let r = gtk::RadioButton::with_label(label);
                first_radio = Some(r.clone());
                r
            }
            Some(f) => gtk::RadioButton::with_label_from_widget(f, label),
        };
        r.set_active(val == mode_now);
        y = check_row(&g, y, &r);
        proxy_modes.push((val.to_string(), r));
    }
    let proxy_pac = entry(&js(&s, "proxy_pac_url"));
    proxy_pac.set_placeholder_text(Some("http://example.com/proxy.pac"));
    y = row(&g, y, "Configuration script address", &proxy_pac);
    let proxy_scheme = combo(
        &[("http", "HTTP"), ("socks5", "SOCKS5"), ("socks4", "SOCKS4")],
        &js(&s, "proxy_scheme"),
    );
    y = row(&g, y, "Proxy type", &proxy_scheme);
    let proxy_host = entry(&js(&s, "proxy_host"));
    proxy_host.set_placeholder_text(Some("127.0.0.1"));
    y = row(&g, y, "Proxy server address", &proxy_host);
    let proxy_port = spin(0.0, 65535.0, ju(&s, "proxy_port") as f64, 0);
    y = row(&g, y, "Port", &proxy_port);
    let proxy_user = entry(&js(&s, "proxy_user"));
    y = row(&g, y, "Proxy user name", &proxy_user);
    let proxy_pass = secret(&js(&s, "proxy_pass"));
    y = row(&g, y, "Proxy password", &proxy_pass);
    let proxy_http = check("HTTP", jb(&s, "proxy_http", true));
    let proxy_https = check("HTTPS", jb(&s, "proxy_https", true));
    let proxy_ftp = check("FTP", jb(&s, "proxy_ftp", true));
    {
        let l = gtk::Label::new(Some("Use this proxy for"));
        l.set_halign(gtk::Align::Start);
        g.attach(&l, 0, y, 1, 1);
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        bx.add(&proxy_http);
        bx.add(&proxy_https);
        bx.add(&proxy_ftp);
        g.attach(&bx, 1, y, 1, 1);
        y += 1;
    }
    let proxy_browser = check(
        "If a download caught from the browser fails, retry it once through the system proxy",
        jb(&s, "proxy_from_browser_on_error", false),
    );
    y = check_row(&g, y, &proxy_browser);
    y = note(&g, y, "Applies to new connections — files, the grabber and video.", false);
    y = note(&g, y, "Site Logins — user names and passwords for servers/sites. A saved login is used automatically for downloads from that site (a site may include a path, e.g. example.com/members).", true);
    let login_store = gtk::ListStore::new(&[
        gtk::glib::types::Type::STRING,
        gtk::glib::types::Type::STRING,
        gtk::glib::types::Type::STRING,
    ]);
    let login_view = gtk::TreeView::with_model(&login_store);
    for (t, c) in [("Site / path", 0), ("User name", 1), ("Password", 2)] {
        let col = gtk::TreeViewColumn::new();
        col.set_title(t);
        let cell = gtk::CellRendererText::new();
        gtk::prelude::CellLayoutExt::pack_start(&col, &cell, true);
        gtk::prelude::CellLayoutExt::add_attribute(&col, &cell, "text", c);
        login_view.append_column(&col);
    }
    let login_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .shadow_type(gtk::ShadowType::In)
        .build();
    login_scroll.set_size_request(-1, 96);
    login_scroll.add(&login_view);
    g.attach(&login_scroll, 0, y, 2, 1);
    y += 1;
    let login_site = entry("");
    login_site.set_placeholder_text(Some("example.com/members"));
    y = row(&g, y, "Site / path", &login_site);
    let login_user = entry("");
    y = row(&g, y, "User name", &login_user);
    let login_pass = secret("");
    y = row(&g, y, "Password", &login_pass);
    let login_btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let login_add = gtk::Button::with_label("Add login");
    let login_remove = gtk::Button::with_label("Remove selected");
    login_btns.add(&login_add);
    login_btns.add(&login_remove);
    g.attach(&login_btns, 0, y, 2, 1);
    nb.append_page(&sc, Some(&gtk::Label::new(Some("Proxy & Site Logins"))));

    // ---- Categories --------------------------------------------------
    let (sc, g) = page_scroll();
    let mut y = 0;
    y = note(&g, y, "Categories — each category has its own folder (used when a download would otherwise go to the default folder), its own file types and its own sites, like IDM's category editor. A download from a listed site goes to that category whatever its file type. Editing the types changes how the app classifies new downloads; the browser extension keeps the built-in list.", true);
    let dirs_v = s.get("category_dirs").cloned().unwrap_or(serde_json::json!({}));
    let exts_v = s.get("category_exts").cloned().unwrap_or(serde_json::json!({}));
    let sites_v = s.get("category_sites").cloned().unwrap_or(serde_json::json!({}));
    let mut cat_dirs = Vec::new();
    let mut cat_exts = Vec::new();
    let mut cat_sites = Vec::new();
    for c in CATS {
        let e = entry(dirs_v.get(c).and_then(|x| x.as_str()).unwrap_or(""));
        e.set_placeholder_text(Some("~/Downloads"));
        y = row(&g, y, &format!("{c} folder"), &e);
        cat_dirs.push(e);
        if CATS_TYPES.contains(&c) {
            let cur = exts_v
                .get(c)
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_else(|| builtin_exts(c).join(", "));
            let te = entry(&cur);
            y = row(&g, y, &format!("{c} types"), &te);
            cat_exts.push(te);
            // site rules for this category: host patterns,
            // comma-separated like the types above ("*.site.com"
            // = any subdomain, "site.com" = host + subdomains);
            // a matching site beats the file types at add time
            let cur_sites = sites_v
                .get(c)
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            let se = entry(&cur_sites);
            se.set_placeholder_text(Some("*.googlevideo.com, example.com"));
            y = row(&g, y, &format!("{c} sites"), &se);
            cat_sites.push(se);
        }
    }
    y = note(&g, y, "Manage categories — rename or delete the categories you created. The built-in categories above cannot be renamed or deleted. Deleting a category moves its downloads to General.", true);
    let cat_store = gtk::ListStore::new(&[gtk::glib::types::Type::STRING]);
    let cat_view = gtk::TreeView::with_model(&cat_store);
    {
        let col = gtk::TreeViewColumn::new();
        col.set_title("Category");
        let cell = gtk::CellRendererText::new();
        gtk::prelude::CellLayoutExt::pack_start(&col, &cell, true);
        gtk::prelude::CellLayoutExt::add_attribute(&col, &cell, "text", 0);
        cat_view.append_column(&col);
    }
    let cat_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .shadow_type(gtk::ShadowType::In)
        .build();
    cat_scroll.set_size_request(-1, 110);
    cat_scroll.add(&cat_view);
    g.attach(&cat_scroll, 0, y, 2, 1);
    y += 1;
    let cat_btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let cat_rename = gtk::Button::with_label("Rename");
    let cat_delete = gtk::Button::with_label("Delete");
    cat_btns.add(&cat_rename);
    cat_btns.add(&cat_delete);
    g.attach(&cat_btns, 0, y, 2, 1);
    y += 1;
    let cat_status = status_label();
    g.attach(&cat_status, 0, y, 2, 1);
    nb.append_page(&sc, Some(&gtk::Label::new(Some("Categories"))));

    let status = status_label();
    content.pack_start(&status, false, false, 6);
    content.show_all();

    let logins: Rc<RefCell<Vec<(String, String, String)>>> = Rc::new(RefCell::new(
        s.get("site_logins")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .map(|l| (js(l, "site"), js(l, "login"), js(l, "password")))
                    .collect()
            })
            .unwrap_or_default(),
    ));
    let o = Rc::new(Opt {
        dir, auto_resume, auto_update, clipboard, exit_done, video_q, conns, conn_exc,
        limit_on, limit_val, quota_mb, quota_hours, simul, complete_dlg, progress_view,
        sounds, power, power_force, completion_program, completion_args,
        proxy_modes, proxy_pac, proxy_scheme, proxy_host,
        proxy_port, proxy_user, proxy_pass, proxy_http, proxy_https, proxy_ftp, proxy_browser,
        logins: logins.clone(), login_store, login_site, login_user, login_pass,
        cat_dirs, cat_exts, cat_sites, cat_store, cat_names: Rc::new(RefCell::new(Vec::new())),
        cat_status, status,
    });
    rebuild_logins(&o);
    rebuild_cats(ui, &o);

    // Proxy mode gating (app.js syncProxyMode): PAC field only
    // in PAC mode, server fields only in Manual.
    let sync_proxy = {
        let o = o.clone();
        move || {
            let mode = o
                .proxy_modes
                .iter()
                .find(|(_, r)| r.is_active())
                .map(|(v, _)| v.clone())
                .unwrap_or_else(|| "none".to_string());
            o.proxy_pac.set_sensitive(mode == "pac");
            for w in [
                o.proxy_host.clone().upcast::<gtk::Widget>(),
                o.proxy_port.clone().upcast(),
                o.proxy_user.clone().upcast(),
                o.proxy_pass.clone().upcast(),
                o.proxy_http.clone().upcast(),
                o.proxy_https.clone().upcast(),
                o.proxy_ftp.clone().upcast(),
            ] {
                w.set_sensitive(mode == "manual");
            }
            o.proxy_scheme.set_sensitive(mode == "manual");
        }
    };
    for (_, r) in &o.proxy_modes {
        let sp = sync_proxy.clone();
        r.connect_toggled(move |_| sp());
    }
    sync_proxy();

    {
        let o = o.clone();
        login_add.connect_clicked(move |_| {
            let site = o.login_site.text().trim().to_string();
            let login = o.login_user.text().trim().to_string();
            let pass = o.login_pass.text().to_string();
            if site.is_empty() || login.is_empty() {
                o.status.set_text("Give the site and the user name first");
                return;
            }
            // replace on same site+login (app.js oLoginAdd)
            let mut list = o.logins.borrow_mut();
            if let Some(ex) = list.iter_mut().find(|(s2, l2, _)| {
                s2.eq_ignore_ascii_case(&site) && l2.eq_ignore_ascii_case(&login)
            }) {
                *ex = (site, login, pass);
            } else {
                list.push((site, login, pass));
            }
            drop(list);
            o.login_site.set_text("");
            o.login_user.set_text("");
            o.login_pass.set_text("");
            rebuild_logins(&o);
        });
    }
    {
        let (o, view) = (o.clone(), login_view.clone());
        login_remove.connect_clicked(move |_| {
            let sel = view.selection();
            if let Some((_, iter)) = sel.selected() {
                if let Some(path) = o.login_store.path(&iter) {
                    if let Some(idx) = path.indices().first().copied() {
                        o.logins.borrow_mut().remove(idx as usize);
                        rebuild_logins(&o);
                    }
                }
            }
        });
    }
    let selected_cat = {
        let (o, view) = (o.clone(), cat_view.clone());
        move || -> Option<String> {
            let (_, iter) = view.selection().selected()?;
            let path = o.cat_store.path(&iter)?;
            let idx = *path.indices().first()? as usize;
            o.cat_names.borrow().get(idx).cloned()
        }
    };
    {
        let (ui2, o, sel) = (ui.clone(), o.clone(), selected_cat.clone());
        cat_rename.connect_clicked(move |_| {
            let Some(name) = sel() else { return };
            if BUILTINS.iter().any(|b| b.eq_ignore_ascii_case(&name)) {
                o.cat_status.set_text("Built-in categories cannot be renamed");
                return;
            }
            let Some(next) = crate::dialogs::show_prompt(
                &ui2.window,
                "Rename category",
                &format!("Rename the category \"{name}\" to:"),
                &name,
            ) else {
                return;
            };
            let next = next.trim().to_string();
            if next.is_empty() || next == name {
                return;
            }
            match sink::rename_category(ui2.ctx.clone(), name.clone(), next.clone()) {
                Ok(()) => {
                    o.cat_status.set_text("");
                    rebuild_cats(&ui2, &o);
                    rebuild_category_tree(&ui2);
                    refresh_list(&ui2);
                }
                Err(e) => o.cat_status.set_text(&format!("Could not rename the category: {e}")),
            }
        });
    }
    {
        let (ui2, o, sel) = (ui.clone(), o.clone(), selected_cat.clone());
        cat_delete.connect_clicked(move |_| {
            let Some(name) = sel() else { return };
            if BUILTINS.iter().any(|b| b.eq_ignore_ascii_case(&name)) {
                o.cat_status.set_text("Built-in categories cannot be deleted");
                return;
            }
            if !crate::dialogs::show_confirm(
                &ui2.window,
                "Delete category",
                &format!("Delete the category \"{name}\"?\nIts downloads move to General."),
                "Delete category",
            ) {
                return;
            }
            match sink::delete_category(ui2.ctx.clone(), name.clone()) {
                Ok(()) => {
                    o.cat_status.set_text("");
                    rebuild_cats(&ui2, &o);
                    rebuild_category_tree(&ui2);
                    refresh_list(&ui2);
                }
                Err(e) => o.cat_status.set_text(&format!("Could not delete the category: {e}")),
            }
        });
    }

    // Save loop — a failed save keeps the dialog open (F-4).
    loop {
        if run(&dlg) != gtk::ResponseType::Ok {
            break;
        }
        match save(ui, &s, &o) {
            Ok(()) => {
                rebuild_category_tree(ui);
                break;
            }
            Err(e) => {
                o.status.set_text(&format!(
                    "These settings could not be saved to disk — {e} Check that the app data folder is writable, then Save again."
                ));
            }
        }
    }
    dlg.hide();
}

fn save(ui: &Rc<Ui>, base: &serde_json::Value, o: &Opt) -> Result<(), String> {
    let mut v = base.clone();
    let obj = v.as_object_mut().ok_or("settings unavailable")?;
    let ins = |obj: &mut serde_json::Map<String, serde_json::Value>, k: &str, val: serde_json::Value| {
        obj.insert(k.to_string(), val);
    };
    ins(obj, "default_dir", serde_json::json!(o.dir.text().trim().if_empty("~/Downloads")));
    ins(obj, "max_connections", serde_json::json!(o.conns.value_as_int().clamp(1, 32)));
    ins(
        obj,
        "speed_limit_bps",
        serde_json::json!(if o.limit_on.is_active() {
            (o.limit_val.value().max(0.1) * 1_048_576.0).round() as u64
        } else {
            0
        }),
    );
    ins(obj, "max_simultaneous", serde_json::json!(o.simul.value_as_int().clamp(0, 10)));
    ins(obj, "auto_resume", serde_json::json!(o.auto_resume.is_active()));
    ins(obj, "auto_update", serde_json::json!(o.auto_update.is_active()));
    ins(obj, "video_quality", serde_json::json!(combo_text(&o.video_q)));
    let mode = o
        .proxy_modes
        .iter()
        .find(|(_, r)| r.is_active())
        .map(|(v, _)| v.clone())
        .unwrap_or_else(|| "none".to_string());
    ins(obj, "proxy_mode", serde_json::json!(mode));
    ins(obj, "proxy_pac_url", serde_json::json!(o.proxy_pac.text().trim()));
    ins(obj, "proxy_scheme", serde_json::json!(combo_text(&o.proxy_scheme)));
    ins(obj, "proxy_host", serde_json::json!(o.proxy_host.text().trim()));
    ins(obj, "proxy_port", serde_json::json!(o.proxy_port.value_as_int().clamp(0, 65535)));
    ins(obj, "proxy_user", serde_json::json!(o.proxy_user.text().trim()));
    ins(obj, "proxy_pass", serde_json::json!(o.proxy_pass.text().to_string()));
    ins(obj, "proxy_http", serde_json::json!(o.proxy_http.is_active()));
    ins(obj, "proxy_https", serde_json::json!(o.proxy_https.is_active()));
    ins(obj, "proxy_ftp", serde_json::json!(o.proxy_ftp.is_active()));
    ins(obj, "proxy_from_browser_on_error", serde_json::json!(o.proxy_browser.is_active()));
    ins(
        obj,
        "site_logins",
        serde_json::Value::Array(
            o.logins
                .borrow()
                .iter()
                .map(|(site, login, password)| {
                    serde_json::json!({"site": site, "login": login, "password": password})
                })
                .collect(),
        ),
    );
    ins(obj, "power_when_done", serde_json::json!(combo_text(&o.power)));
    ins(obj, "power_force", serde_json::json!(o.power_force.is_active()));
    ins(obj, "completion_program", serde_json::json!(o.completion_program.text().trim()));
    ins(obj, "completion_args", serde_json::json!(o.completion_args.text().trim()));
    // category folders: only non-empty entries are stored
    let mut dirs = serde_json::Map::new();
    for (i, c) in CATS.iter().enumerate() {
        let val = o.cat_dirs[i].text().trim().to_string();
        if !val.is_empty() {
            dirs.insert(c.to_string(), serde_json::json!(val));
        }
    }
    ins(obj, "category_dirs", serde_json::Value::Object(dirs));
    // category types: stored only when they differ from the
    // built-in table (an explicitly emptied list IS a difference
    // and is kept as []); non-built-in keys pass through untouched
    let mut exts = serde_json::Map::new();
    if let Some(old) = base.get("category_exts").and_then(|x| x.as_object()) {
        for (k, val) in old {
            if !CATS_TYPES.contains(&k.as_str()) {
                exts.insert(k.clone(), val.clone());
            }
        }
    }
    for (i, c) in CATS_TYPES.iter().enumerate() {
        let list = parse_exts(&o.cat_exts[i].text());
        let builtin = builtin_exts(c);
        let same = list.len() == builtin.len()
            && list.iter().all(|x| builtin.contains(&x.as_str()));
        if !same {
            exts.insert(c.to_string(), serde_json::json!(list));
        }
    }
    ins(obj, "category_exts", serde_json::Value::Object(exts));
    // category sites: stored only when non-empty (unlike types,
    // an empty list is the absence of a rule, not a rule that
    // matches nothing); non-built-in keys pass through untouched
    let mut sites = serde_json::Map::new();
    if let Some(old) = base.get("category_sites").and_then(|x| x.as_object()) {
        for (k, val) in old {
            if !CATS_TYPES.contains(&k.as_str()) {
                sites.insert(k.clone(), val.clone());
            }
        }
    }
    for (i, c) in CATS_TYPES.iter().enumerate() {
        let list: Vec<String> = o.cat_sites[i]
            .text()
            .split(',')
            .map(|x| x.trim().to_ascii_lowercase())
            .filter(|x| !x.is_empty())
            .collect();
        if !list.is_empty() {
            sites.insert(c.to_string(), serde_json::json!(list));
        }
    }
    ins(obj, "category_sites", serde_json::Value::Object(sites));
    ins(
        obj,
        "conn_exceptions",
        serde_json::Value::Array(
            o.conn_exc
                .text()
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    ins(obj, "exit_when_done", serde_json::json!(o.exit_done.is_active()));
    ins(obj, "sound_events", serde_json::json!(o.sounds.is_active()));
    ins(obj, "show_complete_dialog", serde_json::json!(o.complete_dlg.is_active()));
    ins(obj, "progress_dialog_view", serde_json::json!(combo_text(&o.progress_view)));
    ins(obj, "clipboard_monitor", serde_json::json!(o.clipboard.is_active()));
    ins(obj, "quota_mb", serde_json::json!(o.quota_mb.value_as_int().max(0)));
    ins(obj, "quota_hours", serde_json::json!(o.quota_hours.value_as_int().clamp(1, 168)));
    let settings: sink::Settings =
        serde_json::from_value(v).map_err(|e| format!("invalid settings: {e}"))?;
    sink::set_settings(ui.ctx.clone(), settings)
}

trait IfEmpty {
    fn if_empty(self, dflt: &str) -> String;
}
impl IfEmpty for &str {
    fn if_empty(self, dflt: &str) -> String {
        if self.is_empty() {
            dflt.to_string()
        } else {
            self.to_string()
        }
    }
}
