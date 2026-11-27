//! Word Hoard: curate Kobo highlights, notes and vocabulary.

mod card;
mod covers;
mod edit;
mod portal;
mod share;
mod shortcuts;
mod welcome;
mod window;
mod word;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use hoard_core::Library;

/// Development builds use a separate ID, so they run alongside an installed
/// Word Hoard instead of handing their launch over to it.
pub const APP_ID: &str = if cfg!(debug_assertions) {
    "io.github.andrew_lawlor.WordHoard.Devel"
} else {
    "io.github.andrew_lawlor.WordHoard"
};

fn main() -> glib::ExitCode {
    if std::env::var_os("WORD_HOARD_LIBRARY").is_none() {
        match hoard_core::migrate_from_kollate() {
            Ok(Some(old)) => {
                eprintln!("word-hoard: moved Kollate's library from {}", old.display())
            }
            Ok(None) => {}
            Err(e) => eprintln!("word-hoard: couldn't move Kollate's library: {e}"),
        }
    }
    let app = adw::Application::builder().application_id(APP_ID).build();
    // Development only: `WORD_HOARD_SEPARATE=1` runs a second instance
    // beside an open window instead of handing over to it.
    if cfg!(debug_assertions) && std::env::var_os("WORD_HOARD_SEPARATE").is_some() {
        app.set_flags(app.flags() | gio::ApplicationFlags::NON_UNIQUE);
    }
    app.connect_startup(|_| load_css());
    app.connect_activate(activate);
    app.set_accels_for_action("win.search", &["<Control>f"]);
    app.set_accels_for_action("win.import", &["<Control>i"]);
    app.set_accels_for_action("win.add-books", &["<Control>o"]);
    app.set_accels_for_action("win.preferences", &["<Control>comma"]);
    app.set_accels_for_action("win.export", &["<Control>e"]);
    app.set_accels_for_action("win.back", &["<Alt>Left"]);
    app.set_accels_for_action("win.shortcuts", &["<Control>question", "<Control>slash"]);
    app.set_accels_for_action("window.close", &["<Control>w"]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.run()
}

/// The library database: the open library folder's (see
/// `hoard_core::store::location`); `WORD_HOARD_LIBRARY` overrides it.
/// `None` the first time, before a library is chosen.
fn library_path() -> hoard_core::Result<Option<std::path::PathBuf>> {
    // Development only: `WORD_HOARD_OPEN=welcome` shows the welcome window.
    if cfg!(debug_assertions) && std::env::var("WORD_HOARD_OPEN").as_deref() == Ok("welcome") {
        return Ok(None);
    }
    match std::env::var_os("WORD_HOARD_LIBRARY") {
        Some(path) => Ok(Some(path.into())),
        None => hoard_core::chosen_library(),
    }
}

fn load_css() {
    gtk::Window::set_default_icon_name(APP_ID);
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn activate(app: &adw::Application) {
    if let Some(win) = app.active_window() {
        win.present();
        return;
    }

    let quit = gio::SimpleAction::new("quit", None);
    quit.connect_activate(glib::clone!(
        #[weak]
        app,
        move |_, _| app.quit()
    ));
    app.add_action(&quit);

    let path = match library_path() {
        Ok(Some(path)) => Ok(path),
        Ok(None) => return welcome::show(app),
        Err(err) => Err(err),
    };
    let opened = path
        .as_ref()
        .map_err(|e| e.to_string())
        .and_then(|path| Library::open(path).map_err(|e| format!("{}\n\n{e}", path.display())));
    match opened {
        Ok(library) => window::Window::new(app, library).present(),
        Err(err) => {
            let win = adw::ApplicationWindow::builder().application(app).build();
            win.present();
            let dialog = adw::AlertDialog::new(Some("Couldn’t Open Library"), Some(&err));
            dialog.add_response("close", "Close");
            dialog.connect_response(None, move |_, _| win.close());
            dialog.present(None::<&gtk::Widget>);
        }
    }
}
