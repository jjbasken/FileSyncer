use adw::prelude::*;
use filesyncer_core::{Profile, ProfileSet, profile};
use std::cell::RefCell;
use std::rc::Rc;

pub struct AppWindow {
    pub window: adw::ApplicationWindow,
    pub split: adw::NavigationSplitView,
    pub profiles: Rc<RefCell<ProfileSet>>,
    pub sidebar_list: gtk::ListBox,
}

impl AppWindow {
    /// Replace the content pane. Every later task routes its screens
    /// through here so navigation stays in one place.
    pub fn show_content(&self, child: &impl IsA<gtk::Widget>, title: &str) {
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(child));

        let page = adw::NavigationPage::new(&toolbar, title);
        self.split.set_content(Some(&page));
    }

    pub fn show_placeholder(&self) {
        let status = adw::StatusPage::builder()
            .icon_name("folder-symbolic")
            .title("No profile selected")
            .description("Choose a profile from the sidebar, or add one.")
            .build();
        self.show_content(&status, "FileSyncer");
    }

    pub fn reload_profiles(&self) {
        let loaded = ProfileSet::load(&profile::config_path()).unwrap_or_default();
        *self.profiles.borrow_mut() = loaded;
        self.rebuild_sidebar();
    }

    fn rebuild_sidebar(&self) {
        while let Some(child) = self.sidebar_list.first_child() {
            self.sidebar_list.remove(&child);
        }
        for p in &self.profiles.borrow().profiles {
            let row = adw::ActionRow::builder()
                .title(&p.name)
                .subtitle(p.remote.display().to_string())
                .activatable(true)
                .build();
            // The id is not shown, but every handler needs it.
            unsafe { row.set_data("profile-id", p.id.clone()) };
            self.sidebar_list.append(&row);
        }
    }

    pub fn selected_profile(&self, row: &gtk::ListBoxRow) -> Option<Profile> {
        let id: String = unsafe { row.data::<String>("profile-id")?.as_ref().clone() };
        self.profiles.borrow().get(&id).cloned()
    }
}

pub fn build(app: &adw::Application) -> Rc<AppWindow> {
    let sidebar_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(vec!["navigation-sidebar".to_string()])
        .build();

    let add_button = gtk::Button::from_icon_name("list-add-symbolic");
    add_button.set_tooltip_text(Some("Add profile"));

    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.pack_end(&add_button);

    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&sidebar_header);
    sidebar_toolbar.set_content(Some(
        &gtk::ScrolledWindow::builder().child(&sidebar_list).build(),
    ));

    let sidebar_page = adw::NavigationPage::new(&sidebar_toolbar, "Profiles");

    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar_page)
        .min_sidebar_width(220.0)
        .build();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("FileSyncer")
        .default_width(1200)
        .default_height(720)
        .content(&split)
        .build();

    let app_window = Rc::new(AppWindow {
        window,
        split,
        profiles: Rc::new(RefCell::new(ProfileSet::default())),
        sidebar_list,
    });

    app_window.reload_profiles();
    app_window.show_placeholder();

    {
        let aw = app_window.clone();
        app_window
            .sidebar_list
            .connect_row_activated(move |_, row| {
                if let Some(p) = aw.selected_profile(row) {
                    crate::profile_page::show(&aw, &p);
                }
            });
    }

    {
        let aw = app_window.clone();
        add_button.connect_clicked(move |_| {
            crate::profile_dialog::present(&aw);
        });
    }

    app_window
}
