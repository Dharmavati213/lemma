//! `TexView`: the `sourceview5::View` subclass every editor tab uses.
//!
//! It paints the agent-review overlay above the text (`snapshot_layer(AboveText)` →
//! [`crate::editor::review_overlay::paint`]) and knows its [`Document`], so the review code can
//! get from a view to its hunks.

use crate::document::{DocRef, Document};
use crate::editor::keys::KeyLayer;
use crate::editor::review_overlay::ViewReview;
use gtk::glib;
use gtk::subclass::prelude::*;
use lemma_config::EditorCfg;
use sourceview5::prelude::*;
use sourceview5::subclass::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TexView {
        pub document: RefCell<Weak<Document>>,
        /// Review rendering state (WP1.2), set while the view's editor is attached.
        pub review: RefCell<Option<Rc<ViewReview>>>,
        /// The key layer (`editor::keys`), set by `Editor::new`.
        pub keys: RefCell<Option<Rc<KeyLayer>>>,
        /// Lemma's popovers on the view ([`super::TexView::adopt_popover`]).
        pub popovers: RefCell<Vec<glib::WeakRef<gtk::Popover>>>,
        /// [`super::TexView::follow_adjustment`] did not bring the view back to its adjustment:
        /// it does not try again.
        pub offset_stuck: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TexView {
        const NAME: &'static str = "LemmaTexView";
        type Type = super::TexView;
        type ParentType = sourceview5::View;
    }

    impl ObjectImpl for TexView {
        fn dispose(&self) {
            // GtkTextView's own dispose removes only the children it added itself: on any other
            // it warns "not a child" in an endless loop. Popovers still on the view (the hover's
            // lives as long as the view) go first.
            let view = self.obj();
            for popover in self.popovers.take() {
                if let Some(popover) = popover.upgrade()
                    && popover.parent().as_ref() == Some(view.upcast_ref::<gtk::Widget>())
                {
                    popover.unparent();
                }
            }
        }
    }
    impl WidgetImpl for TexView {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.obj().follow_adjustment();
        }
    }

    impl TextViewImpl for TexView {
        fn snapshot_layer(&self, layer: gtk::TextViewLayer, snapshot: gtk::Snapshot) {
            self.parent_snapshot_layer(layer, snapshot.clone());
            if layer == gtk::TextViewLayer::AboveText {
                crate::editor::review_overlay::paint(&self.obj(), &snapshot);
            }
        }
    }

    impl ViewImpl for TexView {}
}

glib::wrapper! {
    /// The LaTeX editor view.
    pub struct TexView(ObjectSubclass<imp::TexView>)
        @extends sourceview5::View, gtk::TextView, gtk::Widget,
        @implements gtk::Accessible, gtk::AccessibleText, gtk::Buildable, gtk::ConstraintTarget,
                    gtk::Scrollable;
}

impl TexView {
    /// A view of `doc`'s buffer, configured from `cfg`. `None` when `doc` is closed.
    pub fn new(doc: &DocRef, cfg: &EditorCfg) -> Option<Self> {
        let buffer = doc.buffer()?;
        let view: Self = glib::Object::builder().property("buffer", &buffer).build();
        view.imp().document.replace(Rc::downgrade(doc));
        view.configure(cfg);
        Some(view)
    }

    /// The document shown, while it exists.
    pub fn document(&self) -> Option<DocRef> {
        self.imp().document.borrow().upgrade()
    }

    /// The review rendering state (tags, phantom lines, action bars) of this view, while the
    /// review controller has it attached.
    pub fn review_state(&self) -> Option<Rc<ViewReview>> {
        self.imp().review.borrow().clone()
    }

    /// Sets (on attach) or clears (on detach) the review rendering state.
    pub fn set_review_state(&self, state: Option<Rc<ViewReview>>) {
        self.imp().review.replace(state);
    }

    /// The key layer (`editor::keys`), once `Editor::new` installed it.
    pub fn key_layer(&self) -> Option<Rc<KeyLayer>> {
        self.imp().keys.borrow().clone()
    }

    /// Sets or clears the key layer (`editor::keys::install`).
    pub fn set_key_layer(&self, layer: Option<Rc<KeyLayer>>) {
        // Dropped outside the borrow: dropping a layer removes its controller.
        let old = self.imp().keys.replace(layer);
        drop(old);
    }

    /// Parents `popover` to the view (`set_parent`), and unparents it when the view goes if it is
    /// still there then. Lemma's popovers on the view all come through here.
    pub fn adopt_popover(&self, popover: &gtk::Popover) {
        popover.set_parent(self);
        let mut popovers = self.imp().popovers.borrow_mut();
        let view = self.upcast_ref::<gtk::Widget>();
        popovers.retain(|p| {
            p.upgrade()
                .is_some_and(|p| p.parent().as_ref() == Some(view))
        });
        popovers.push(popover.downgrade());
    }

    /// Puts the view back where its scroll adjustment says, when GTK moved the view alone.
    ///
    /// GtkTextView keeps the first paragraph on screen in its place while the lines above it
    /// get their heights. A view scrolled before it had a size (a file opened at a line) can
    /// take the last line of a text not laid out yet for that paragraph; once the lines are
    /// measured it moves its offset down by the whole text and asks the adjustment to follow.
    /// When the text fits the view, the adjustment has no range and stays at 0: the view shows
    /// the empty space past the end, and no scroll brings the text back, since none changes
    /// the adjustment's value (GTK 4.24; seen where the view gets its size before the lines
    /// are measured: as a Windows program under Wine). Sending the adjustment's value again
    /// makes the view take it. Called after each allocation: the text view asks for one when
    /// it moves its offset.
    fn follow_adjustment(&self) {
        let imp = self.imp();
        let Some(adjustment) = self.vadjustment() else {
            return;
        };
        // Only a view that cannot scroll is stuck: with a range, the adjustment follows the
        // view, or the view the adjustment, within the same allocation.
        if adjustment.upper() - adjustment.page_size() >= 1.0 {
            return;
        }
        // The view's offset is its adjustment's value less the top margin, in whole pixels.
        let apart = |view: &Self| {
            let shown = f64::from(view.visible_rect().y() + view.top_margin());
            (shown - adjustment.value()).abs() >= 1.0
        };
        if imp.offset_stuck.get() || !apart(self) {
            return;
        }
        adjustment.emit_by_name::<()>("value-changed", &[]);
        if apart(self) {
            imp.offset_stuck.set(true);
            tracing::warn!("the editor's view does not follow its scroll adjustment");
        }
    }

    /// Applies the editor settings: monospace font (CSS class `lemma-editor`, see
    /// [`crate::editor::install_css`]), line numbers, current-line highlight, auto-indent,
    /// smart Home/End, two-space indentation with spaces, and soft wrap per `cfg.wrap`.
    pub fn configure(&self, cfg: &EditorCfg) {
        self.add_css_class("lemma-editor");
        self.set_monospace(true);
        self.set_show_line_numbers(true);
        self.set_highlight_current_line(true);
        self.set_auto_indent(true);
        self.set_indent_on_tab(true);
        self.set_tab_width(2);
        self.set_indent_width(2);
        self.set_insert_spaces_instead_of_tabs(true);
        self.set_smart_backspace(true);
        self.set_smart_home_end(sourceview5::SmartHomeEndType::Before);
        self.set_wrap_mode(if cfg.wrap {
            gtk::WrapMode::WordChar
        } else {
            gtk::WrapMode::None
        });
        self.set_left_margin(10);
        self.set_right_margin(16);
        self.set_top_margin(8);
        self.set_bottom_margin(160);
    }
}
