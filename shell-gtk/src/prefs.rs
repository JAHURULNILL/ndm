//! UI preferences — the native mirror of the web UI's
//! localStorage keys (`ndm-columns-v2`, `ndm-toolbar-v1`, same
//! JSON shapes), kept in one small file in the app-data dir
//! (`ui-prefs.json`: a flat {key: value} object) — the Windows
//! shell's prefs.rs precedent, written for GTK. Shell-local
//! presentation state only; download settings live in core.

use std::path::PathBuf;

fn prefs_path() -> PathBuf {
    crate::app_data_dir().join("ui-prefs.json")
}

fn load_all() -> serde_json::Value {
    std::fs::read(prefs_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

pub fn get(key: &str) -> Option<serde_json::Value> {
    load_all().get(key).cloned()
}

pub fn set(key: &str, value: serde_json::Value) {
    let mut all = load_all();
    all[key] = value;
    if let Ok(body) = serde_json::to_string(&all) {
        let _ = std::fs::write(prefs_path(), body);
    }
}

// ---- Columns (app.js COLUMN_DEFS / loadColumnCfg) ---------------

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnCfg {
    pub id: String,
    pub visible: bool,
    pub width: i32,
}

/// (id, label, default width, default-off) — app.js COLUMN_DEFS.
pub const COLUMN_DEFS: [(&str, &str, i32, bool); 10] = [
    ("name", "File Name", 0, false),
    ("q", "Q", 30, false),
    ("size", "Size", 84, false),
    ("status", "Status", 92, false),
    ("timeleft", "Time left", 96, false),
    ("rate", "Transfer rate", 96, false),
    ("lasttry", "Last Try Date", 138, false),
    ("desc", "Description", 150, false),
    ("added", "Date Added", 138, true),
    ("saveto", "Save To", 240, true),
];

pub fn default_columns() -> Vec<ColumnCfg> {
    COLUMN_DEFS
        .iter()
        .map(|(id, _, w, off)| ColumnCfg {
            id: id.to_string(),
            visible: !off,
            width: *w,
        })
        .collect()
}

pub fn load_columns() -> Vec<ColumnCfg> {
    let Some(raw) = get("ndm-columns-v2").and_then(|v| v.as_array().cloned()) else {
        return default_columns();
    };
    let mut out: Vec<ColumnCfg> = Vec::new();
    for r in &raw {
        let Some(id) = r.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        if !COLUMN_DEFS.iter().any(|(d, _, _, _)| *d == id) || out.iter().any(|o| o.id == id) {
            continue;
        }
        out.push(ColumnCfg {
            id: id.to_string(),
            visible: r.get("visible").and_then(|x| x.as_bool()).unwrap_or(true),
            width: (r.get("width").and_then(|x| x.as_i64()).unwrap_or(0) as i32).max(0),
        });
    }
    for d in default_columns() {
        if !out.iter().any(|o| o.id == d.id) {
            out.push(d);
        }
    }
    // the name column is the list's anchor — it cannot be hidden
    if let Some(nm) = out.iter_mut().find(|o| o.id == "name") {
        nm.visible = true;
    }
    out
}

pub fn save_columns(cfg: &[ColumnCfg]) {
    let arr: Vec<serde_json::Value> = cfg
        .iter()
        .map(|c| serde_json::json!({"id": c.id, "visible": c.visible, "width": c.width}))
        .collect();
    set("ndm-columns-v2", serde_json::Value::Array(arr));
}

// ---- Toolbar (app.js TOOLBAR_DEFS / loadToolbarCfg) -------------

/// (id, label) — app.js TOOLBAR_DEFS, in bar order.
pub const TOOLBAR_DEFS: [(&str, &str); 11] = [
    ("btnAdd", "Add URL"),
    ("btnResume", "Resume"),
    ("btnStop", "Stop"),
    ("btnStopAll", "Stop All"),
    ("btnDelete", "Delete"),
    ("btnDelDone", "Delete Completed"),
    ("btnOptions", "Options"),
    ("btnScheduler", "Scheduler"),
    ("btnStartQueue", "Start Queue"),
    ("btnStopQueue", "Stop Queue"),
    ("btnGrabber", "Grabber"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct ToolbarCfg {
    pub hidden: Vec<String>,
    pub size: String, // "large" | "small"
}

pub fn load_toolbar() -> ToolbarCfg {
    let Some(raw) = get("ndm-toolbar-v1") else {
        return ToolbarCfg { hidden: Vec::new(), size: "large".to_string() };
    };
    let hidden = raw
        .get("hidden")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .filter(|id| TOOLBAR_DEFS.iter().any(|(d, _)| d == id))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    ToolbarCfg {
        hidden,
        size: if raw.get("size").and_then(|x| x.as_str()) == Some("small") {
            "small".to_string()
        } else {
            "large".to_string()
        },
    }
}

pub fn save_toolbar(cfg: &ToolbarCfg) {
    set(
        "ndm-toolbar-v1",
        serde_json::json!({"hidden": cfg.hidden, "size": cfg.size}),
    );
}
