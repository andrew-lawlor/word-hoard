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
        let movable = db.as_deref().and_then(location::folder_of).is_some();
        if movable
            && !location::is_library(&chosen)
            && hoard_core::calibre::is_calibre_library(&chosen)
        {
            return self.confirm_calibre_in_place(prefs, chosen, db.unwrap_or_default());
        }
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

    /// The chosen folder is a Calibre library: offer to use its books where
    /// they are, bringing this library's highlights, vocabulary and Kobo
    /// records along (see `location::use_folder_in_place`).
    fn confirm_calibre_in_place(
        self: &Rc<Self>,
        prefs: &adw::PreferencesDialog,
        chosen: PathBuf,
        db: PathBuf,
    ) {
        let dialog = adw::AlertDialog::new(
            Some("Use Your Calibre Library Where It Is?"),
            Some(&format!(
                "{} becomes your library, and Word Hoard uses the books there instead of its own copies. Books that are in both are matched by their contents; any only in Word Hoard are moved there. Your highlights, vocabulary and Kobo records come along, and books in Calibre that Word Hoard doesn’t have yet are added.\n\nCalibre’s own database isn’t changed, so Calibre won’t know about books added in Word Hoard. Afterwards you can move the copies Word Hoard no longer uses to the Trash.",
                chosen.display()
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("use", "Use in Place")]);
        dialog.set_response_appearance("use", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("use"));
        dialog.set_close_response("cancel");
        let this = self.clone();
        let prefs = prefs.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "use" {
                prefs.close();
                this.use_in_place(db.clone(), chosen.clone());
            }
        });
        dialog.present(Some(&self.win));
    }

    /// Re-homes the library onto `folder` (a Calibre library) in the
    /// background, then brings in Calibre's details and any books it has that
    /// the library doesn't, then offers to move the unused copies to Trash.
    fn use_in_place(self: &Rc<Self>, db: PathBuf, folder: PathBuf) {
        self.importing.set(true);
        *self.lib.borrow_mut() = match Library::open_in_memory() {
            Ok(lib) => lib,
            Err(err) => {
                self.importing.set(false);
                return self.error("Couldn’t Use the Calibre Library", err);
            }
        };
        self.start_progress("Matching your books with Calibre’s…");
        let this = self.clone();
        glib::spawn_future_local(async move {
            let job = (db.clone(), folder.clone());
            let result =
                gio::spawn_blocking(move || location::use_folder_in_place(&job.0, &job.1)).await;
            this.importing.set(false);
            this.set_progress(None);
            let (open, outcome) = match result {
                Ok(Ok(stats)) => (stats.db.clone(), Ok(stats)),
                Ok(Err(err)) => (db, Err(err.to_string())),
                Err(_) => (db, Err("Matching stopped unexpectedly.".to_owned())),
            };
            match Library::open(&open) {
                Ok(lib) => *this.lib.borrow_mut() = lib,
                Err(err) => return this.error("Couldn’t Open the Library", err),
            }
            let stats = match outcome {
                Ok(stats) => stats,
                Err(why) => {
                    this.library_changed();
                    return this.error(
                        "Couldn’t Use the Calibre Library",
                        format!("{why}\n\nYour library is still where it was."),
                    );
                }
            };
            if let Err(err) = location::set_current_library(&folder) {
                this.error("Couldn’t Remember the Library Folder", err);
            }
            this.library_changed();
            // Calibre's details, and its books the library didn't have.
            this.import_calibre_from(folder.clone());
            this.offer_trash_old_copies(stats);
        });
    }

    /// After using a folder in place: offers to move the copies the library
    /// no longer uses to the Trash (recoverable), then tidies empty folders.
    fn offer_trash_old_copies(self: &Rc<Self>, stats: location::InPlaceStats) {
        let copies: Vec<PathBuf> = stats
            .old_copies
            .into_iter()
            .filter(|p| p.is_file())
            .collect();
        if copies.is_empty() {
            return;
        }
        let size: u64 = copies
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        let old_root = Some(stats.old_folder.clone());
        let dialog = adw::AlertDialog::new(
            Some("Move Unused Copies to the Trash?"),
            Some(&format!(
                "Your library now uses the books in your Calibre folder. The {} it used before{} ({}) are no longer needed. Moving them to the Trash frees the space; you can restore them from there.",
                plural(copies.len(), "copy", "copies"),
                old_root
                    .as_ref()
                    .map(|r| format!(" in {}", r.display()))
                    .unwrap_or_default(),
                glib::format_size(size)
            )),
        );
        dialog.add_responses(&[("keep", "Keep Them"), ("trash", "Move to Trash")]);
        dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("keep"));
        dialog.set_close_response("keep");
        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "trash" {
                return;
            }
            let mut failed = 0;
            for copy in &copies {
                if gio::File::for_path(copy)
                    .trash(gio::Cancellable::NONE)
                    .is_err()
                {
                    failed += 1;
                    continue;
                }
                // Remove the folders left empty (never the books folder itself).
                let mut dir = copy.clone();
                while dir.pop()
                    && old_root
                        .as_ref()
                        .is_some_and(|r| dir.starts_with(r) && dir != *r)
                    && std::fs::remove_dir(&dir).is_ok()
                {}
            }
            if failed > 0 {
                this.error(
                    "Some Copies Weren’t Moved",
                    format!(
                        "{} couldn’t be moved to the Trash.",
                        plural(failed, "copy", "copies")
                    ),
                );
            } else {
                this.toast(&format!(
                    "Moved {} to the Trash",
                    plural(copies.len(), "copy", "copies")
                ));
            }
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
