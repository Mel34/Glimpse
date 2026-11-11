//! Glimpse Application Entry Point (GTK4 + libadwaita)

mod controls;
mod interaction;
mod track_menu;
mod theme;
mod window;
mod presentation_sink;
mod video_texture;
mod rotatable_picture;
#[cfg(feature = "vaapi-hdr")]
mod vaapi_texture;

use adw::prelude::*;
use adw::Application;
use gio::ApplicationFlags;
use window::GlimpseWindow;

const APP_ID: &str = "com.github.Mel34.Glimpse";

fn main() -> glib::ExitCode {
    // Initialize tracing logger
    tracing_subscriber::fmt::init();

    let app = Application::builder()
        .application_id(APP_ID)
        .flags(ApplicationFlags::HANDLES_OPEN)
        .build();

    app.connect_startup(|app| {
        // Respect the saved color scheme, defaulting to the desktop preference.
        crate::theme::load().apply();
        gtk4::Window::set_default_icon_name("com.github.Mel34.Glimpse");

        let provider = gtk4::CssProvider::new();
        provider.load_from_string(include_str!("style.css"));
        if let Some(display) = gdk4::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        app.set_accels_for_action("win.open", &["<Control>o"]);
        app.set_accels_for_action("win.new-window", &["<Control>n"]);
        app.set_accels_for_action("win.shortcuts", &["<Control>question"]);
    });

    app.connect_activate(|app| {
        let win = GlimpseWindow::new(app);
        win.present();
    });

    app.connect_open(|app, files, _hint| {
        if files.is_empty() {
            let win = GlimpseWindow::new(app);
            win.present();
            return;
        }

        // Open every supplied media source in its own window. Using the URI
        // preserves non-UTF-8 local paths and supports remote GFile sources.
        for file in files {
            let win = GlimpseWindow::new(app);
            let uri = file.uri();
            win.open_file(uri.as_str());
            win.present();
        }
    });

    app.run()
}
