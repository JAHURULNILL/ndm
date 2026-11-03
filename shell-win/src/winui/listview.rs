//! SysListView32 wrapper — report view, full-row-select, gridlines,
//! double-buffer, columns from a spec table, row insert/update, and
//! NM_CUSTOMDRAW forwarding to `customdraw`.

use super::{instance, send, wide};
use windows::core::{Result, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{
    LIST_VIEW_ITEM_STATE_FLAGS, LVCOLUMNW, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVIF_TEXT,
    LVIS_SELECTED, LVIS_STATEIMAGEMASK, LVITEMW, LVM_DELETEALLITEMS, LVM_DELETECOLUMN,
    LVM_DELETEITEM, LVM_GETITEMCOUNT, LVM_GETITEMSTATE, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW,
    LVM_INSERTITEMW, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE,
    LVM_SETITEMTEXTW, LVS_EX_CHECKBOXES, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT,
    LVS_EX_GRIDLINES, LVS_REPORT, LVS_SHOWSELALWAYS, LVNI_SELECTED, WC_LISTVIEWW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, HMENU, WINDOW_STYLE, WS_CHILD, WS_EX_CLIENTEDGE, WS_VISIBLE,
};

/// One column of the ListView spec table.
#[derive(Clone, Copy, Debug)]
pub struct ColumnSpec {
    pub label: &'static str,
    pub width: i32,
}

pub struct ListView {
    pub hwnd: HWND,
}

impl ListView {
    pub fn create(parent: HWND, control_id: i32, columns: &[ColumnSpec]) -> Result<Self> {
        Self::create_ex(parent, control_id, columns, false)
    }

    /// `checkboxes` adds LVS_EX_CHECKBOXES (the Columns / Toolbar /
    /// Grabber lists): per-row check state read via `is_checked`.
    pub fn create_ex(parent: HWND, control_id: i32, columns: &[ColumnSpec], checkboxes: bool) -> Result<Self> {
        unsafe {
            let style = WS_CHILD | WS_VISIBLE | WINDOW_STYLE(LVS_REPORT | LVS_SHOWSELALWAYS);
            let hwnd = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                WC_LISTVIEWW,
                PCWSTR::null(),
                style,
                0,
                0,
                0,
                0,
                Some(parent),
                Some(HMENU(control_id as *mut core::ffi::c_void)),
                Some(instance()),
                None,
            )?;
            let mut extended = (LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_DOUBLEBUFFER) as isize;
            if checkboxes {
                extended |= LVS_EX_CHECKBOXES as isize;
            }
            send(hwnd, LVM_SETEXTENDEDLISTVIEWSTYLE, WPARAM(0), LPARAM(extended));
            let lv = Self { hwnd };
            for (i, col) in columns.iter().enumerate() {
                lv.insert_column(i as i32, col);
            }
            Ok(lv)
        }
    }

    pub fn insert_column(&self, index: i32, col: &ColumnSpec) {
        let mut buf = wide(col.label);
        let c = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
            iSubItem: index,
            cx: col.width,
            pszText: PWSTR(buf.as_mut_ptr()),
            ..Default::default()
        };
        unsafe {
            send(self.hwnd, LVM_INSERTCOLUMNW, WPARAM(index as usize), LPARAM(&c as *const LVCOLUMNW as isize));
        }
    }

    /// Insert a new row from its cells (cells[0] creates the item,
    /// the rest become subitems). Returns the row index used.
    pub fn insert_row(&self, row: i32, cells: &[String]) -> i32 {
        for (j, cell) in cells.iter().enumerate() {
            self.set_cell(row, j as i32, cell, true);
        }
        row
    }

    /// Update one cell of an existing row.
    pub fn set_text(&self, row: i32, subitem: i32, text: &str) {
        self.set_cell(row, subitem, text, false);
    }

    /// Update all cells of an existing row.
    pub fn update_row(&self, row: i32, cells: &[String]) {
        for (j, cell) in cells.iter().enumerate() {
            self.set_cell(row, j as i32, cell, false);
        }
    }

    fn set_cell(&self, row: i32, subitem: i32, text: &str, create: bool) {
        let mut buf = wide(text);
        let item = LVITEMW {
            mask: LVIF_TEXT,
            iItem: row,
            iSubItem: subitem,
            pszText: PWSTR(buf.as_mut_ptr()),
            ..Default::default()
        };
        unsafe {
            if create && subitem == 0 {
                send(self.hwnd, LVM_INSERTITEMW, WPARAM(0), LPARAM(&item as *const LVITEMW as isize));
            } else {
                send(self.hwnd, LVM_SETITEMTEXTW, WPARAM(row as usize), LPARAM(&item as *const LVITEMW as isize));
            }
        }
    }

    pub fn delete_row(&self, row: i32) {
        unsafe {
            send(self.hwnd, LVM_DELETEITEM, WPARAM(row as usize), LPARAM(0));
        }
    }

    pub fn clear(&self) {
        unsafe {
            send(self.hwnd, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));
        }
    }

    /// The selected row indices, ascending (LVM_GETNEXTITEM walk).
    pub fn selected_rows(&self) -> Vec<i32> {
        let mut out = Vec::new();
        unsafe {
            let mut next = -1isize;
            loop {
                let r = send(
                    self.hwnd,
                    LVM_GETNEXTITEM,
                    WPARAM(next as usize),
                    LPARAM(LVNI_SELECTED as isize),
                );
                if r < 0 {
                    break;
                }
                out.push(r as i32);
                next = r;
            }
        }
        out
    }

    /// Select or deselect one row.
    pub fn set_selected(&self, row: i32, on: bool) {
        let item = LVITEMW {
            stateMask: LVIS_SELECTED,
            state: if on { LVIS_SELECTED } else { LIST_VIEW_ITEM_STATE_FLAGS(0) },
            ..Default::default()
        };
        unsafe {
            send(self.hwnd, LVM_SETITEMSTATE, WPARAM(row as usize), LPARAM(&item as *const LVITEMW as isize));
        }
    }

    /// Select every row (wparam -1 = all items).
    pub fn select_all(&self) {
        let item = LVITEMW {
            stateMask: LVIS_SELECTED,
            state: LVIS_SELECTED,
            ..Default::default()
        };
        unsafe {
            send(self.hwnd, LVM_SETITEMSTATE, WPARAM(usize::MAX), LPARAM(&item as *const LVITEMW as isize));
        }
    }

    pub fn row_count(&self) -> i32 {
        unsafe { send(self.hwnd, LVM_GETITEMCOUNT, WPARAM(0), LPARAM(0)) as i32 }
    }

    /// Checkbox state of one row (LVS_EX_CHECKBOXES lists): the
    /// state image index is 1 = unchecked, 2 = checked.
    pub fn is_checked(&self, row: i32) -> bool {
        unsafe {
            send(
                self.hwnd,
                LVM_GETITEMSTATE,
                WPARAM(row as usize),
                LPARAM(LVIS_STATEIMAGEMASK.0 as isize),
            ) >> 12
                == 2
        }
    }

    pub fn set_checked(&self, row: i32, on: bool) {
        let item = LVITEMW {
            stateMask: LVIS_STATEIMAGEMASK,
            state: LIST_VIEW_ITEM_STATE_FLAGS(((if on { 2 } else { 1 }) << 12) as u32),
            ..Default::default()
        };
        unsafe {
            send(self.hwnd, LVM_SETITEMSTATE, WPARAM(row as usize), LPARAM(&item as *const LVITEMW as isize));
        }
    }

    /// Remove `count` columns (always from index 0 — the caller's
    /// rebuild re-inserts the full set right after).
    pub fn delete_columns(&self, count: i32) {
        unsafe {
            for _ in 0..count {
                send(self.hwnd, LVM_DELETECOLUMN, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Forward an NM_CUSTOMDRAW notification (called from the parent
    /// window's WM_NOTIFY handler). `has_block_map(row)` is the row
    /// model's answer for the row being drawn.
    pub unsafe fn on_custom_draw(&self, lparam: LPARAM, has_block_map: impl Fn(i32) -> bool) -> isize {
        use windows::Win32::UI::Controls::NMLVCUSTOMDRAW;
        let nm = lparam.0 as *mut NMLVCUSTOMDRAW;
        super::customdraw::on_custom_draw(nm, has_block_map)
    }
}
