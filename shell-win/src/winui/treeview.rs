//! SysTreeView32 wrapper: creation + node insertion.

use super::{instance, send, wide};
use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{
    HTREEITEM, TVE_EXPAND, TVIF_PARAM, TVIF_TEXT, TVI_LAST, TVI_ROOT, TVINSERTSTRUCTW,
    TVINSERTSTRUCTW_0, TVITEMW, TVM_DELETEITEM, TVM_EXPAND, TVM_INSERTITEMW, WC_TREEVIEWW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, HMENU, WINDOW_STYLE, WS_CHILD, WS_EX_CLIENTEDGE, WS_VISIBLE,
};

/// Tree style bits (commctrl values, not wrapped as a style type by
/// the windows crate): has-buttons, has-lines, lines-at-root,
/// showsel-always — the IDM category-tree look.
const TVS_HASBUTTONS: u32 = 0x0001;
const TVS_HASLINES: u32 = 0x0002;
const TVS_LINESATROOT: u32 = 0x0004;
const TVS_SHOWSELALWAYS: u32 = 0x0020;

pub struct TreeView {
    pub hwnd: HWND,
}

impl TreeView {
    pub fn create(parent: HWND, control_id: i32) -> Result<Self> {
        unsafe {
            let style = WS_CHILD
                | WS_VISIBLE
                | WINDOW_STYLE(TVS_HASBUTTONS | TVS_HASLINES | TVS_LINESATROOT | TVS_SHOWSELALWAYS);
            let hwnd = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                WC_TREEVIEWW,
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
            Ok(Self { hwnd })
        }
    }

    /// Insert a node carrying an application tag in its lParam
    /// (TVIF_PARAM) — the category tree's filter keys ride here, so
    /// TVN_SELCHANGEDW's NMTREEVIEWW.itemNew.lParam names the
    /// filter the node selects without a text lookup.
    pub fn insert_tagged(&self, parent: Option<HTREEITEM>, text: &str, tag: isize) -> HTREEITEM {
        let mut buf = wide(text);
        let item = TVITEMW {
            mask: TVIF_TEXT | TVIF_PARAM,
            pszText: windows::core::PWSTR(buf.as_mut_ptr()),
            lParam: LPARAM(tag),
            ..Default::default()
        };
        let ins = TVINSERTSTRUCTW {
            hParent: parent.unwrap_or(TVI_ROOT),
            hInsertAfter: TVI_LAST,
            Anonymous: TVINSERTSTRUCTW_0 { item },
        };
        unsafe {
            let r = send(self.hwnd, TVM_INSERTITEMW, WPARAM(0), LPARAM(&ins as *const TVINSERTSTRUCTW as isize));
            HTREEITEM(r)
        }
    }

    /// Remove every node (the tree is rebuilt when the category /
    /// queue signature changes).
    pub fn delete_all(&self) {
        unsafe {
            send(self.hwnd, TVM_DELETEITEM, WPARAM(0), LPARAM(TVI_ROOT.0));
        }
    }

    /// Expand a node so its children are visible.
    pub fn expand(&self, item: HTREEITEM) {
        unsafe {
            send(self.hwnd, TVM_EXPAND, WPARAM(TVE_EXPAND.0 as usize), LPARAM(item.0));
        }
    }
}
