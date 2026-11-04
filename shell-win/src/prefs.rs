//! UI preferences — the native mirror of the web UI's
//! localStorage keys (`ndm-columns-v2`, `ndm-toolbar-v1`, same
//! JSON shapes), kept in one small file in the app-data dir
//! (`ui-prefs.json`: a flat {key: value} object). Shell-local
//! presentation state only; download settings live in core.

use std::sync::Mutex;

static CACHE: Mutex<Option<serde_json::Value>> = Mutex::new(None);

fn load() -> serde_json::Value {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = guard.as_ref() {
        return v.clone();
    }
    let path = crate::app_data_dir().join("ui-prefs.json");
    let v = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    *guard = Some(v.clone());
    v
}

pub fn get(key: &str) -> Option<serde_json::Value> {
    load().get(key).cloned()
}

pub fn set(key: &str, value: serde_json::Value) {
    let mut all = load();
    all[key] = value;
    {
        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(all.clone());
    }
    let path = crate::app_data_dir().join("ui-prefs.json");
    if let Ok(body) = serde_json::to_string(&all) {
        // Temp + rename: a crash mid-write used to leave a torn
        // file that load() read as {} — every UI preference gone.
        let tmp = crate::app_data_dir().join("ui-prefs.json.tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}
