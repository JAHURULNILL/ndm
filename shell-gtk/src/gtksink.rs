//! The GTK `EventSink`: core worker threads → GTK main thread.
//!
//! Core emits from its own threads (emit loops, scheduler,
//! incoming watchers — NATIVE-PHASE1-LOG.md §3). GTK widgets may
//! only be touched on the main thread, so this sink never touches
//! a widget: it posts every event into a `glib::MainContext`
//! channel, and the receiver — attached to the default main
//! context in `main.rs` — dispatches to the UI handler there.
//! (`MainContext::channel` is the bridge the plan names; glib
//! marks it deprecated in favour of an async-channel +
//! `spawn_future_local` pairing, which is the drop-in upgrade if
//! this shell ever needs back-pressure — the sink's surface does
//! not change either way.)

use crate::sink::{AppEvent, EventSink};

/// Everything the sink can ask the main thread to do. `emit` is
/// the event surface; `RaiseMainWindow` / `ExitApp` are the two
/// capability calls from the `EventSink` contract, carried over
/// the same channel so ordering with events is preserved.
pub enum UiMessage {
    Event(AppEvent),
    RaiseMainWindow,
    ExitApp,
}

/// The channel's receiving end, handed to `main.rs` for
/// attachment. Wrapped in an option cell because
/// `Application::connect_activate` requires an `Fn` closure while
/// `Receiver::attach` consumes the receiver.
pub struct ReceiverSlot {
    inner: std::cell::RefCell<Option<glib::Receiver<UiMessage>>>,
}

impl ReceiverSlot {
    #[allow(deprecated)]
    pub fn new(receiver: glib::Receiver<UiMessage>) -> Self {
        ReceiverSlot {
            inner: std::cell::RefCell::new(Some(receiver)),
        }
    }

    /// Attach the receiver to the thread-default main context
    /// with `handler` as the dispatch closure. No-op if already
    /// attached (activate can fire again on re-launch attempts).
    pub fn attach<F: FnMut(UiMessage) -> glib::ControlFlow + 'static>(&self, handler: F) {
        if let Some(receiver) = self.inner.borrow_mut().take() {
            receiver.attach(None, handler);
        }
    }
}

/// `EventSink` implementation backed by a main-context channel.
/// Cheap to clone the sender into as many `Ctx` clones as core
/// spawns; the sink itself is shared behind `Arc<dyn EventSink>`.
pub struct GtkEventSink {
    #[allow(deprecated)]
    sender: glib::Sender<UiMessage>,
}

impl GtkEventSink {
    #[allow(deprecated)]
    pub fn new(sender: glib::Sender<UiMessage>) -> Self {
        GtkEventSink { sender }
    }
}

impl EventSink for GtkEventSink {
    fn emit(&self, event: AppEvent) {
        // A failed send means the UI is gone (receiver dropped
        // during shutdown) — the event has nowhere honest to go,
        // so it is dropped, exactly as a Tauri emit into a closed
        // window was dropped.
        let _ = self.sender.send(UiMessage::Event(event));
    }

    fn raise_main_window(&self) {
        let _ = self.sender.send(UiMessage::RaiseMainWindow);
    }

    fn exit_app(&self) {
        let _ = self.sender.send(UiMessage::ExitApp);
    }
}
