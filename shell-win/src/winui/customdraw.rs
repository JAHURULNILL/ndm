//! NM_CUSTOMDRAW hook points for the download ListView.
//!
//! The ListView wrapper forwards NM_CUSTOMDRAW notifications here.
//! Phase 2's real payload is the per-row block map (the 128-char
//! `blocks` string + G1 `conns_state` data on ProgressPayload): one
//! cell paints a segmented strip — sealed / active / pending spans —
//! in the row, the same information the web UI draws on a canvas.
//!
//! For the scaffold the paint body is a faithful PLACEHOLDER:
//! `paint_block_map_placeholder` draws the strip's geometry exactly
//! (full-cell strip, 128 equal segments, alternating sealed/pending
//! shading by segment parity) so layout, clipping and the custom-draw
//! return-value protocol are all exercised; only the per-segment
//! state lookup is missing.
//!
//! MARKER (Phase 2): replace the parity test in
//! `paint_block_map_placeholder` with the row's real block state:
//! '2'/sealed = solid accent, '1'/active = hatched accent,
//! '0'/pending = background — fed from the row model the Progress
//! handler maintains in main.rs.

use std::sync::Mutex;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, HDC, HGDIOBJ};
use windows::Win32::UI::Controls::{
    CDRF_DODEFAULT, CDRF_NOTIFYITEMDRAW, CDRF_NOTIFYSUBITEMDRAW, CDDS_ITEMPREPAINT, CDDS_PREPAINT,
    CDDS_SUBITEM, NMLVCUSTOMDRAW,
};

/// The listview column index whose cells carry the block map.
/// PHASE-2 WIRING: the shipped column set (ui/app.js COLUMN_DEFS) has
/// no block-map column — in the web UI the map lives in the progress
/// dialog's canvas. Phase 2 decides whether the map rides the Status
/// subitem or a dedicated column; until then custom draw is armed
/// for this subitem index and paints only when a row model marks
/// the row as carrying a map (the row model's `has_block_map`
/// answer in `on_custom_draw`).
pub const BLOCK_MAP_SUBITEM: i32 = 3;

/// Handle one NM_CUSTOMDRAW for the list. Returns the CDRF_* value
/// the window proc must return for WM_NOTIFY. `has_block_map` is the
/// row model's answer for the item being drawn.
pub unsafe fn on_custom_draw(nm: *mut NMLVCUSTOMDRAW, has_block_map: impl Fn(i32) -> bool) -> isize {
    if nm.is_null() {
        return CDRF_DODEFAULT as isize;
    }
    let cd = &mut (*nm).nmcd;
    let stage = cd.dwDrawStage;
    if stage == CDDS_PREPAINT {
        // Ask for per-item (then per-subitem) notifications.
        return CDRF_NOTIFYITEMDRAW as isize;
    }
    if stage.0 == (CDDS_ITEMPREPAINT.0 | CDDS_SUBITEM.0) {
        // A subitem of a row is about to paint.
        if (*nm).iSubItem == BLOCK_MAP_SUBITEM && has_block_map(cd.dwItemSpec as i32) {
            paint_block_map_placeholder(cd.hdc, &cd.rc);
            // We painted the cell content; still let the control
            // paint text/borders around it.
            return CDRF_NOTIFYSUBITEMDRAW as isize;
        }
        return CDRF_NOTIFYSUBITEMDRAW as isize;
    }
    if stage == CDDS_ITEMPREPAINT {
        return CDRF_NOTIFYSUBITEMDRAW as isize;
    }
    CDRF_DODEFAULT as isize
}

// ----------------------------------------------------------------
// The progress dialog's segment strip (PROGRESS-DIALOG-SPEC G5):
// contiguous cells left→right = file start→end (a second row only
// when the cell count forces it, >512), '1' = full fill, '2' =
// fractional fill from the owning connection's in-flight bytes,
// '0' = empty track, and a start tick under each active
// connection's current block. Fed through `set_strip_data` — the
// dialog updates it only when the payload's strip fields actually
// change, and the paint reads one snapshot.
// ----------------------------------------------------------------

/// One dialog's strip payload (the §3 fields, verbatim shapes).
#[derive(Clone, Default)]
pub struct StripData {
    /// '0' pending / '1' done / '2' active, one char per cell
    /// (core's `downsample_blocks` output, ≤1024 cells).
    pub blocks: String,
    /// G1 per-connection in-flight bytes, parallel to conns_block.
    pub conns_partial: Vec<u64>,
    /// G1 per-connection current engine block index (-1 = none).
    pub conns_block: Vec<i64>,
    /// G1 per-connection phase (2 = receiving), parallel.
    pub conns_state: Vec<u8>,
    pub total: u64,
    pub finished: bool,
}

/// Strip snapshots keyed by the strip control's HWND value.
/// One shared snapshot for all dialogs meant a non-active
/// dialog painted the ACTIVE download's block map (audit-233:
/// "strip shows wrong download's blocks"); each strip now
/// paints only its own dialog's data.
static STRIP_DATA: std::sync::LazyLock<Mutex<std::collections::HashMap<isize, StripData>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Replace one strip's snapshot (called by the progress dialog
/// only when its strip inputs changed — never per progress tick).
pub fn set_strip_data(key: isize, data: StripData) {
    STRIP_DATA
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, data);
}

/// Drop a strip's snapshot when its dialog dies.
pub fn clear_strip_data(key: isize) {
    STRIP_DATA
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
}

/// The engine's block size (ndm::DEFAULT_BLOCK, 1 MiB): the
/// payload carries no block-size field, so fractional fill and
/// the downsample mapping use it — partial bytes can never
/// overstate a fill, since a connection's in-flight bytes belong
/// to exactly one engine block.
const ENGINE_BLOCK: u64 = 1 << 20;

/// Paint the segment strip into `rc` (the strip control's client
/// rect). The bottom 4 px are the start-tick row. `key` is the
/// strip control's HWND value — its own dialog's snapshot only.
pub unsafe fn paint_segment_strip(hdc: HDC, rc: &RECT, key: isize) {
    let data = STRIP_DATA
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
        .unwrap_or_default();
    let width = rc.right - rc.left;
    let cell_bottom = rc.bottom - 4;
    if width <= 0 || cell_bottom <= rc.top {
        return;
    }
    // The web UI's strip palette (ui/app.js drawSegMap fallbacks):
    // done #3f9de0, active #f5c542, pending #c9d2da (COLORREF is
    // 0x00BBGGRR).
    let done = CreateSolidBrush(COLORREF(0x00E0_9D3F));
    let active = CreateSolidBrush(COLORREF(0x0042_C5F5));
    let pending = CreateSolidBrush(COLORREF(0x00DA_D2C9));
    let n = data.blocks.len().min(1024);
    if n == 0 {
        // No block map yet (total unknown): an honest empty track.
        let track = RECT { left: rc.left, top: rc.top, right: rc.right, bottom: cell_bottom };
        FillRect(hdc, &track, pending);
    } else {
        let rows: i32 = if n > 512 { 2 } else { 1 };
        let cols = (n as i32 + rows - 1) / rows;
        let row_h = (cell_bottom - rc.top) / rows;
        // Engine block count estimate for mapping G1's raw block
        // indices onto the (possibly downsampled) cell space.
        let total_blocks = if data.total > 0 {
            (data.total.div_ceil(ENGINE_BLOCK) as usize).max(n)
        } else {
            n
        };
        let cell_of = |block: i64| -> Option<usize> {
            if block < 0 {
                return None;
            }
            let b = block as usize;
            let cell = if total_blocks <= n { b } else { b * n / total_blocks };
            if cell < n { Some(cell) } else { None }
        };
        let frac_of = |cell: usize| -> f64 {
            for (w, b) in data.conns_block.iter().enumerate() {
                if cell_of(*b) == Some(cell) {
                    let p = data.conns_partial.get(w).copied().unwrap_or(0);
                    return (p as f64 / ENGINE_BLOCK as f64).min(1.0);
                }
            }
            0.0
        };
        let chars: Vec<char> = data.blocks.chars().collect();
        for i in 0..n {
            let row = (i as i32) / cols;
            let col = (i as i32) % cols;
            let x0 = rc.left + (width * col) / cols;
            let x1 = rc.left + (width * (col + 1)) / cols;
            let y0 = rc.top + row_h * row;
            let y1 = rc.top + row_h * (row + 1);
            // Sub-pixel cells (1024 over a dialog width) still get
            // their 1 px; the next cell paints over any overlap.
            let gap = if x1 - x0 >= 4 { 1 } else { 0 };
            let cell = RECT { left: x0, top: y0, right: (x1 - gap).max(x0 + 1), bottom: y1 - 1 };
            let c = if data.finished { '1' } else { chars.get(i).copied().unwrap_or('0') };
            match c {
                '1' => {
                    FillRect(hdc, &cell, done);
                }
                '2' => {
                    FillRect(hdc, &cell, pending);
                    // A claimed block with no bytes yet still shows
                    // a sliver, so an active cell never reads as
                    // untouched track (the web strip's rule).
                    let f = frac_of(i).max(0.08);
                    let w = ((cell.right - cell.left) as f64 * f).ceil() as i32;
                    let fill = RECT { right: (cell.left + w).min(cell.right), ..cell };
                    FillRect(hdc, &fill, active);
                }
                _ => {
                    FillRect(hdc, &cell, pending);
                }
            }
        }
        // Start ticks: one under each active connection's current
        // block (a connection counts active when it holds bytes
        // of the block or is in the receiving phase — G1).
        for (w, b) in data.conns_block.iter().enumerate() {
            let Some(cell_idx) = cell_of(*b) else { continue };
            let has_bytes = data.conns_partial.get(w).copied().unwrap_or(0) > 0;
            let receiving = data.conns_state.get(w).copied().unwrap_or(0) == 2;
            if !has_bytes && !receiving {
                continue;
            }
            let row = (cell_idx as i32) / cols;
            let col = (cell_idx as i32) % cols;
            let cx = rc.left + (width * col) / cols + (width / cols) / 2;
            let tick = RECT {
                left: cx - 1,
                top: rc.top + row_h * (row + 1) - 3,
                right: cx + 2,
                bottom: rc.top + row_h * (row + 1),
            };
            FillRect(hdc, &tick, done);
        }
    }
    let _ = DeleteObject(HGDIOBJ(done.0));
    let _ = DeleteObject(HGDIOBJ(active.0));
    let _ = DeleteObject(HGDIOBJ(pending.0));
}

/// Placeholder strip paint — see the module header. 128 segments
/// across the cell rect, inset by 2px, height centred.
unsafe fn paint_block_map_placeholder(hdc: HDC, rc: &RECT) {
    let width = (rc.right - rc.left - 4).max(0);
    let top = rc.top + 3;
    let bottom = rc.bottom - 3;
    if width <= 0 || bottom <= top {
        return;
    }
    // Segment colours (COLORREF is 0x00BBGGRR): sealed = blue-grey,
    // pending = light grey. These are placeholders, not a palette.
    let sealed = CreateSolidBrush(COLORREF(0x00A6_8D7C));
    let pending = CreateSolidBrush(COLORREF(0x00E8_E8E8));
    for i in 0..128i32 {
        let x0 = rc.left + 2 + (width * i) / 128;
        let x1 = rc.left + 2 + (width * (i + 1)) / 128;
        if x1 <= x0 {
            continue;
        }
        let seg = RECT { left: x0, top, right: x1, bottom };
        // PLACEHOLDER state: parity stands in for the real block
        // state lookup (see module MARKER).
        let brush = if i % 2 == 0 { sealed } else { pending };
        FillRect(hdc, &seg, brush);
    }
    let _ = DeleteObject(HGDIOBJ(sealed.0));
    let _ = DeleteObject(HGDIOBJ(pending.0));
}
