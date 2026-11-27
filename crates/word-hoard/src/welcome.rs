//! The welcome window: the first time Word Hoard runs, before any library
//! exists, it asks where the user's books are now, and sets the library up
//! from there. Nothing is written until they choose.
//!
//! - In Calibre: import it (books hard-linked where possible, so no extra
//!   space; Calibre's library isn't changed).
//! - In a folder: make that folder the library, books left where they are,
//!   or copy them into the library folder.
//! - In a Word Hoard library (synced from another computer, or a backup):
//!   open it.
//! - Nowhere yet: an empty library.
//!
//! The library folder (where Calibre and copied books go) is shown with a
//! Change… button rather than asked as a step. A connected Kobo's highlights
//! are imported on any path, as usual.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use hoard_core::store::location;
use hoard_core::{Library, calibre};

use crate::window::Window;

/// What to do once the library is open.
enum Start {
    Calibre(PathBuf),
    AddFolder(PathBuf),
    Nothing,
}

struct Welcome {
    app: adw::Application,
    win: adw::ApplicationWindow,
    /// Where a new library goes (Calibre books, copied books, empty).
    folder: RefCell<PathBuf>,
    folder_row: adw::ActionRow,
    existing_row: adw::ActionRow,
    calibre_row: adw::ActionRow,
    /// The Calibre library found, with its book count.
    calibre: RefCell<Option<(PathBuf, Option<usize>)>>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// `~/…` for paths in the home folder.
fn tidy(path: &Path) -> String {
    match path.strip_prefix(home()) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn choice_row(icon: &str, title: &str, subtitle: &str, chevron: bool) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    if chevron {
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    }
    row
}

/// The app's icon: from the icon theme when installed (the Flatpak), else
/// the copy built in (development builds).
fn app_icon(size: i32) -> gtk::Image {
    let image = gtk::Image::builder().pixel_size(size).build();
    let installed = gtk::gdk::Display::default()
        .map(|d| gtk::IconTheme::for_display(&d).has_icon(crate::APP_ID))
        .unwrap_or(false);
    if installed {
        image.set_icon_name(Some(crate::APP_ID));
        return image;
    }
    let svg = glib::Bytes::from_static(include_bytes!(
        "../data/io.github.andrew_lawlor.WordHoard.svg"
    ));
    let stream = gio::MemoryInputStream::from_bytes(&svg);
    match gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(
        &stream,
        size * 2,
        size * 2,
        true,
        gio::Cancellable::NONE,
    ) {
        Ok(pixbuf) => image.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&pixbuf))),
        Err(_) => image.set_icon_name(Some("accessories-dictionary-symbolic")),
    }
    image
}

pub fn show(app: &adw::Application) {
    let folder = hoard_core::store::books::default_books_root();

    let calibre_row = choice_row(
        "x-office-address-book-symbolic",
        "In Calibre",
        "Looking for your Calibre library…",
        true,
    );
    let folder_choice = choice_row(
        "folder-symbolic",
        "In a Folder",
        "EPUB books, organised however you like",
        true,
    );
    let existing_row = choice_row(
        "emblem-synchronizing-symbolic",
        "In a Word Hoard Library",
        "From another computer, or a backup",
        true,
    );
    let empty_row = choice_row(
        "document-new-symbolic",
        "Nowhere Yet",
        "Start with an empty library",
        false,
    );
    let choices = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    for row in [&calibre_row, &folder_choice, &existing_row, &empty_row] {
        choices.append(row);
    }

    let folder_row = adw::ActionRow::builder()
        .title("Library Folder")
        .subtitle(tidy(&folder))
        .build();
    let change = gtk::Button::builder()
        .label("Change…")
        .valign(gtk::Align::Center)
        .build();
    folder_row.add_suffix(&change);
    let folder_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    folder_list.append(&folder_row);
    let folder_note = gtk::Label::builder()
        .label("Your books and everything Word Hoard knows about them live here, so the folder is the whole library.")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();

    let kobo_note = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .build();
    match hoard_core::kobo::device::find_mounted_kobos().first() {
        Some(kobo) => {
            let name = hoard_core::kobo::device::DeviceInfo::read(kobo)
                .map(|d| d.model_name())
                .unwrap_or("Kobo");
            kobo_note.set_label(&format!(
                "Your {name} is connected: its highlights will be imported too."
            ));
        }
        None => kobo_note
            .set_label("Connect your Kobo with a USB cable any time to bring in its highlights."),
    }

    let icon = app_icon(96);
    let title = gtk::Label::builder()
        .label("Welcome to Word Hoard")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["title-1"])
        .build();
    let question = gtk::Label::builder()
        .label("Where are your books right now?")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["title-4", "dim-label"])
        .build();

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .build();
    body.append(&icon);
    body.append(&title);
    body.append(&question);
    choices.set_margin_top(12);
    body.append(&choices);
    folder_list.set_margin_top(18);
    body.append(&folder_list);
    body.append(&folder_note);
    kobo_note.set_margin_top(12);
    body.append(&kobo_note);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(
        &adw::HeaderBar::builder()
            .show_title(false)
            .css_classes(["flat"])
            .build(),
    );
    toolbar.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&adw::Clamp::builder().maximum_size(520).child(&body).build())
            .build(),
    ));
    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("Welcome to Word Hoard")
        .default_width(620)
        .default_height(760)
        .width_request(360)
        .content(&toolbar)
        .build();

    let this = Rc::new(Welcome {
        app: app.clone(),
        win,
        folder: RefCell::new(folder),
        folder_row,
        existing_row,
        calibre_row,
        calibre: RefCell::default(),
    });
    this.update_existing();

    // Look for Calibre without holding up the window.
    {
        let this = this.clone();
        glib::spawn_future_local(async move {
            let found = gio::spawn_blocking(|| {
                calibre::find_libraries(&home())
                    .into_iter()
                    .next()
                    .map(|p| {
                        let n = calibre::count_books(&p);
                        (p, n)
                    })
            })
            .await
            .ok()
            .flatten();
            this.calibre_row.set_subtitle(&match &found {
                Some((path, Some(n))) => {
                    format!("{} · {}", tidy(path), plural(*n, "book", "books"))
                }
                Some((path, None)) => tidy(path),
                None => "Choose your Calibre library folder".to_owned(),
            });
            *this.calibre.borrow_mut() = found;
        });
    }

    let weak = Rc::downgrade(&this);
    change.connect_clicked(move |_| {
        if let Some(this) = weak.upgrade() {
            this.change_folder();
        }
    });
    let weak = Rc::downgrade(&this);
    this.calibre_row.connect_activated(move |_| {
        if let Some(this) = weak.upgrade() {
            this.from_calibre();
        }
    });
    let weak = Rc::downgrade(&this);
    folder_choice.connect_activated(move |_| {
        if let Some(this) = weak.upgrade() {
            this.from_folder();
        }
    });
    let weak = Rc::downgrade(&this);
    this.existing_row.connect_activated(move |_| {
        if let Some(this) = weak.upgrade() {
            this.from_existing();
        }
    });
    let weak = Rc::downgrade(&this);
    empty_row.connect_activated(move |_| {
        if let Some(this) = weak.upgrade() {
            let folder = this.folder.borrow().clone();
            this.finish(&folder, Start::Nothing);
        }
    });

    this.win.present();
}

impl Welcome {
    /// Points "In a Word Hoard Library" at a library already in the library
    /// folder (a synced Documents folder, say).
    fn update_existing(&self) {
        let folder = self.folder.borrow();
        self.existing_row
            .set_subtitle(&if location::is_library(&folder) {
                format!("Found in {}", tidy(&folder))
            } else {
                "From another computer, or a backup".to_owned()
            });
    }

    async fn pick_folder(
        &self,
        title: &str,
        accept: &str,
        start: Option<&Path>,
    ) -> Option<PathBuf> {
        let dialog = gtk::FileDialog::builder()
            .title(title)
            .accept_label(accept)
            .modal(true)
            .build();
        if let Some(start) = start.filter(|p| p.is_dir()) {
            dialog.set_initial_folder(Some(&gio::File::for_path(start)));
        }
        dialog
            .select_folder_future(Some(&self.win))
            .await
            .ok()
            .and_then(|f| f.path())
    }

    fn error(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response("close", "OK");
        dialog.present(Some(&self.win));
    }

    fn change_folder(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            let current = this.folder.borrow().clone();
            let start = current.parent().map(Path::to_path_buf);
            let Some(folder) = this
                .pick_folder("Choose a Library Folder", "Choose", start.as_deref())
                .await
            else {
                return;
            };
            this.folder_row.set_subtitle(&tidy(&folder));
            *this.folder.borrow_mut() = folder;
            this.update_existing();
        });
    }

    fn from_calibre(self: &Rc<Self>) {
        let found = self.calibre.borrow().clone();
        match found {
            Some((path, count)) => self.confirm_calibre(path, count, true),
            None => self.choose_calibre(),
        }
    }

    /// Asks how to bring in the Calibre library at `path`: use it where it is
    /// (the Calibre folder becomes the library; the usual choice when leaving
    /// Calibre for Word Hoard), or import a copy into the library folder.
    fn confirm_calibre(
        self: &Rc<Self>,
        path: PathBuf,
        count: Option<usize>,
        can_choose_another: bool,
    ) {
        let folder = self.folder.borrow().clone();
        let books = count.map_or("Its books".to_owned(), |n| {
            format!("Its {}", plural(n, "book", "books"))
        });
        let dialog = adw::AlertDialog::new(
            Some("Use Your Calibre Library?"),
            Some(&format!(
                "Word Hoard can use {} where it is: that folder becomes your library, with no copies made. {books} come with their covers, series, tags and ratings, and books you add later are filed there too. Calibre’s own database isn’t changed, so Calibre won’t know about books added in Word Hoard.\n\nOr import a copy into {}, leaving the Calibre library as it is. On the same drive, copies are linked, so they take no extra space.",
                tidy(&path),
                tidy(&folder)
            )),
        );
        dialog.add_response("cancel", "Cancel");
        if can_choose_another {
            dialog.add_response("other", "Choose Another…");
        }
        dialog.add_response("copy", "Import a Copy");
        dialog.add_response("in-place", "Use It Where It Is");
        dialog.set_response_appearance("in-place", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("in-place"));
        dialog.set_close_response("cancel");
        let this = self.clone();
        dialog.connect_response(None, move |_, response| match response {
            "in-place" => this.finish(&path, Start::Calibre(path.clone())),
            "copy" => this.finish(&folder, Start::Calibre(path.clone())),
            "other" => this.choose_calibre(),
            _ => {}
        });
        dialog.present(Some(&self.win));
    }

    fn choose_calibre(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            let start = this.calibre.borrow().as_ref().map(|(p, _)| p.clone());
            let Some(path) = this
                .pick_folder("Choose Your Calibre Library", "Choose", start.as_deref())
                .await
            else {
                return;
            };
            if !calibre::is_calibre_library(&path) {
                return this.error(
                    "Not a Calibre Library",
                    "Choose the folder that holds Calibre’s metadata.db (usually “Calibre Library” in your home folder).",
                );
            }
            let count = calibre::count_books(&path);
            this.confirm_calibre(path, count, false);
        });
    }

    fn from_folder(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Some(chosen) = this
                .pick_folder("Choose Your Books Folder", "Choose", Some(&home()))
                .await
            else {
                return;
            };
            if location::is_library(&chosen) {
                return this.finish(&chosen, Start::Nothing);
            }
            let scan = chosen.clone();
            let count = gio::spawn_blocking(move || crate::window::collect_books(&[scan]).0.len())
                .await
                .unwrap_or(0);
            if count == 0 {
                return this.error(
                    "No Books Found",
                    &format!(
                        "There are no EPUB books in {}, or in the folders inside it.",
                        tidy(&chosen)
                    ),
                );
            }
            let folder = this.folder.borrow().clone();
            let name = chosen
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| tidy(&chosen));
            let dialog = adw::AlertDialog::new(
                Some(&format!("Make “{name}” Your Library?")),
                Some(&format!(
                    "Word Hoard found {} there. Keep them where they are and make this folder your library: a hidden .word-hoard folder is added beside them, and books you add later are filed into it by author and title.\n\nOr copy them into {}, leaving this folder as it is.",
                    plural(count, "book", "books"),
                    tidy(&folder)
                )),
            );
            dialog.add_responses(&[
                ("cancel", "Cancel"),
                ("copy", "Copy Books"),
                ("use", "Use This Folder"),
            ]);
            dialog.set_response_appearance("use", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("use"));
            dialog.set_close_response("cancel");
            let this2 = this.clone();
            dialog.connect_response(None, move |_, response| match response {
                "use" => this2.finish(&chosen, Start::AddFolder(chosen.clone())),
                "copy" => this2.finish(&folder, Start::AddFolder(chosen.clone())),
                _ => {}
            });
            dialog.present(Some(&this.win));
        });
    }

    fn from_existing(self: &Rc<Self>) {
        let folder = self.folder.borrow().clone();
        if location::is_library(&folder) {
            return self.finish(&folder, Start::Nothing);
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Some(chosen) = this
                .pick_folder("Choose Your Word Hoard Library", "Open", Some(&home()))
                .await
            else {
                return;
            };
            if !location::is_library(&chosen) {
                return this.error(
                    "No Word Hoard Library Here",
                    &format!(
                        "{} has no Word Hoard library. Choose the folder that holds your books and the hidden .word-hoard folder.",
                        tidy(&chosen)
                    ),
                );
            }
            this.finish(&chosen, Start::Nothing);
        });
    }

    /// Opens (creating) the library in `folder`, remembers it, swaps this
    /// window for the main one, and starts `start` there.
    fn finish(&self, folder: &Path, start: Start) {
        let opened = Library::open(&location::library_db(folder))
            .and_then(|lib| location::set_current_library(folder).map(|()| lib));
        let lib = match opened {
            Ok(lib) => lib,
            Err(err) => {
                return self.error(
                    "Couldn’t Create the Library",
                    &format!("{}\n\n{err}", tidy(folder)),
                );
            }
        };
        let window = Window::new(&self.app, lib);
        window.present();
        self.win.close();
        match start {
            Start::Calibre(path) => window.import_calibre_from(path),
            Start::AddFolder(path) => window.add_book_files(&[path]),
            Start::Nothing => {}
        }
    }
}
