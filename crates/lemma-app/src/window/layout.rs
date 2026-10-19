//! Widget construction for the main window. No behaviour lives here: `window.rs` connects the
//! actions and signals.

use super::{
    AgentPane, Header, PAGE_CHANGES, PAGE_OUTLINE, PAGE_PROBLEMS, PAGE_SEARCH, PdfSidebar, Sidebar,
    StatusBar,
};
use adw::prelude::*;
use std::cell::Cell;

const DEFAULT_WIDTH: i32 = 1360;
const DEFAULT_HEIGHT: i32 = 860;
const MIN_WIDTH: i32 = 640;
const MIN_HEIGHT: i32 = 400;
/// Share of the editor/PDF split that the editor gets at start-up.
const EDITOR_SHARE: f64 = 0.55;
/// Below this window width (in sp) both sidebars become overlays and the left one starts hidden.
const NARROW_WIDTH_SP: f64 = 1100.0;

/// Every widget of a new window.
pub(super) struct Layout {
    pub window: adw::ApplicationWindow,
    pub toasts: adw::ToastOverlay,
    pub banner: adw::Banner,
    pub header: Header,
    pub sidebar: Sidebar,
    pub agent: AgentPane,
    pub paned: gtk::Paned,
    pub pdf_pane: adw::Bin,
    pub pdf_sidebar: PdfSidebar,
    pub tab_view: adw::TabView,
    pub tab_bar: adw::TabBar,
    /// "empty" (a status page) or "tabs" (tab bar and tab view).
    pub editor_stack: gtk::Stack,
    pub status: StatusBar,
}

/// Builds the window's widget tree (see the diagram in `window.rs`).
pub(super) fn build(app: &adw::Application) -> Layout {
    let header = build_header();
    let sidebar = build_sidebar();
    let status = build_status_bar();
    let pdf_sidebar = build_pdf_sidebar();
    let banner = adw::Banner::builder().revealed(false).build();

    let tab_view = adw::TabView::builder().vexpand(true).build();
    let tab_bar = adw::TabBar::builder()
        .view(&tab_view)
        .autohide(false)
        .build();
    let tabs = gtk::Box::new(gtk::Orientation::Vertical, 0);
    tabs.append(&tab_bar);
    tabs.append(&tab_view);
    let open_button = gtk::Button::builder()
        .label("_Open…")
        .use_underline(true)
        .action_name("app.open")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    let empty = adw::StatusPage::builder()
        .icon_name("text-editor-symbolic")
        .title("No Document Open")
        .description("Open a LaTeX document with Ctrl+O")
        .child(&open_button)
        .build();
    let editor_stack = gtk::Stack::builder()
        .hexpand(true)
        .width_request(320)
        .build();
    editor_stack.add_named(&empty, Some("empty"));
    editor_stack.add_named(&tabs, Some("tabs"));

    let pdf_pane = adw::Bin::builder()
        .child(&placeholder(
            "view-paged-symbolic",
            "No PDF Yet",
            "Build with F5 to see the preview here.",
        ))
        .width_request(260)
        .build();
    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&editor_stack)
        .end_child(&pdf_pane)
        .resize_start_child(true)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .build();
    set_initial_split(&paned);

    let agent_slot = adw::Bin::builder()
        .child(&placeholder(
            "chat-message-new-symbolic",
            "Agent",
            "The agent panel arrives in a later milestone.",
        ))
        .build();
    let agent_split = adw::OverlaySplitView::builder()
        .sidebar(&agent_slot)
        .content(&paned)
        .sidebar_position(gtk::PackType::End)
        .show_sidebar(false)
        .min_sidebar_width(320.0)
        .max_sidebar_width(480.0)
        .sidebar_width_fraction(0.3)
        .build();
    sidebar.split.set_content(Some(&agent_split));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header.bar);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&sidebar.split));
    toolbar.add_bottom_bar(&status.bar);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));

    let (width, height) = initial_size();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Lemma")
        .default_width(width)
        .default_height(height)
        .width_request(MIN_WIDTH)
        .height_request(MIN_HEIGHT)
        .content(&toasts)
        .build();
    let agent = AgentPane {
        split: agent_split,
        slot: agent_slot,
        controller: std::cell::OnceCell::new(),
    };
    add_narrow_breakpoint(&window, &sidebar, &agent);
    Layout {
        window,
        toasts,
        banner,
        header,
        sidebar,
        agent,
        paned,
        pdf_pane,
        pdf_sidebar,
        tab_view,
        tab_bar,
        editor_stack,
        status,
    }
}

fn build_header() -> Header {
    let bar = adw::HeaderBar::new();
    let sidebar_button = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Sidebar (F9)")
        .action_name("win.toggle-sidebar")
        .build();
    let open_button = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text("Open (Ctrl+O)")
        .action_name("app.open")
        .build();
    let save_button = gtk::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Save (Ctrl+S)")
        .action_name("win.save")
        .build();
    bar.pack_start(&sidebar_button);
    bar.pack_start(&open_button);
    bar.pack_start(&save_button);

    let title = adw::WindowTitle::new("Lemma", "");
    bar.set_title_widget(Some(&title));

    let menu = gio::Menu::new();
    let files = gio::Menu::new();
    files.append(Some("Save _All"), Some("win.save-all"));
    files.append(Some("_Close Tab"), Some("win.close-tab"));
    menu.append_section(None, &files);
    let go = gio::Menu::new();
    go.append(Some("Command _Palette…"), Some("win.command-palette"));
    go.append(Some("_Go to File…"), Some("win.quick-open"));
    go.append(Some("_Find in Project…"), Some("win.project-search"));
    menu.append_section(None, &go);
    let view = gio::Menu::new();
    view.append(Some("_Vim Mode"), Some("win.toggle-vim"));
    view.append(Some("_Sidebar"), Some("win.toggle-sidebar"));
    view.append(Some("_PDF Preview"), Some("win.toggle-pdf"));
    view.append(Some("PDF in Separate _Window"), Some("win.pdf-detach"));
    view.append(Some("_Agent Panel"), Some("win.toggle-agent"));
    menu.append_section(None, &view);
    let app = gio::Menu::new();
    app.append(Some("_Preferences"), Some("win.preferences"));
    app.append(Some("_Keyboard Shortcuts"), Some("win.shortcuts"));
    app.append(Some("_Quit"), Some("app.quit"));
    menu.append_section(None, &app);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .menu_model(&menu)
        .build();
    let agent_button = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-right-symbolic")
        .tooltip_text("Agent Panel (F10)")
        .action_name("win.toggle-agent")
        .build();
    let pdf_button = gtk::ToggleButton::builder()
        .icon_name("view-dual-symbolic")
        .tooltip_text("PDF Preview (F8)")
        .action_name("win.toggle-pdf")
        .build();
    let build_button = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Build (F5)")
        .action_name("win.build")
        .build();
    let build_indicator = adw::Bin::new();
    let review_summary = adw::Bin::new();
    bar.pack_end(&menu_button);
    bar.pack_end(&agent_button);
    bar.pack_end(&pdf_button);
    bar.pack_end(&build_button);
    bar.pack_end(&build_indicator);
    bar.pack_end(&review_summary);
    Header {
        bar,
        title,
        build_button,
        build_indicator,
        review_summary,
        menu,
        menu_files: files,
        menu_go: go,
        menu_view: view,
    }
}

fn build_sidebar() -> Sidebar {
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    // The switcher shows each page's icon, with its title as the tooltip: four labels do not
    // fit the sidebar's 200 px minimum.
    let page = |name: &str, title: &str, icon: &str, heading: &str, text: &str| {
        let bin = adw::Bin::builder()
            .child(&placeholder(icon, heading, text))
            .build();
        stack
            .add_titled(&bin, Some(name), title)
            .set_icon_name(icon);
        bin
    };
    let outline = page(
        PAGE_OUTLINE,
        "Outline",
        "view-list-bullet-symbolic",
        "No Outline",
        "Sections, theorems and labels of the document appear here.",
    );
    let changes = page(
        PAGE_CHANGES,
        "Changes",
        "document-edit-symbolic",
        "No Changes",
        "Edits made by the agent appear here for review.",
    );
    let problems = page(
        PAGE_PROBLEMS,
        "Problems",
        "dialog-warning-symbolic",
        "No Problems",
        "Errors and warnings of the last build appear here.",
    );
    let search = page(
        PAGE_SEARCH,
        "Search",
        "edit-find-symbolic",
        "No Results",
        "Find and replace text in every file of the project with Ctrl+Shift+F.",
    );
    let switcher = gtk::StackSwitcher::builder().stack(&stack).build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("lemma-sidebar");
    content.append(&switcher);
    content.append(&stack);
    let split = adw::OverlaySplitView::builder()
        .sidebar(&content)
        .sidebar_position(gtk::PackType::Start)
        .show_sidebar(true)
        .min_sidebar_width(200.0)
        .max_sidebar_width(300.0)
        .sidebar_width_fraction(0.18)
        .build();
    Sidebar {
        split,
        stack,
        outline,
        changes,
        problems,
        search,
    }
}

/// The PDF pane's strip of thumbnails and bookmarks (hidden): a split view whose content the
/// PDF controller sets to the PDF view, its slot, and the toolbar button that shows it.
fn build_pdf_sidebar() -> PdfSidebar {
    // A status page is too big for a strip this narrow: an icon and a caption.
    let empty = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .valign(gtk::Align::Center)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["dim-label"])
        .build();
    empty.append(
        &gtk::Image::builder()
            .icon_name("view-grid-symbolic")
            .pixel_size(32)
            .build(),
    );
    empty.append(
        &gtk::Label::builder()
            .label("Thumbnails and Bookmarks")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["caption"])
            .build(),
    );
    let slot = adw::Bin::builder().child(&empty).build();
    slot.add_css_class("lemma-pdf-sidebar");
    let split = adw::OverlaySplitView::builder()
        .sidebar(&slot)
        .sidebar_position(gtk::PackType::Start)
        .show_sidebar(false)
        .collapsed(false)
        .min_sidebar_width(110.0)
        .max_sidebar_width(220.0)
        .sidebar_width_fraction(0.25)
        .vexpand(true)
        .build();
    let toggle = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Thumbnails and Bookmarks")
        .action_name("win.toggle-pdf-sidebar")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    PdfSidebar {
        split,
        slot,
        toggle,
    }
}

fn build_status_bar() -> StatusBar {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    bar.add_css_class("lemma-statusbar");
    let label = |classes: &[&str]| {
        gtk::Label::builder()
            .css_classes(classes.iter().map(|c| c.to_string()).collect::<Vec<_>>())
            .build()
    };
    let file = label(&[]);
    file.set_xalign(0.0);
    file.set_hexpand(true);
    file.set_ellipsize(pango::EllipsizeMode::Middle);
    let vim_command = label(&["monospace", "dim-label"]);
    let mode = label(&["mode"]);
    mode.set_visible(false);
    let word_count = adw::Bin::builder().visible(false).build();
    let position = label(&["numeric"]);
    let build = label(&["dim-label"]);
    build.set_text("Not built");
    bar.append(&file);
    bar.append(&vim_command);
    bar.append(&mode);
    bar.append(&word_count);
    bar.append(&position);
    bar.append(&build);
    StatusBar {
        bar,
        file,
        vim_command,
        mode,
        word_count,
        position,
        build,
    }
}

/// A compact status page used as a pane's placeholder.
fn placeholder(icon: &str, title: &str, description: &str) -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(description)
        .css_classes(["compact", "dim-label"])
        .build()
}

/// The initial window size: `LEMMA_WINDOW_SIZE=WxH` (a testing aid, e.g. for narrow-window
/// snapshots), else the default.
fn initial_size() -> (i32, i32) {
    std::env::var("LEMMA_WINDOW_SIZE")
        .ok()
        .and_then(|v| parse_size(&v))
        .unwrap_or((DEFAULT_WIDTH, DEFAULT_HEIGHT))
}

fn parse_size(text: &str) -> Option<(i32, i32)> {
    let (w, h) = text.trim().split_once('x')?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    (w >= MIN_WIDTH && h >= MIN_HEIGHT).then_some((w, h))
}

/// Gives the editor [`EDITOR_SHARE`] of the editor/PDF split when the paned is first allocated.
/// Afterwards GtkPaned keeps the proportion when the window is resized (both children resize).
fn set_initial_split(paned: &gtk::Paned) {
    let done = Cell::new(false);
    paned.connect_max_position_notify(move |paned| {
        let width = paned.width();
        if !done.get() && width > 0 {
            done.set(true);
            paned.set_position((f64::from(width) * EDITOR_SHARE).round() as i32);
        }
    });
}

fn add_narrow_breakpoint(window: &adw::ApplicationWindow, sidebar: &Sidebar, agent: &AgentPane) {
    let condition = adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        NARROW_WIDTH_SP,
        adw::LengthUnit::Sp,
    );
    let narrow = adw::Breakpoint::new(condition);
    narrow.add_setter(&sidebar.split, "collapsed", Some(&true.to_value()));
    narrow.add_setter(&sidebar.split, "show-sidebar", Some(&false.to_value()));
    narrow.add_setter(&agent.split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(narrow);
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn window_sizes() {
        assert_eq!(parse_size("800x600"), Some((800, 600)));
        assert_eq!(parse_size(" 1024x768 "), Some((1024, 768)));
        assert_eq!(parse_size("100x100"), None, "below the minimum size");
        assert_eq!(parse_size("800"), None);
        assert_eq!(parse_size("wide x tall"), None);
    }
}
