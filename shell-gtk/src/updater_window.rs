//! 2.3.7 — the standalone update checker: `ndm-app --ndm-update-check`.
//!
//! His order: an update must announce itself even when the app is
//! closed — a small window in the centre of the screen, with an
//! Update button that updates right there, and after the restart
//! the user lands inside the app. No resident process: this mode
//! runs, decides, and exits.
//!
//! Discipline:
//! - The app already running → exit silently (the in-app offer
//!   owns the update; two windows would be the stacked-dialog sin
//!   the 2.3.6 capture queue abolished).
//! - No offer, a failed fetch, no display → exit silently. A
//!   checker that cannot reach the feed must never nag.
//! - "Later" snoozes the SAME version for 24 h (core's snooze
//!   helpers); a newer version speaks again at once.
//! - The download + install are core's own steps
//!   (download_update_package, install_deb_with_admin) — the
//!   identical chain the in-app update runs, hash-verified.
//! - While the window is up, the checker HOLDS the instance lock
//!   it probed, so the app cannot start mid-install and race the
//!   package swap; the lock is dropped right before the new app
//!   is spawned at the end.

use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const FLAG: &str = "--ndm-update-check";

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Probe the app's instance lock without the app guard's
/// exit-on-held behaviour: Some(file) = the lock is free and now
/// held by US; None = the app holds it, or the probe failed
/// (either way the checker stays silent).
#[cfg(unix)]
fn probe_instance_lock(dir: &Path) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    std::fs::create_dir_all(dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(dir.join("instance.lock"))
        .ok()?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Some(file)
    } else {
        None
    }
}

/// Best-effort +6 h re-check so a machine that stays logged in
/// still hears about an update the same day. Installed copies
/// only (a portable/dev binary must not arm timers pointing at
/// itself); every failure is ignored — the login check remains.
fn rearm() {
    let Ok(exe) = std::env::current_exe() else { return };
    if !exe.starts_with("/usr/") {
        return;
    }
    let unit = format!("ndm-update-check-{}", std::process::id());
    let _ = std::process::Command::new("systemd-run")
        .args(["--user", "--on-active=6h", "--unit"])
        .arg(&unit)
        .arg(&exe)
        .arg(FLAG)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// First-run registration (called by the app's normal startup):
/// an autostart entry that runs the checker at every login.
/// Installed copies only; the file is rewritten only when its
/// content differs, so a moved binary heals itself.
pub fn register_autostart() {
    let Ok(exe) = std::env::current_exe() else { return };
    if !exe.starts_with("/usr/") {
        return;
    }
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(".config"),
            None => return,
        },
    };
    let dir = config.join("autostart");
    let path = dir.join("ndm-update-check.desktop");
    let content = format!(
        "[Desktop Entry]\nType=Application\nName=NDM Update Check\nComment=Checks for NDM updates in the background\nExec={} {FLAG}\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
        exe.display()
    );
    if std::fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
        return;
    }
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(&path, content);
    }
}

enum CheckMsg {
    Progress(u64, u64),
    Installing,
    Done(Result<&'static str, String>),
}

fn label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_line_wrap(true);
    l.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_max_width_chars(64);
    l
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    let _ = provider.load_from_data(
        b"\
.update-title { font-size: 16px; font-weight: 700; }
.update-sub { opacity: 0.75; }
.update-error { color: #c0392b; }
button { border-radius: 8px; padding: 6px 14px; }
button:focus { outline-color: alpha(#2f7fd0, 0.55); outline-style: solid; outline-width: 2px; outline-offset: -2px; }
progressbar trough { min-height: 8px; border-radius: 4px; }
progressbar progress { border-radius: 4px; }
",
    );
    if let Some(screen) = gtk::gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub fn run() -> ! {
    let dir = crate::app_data_dir();
    #[cfg(unix)]
    let lock = match probe_instance_lock(&dir) {
        Some(f) => f,
        None => std::process::exit(0),
    };
    #[cfg(unix)]
    let lock = Rc::new(RefCell::new(Some(lock)));

    let info = match ndm_core::update_offer() {
        Ok(Some(info)) => info,
        _ => {
            rearm();
            std::process::exit(0);
        }
    };
    let snooze = dir.join("update-check.json");
    if ndm_core::update_shown_recently(&snooze, &info.version, now_unix()) {
        rearm();
        std::process::exit(0);
    }
    if gtk::init().is_err() {
        // no display (a timer fired on a headless session): the
        // login check will speak when there is a screen
        std::process::exit(0);
    }
    ndm_core::record_update_shown(&snooze, &info.version, now_unix());
    install_css();

    let version = info.version.clone();
    let current = env!("CARGO_PKG_VERSION");
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("NDM Update");
    window.set_icon_name(Some("ndm-app"));
    window.set_window_position(gtk::WindowPosition::Center);
    window.set_default_size(460, -1);
    window.set_resizable(false);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.set_margin_top(16);
    body.set_margin_bottom(16);

    let title = label(&format!("NDM {version} is available"));
    title.style_context().add_class("update-title");
    let sub = label(&format!(
        "You have NDM {current}. The update downloads and installs here — NDM opens on the new version when it is done."
    ));
    sub.style_context().add_class("update-sub");
    let notes = label(&info.notes);
    let notes_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();
    notes_scroll.set_size_request(-1, 140);
    notes_scroll.add(&notes);

    let bar = gtk::ProgressBar::new();
    bar.set_show_text(false);
    let bar_text = label("");
    let bar_wrap = gtk::Box::new(gtk::Orientation::Vertical, 4);
    bar_wrap.pack_start(&bar, false, false, 0);
    bar_wrap.pack_start(&bar_text, false, false, 0);

    let error = label("");
    error.style_context().add_class("update-error");

    let btn_update = gtk::Button::with_label("Update Now");
    btn_update.style_context().add_class("suggested-action");
    let btn_later = gtk::Button::with_label("Later");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.pack_start(&btn_update, false, false, 0);
    buttons.pack_end(&btn_later, false, false, 0);

    body.pack_start(&title, false, false, 0);
    body.pack_start(&sub, false, false, 0);
    body.pack_start(&notes_scroll, false, false, 0);
    body.pack_start(&bar_wrap, false, false, 0);
    body.pack_start(&error, false, false, 0);
    body.pack_start(&buttons, false, false, 0);
    window.add(&body);
    window.show_all();
    bar_wrap.set_visible(false);
    error.set_visible(false);

    let installing = Rc::new(Cell::new(false));
    #[allow(deprecated)]
    let (tx, rx) = gtk::glib::MainContext::channel::<CheckMsg>(gtk::glib::Priority::DEFAULT);
    {
        let (bar, bar_text, error, btn_update, btn_later, window) = (
            bar.clone(),
            bar_text.clone(),
            error.clone(),
            btn_update.clone(),
            btn_later.clone(),
            window.clone(),
        );
        let lock = lock.clone();
        let version = version.clone();
        rx.attach(None, move |msg| {
            match msg {
                CheckMsg::Progress(done, total) => {
                    if total > 0 {
                        bar.set_fraction(done as f64 / total as f64);
                        bar_text.set_text(&format!(
                            "Downloading the update… {}%",
                            (done.saturating_mul(100) / total.max(1)).min(100)
                        ));
                    } else {
                        bar.pulse();
                        bar_text.set_text("Downloading the update…");
                    }
                }
                CheckMsg::Installing => {
                    bar.set_fraction(1.0);
                    bar_text.set_text(
                        "Installing… confirm the system password prompt if it appears.",
                    );
                }
                CheckMsg::Done(Ok("installed")) => {
                    bar_text.set_text(&format!(
                        "Updated to NDM {version} — opening NDM…"
                    ));
                    btn_later.set_label("Close");
                    btn_later.set_visible(true);
                    // hand the instance lock to the new app BEFORE
                    // spawning it, or its own guard waves it away
                    #[cfg(unix)]
                    {
                        let _ = lock.borrow_mut().take();
                    }
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe).spawn();
                    }
                    let w = window.clone();
                    gtk::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(1200),
                        move || w.close(),
                    );
                }
                CheckMsg::Done(Ok(_)) => {
                    // "opened": the desktop installer owns the rest
                    bar_text.set_text(
                        "The update opened in your software installer — finish it there, then open NDM.",
                    );
                    btn_later.set_label("Close");
                    btn_later.set_visible(true);
                }
                CheckMsg::Done(Err(e)) => {
                    error.set_text(&format!("The update could not be installed: {e}"));
                    error.set_visible(true);
                    bar_text.set_text("");
                    btn_update.set_label("Try Again");
                    btn_update.set_visible(true);
                    btn_update.set_sensitive(true);
                    btn_later.set_label("Close");
                    btn_later.set_visible(true);
                }
                _ => {}
            }
            gtk::glib::ControlFlow::Continue
        });
    }
    {
        let (installing, btn_update, btn_later, bar_wrap, error) = (
            installing.clone(),
            btn_update.clone(),
            btn_later.clone(),
            bar_wrap.clone(),
            error.clone(),
        );
        let info = info.clone();
        let dir = dir.clone();
        let btn_update_in = btn_update.clone();
        btn_update.connect_clicked(move |_| {
            if installing.get() {
                return;
            }
            installing.set(true);
            btn_update_in.set_visible(false);
            btn_later.set_visible(false);
            error.set_visible(false);
            bar_wrap.set_visible(true);
            let tx2 = tx.clone();
            let cb_tx = tx.clone();
            let info = info.clone();
            let dir = dir.clone();
            std::thread::spawn(move || {
                let cb = move |done: u64, total: u64| {
                    let _ = cb_tx.send(CheckMsg::Progress(done, total));
                };
                let outcome = ndm_core::download_update_package(
                    &info,
                    &dir.join("updates"),
                    &cb,
                )
                .and_then(|dest| {
                    let _ = tx2.send(CheckMsg::Installing);
                    ndm_core::install_deb_with_admin(&dest)
                });
                let _ = tx2.send(CheckMsg::Done(outcome));
            });
        });
    }
    {
        let window = window.clone();
        btn_later.connect_clicked(move |_| window.close());
    }
    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        gtk::glib::Propagation::Proceed
    });
    let _ = std::io::stdout().flush();
    gtk::main();
    std::process::exit(0);
}
