// Nill Download Manager (Tauri shell)
// A thin Tauri shell over the extracted `ndm-core` crate (Phase 1
// of the native UI programme): this file holds only window/process
// plumbing — main(), the single-instance guard, the TauriSink that
// implements core's EventSink, the shell-native commands, and one
// thin wrapper per core command. All behaviour lives in ndm-core.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

use ndm_core::{Ctx, GrabExploreArgs, GrabSiteDto, ProgressPayload, Settings};

/// The Tauri implementation of ndm-core's EventSink: events go to
/// the webview under their exact wire names, exit maps to
/// AppHandle::exit(0) (the inline call the code made before the
/// extraction), and raise uses the shell's own window raise.
struct TauriSink {
    app: AppHandle,
}

impl ndm_core::EventSink for TauriSink {
    fn emit(&self, event: ndm_core::AppEvent) {
        let _ = self.app.emit(event.name(), event.payload());
    }

    fn raise_main_window(&self) {
        raise_main_window(&self.app);
    }

    fn exit_app(&self) {
        self.app.exit(0);
    }
}

/* ---------- shell-native commands ----------
   The commands whose bodies ARE the shell (window/process
   operations); everything else is a thin wrapper into ndm-core. */

/// The shell crate's own version (== the app version; ndm-core
/// carries the same version by release discipline).
#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

/// Show or hide the drop-basket window (View menu / basket's own
/// close button).
#[tauri::command]
fn set_basket_visible(app: AppHandle, visible: bool) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("basket") {
        if visible {
            let _ = w.show();
        } else {
            let _ = w.hide();
        }
    }
    Ok(())
}

/// EXT §2: dismissing the banner changes nothing in state —
/// ext_notice_for was already recorded at show time, so the
/// notice stays one-time per bundled version either way.
#[tauri::command]
fn dismiss_ext_notice() {}

/// 208B D12: the values the Scheduler / Basket / Categories
/// dialogs open with, in one payload - the full current settings
/// (queues carry the Scheduler values; category_dirs and
/// category_exts the Categories values), the category names in
/// use, and the basket window's current visibility. The settings
/// and categories come from ndm-core; the basket visibility is
/// window state, so this command stays shell-native.
#[tauri::command]
fn dialog_open_values(app: AppHandle, ctx: State<'_, Ctx>) -> serde_json::Value {
    let state = ctx.state();
    let s = ndm_core::current_settings(state);
    let categories = ndm_core::category_names_in_use(state, &s);
    serde_json::json!({
        "settings": s,
        "categories": categories,
        "basket_visible": app
            .get_webview_window("basket")
            .map(|w| w.is_visible().unwrap_or(false))
            .unwrap_or(false),
    })
}

/* ---------- thin wrappers into ndm-core ---------- */

#[tauri::command]
async fn add_download(ctx: State<'_, Ctx>, url: String, dir: String, later: Option<bool>, category: Option<String>, name: Option<String>, cookies: Option<String>, referer: Option<String>, force: Option<bool>, queue: Option<String>, login: Option<String>, password: Option<String>, description: Option<String>, total: Option<u64>, dir_explicit: Option<bool>) -> Result<String, String> {
    ndm_core::add_download(ctx.inner().clone(), url, dir, later, category, name, cookies, referer, force, queue, login, password, description, total, dir_explicit).await
}

#[tauri::command]
fn pause_download(ctx: State<'_, Ctx>, id: String) -> Result<(), String> {
    ndm_core::pause_download(ctx.inner().clone(), id)
}

#[tauri::command]
async fn resume_download(ctx: State<'_, Ctx>, id: String) -> Result<(), String> {
    ndm_core::resume_download(ctx.inner().clone(), id).await
}

#[tauri::command]
fn remove_download(ctx: State<'_, Ctx>, id: String, delete_file: Option<bool>) -> Result<(), String> {
    ndm_core::remove_download(ctx.inner().clone(), id, delete_file)
}

#[tauri::command]
fn remove_downloads(ctx: State<'_, Ctx>, ids: Vec<String>, delete_file: Option<bool>) -> Vec<serde_json::Value> {
    ndm_core::remove_downloads(ctx.inner().clone(), ids, delete_file)
}

#[tauri::command]
fn list_downloads(ctx: State<'_, Ctx>) -> Vec<ProgressPayload> {
    ndm_core::list_downloads(ctx.inner().clone())
}

#[tauri::command]
async fn probe_file(ctx: State<'_, Ctx>, url: String, cookies: Option<String>, referer: Option<String>, login: Option<String>, password: Option<String>) -> Result<serde_json::Value, String> {
    ndm_core::probe_file(ctx.inner().clone(), url, cookies, referer, login, password).await
}

#[tauri::command]
async fn probe_video(ctx: State<'_, Ctx>, url: String, cookies: Option<String>) -> Result<ndm_core::video::VideoInfo, String> {
    ndm_core::probe_video(ctx.inner().clone(), url, cookies).await
}

#[tauri::command]
// a Tauri command's parameters ARE its IPC surface (url, folder,
// format choice, captured-session token) — named inputs are the
// honest shape here, not a code smell
#[allow(clippy::too_many_arguments)]
async fn add_video(ctx: State<'_, Ctx>, url: String, dir: String, selector: String, audio: bool, title: String, cookies: Option<String>, later: Option<bool>, total: Option<u64>, duration: Option<f64>, force: Option<bool>, queue: Option<String>, description: Option<String>, dir_explicit: Option<bool>) -> Result<String, String> {
    ndm_core::add_video(ctx.inner().clone(), url, dir, selector, audio, title, cookies, later, total, duration, force, queue, description, dir_explicit).await
}

#[tauri::command]
async fn grab_explore(ctx: State<'_, Ctx>, args: GrabExploreArgs) -> Result<GrabSiteDto, String> {
    ndm_core::grab_explore(ctx.inner().clone(), args).await
}

#[tauri::command]
fn get_settings(ctx: State<'_, Ctx>) -> Settings {
    ndm_core::get_settings(ctx.inner().clone())
}

#[tauri::command]
fn set_settings(ctx: State<'_, Ctx>, settings: Settings) -> Result<(), String> {
    ndm_core::set_settings(ctx.inner().clone(), settings)
}

#[tauri::command]
fn set_entry_queue(ctx: State<'_, Ctx>, id: String, queue: String) -> Result<(), String> {
    ndm_core::set_entry_queue(ctx.inner().clone(), id, queue)
}

#[tauri::command]
fn set_entry_limit(ctx: State<'_, Ctx>, id: String, limit: u64) -> Result<(), String> {
    ndm_core::set_entry_limit(ctx.inner().clone(), id, limit)
}

#[tauri::command]
fn set_entry_description(ctx: State<'_, Ctx>, id: String, description: String) -> Result<(), String> {
    ndm_core::set_entry_description(ctx.inner().clone(), id, description)
}

#[tauri::command]
fn move_in_queue(ctx: State<'_, Ctx>, id: String, up: bool) -> Result<(), String> {
    ndm_core::move_in_queue(ctx.inner().clone(), id, up)
}

#[tauri::command]
fn basket_add(ctx: State<'_, Ctx>, url: String) -> Result<(), String> {
    ndm_core::basket_add(ctx.inner().clone(), url)
}

#[tauri::command]
fn scheduler_start_now(ctx: State<'_, Ctx>, queue: Option<String>) -> Result<String, String> {
    ndm_core::scheduler_start_now(ctx.inner().clone(), queue)
}

#[tauri::command]
fn start_all(ctx: State<'_, Ctx>) -> usize {
    ndm_core::start_all(ctx.inner().clone())
}

#[tauri::command]
fn scheduler_stop_now(ctx: State<'_, Ctx>, queue: Option<String>) {
    ndm_core::scheduler_stop_now(ctx.inner().clone(), queue)
}

#[tauri::command]
fn open_path(ctx: State<'_, Ctx>, id: String, folder: bool) -> Result<(), String> {
    ndm_core::open_path(ctx.inner().clone(), id, folder)
}

#[tauri::command]
fn set_completion(ctx: State<'_, Ctx>, id: String, action: String) -> Result<(), String> {
    ndm_core::set_completion(ctx.inner().clone(), id, action)
}

#[tauri::command]
fn set_completion_actions(ctx: State<'_, Ctx>, id: String, actions: Vec<String>) -> Result<(), String> {
    ndm_core::set_completion_actions(ctx.inner().clone(), id, actions)
}

#[tauri::command]
fn set_show_complete_override(ctx: State<'_, Ctx>, id: String, show: Option<bool>) -> Result<(), String> {
    ndm_core::set_show_complete_override(ctx.inner().clone(), id, show)
}

#[tauri::command]
fn rename_category(ctx: State<'_, Ctx>, old: String, new: String) -> Result<(), String> {
    ndm_core::rename_category(ctx.inner().clone(), old, new)
}

#[tauri::command]
fn delete_category(ctx: State<'_, Ctx>, name: String) -> Result<(), String> {
    ndm_core::delete_category(ctx.inner().clone(), name)
}

#[tauri::command]
/// Refresh Address (IDM): point an entry at a fresh URL when the
/// new address serves the same file — or discard the journal and
/// restart from zero when it does not. The entry keeps its name,
/// folder and place in the list, and stays paused either way;
/// which of the two happened is decided at that start.
fn refresh_url(ctx: State<'_, Ctx>, id: String, new_url: String) -> Result<(), String> {
    ndm_core::refresh_url(ctx.inner().clone(), id, new_url)
}

#[tauri::command]
fn take_pending_videos(ctx: State<'_, Ctx>) -> Vec<serde_json::Value> {
    ndm_core::take_pending_videos(ctx.inner().clone())
}

#[tauri::command]
fn take_startup_notice(ctx: State<'_, Ctx>) -> Option<String> {
    ndm_core::take_startup_notice(ctx.inner().clone())
}

#[tauri::command]
fn take_pending_ext_notice() -> Option<serde_json::Value> {
    ndm_core::take_pending_ext_notice()
}

#[tauri::command]
fn take_pending_files(ctx: State<'_, Ctx>) -> Vec<serde_json::Value> {
    ndm_core::take_pending_files(ctx.inner().clone())
}

#[tauri::command]
fn take_pending_grabs(ctx: State<'_, Ctx>) -> Vec<serde_json::Value> {
    ndm_core::take_pending_grabs(ctx.inner().clone())
}

#[tauri::command]
fn components_status(ctx: State<'_, Ctx>) -> serde_json::Value {
    ndm_core::components_status(ctx.inner().clone())
}

#[tauri::command]
fn ensure_components(ctx: State<'_, Ctx>) {
    ndm_core::ensure_components(ctx.inner().clone())
}

#[tauri::command]
fn export_downloads(ctx: State<'_, Ctx>) -> Result<String, String> {
    ndm_core::export_downloads(ctx.inner().clone())
}

#[tauri::command]
async fn check_update(ctx: State<'_, Ctx>) -> Result<serde_json::Value, String> {
    ndm_core::check_update(ctx.inner().clone()).await
}

#[tauri::command]
fn get_pending_update(ctx: State<'_, Ctx>) -> serde_json::Value {
    ndm_core::get_pending_update(ctx.inner().clone())
}

#[tauri::command]
async fn install_update(ctx: State<'_, Ctx>) -> Result<(), String> {
    ndm_core::install_update(ctx.inner().clone()).await
}

#[tauri::command]
fn discard_cookies(ctx: State<'_, Ctx>, token: String) {
    ndm_core::discard_cookies(ctx.inner().clone(), token)
}

/// Reliably raise the main NDM window above the browser on X11 & Wayland.
fn raise_main_window(handle: &AppHandle) {
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_always_on_top(true);
        let _ = w.set_focus();
        let w2 = w.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            let _ = w2.set_always_on_top(false);
        });
    }
}

/* ---------- single-instance guard ----------
   downloads.json has exactly one writer discipline: the running
   app's in-memory list, snapshotted by persist() — including the
   5-second periodic snapshot. A SECOND process on the same profile
   holds its own copy of the list and re-persists it on the same
   cadence, so a delete performed in one window is silently undone
   by the other process within seconds, forever (the user's
   "deleted downloads keep coming back after a restart": the
   resurrected journal is what the next launch reads). Nothing
   guarded against a twin: a launcher double-click, a native-host
   spawn race, or an updater restart could each start one.

   The guard is a loopback TCP listener on a port derived from the
   app-data dir (the same path computation ndm-host uses to find
   the incoming queue), so it is stable per profile — other users
   and other profiles never collide. Whoever binds it is the
   primary. A newcomer that cannot bind watches the port for a
   short grace window: an updater restart's old process exits
   inside it and frees the port (the newcomer then takes over as
   primary); a primary still holding AND answering the magic ping
   at the end of the window is a genuine twin — the newcomer asks
   it to raise its window and exits before building any state of
   its own. If the port is held by something that never answers
   the magic, the newcomer runs unguarded rather than not at all.
   std-only — no new dependencies.

   Two hardenings from the previous audit ride on this design:
   - C-MAJOR: the protocol is SERVED from the moment of bind, in
     main(), before Tauri starts. Serving used to begin only in
     setup() (seconds into a cold start): a newcomer in that
     window connected into the backlog, never got a reply, and
     concluded "not NDM" — the guard failed open exactly during
     primary startup. A "raise" that arrives before the window
     exists is queued and applied in setup().
   - m-1: after handing a raise to a live primary, the newcomer
     watches the port for a bounded window before exiting. An
     updater handover looks identical to a twin launch for its
     first moments (old process still answering, then exits to
     swap binaries): if the holder disappears inside the window,
     the newcomer binds and takes over as primary instead of
     leaving the user with no NDM at all. */
static INSTANCE_HANDLE: Mutex<Option<AppHandle>> = Mutex::new(None);
static INSTANCE_RAISE_PENDING: AtomicBool = AtomicBool::new(false);

enum Instance {
    Primary,
    Secondary,
}

/// Bind the guard port; on success start serving the ping/raise
/// protocol IMMEDIATELY (see C-MAJOR above) and report true.
fn bind_and_serve_instance(port: u16) -> bool {
    let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) else {
        return false;
    };
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).unwrap_or(0);
                let line = String::from_utf8_lossy(&buf[..n]);
                let line = line.trim();
                if line == "ping" || line == "raise" {
                    let _ = s.write_all(b"ndm-here\n");
                    if line == "raise" {
                        match INSTANCE_HANDLE.lock().unwrap().clone() {
                            Some(h) => raise_main_window(&h),
                            // the window does not exist yet (the
                            // primary is still starting): queue the
                            // raise for setup() to apply
                            None => INSTANCE_RAISE_PENDING
                                .store(true, Ordering::SeqCst),
                        }
                    }
                }
            });
        }
    });
    true
}

/// Called from setup(): publish the AppHandle the guard's raise
/// path needs, and apply any raise that arrived while the primary
/// was still starting.
fn register_instance_handle(app: &AppHandle) {
    *INSTANCE_HANDLE.lock().unwrap() = Some(app.clone());
    if INSTANCE_RAISE_PENDING.swap(false, Ordering::SeqCst) {
        raise_main_window(app);
    }
}

fn instance_port() -> u16 {
    let dir = {
        #[cfg(windows)]
        let base = std::env::var_os("APPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = match std::env::var_os("XDG_DATA_HOME") {
            Some(x) if !x.is_empty() => Some(PathBuf::from(x)),
            _ => std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".local").join("share")),
        };
        base.map(|b| b.join("com.ndm.downloadmanager"))
            .unwrap_or_else(|| PathBuf::from("com.ndm.downloadmanager"))
    };
    // m-2: hash the CANONICAL path when it can be resolved, so two
    // launches that reach the same profile through different
    // environment spellings (a symlinked HOME, an XDG_DATA_HOME
    // that names the same directory through another route) still
    // derive the same port and see each other. The directory may
    // not exist yet on a first-ever launch (setup creates it
    // later); the raw path is then hashed, as before.
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in dir.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    41000 + (h % 12000) as u16
}

fn instance_ping(port: u16, verb: &str) -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
    if s.write_all(format!("{verb}\n").as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 32];
    let n = s.read(&mut buf).unwrap_or(0);
    buf[..n].starts_with(b"ndm-here")
}

fn acquire_instance() -> Instance {
    let port = instance_port();
    if bind_and_serve_instance(port) {
        return Instance::Primary;
    }
    // the port is held: an updater restart's old process may free
    // it inside this grace window (then WE are the primary), and a
    // holder answering the magic at the end of it is a live primary
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut primary_alive = false;
    loop {
        if instance_ping(port, "ping") {
            primary_alive = true;
        }
        if bind_and_serve_instance(port) {
            return Instance::Primary;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !primary_alive {
        // the port is held by something that is not NDM (or by
        // nothing reachable): run unguarded rather than not at all
        return Instance::Primary;
    }
    // a live primary holds the port: hand the user to it — unless
    // this process was started hidden by the native host, where
    // raising the window would steal focus for a spawn race the
    // user never saw
    if !std::env::args().any(|a| a == "--background") {
        let _ = instance_ping(port, "raise");
    }
    // m-1: the updater handover is indistinguishable from a twin
    // launch right now — the old process answered us, and is about
    // to exit so the new binary can take its place. Exiting
    // unconditionally here left NO NDM running when that was the
    // case. Watch the port for a bounded window: if the holder
    // disappears, bind and take over; a primary still holding it
    // at the end was a genuine twin launch, and only then do we
    // exit as a secondary.
    let watch_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if bind_and_serve_instance(port) {
            return Instance::Primary;
        }
        if Instant::now() >= watch_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Instance::Secondary
}

fn main() {
    // Windows portable self-update hands the file swap to a copy
    // of this binary in temp, re-invoked with --apply-update: it
    // is not a second instance and must run BEFORE the guard and
    // before any Tauri state exists (it never returns)
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.len() >= 4 && args[1] == "--apply-update" {
            ndm_core::apply_staged_update(
                std::path::Path::new(&args[2]),
                std::path::Path::new(&args[3]),
            );
        }
    }
    // single-instance guard FIRST: a second process must exit
    // before it builds any state — its persist() snapshots would
    // resurrect whatever the real window deletes (see above)
    if matches!(acquire_instance(), Instance::Secondary) {
        // 208B F3: a forwarding/secondary launch never exits
        // silently - the trace is the only sign a handed-over
        // launch ever existed
        eprintln!("[ndm] another NDM instance is already running; this launch hands over to it and exits");
        return;
    }
    tauri::Builder::default()
        .on_page_load(|webview, payload| {
            // A4: the main window starts hidden ("visible": false
            // in tauri.conf.json) and is shown only once its first
            // page has finished loading — i.e. once it has actually
            // painted. Showing it from setup() flashed a blank
            // white window on cold start. A --background launch
            // stays hidden (its later page loads included); setup()
            // keeps a 3 s fallback show in case this event never
            // arrives. Later loads of an already-visible window make
            // show() a no-op.
            if webview.label() == "main"
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
                && !std::env::args().any(|a| a == "--background")
            {
                let _ = webview.window().show();
            }
        })
        .on_window_event(|window, event| {
            // N-12: closing the main window IS the user leaving —
            // there is no tray and no close-to-tray mode anywhere
            // in the app. Tauri only requests an exit when its
            // window map becomes EMPTY, and the hidden drop-basket
            // window keeps that map non-empty forever, so closing
            // main used to leave a windowless zombie process (its
            // WebKit children, bridge sockets and "downloading"
            // claims all alive) with no visible way back. An
            // explicit close of the main window now exits fully,
            // exactly like File → Exit / Ctrl+Q (quit_app).
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { .. } = event {
                    window.app_handle().exit(0);
                }
            }
            // W193-4: an OS-level drop on either window — files,
            // shortcut files, or (on some platforms) the link
            // itself — becomes a download offer in the main
            // window; a plain local file gets an honest refusal
            // on the window that received it
            if let WindowEvent::DragDrop(tauri::DragDropEvent::Drop {
                paths, ..
            }) = event
            {
                let on_basket = window.label() == "basket";
                for p in paths {
                    if let Some(ctx) = window.app_handle().try_state::<Ctx>() {
                        ndm_core::handle_dropped_path(ctx.inner(), p, on_basket);
                    }
                }
            }
        })
        .setup(|app| {
            // a completed portable self-update leaves its staging
            // folder and .old files beside the exe — sweep them
            // now that the new build is the one running
            #[cfg(windows)]
            ndm_core::cleanup_update_leftovers();
            // the single-instance guard has been serving since
            // main() bound the port; setup only publishes the
            // AppHandle its raise path needs (and applies a raise
            // queued during startup)
            register_instance_handle(app.handle());
            if let Some(w) = app.get_webview_window("main") {
                if std::env::args().any(|a| a == "--background") {
                    let _ = w.hide();
                } else {
                    // A4: the window is shown by the builder's
                    // on_page_load handler once the first page has
                    // painted (no blank-window flash). This timer is
                    // only the fallback for a page load that never
                    // finishes, so the app can never sit invisible
                    // forever — and it leaves an already-shown
                    // window exactly as it is.
                    let w2 = w.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(3));
                        if !w2.is_visible().unwrap_or(false) {
                            let _ = w2.show();
                        }
                    });
                }
            }
            let dir = app.path().app_data_dir()?;
            // Build the Phase 1 seam: the core state plus this
            // shell's event sink, managed as the Ctx every command
            // wrapper forwards into ndm-core.
            let state = Arc::new(ndm_core::AppState::new());
            let sink: Arc<dyn ndm_core::EventSink> = Arc::new(TauriSink {
                app: app.handle().clone(),
            });
            let ctx = Ctx::new(state.clone(), sink);
            app.manage(ctx.clone());
            state.init_paths(&dir)?;
            // Batch-3 (N2/F1): stale .ndm.lock sweep over the
            // restored entries' folders + a rolled-back portable
            // update's marker, surfaced before the ext notice —
            // the monolith setup() order
            ndm_core::run_startup_sweeps(&state);
            // EXT §1c: the extension-update notice is also
            // evaluated once at startup (and then throttled
            // through drain_incoming)
            ndm_core::check_ext_notice(&ctx, &state);
            // The background bodies setup() used to inline are
            // ndm-core entry points now; the shell only calls them.
            ndm_core::run_launch_update_check(ctx.clone());
            ndm_core::run_update_recheck_loop(ctx.clone());
            ndm_core::run_component_warmup(ctx.clone());
            ndm_core::startup_resume(&ctx);
            ndm_core::run_scheduler_loop(ctx.clone());
            ndm_core::run_clipboard_watcher(ctx.clone());
            ndm_core::run_snapshot_loop(ctx.clone());
            ndm_core::run_incoming_loop(ctx, dir);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add_download,
            pause_download,
            resume_download,
            remove_download,
            remove_downloads,
            list_downloads,
            probe_file,
            probe_video,
            add_video,
            grab_explore,
            get_settings,
            dialog_open_values,
            set_settings,
            set_entry_queue,
            set_entry_limit,
            set_entry_description,
            move_in_queue,
            basket_add,
            set_basket_visible,
            scheduler_start_now,
            start_all,
            scheduler_stop_now,
            open_path,
            set_completion,
            set_completion_actions,
            set_show_complete_override,
            rename_category,
            delete_category,
            dismiss_ext_notice,
            refresh_url,
            take_pending_videos,
            take_startup_notice,
            take_pending_ext_notice,
            take_pending_files,
            take_pending_grabs,
            components_status,
            ensure_components,
            app_version,
            quit_app,
            restart_app,
            export_downloads,
            check_update,
            get_pending_update,
            install_update,
            discard_cookies
        ])
        .run(tauri::generate_context!())
        .expect("error while running NDM");
}
