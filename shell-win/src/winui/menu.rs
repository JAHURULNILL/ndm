//! Menu builder from a table. Owner-draw ready: an item flagged
//! `owner_draw` is appended with MF_OWNERDRAW instead of MF_STRING,
//! so the parent can answer WM_MEASUREITEM / WM_DRAWITEM for it
//! (the Phase-2 premium finish hooks) without changing the table.

use super::{pcw, wide};
use windows::core::{PCWSTR, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreatePopupMenu, DrawMenuBar, SetMenu, HMENU, MF_OWNERDRAW, MF_POPUP,
    MF_SEPARATOR, MF_STRING,
};

/// One row of a menu table.
#[derive(Clone, Debug)]
pub enum MenuItem {
    Command { id: i32, label: &'static str, owner_draw: bool },
    Separator,
    Submenu { label: &'static str, items: Vec<MenuItem> },
}

impl MenuItem {
    pub fn cmd(id: i32, label: &'static str) -> Self {
        MenuItem::Command { id, label, owner_draw: false }
    }
    pub fn submenu(label: &'static str, items: Vec<MenuItem>) -> Self {
        MenuItem::Submenu { label, items }
    }
}

/// Build an HMENU (popup or bar, per `popup`) from a table.
pub fn build_menu(items: &[MenuItem], popup: bool) -> Result<HMENU> {
    unsafe {
        let menu = if popup { CreatePopupMenu()? } else { CreateMenu()? };
        fill(menu, items)?;
        Ok(menu)
    }
}

unsafe fn fill(menu: HMENU, items: &[MenuItem]) -> Result<()> {
    for item in items {
        match item {
            MenuItem::Separator => {
                AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())?;
            }
            MenuItem::Command { id, label, owner_draw } => {
                let buf = wide(label);
                let flags = if *owner_draw { MF_OWNERDRAW } else { MF_STRING };
                // For MF_OWNERDRAW the "string" pointer is app data;
                // pass the label pointer cast through the string slot
                // (classic Win32 usage) — the draw handler reads it.
                AppendMenuW(menu, flags, *id as usize, pcw(&buf))?;
            }
            MenuItem::Submenu { label, items } => {
                let sub = build_menu(items, true)?;
                let buf = wide(label);
                AppendMenuW(menu, MF_POPUP, sub.0 as usize, pcw(&buf))?;
            }
        }
    }
    Ok(())
}

/// Build a menu bar from a table and attach it to a window.
pub fn set_menu_bar(hwnd: HWND, items: &[MenuItem]) -> Result<HMENU> {
    unsafe {
        let menu = build_menu(items, false)?;
        SetMenu(hwnd, Some(menu))?;
        let _ = DrawMenuBar(hwnd);
        Ok(menu)
    }
}

// Re-export so callers building popup (context) menus share one path.
pub fn build_popup(items: &[MenuItem]) -> Result<HMENU> {
    build_menu(items, true)
}
