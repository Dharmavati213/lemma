//! `PagePicture`: one PDF page (or a region of it) drawn from a texture, sized by the page's
//! aspect ratio (height for width). The thumbnails of the PDF strip and the link previews use it.
//!
//! Until a texture is set the page is drawn white, so a list of thumbnails has its final layout
//! at once. In dark mode the same colour matrix as the PDF view is applied (no re-render).
//! CSS node: `pagepicture` (the strip and the link popover style it).

use super::geometry;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// A region of a page in bp, top-left origin: `(x, y, width, height)`.
pub type Region = (f64, f64, f64, f64);

type ResizedFn = dyn Fn(&PagePicture);

mod imp {
    use super::*;

    pub struct PagePicture {
        /// The page size in bp.
        pub(super) page_size: Cell<(f64, f64)>,
        /// The region shown (the whole page when `None`).
        pub(super) region: Cell<Option<Region>>,
        pub(super) texture: RefCell<Option<gdk::Texture>>,
        pub(super) dark: Cell<bool>,
        /// A fixed width in logical pixels (link previews); `None` fills the width given.
        pub(super) fixed_width: Cell<Option<i32>>,
        pub(super) resized: RefCell<Option<Rc<ResizedFn>>>,
        pub(super) allocated_width: Cell<i32>,
    }

    impl Default for PagePicture {
        fn default() -> Self {
            Self {
                page_size: Cell::new((612.0, 792.0)),
                region: Cell::new(None),
                texture: RefCell::new(None),
                dark: Cell::new(false),
                fixed_width: Cell::new(None),
                resized: RefCell::new(None),
                allocated_width: Cell::new(0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PagePicture {
        const NAME: &'static str = "LemmaPagePicture";
        type Type = super::PagePicture;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("pagepicture");
            klass.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    impl ObjectImpl for PagePicture {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_overflow(gtk::Overflow::Hidden);
        }
    }

    impl WidgetImpl for PagePicture {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let aspect = self.aspect();
            match orientation {
                gtk::Orientation::Horizontal => match self.fixed_width.get() {
                    Some(w) => (w, w, -1, -1),
                    None => {
                        let natural = if for_size > 0 {
                            (f64::from(for_size) / aspect).round() as i32
                        } else {
                            120
                        };
                        (16, natural.max(16), -1, -1)
                    }
                },
                _ => {
                    let width = if for_size > 0 {
                        for_size
                    } else {
                        self.fixed_width.get().unwrap_or(120)
                    };
                    let h = (f64::from(width) * aspect).round().max(1.0) as i32;
                    (h, h, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            if self.allocated_width.replace(width) != width {
                let resized = self.resized.borrow().clone();
                if let Some(resized) = resized {
                    resized(&self.obj());
                }
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (w, h) = (f64::from(obj.width()), f64::from(obj.height()));
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let bounds = graphene::Rect::new(0.0, 0.0, w as f32, h as f32);
            let dark = self.dark.get();
            if dark {
                let (matrix, offset) = geometry::dark_color_transform();
                snapshot.push_color_matrix(
                    &graphene::Matrix::from_float(matrix),
                    &graphene::Vec4::from_float(offset),
                );
            }
            snapshot.append_color(&gdk::RGBA::WHITE, &bounds);
            if let Some(texture) = self.texture.borrow().as_ref() {
                let (pw, ph) = self.page_size.get();
                let (rx, ry, rw, _) = self.region.get().unwrap_or((0.0, 0.0, pw, ph));
                if rw > 0.0 && pw > 0.0 {
                    let s = w / rw;
                    let target = graphene::Rect::new(
                        (-rx * s) as f32,
                        (-ry * s) as f32,
                        (pw * s) as f32,
                        (ph * s) as f32,
                    );
                    let scale = obj
                        .native()
                        .and_then(|n| n.surface())
                        .map_or(1.0, |s| s.scale());
                    let shrink = f64::from(texture.width()) > pw * s * scale * 1.25;
                    let filter = if shrink {
                        gsk::ScalingFilter::Trilinear
                    } else {
                        gsk::ScalingFilter::Linear
                    };
                    snapshot.push_clip(&bounds);
                    snapshot.append_scaled_texture(texture, filter, &target);
                    snapshot.pop();
                }
            }
            if dark {
                snapshot.pop();
            }
        }
    }

    impl PagePicture {
        /// Height over width of what is shown.
        fn aspect(&self) -> f64 {
            let (pw, ph) = self.page_size.get();
            let (_, _, w, h) = self.region.get().unwrap_or((0.0, 0.0, pw, ph));
            if w > 0.0 && h > 0.0 { h / w } else { 1.294 }
        }
    }
}

glib::wrapper! {
    /// A page (or a region of it) drawn from a texture (see the module documentation).
    pub struct PagePicture(ObjectSubclass<imp::PagePicture>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for PagePicture {
    fn default() -> Self {
        Self::new()
    }
}

impl PagePicture {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Sets the size of the page in bp (decides the aspect ratio).
    pub fn set_page_size(&self, size: (f64, f64)) {
        let imp = self.imp();
        if imp.page_size.replace(size) != size {
            self.queue_resize();
        }
    }

    pub fn page_size(&self) -> (f64, f64) {
        self.imp().page_size.get()
    }

    /// Shows only `region` of the page (bp), or the whole page.
    pub fn set_region(&self, region: Option<Region>) {
        if self.imp().region.replace(region) != region {
            self.queue_resize();
        }
    }

    /// The texture of the whole page (any resolution), or `None` for a white page.
    pub fn set_texture(&self, texture: Option<&gdk::Texture>) {
        self.imp().texture.replace(texture.cloned());
        self.queue_draw();
    }

    pub fn texture(&self) -> Option<gdk::Texture> {
        self.imp().texture.borrow().clone()
    }

    /// Draws the page with the PDF view's dark-mode colour matrix.
    pub fn set_dark(&self, dark: bool) {
        if self.imp().dark.replace(dark) != dark {
            self.queue_draw();
        }
    }

    pub fn is_dark(&self) -> bool {
        self.imp().dark.get()
    }

    /// A fixed width in logical pixels, or `None` to fill the width the parent gives.
    pub fn set_fixed_width(&self, width: Option<i32>) {
        if self.imp().fixed_width.replace(width) != width {
            self.queue_resize();
        }
    }

    /// Calls `f` whenever the allocated width changes (to render at the new width).
    pub fn connect_resized(&self, f: impl Fn(&PagePicture) + 'static) {
        self.imp().resized.replace(Some(Rc::new(f)));
    }
}
