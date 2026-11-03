//! Bundled icon set (item 17): NDM's own stroke glyphs — one ink
//! (#3d4f63), one accent (#2f7fd0), a 24-unit grid, round caps —
//! embedded as SVG and rasterised through gdk-pixbuf at the
//! widget's scale factor, so they stay crisp on HiDPI. Pixbufs are
//! parsed once per (name, pixel size) and cached; the list's
//! per-tick row fill only clones the cached handle.

use std::cell::RefCell;
use std::collections::HashMap;

use gtk::gdk_pixbuf::Pixbuf;
use gtk::gio;
use gtk::glib::Bytes;
use gtk::prelude::*;

fn bytes(name: &str) -> Option<&'static [u8]> {
    Some(match name {
        "add" => include_bytes!("../assets/icons/add.svg"),
        "resume" => include_bytes!("../assets/icons/resume.svg"),
        "stop" => include_bytes!("../assets/icons/stop.svg"),
        "stop_all" => include_bytes!("../assets/icons/stop_all.svg"),
        "delete" => include_bytes!("../assets/icons/delete.svg"),
        "delete_completed" => include_bytes!("../assets/icons/delete_completed.svg"),
        "options" => include_bytes!("../assets/icons/options.svg"),
        "scheduler" => include_bytes!("../assets/icons/scheduler.svg"),
        "queue_start" => include_bytes!("../assets/icons/queue_start.svg"),
        "queue_stop" => include_bytes!("../assets/icons/queue_stop.svg"),
        "grabber" => include_bytes!("../assets/icons/grabber.svg"),
        "logo" => include_bytes!("../assets/icons/logo.svg"),
        "folder" => include_bytes!("../assets/icons/folder.svg"),
        "file_video" => include_bytes!("../assets/icons/file_video.svg"),
        "file_audio" => include_bytes!("../assets/icons/file_audio.svg"),
        "file_image" => include_bytes!("../assets/icons/file_image.svg"),
        "file_archive" => include_bytes!("../assets/icons/file_archive.svg"),
        "file_program" => include_bytes!("../assets/icons/file_program.svg"),
        "file_document" => include_bytes!("../assets/icons/file_document.svg"),
        _ => include_bytes!("../assets/icons/file_generic.svg"),
    })
}

thread_local! {
    static CACHE: RefCell<HashMap<(String, i32), Option<Pixbuf>>> =
        RefCell::new(HashMap::new());
}

/// The glyph at `logical` CSS pixels for a widget with the given
/// scale factor. None only when the SVG loader is missing (never
/// on a stock Ubuntu/Zorin install) — callers then go icon-less
/// rather than crash.
pub fn pixbuf(name: &str, logical: i32, scale: i32) -> Option<Pixbuf> {
    let px = logical * scale.max(1);
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        cache
            .entry((name.to_string(), px))
            .or_insert_with(|| {
                let data = bytes(name)?;
                let stream = gio::MemoryInputStream::from_bytes(&Bytes::from_static(data));
                Pixbuf::from_stream_at_scale(
                    &stream,
                    px,
                    px,
                    true,
                    None::<&gio::Cancellable>,
                )
                .ok()
            })
            .clone()
    })
}

pub fn image(name: &str, logical: i32, scale: i32) -> Option<gtk::Image> {
    pixbuf(name, logical, scale).map(|p| gtk::Image::from_pixbuf(Some(&p)))
}

/// Main-list row glyph for an entry: its category first (a video
/// job still shows the film glyph while its file name is only a
/// page title), the file extension as the tie-breaker.
pub fn row_icon_name(category: &str, name: &str) -> &'static str {
    match category {
        "Video" => return "file_video",
        "Music" => return "file_audio",
        "Pictures" | "Images" => return "file_image",
        "Compressed" => return "file_archive",
        "Programs" => return "file_program",
        "Documents" => return "file_document",
        _ => {}
    }
    let ext = name
        .rsplit('.')
        .next()
        .filter(|e| e.len() <= 5 && !e.contains('/'))
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "ts" => "file_video",
        "mp3" | "aac" | "flac" | "wav" | "ogg" | "m4a" | "opus" => "file_audio",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "avif" | "ico" => {
            "file_image"
        }
        "zip" | "rar" | "7z" | "tar" | "gz" | "xz" | "bz2" | "iso" | "img" | "dmg" => {
            "file_archive"
        }
        "exe" | "msi" | "deb" | "rpm" | "appimage" | "bin" | "run" | "apk" => {
            "file_program"
        }
        "pdf" | "doc" | "docx" | "txt" | "md" | "odt" | "rtf" | "xls" | "xlsx" | "ppt"
        | "pptx" => "file_document",
        _ => "file_generic",
    }
}
