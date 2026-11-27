//! Choosing the library folder (Preferences ▸ Library): open the library in
//! another folder, start a new one, or move this one there.

use std::path::Path;

use hoard_core::store::location;

use super::*;

impl Window {
    /// The Library group for Preferences.
    pub(super) fn library_group(
        self: &Rc<Self>,
        prefs: &adw::PreferencesDialog,
    ) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Library")
            .description(
                "Your library is one folder: your books, and a hidden .word-hoard folder with \
                 everything Word Hoard knows about them. Back it up or sync it as a whole.",
            )
            .build();
        let folder = self.lib.borrow().books_root().ok();
        let row = adw::ActionRow::builder()
            .title("Library Folder")
            .subtitle(
                folder
                    .as_ref()
                    .map(|f| f.display().to_string())
                    .unwrap_or_default(),
            )
            .subtitle_selectable(true)
            .build();
        let change = gtk::Button::builder()
            .label("Change…")
            .valign(gtk::Align::Center)
            .build();
        let weak = Rc::downgrade(self);
        let prefs_weak = prefs.downgrade();
        change.connect_clicked(move |_| {
            if let (Some(this), Some(prefs)) = (weak.upgrade(), prefs_weak.upgrade()) {
                this.choose_library_folder(&prefs);
            }
        });
        row.add_suffix(&change);
        group.add(&row);
        group
    }

    fn choose_library_folder(self: &Rc<Self>, prefs: &adw::PreferencesDialog) {
        if self.importing.get() || self.adding_books.get() || self.sending.get() {
            return self.error(
                "Library Busy",
                "Wait for Word Hoard to finish importing or sending books, then change the library folder.",
            );
        }
        let dialog = gtk::FileDialog::builder()
            .title("Choose a Library Folder")
            .accept_label("Choose")
            .modal(true)
            .build();
        if let Ok(current) = self.lib.borrow().books_root() {
            dialog.set_initial_folder(Some(&gio::File::for_path(current)));
        }
        let this = self.clone();
        let prefs = prefs.clone();
        glib::spawn_future_local(async move {
            let Ok(chosen) = dialog.select_folder_future(Some(&this.win)).await else {
                return;
            };
            let Some(chosen) = chosen.path() else { return };
            this.confirm_library_folder(&prefs, chosen);
        });
    }

    fn confirm_library_folder(self: &Rc<Self>, prefs: &adw::PreferencesDialog, chosen: PathBuf) {
        let db = self.lib.borrow().path().map(Path::to_path_buf);
        let current = self.lib.borrow().books_root().ok();
        if current.as_deref() == Some(chosen.as_path()) {
            return;
        }
        let name = chosen
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| chosen.display().to_string());
        let dialog;
        if location::is_library(&chosen) {
            dialog = adw::AlertDialog::new(
                Some(&format!("Open the Library in “{name}”?")),
                Some(&format!(
                    "{} already holds a Word Hoard library. This one stays where it is, and you can switch back any time.",
                    chosen.display()
                )),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("open", "Open Library")]);
            dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("open"));
        } else {
            let movable = db.as_deref().and_then(location::folder_of).is_some();
            dialog = adw::AlertDialog::new(
                Some(&format!("Use “{name}” for Your Library?")),
                Some(&if movable {
                    format!(
                        "Move this library to {}: its books and everything about them. Other files in {} stay where they are.\n\nOr start a new, empty library there; this one stays where it is.",
                        chosen.display(),
                        current
                            .as_ref()
                            .map(|c| c.display().to_string())
                            .unwrap_or_default()
                    )
                } else {
                    format!("Start a new, empty library in {}.", chosen.display())
                }),
            );
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("new", "New Library");
            if movable {
                dialog.add_response("move", "Move Library");
                dialog.set_response_appearance("move", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("move"));
            }
        }
        dialog.set_close_response("cancel");
        let this = self.clone();
        let prefs = prefs.clone();
        dialog.connect_response(None, move |_, response| match response {
            "open" | "new" => {
                prefs.close();
                this.switch_library(&chosen);
            }
            "move" => {
                prefs.close();
                if let Some(db) = &db {
                    this.move_library(db.clone(), chosen.clone());
                }
            }
            _ => {}
        });
        dialog.present(Some(&self.win));
    }

    /// Opens (creating if needed) the library in `folder`, from now on.
    fn switch_library(self: &Rc<Self>, folder: &Path) {
        let opened = Library::open(&location::library_db(folder))
            .and_then(|lib| location::set_current_library(folder).map(|()| lib));
        match opened {
            Ok(lib) => {
                *self.lib.borrow_mut() = lib;
                self.library_changed();
                self.toast(&format!("Opened the library in {}", folder.display()));
            }
            Err(err) => self.error("Couldn’t Open the Library", err),
        }
    }

    /// Moves the library to `folder` in the background. The library is
    /// closed meanwhile (an empty one stands in), and reopened from its new
    /// place, or its old one if the move failed.
    fn move_library(self: &Rc<Self>, db: PathBuf, folder: PathBuf) {
        self.importing.set(true);
        *self.lib.borrow_mut() = match Library::open_in_memory() {
            Ok(lib) => lib,
            Err(err) => {
                self.importing.set(false);
                return self.error("Couldn’t Move the Library", err);
            }
        };
        self.start_progress("Moving your library…");
        let this = self.clone();
        glib::spawn_future_local(async move {
            let job = (db.clone(), folder.clone());
            let moved = gio::spawn_blocking(move || location::move_library(&job.0, &job.1)).await;
            this.importing.set(false);
            this.set_progress(None);
            let (open, failure) = match moved {
                Ok(Ok(new_db)) => (new_db, None),
                Ok(Err(err)) => (db, Some(err.to_string())),
                Err(_) => (db, Some("Moving stopped unexpectedly.".to_owned())),
            };
            match Library::open(&open) {
                Ok(lib) => *this.lib.borrow_mut() = lib,
                Err(err) => return this.error("Couldn’t Open the Library", err),
            }
            if let Some(why) = failure {
                this.library_changed();
                return this.error(
                    "Couldn’t Move the Library",
                    format!("{why}\n\nYour library is still where it was."),
                );
            }
            if let Err(err) = location::set_current_library(&folder) {
                this.error("Couldn’t Remember the Library Folder", err);
            }
            this.library_changed();
            this.toast(&format!("Moved your library to {}", folder.display()));
        });
    }

    /// Shows the library that's open now.
    fn library_changed(self: &Rc<Self>) {
        self.set_selecting(false);
        self.rebuild_sidebar();
        self.reload();
        self.refresh_on_device();
    }
}
