//! `ReviewGutter` (WP1.2, ARCHITECTURE §4.3): line numbers aligned to the text baseline (so they
//! stay correct next to phantom deleted lines) plus green/red/amber hunk markers.
//!
//! GtkSourceView's stock line numbers centre each number in the whole line cell, which includes
//! the space a hunk reserves above its anchor line; this renderer draws each number at the top of
//! the line's text instead, and replaces the stock renderer. Markers, at the gutter's right edge:
//!
//! - a green bar next to the current lines of a hunk (amber when the user edited the hunk);
//! - a red bar next to drawn deleted lines, and a red wedge in each band of a hunk that deleted
//!   text (the only red mark in reading mode).

use super::TexView;
use super::review_overlay::Palette;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, graphene, gsk};
use sourceview5::prelude::*;
use sourceview5::subclass::prelude::*;
use std::cell::{Cell, RefCell};

/// Gutter position of the renderer (the stock line numbers' position).
const POSITION: i32 = -30;
const LEFT_PAD: i32 = 8;
/// Space between the numbers and the markers.
const GAP: i32 = 6;
const MARK_WIDTH: i32 = 3;
const RIGHT_PAD: i32 = 3;
const WEDGE_WIDTH: f32 = 6.0;
const WEDGE_HEIGHT: f32 = 8.0;

/// Colours fetched once per gutter frame.
#[derive(Clone, Copy, Debug)]
struct Colors {
    number: gdk::RGBA,
    current: gdk::RGBA,
    /// Review colours (`None` for a buffer that is not a GtkSourceView buffer).
    palette: Option<Palette>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ReviewGutter {
        /// Digits of the largest line number (the width depends on it).
        digits: Cell<u32>,
        /// Width of one digit of the gutter's font.
        digit_width: Cell<i32>,
        layout: RefCell<Option<pango::Layout>>,
        colors: Cell<Option<Colors>>,
        resize_queued: Cell<bool>,
        /// The last line drawn in this frame: (GTK line, `LineIndex` line).
        last_line: Cell<Option<(u32, u32)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ReviewGutter {
        const NAME: &'static str = "LemmaReviewGutter";
        type Type = super::ReviewGutter;
        type ParentType = sourceview5::GutterRenderer;
    }

    impl ObjectImpl for ReviewGutter {}

    impl WidgetImpl for ReviewGutter {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let width = self.natural_width();
                (width, width, -1, -1)
            } else {
                (0, 0, -1, -1)
            }
        }
    }

    impl GutterRendererImpl for ReviewGutter {
        fn begin(&self, lines: &sourceview5::GutterLines) {
            // More digits or another font: resize, but not while the gutter is being drawn.
            let digits = digits(lines.buffer().line_count());
            if (digits != self.digits.get() || self.measure_digit() != self.digit_width.get())
                && !self.resize_queued.replace(true)
            {
                let weak = self.obj().downgrade();
                glib::idle_add_local_once(move || {
                    if let Some(renderer) = weak.upgrade() {
                        renderer.imp().resize_queued.set(false);
                        renderer.queue_resize();
                    }
                });
            }
            self.colors.set(Some(self.colors(lines)));
            self.last_line.set(None);
        }

        fn query_data(&self, _lines: &sourceview5::GutterLines, _line: u32) {}

        fn snapshot_line(
            &self,
            snapshot: &gtk::Snapshot,
            lines: &sourceview5::GutterLines,
            line: u32,
        ) {
            self.draw_line(snapshot, lines, line);
        }
    }

    impl ReviewGutter {
        fn layout(&self) -> pango::Layout {
            self.layout
                .borrow_mut()
                .get_or_insert_with(|| self.obj().create_pango_layout(None))
                .clone()
        }

        fn measure_digit(&self) -> i32 {
            let layout = self.layout();
            layout.set_text("0");
            layout.pixel_size().0.max(1)
        }

        /// The width for the current line count and font; remembers what it measured.
        fn natural_width(&self) -> i32 {
            // The renderer has no view before it is inserted into a gutter.
            let view = self.obj().property::<Option<sourceview5::View>>("view");
            let lines = view.map_or(1, |v| v.buffer().line_count());
            self.digits.set(digits(lines));
            self.digit_width.set(self.measure_digit());
            let digits = self.digits.get().max(2) as i32;
            LEFT_PAD + digits * self.digit_width.get() + GAP + MARK_WIDTH + RIGHT_PAD
        }

        fn colors(&self, lines: &sourceview5::GutterLines) -> Colors {
            let view = lines.view();
            let buffer = lines.buffer().downcast::<sourceview5::Buffer>().ok();
            let scheme = buffer.as_ref().and_then(|b| b.style_scheme());
            let fg = |style: &str| {
                scheme
                    .as_ref()
                    .and_then(|s| s.style(style))
                    .filter(|s| s.is_foreground_set())
                    .and_then(|s| s.foreground())
                    .and_then(|c| gdk::RGBA::parse(c.as_str()).ok())
            };
            let color = self.obj().color();
            let number = fg("line-numbers")
                .unwrap_or_else(|| gdk::RGBA::new(color.red(), color.green(), color.blue(), 0.45));
            let current = fg("current-line-number").unwrap_or(color);
            let palette = buffer
                .as_ref()
                .map(|b| Palette::new(view.downcast_ref::<TexView>(), b));
            Colors {
                number,
                current,
                palette,
            }
        }

        /// The `LineIndex` line of GTK line `line` (which starts at `iter`). GTK also breaks lines
        /// at a lone `'\r'` and at U+2029, where `Document::byte_offset` has to measure the text
        /// (O(n)); lines are drawn top to bottom, so after the first line of a frame each one is
        /// found from the previous one by looking at the line break between them.
        fn document_line(
            &self,
            doc: &crate::document::Document,
            iter: &gtk::TextIter,
            line: u32,
        ) -> u32 {
            let doc_line = match self.last_line.get() {
                Some((gtk_line, doc_line)) if gtk_line == line => doc_line,
                Some((gtk_line, doc_line)) if gtk_line + 1 == line => {
                    let mut previous = *iter;
                    let newline = previous.backward_char() && previous.char() == '\n';
                    doc_line + u32::from(newline)
                }
                _ => doc.line_index().line_of(doc.byte_offset(iter)),
            };
            self.last_line.set(Some((line, doc_line)));
            doc_line
        }

        fn draw_line(&self, snapshot: &gtk::Snapshot, lines: &sourceview5::GutterLines, line: u32) {
            let view = lines.view();
            let Some(colors) = self.colors.get() else {
                return;
            };
            let iter = lines.iter_at_line(line);
            let (cell_y, cell_h) =
                lines.line_yrange(line, sourceview5::GutterRendererAlignmentMode::Cell);
            let (line_y, _) = view.line_yrange(&iter);
            // Buffer y → renderer y.
            let shift = cell_y - line_y;
            let text_top = view.iter_location(&iter).y() + shift;
            let width = self.obj().width();
            let mark_x = (width - RIGHT_PAD - MARK_WIDTH) as f32;

            let layout = self.layout();
            layout.set_text(&(line + 1).to_string());
            let number_w = layout.pixel_size().0;
            let color = if lines.is_cursor(line) {
                colors.current
            } else {
                colors.number
            };
            snapshot.save();
            snapshot.translate(&graphene::Point::new(
                (width - RIGHT_PAD - MARK_WIDTH - GAP - number_w) as f32,
                text_top as f32,
            ));
            snapshot.append_layout(&layout, &color);
            snapshot.restore();

            let Some(tex_view) = view.downcast_ref::<TexView>() else {
                return;
            };
            let (Some(review), Some(doc), Some(palette)) = (
                tex_view.review_state(),
                tex_view.document(),
                colors.palette.as_ref(),
            ) else {
                return;
            };
            if review.is_empty() {
                return;
            }
            let doc_line = self.document_line(&doc, &iter, line);
            let marks = review.gutter_marks(&iter, doc_line);
            if marks.added {
                let bottom = cell_y + cell_h - marks.below_total;
                let color = if marks.touched {
                    palette.touched
                } else {
                    palette.added_mark
                };
                snapshot.append_color(
                    &color,
                    &graphene::Rect::new(
                        mark_x,
                        text_top as f32,
                        MARK_WIDTH as f32,
                        (bottom - text_top) as f32,
                    ),
                );
            }
            for block in &marks.blocks {
                let top = if block.below {
                    cell_y + cell_h - marks.below_total + block.offset
                } else {
                    cell_y + block.offset
                };
                let band = graphene::Rect::new(0.0, top as f32, width as f32, block.header as f32);
                snapshot.append_color(&palette.band, &band);
                if block.deleted > 0 {
                    let area = graphene::Rect::new(
                        0.0,
                        (top + block.header) as f32,
                        width as f32,
                        block.deleted as f32,
                    );
                    snapshot.append_color(&palette.deleted_bg, &area);
                    let color = if block.touched {
                        palette.touched
                    } else {
                        palette.deleted_mark
                    };
                    snapshot.append_color(
                        &color,
                        &graphene::Rect::new(
                            mark_x,
                            (top + block.header) as f32,
                            MARK_WIDTH as f32,
                            block.deleted as f32,
                        ),
                    );
                }
                if block.has_old {
                    let color = if block.touched {
                        palette.touched
                    } else {
                        palette.deleted_mark
                    };
                    let cy = top as f32 + block.header as f32 / 2.0;
                    let right = (width - RIGHT_PAD) as f32;
                    let builder = gsk::PathBuilder::new();
                    builder.move_to(right - WEDGE_WIDTH, cy - WEDGE_HEIGHT / 2.0);
                    builder.line_to(right, cy);
                    builder.line_to(right - WEDGE_WIDTH, cy + WEDGE_HEIGHT / 2.0);
                    builder.close();
                    snapshot.append_fill(&builder.to_path(), gsk::FillRule::Winding, &color);
                }
            }
        }
    }

    /// Decimal digits of `n` (at least 1).
    pub fn digits(n: i32) -> u32 {
        n.max(1).unsigned_abs().ilog10() + 1
    }
}

glib::wrapper! {
    /// Line numbers aligned to the text plus review markers.
    pub struct ReviewGutter(ObjectSubclass<imp::ReviewGutter>)
        @extends sourceview5::GutterRenderer, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ReviewGutter {
    fn default() -> Self {
        glib::Object::new()
    }
}

/// Installs the review gutter on a newly created view and turns the stock line numbers off.
pub fn attach(view: &TexView) {
    view.set_show_line_numbers(false);
    let renderer = ReviewGutter::default();
    sourceview5::prelude::ViewExt::gutter(view, gtk::TextWindowType::Left)
        .insert(&renderer, POSITION);
}

#[cfg(test)]
mod tests {
    use super::imp::digits;

    #[test]
    fn digit_counts() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(60_003), 5);
    }
}
