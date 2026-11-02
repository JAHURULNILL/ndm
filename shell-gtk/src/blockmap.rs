//! Block-map cell renderer — the Phase-5 renderer seam.
//!
//! NATIVE-PLAN.md §3 assigns the download list's block map to a
//! custom GtkCellRenderer, exactly parallel to the Windows
//! shell's NM_CUSTOMDRAW segment strip: the model's `blocks`
//! column carries the payload's 128-char block map and the
//! `fraction` column the overall done/total ratio.
//!
//! This scaffold paints a faithful placeholder of that strip:
//! one segment per block-map char (sealed = green, active =
//! blue, pending = pale), falling back to a plain fraction bar
//! when a row has no block map yet (video jobs, restored
//! entries). Phase 5 replaces the paint body with the real
//! per-connection strip fed by `conns_state` / `conns_partial` /
//! `conns_block` (spec G1) — the property surface below is the
//! seam it plugs into, and does not change.

use gtk::glib;
use gtk::subclass::prelude::*;
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;
    use glib::ToValue;

    #[derive(Default)]
    pub struct BlockMapRenderer {
        /// The row's 128-char block map (model column `blocks`).
        pub blocks: RefCell<String>,
        /// Overall progress 0.0..=1.0 (model column `fraction`).
        pub fraction: Cell<f64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BlockMapRenderer {
        const NAME: &'static str = "NdmBlockMapRenderer";
        type Type = super::BlockMapRenderer;
        type ParentType = gtk::CellRenderer;
    }

    impl ObjectImpl for BlockMapRenderer {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: std::sync::OnceLock<Vec<glib::ParamSpec>> =
                std::sync::OnceLock::new();
            PROPERTIES.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("blocks").build(),
                    glib::ParamSpecDouble::builder("fraction")
                        .minimum(0.0)
                        .maximum(1.0)
                        .default_value(0.0)
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "blocks" => {
                    let s: String = value.get().unwrap_or_default();
                    *self.blocks.borrow_mut() = s;
                }
                "fraction" => {
                    let f: f64 = value.get().unwrap_or_default();
                    self.fraction.set(f.clamp(0.0, 1.0));
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "blocks" => self.blocks.borrow().to_value(),
                "fraction" => self.fraction.get().to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl CellRendererImpl for BlockMapRenderer {
        fn preferred_width<P: gtk::prelude::IsA<gtk::Widget>>(
            &self,
            _widget: &P,
        ) -> (i32, i32) {
            // The strip wants room for 128 segments but must not
            // starve the name column on a minimum-size window.
            (64, 160)
        }

        fn preferred_height<P: gtk::prelude::IsA<gtk::Widget>>(
            &self,
            _widget: &P,
        ) -> (i32, i32) {
            (16, 16)
        }

        fn render<P: gtk::prelude::IsA<gtk::Widget>>(
            &self,
            cr: &gtk::cairo::Context,
            _widget: &P,
            _background_area: &gtk::gdk::Rectangle,
            cell_area: &gtk::gdk::Rectangle,
            _flags: gtk::CellRendererState,
        ) {
            // PHASE-5 RENDERER SEAM: placeholder paint — segment
            // colours mirror the web UI's canvas strip (sealed
            // green / active blue / pending pale) so the shape is
            // already the real one; only the data source grows
            // (per-connection G1 fields) in Phase 5.
            let x = cell_area.x() as f64 + 1.0;
            let y = cell_area.y() as f64 + 2.0;
            let w = (cell_area.width() as f64 - 2.0).max(0.0);
            let h = (cell_area.height() as f64 - 4.0).max(0.0);
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let blocks = self.blocks.borrow();
            if !blocks.is_empty() {
                let n = blocks.chars().count() as f64;
                let seg_w = w / n;
                for (i, ch) in blocks.chars().enumerate() {
                    let (r, g, b) = match ch {
                        // sealed / downloaded segment
                        '#' | '1' => (0.18, 0.62, 0.31),
                        // in-flight segment
                        '+' | '=' | '2' => (0.14, 0.34, 0.65),
                        // not started yet
                        _ => (0.85, 0.89, 0.93),
                    };
                    cr.set_source_rgb(r, g, b);
                    let sx = x + seg_w * i as f64;
                    // 0.5px gap keeps segments legible at 128-wide
                    cr.rectangle(sx, y, (seg_w - 0.5).max(0.5), h);
                    let _ = cr.fill();
                }
            } else {
                // No block map (video / restored row): plain
                // fraction bar in the same strip shape.
                cr.set_source_rgb(0.85, 0.89, 0.93);
                cr.rectangle(x, y, w, h);
                let _ = cr.fill();
                let fw = w * self.fraction.get();
                if fw > 0.0 {
                    cr.set_source_rgb(0.18, 0.62, 0.31);
                    cr.rectangle(x, y, fw, h);
                    let _ = cr.fill();
                }
            }
            // Strip outline, matching the list's gridline tone.
            cr.set_source_rgb(0.62, 0.68, 0.75);
            cr.set_line_width(0.5);
            cr.rectangle(x, y, w, h);
            let _ = cr.stroke();
        }
    }
}

glib::wrapper! {
    /// Custom cell renderer painting a download's block map.
    /// See the module docs for the Phase-5 seam.
    pub struct BlockMapRenderer(ObjectSubclass<imp::BlockMapRenderer>)
        @extends gtk::CellRenderer;
}

impl BlockMapRenderer {
    pub fn new() -> Self {
        glib::Object::new()
    }
}

impl Default for BlockMapRenderer {
    fn default() -> Self {
        Self::new()
    }
}
