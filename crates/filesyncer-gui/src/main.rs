mod plan_view;
mod profile_dialog;
mod profile_page;
mod run_view;
mod scan_view;
mod window;

use adw::prelude::*;
use gtk::glib;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("net.jjbasken.FileSyncer")
        .build();
    app.connect_activate(|app| {
        let w = window::build(app);
        w.window.present();
    });
    app.run()
}
