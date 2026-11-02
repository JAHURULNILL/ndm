//! The core seam — the real `ndm-core`, re-exported.
//!
//! This module used to hold a 1:1 mirror of the seam (written
//! because `../core` did not exist when the scaffold was built).
//! The Phase-1 extraction has landed, so the mirror types are
//! gone: every seam item below is `ndm_core`'s own, and call
//! sites in this crate keep naming `crate::sink::{...}`.
//!
//! Two real deltas the switch exposes, both adapted here in the
//! shell (core is unchanged):
//!   * Core's `AppEvent::UpdateInstalled` / `ExitVetoed` carry a
//!     `serde_json::Value` payload; the mirror had them as unit
//!     variants. The match arms in main.rs now bind the payload.
//!   * Core's `ProgressPayload` keeps its fields private and
//!     derives only `Clone + Serialize` (constructed inside core
//!     by `payload_of`). The scaffold read payload fields
//!     directly, so the shell now reads them through
//!     `ProgressView` — a shell-owned, field-readable decode of
//!     the payload's own JSON wire shape (the exact
//!     serialization `AppEvent::payload()` produces).
//!
//! `NullSink` stays shell-owned: a drop-everything sink for
//! headless wiring and tests, implementing core's `EventSink`.

pub use ndm_core::{AppEvent, AppState, Ctx, EventSink, ProgressPayload};

/// Field-readable view of a core `ProgressPayload`, decoded from
/// its serialized wire shape. Field names, order and types match
/// core's struct exactly (the old mirror's list); missing keys
/// decode to the type's zero value, matching how the web UI
/// treated absent fields.
#[derive(Clone, Debug, Default)]
pub struct ProgressView {
    pub id: String,
    pub name: String,
    pub url: String,
    pub total: u64,
    pub done_bytes: u64,
    pub speed_bps: f64,
    pub conns: usize,
    pub status: String,
    /// 128-char block map ('#'/sealed, '+'/'='/active, '.'/pending…)
    pub blocks: String,
    pub category: String,
    pub added: String,
    pub last_try: String,
    pub resume_capability: String,
    pub completion: String,
    pub error: String,
    pub queue: String,
    pub conns_detail: Vec<u64>,
    /// G1 per-connection state codes (progress-dialog segment strip)
    pub conns_state: Vec<u8>,
    pub conns_partial: Vec<u64>,
    pub conns_block: Vec<i64>,
    pub description: String,
    pub speed_limit: u64,
    pub save_to: String,
    pub referer: String,
    /// G2 completion action set + show-complete override
    pub completion_actions: Vec<String>,
    pub show_complete_override: Option<bool>,
    pub final_path: String,
    /// truthful sub-status ("Probing connection…"); "" = none
    pub detail: String,
}

impl From<&ProgressPayload> for ProgressView {
    fn from(payload: &ProgressPayload) -> Self {
        let v = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
        fn s(v: &serde_json::Value, key: &str) -> String {
            v.get(key)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        }
        fn u(v: &serde_json::Value, key: &str) -> u64 {
            v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
        }
        fn f(v: &serde_json::Value, key: &str) -> f64 {
            v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0)
        }
        fn vu(v: &serde_json::Value, key: &str) -> Vec<u64> {
            v.get(key)
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
                .unwrap_or_default()
        }
        fn vi(v: &serde_json::Value, key: &str) -> Vec<i64> {
            v.get(key)
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default()
        }
        fn vs(v: &serde_json::Value, key: &str) -> Vec<String> {
            v.get(key)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        }
        ProgressView {
            id: s(&v, "id"),
            name: s(&v, "name"),
            url: s(&v, "url"),
            total: u(&v, "total"),
            done_bytes: u(&v, "done_bytes"),
            speed_bps: f(&v, "speed_bps"),
            conns: u(&v, "conns") as usize,
            status: s(&v, "status"),
            blocks: s(&v, "blocks"),
            category: s(&v, "category"),
            added: s(&v, "added"),
            last_try: s(&v, "last_try"),
            resume_capability: s(&v, "resume_capability"),
            completion: s(&v, "completion"),
            error: s(&v, "error"),
            queue: s(&v, "queue"),
            conns_detail: vu(&v, "conns_detail"),
            conns_state: vu(&v, "conns_state")
                .into_iter()
                .map(|n| n as u8)
                .collect(),
            conns_partial: vu(&v, "conns_partial"),
            conns_block: vi(&v, "conns_block"),
            description: s(&v, "description"),
            speed_limit: u(&v, "speed_limit"),
            save_to: s(&v, "save_to"),
            referer: s(&v, "referer"),
            completion_actions: vs(&v, "completion_actions"),
            show_complete_override: v
                .get("show_complete_override")
                .and_then(|x| x.as_bool()),
            final_path: s(&v, "final_path"),
            detail: s(&v, "detail"),
        }
    }
}

/// A sink that drops everything — for headless wiring and tests.
pub struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AppEvent) {}
}
