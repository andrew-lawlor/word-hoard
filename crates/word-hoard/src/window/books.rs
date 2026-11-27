//! Adding EPUB files to the Library.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use hoard_core::store::{AddMode, AddOutcome};

use super::*;

impl Window {
    /// Asks for EPUB files and adds them to the books folder.
    pub(super) fn add_books(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("EPUB Books"));
        filter.add_suffix("epub");
        filter.add_mime_type("application/epub+zip");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Add Books")
            .accept_label("Add")
            .filters(&filters)
            .default_filter(&filter)
            .modal(true)
            .build();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Ok(files) = dialog.open_multiple_future(Some(&this.win)).await else {
                return;
            };
            let paths: Vec<std::path::PathBuf> = (0..files.n_items())
                .filter_map(|i| files.item(i).and_downcast::<gio::File>())
                .filter_map(|f| f.path())
                .collect();
            this.add_book_files(&paths);
        });
    }

    fn add_book_files(self: &Rc<Self>, paths: &[std::path::PathBuf]) {
        let (mut added, mut known, mut failed) = (0usize, 0usize, Vec::new());
        {
            let lib = self.lib.borrow();
            for path in paths {
                match lib.add_file(path, AddMode::Copy) {
                    Ok(a) if a.outcome == AddOutcome::AlreadyInLibrary => known += 1,
                    Ok(_) => added += 1,
                    Err(err) => failed.push(format!(
                        "{}: {err}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )),
                }
            }
        }
        self.rebuild_sidebar();
        self.reload();
        if !failed.is_empty() {
            self.error(
                &format!("Couldn’t Add {}", plural(failed.len(), "Book", "Books")),
                failed.join("\n"),
            );
        }
        let mut parts = Vec::new();
        if added > 0 {
            parts.push(format!("Added {}", plural(added, "book", "books")));
        }
        if known > 0 {
            parts.push(format!(
                "{} already in the library",
                plural(known, "book", "books")
            ));
        }
        if !parts.is_empty() {
            self.toast(&parts.join(" · "));
        }
    }

    /// Asks for a Calibre library folder and imports it in the background.
    pub(super) fn import_calibre(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose Your Calibre Library")
            .accept_label("Import")
            .modal(true)
            .build();
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let default = home.join("Calibre Library");
        if default.is_dir() {
            dialog.set_initial_folder(Some(&gio::File::for_path(default)));
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Ok(folder) = dialog.select_folder_future(Some(&this.win)).await else {
                return;
            };
            let Some(path) = folder.path() else { return };
            if !path.join("metadata.db").is_file() {
                return this.error(
                    "Not a Calibre Library",
                    "Choose the folder that holds Calibre’s metadata.db (usually “Calibre Library” in your home folder).",
                );
            }
            let Some(db) = this.lib.borrow().path().map(Path::to_path_buf) else {
                return;
            };
            let progress = std::sync::Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
            this.start_progress("Importing your Calibre library…");
            let ticking = Rc::new(Cell::new(true));
            {
                let (this, progress, ticking) = (this.clone(), progress.clone(), ticking.clone());
                glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
                    if !ticking.get() {
                        return glib::ControlFlow::Break;
                    }
                    let (done, total) = (
                        progress.0.load(Ordering::Relaxed),
                        progress.1.load(Ordering::Relaxed),
                    );
                    if total > 0 {
                        this.set_progress(Some(&format!(
                            "Importing from Calibre… {done} of {total} books"
                        )));
                    }
                    glib::ControlFlow::Continue
                });
            }
            let job_progress = progress.clone();
            let result = gio::spawn_blocking(move || {
                let lib = hoard_core::Library::open(&db)?;
                lib.import_calibre(&path, AddMode::Copy, |done, total| {
                    job_progress.0.store(done, Ordering::Relaxed);
                    job_progress.1.store(total, Ordering::Relaxed);
                })
            })
            .await;
            ticking.set(false);
            this.set_progress(None);
            this.rebuild_sidebar();
            this.reload();
            match result {
                Ok(Ok(stats)) => {
                    let mut parts = vec![format!(
                        "Added {}",
                        plural(stats.books_new, "book", "books")
                    )];
                    if stats.books_matched > 0 {
                        parts.push(format!(
                            "{} already here",
                            plural(stats.books_matched, "book", "books")
                        ));
                    }
                    if stats.files_missing > 0 {
                        parts.push(format!(
                            "{} missing in Calibre",
                            plural(stats.files_missing, "file", "files")
                        ));
                    }
                    this.toast(&parts.join(" · "));
                }
                Ok(Err(err)) => this.error("Couldn’t Import from Calibre", err),
                Err(_) => this.error(
                    "Couldn’t Import from Calibre",
                    "The import stopped unexpectedly.",
                ),
            }
        });
    }

    /// Shows Send to Kobo on a book's page while a Kobo is connected.
    pub(super) fn update_send_button(&self) {
        let on_book = matches!(self.current.get(), Nav::Annotations(View::Book(_)));
        self.send_button
            .set_visible(on_book && self.kobo.borrow().is_some());
        self.send_button
            .set_tooltip_text(Some(&format!("Send to Your {}", self.kobo_name())));
    }

    /// Sends the book on screen to the connected Kobo, after showing what
    /// will be written and asking.
    pub(super) fn send_current_book(self: &Rc<Self>) {
        let Nav::Annotations(View::Book(id)) = self.current.get() else {
            return;
        };
        let Some(root) = self.kobo.borrow().as_ref().map(|k| k.root.clone()) else {
            return self.error(
                "No Kobo Connected",
                "Connect your Kobo with a USB cable to send books to it.",
            );
        };
        let plan = match self.lib.borrow().plan_send(&root, &[id]) {
            Ok(plan) => plan,
            Err(err) => return self.error("Couldn’t Send the Book", err),
        };
        let name = self.kobo_name();
        let Some(item) = plan.items.first().cloned() else {
            let reason = plan
                .skipped
                .first()
                .map_or("nothing to send", |s| s.reason.as_str());
            return self.toast(&format!("Not sent: {reason}"));
        };
        let how = match item.action {
            hoard_core::store::send::SendAction::Convert => "converted for the Kobo and copied",
            hoard_core::store::send::SendAction::Copy => "copied",
        };
        let dialog = adw::AlertDialog::new(
            Some(&format!("Send to Your {name}?")),
            Some(&format!(
                "“{}” will be {how} to {} ({}). Word Hoard backs up the Kobo’s database first and changes nothing else on it.\n\nEject the Kobo afterwards and it adds the book.",
                item.title,
                item.dest.display(),
                glib::format_size(item.size)
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("send", "Send")]);
        dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("send"));
        dialog.set_close_response("cancel");
        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "send" {
                return;
            }
            let Some(db) = this.lib.borrow().path().map(Path::to_path_buf) else {
                return;
            };
            let Some(backups) = this
                .lib
                .borrow()
                .assets_dir()
                .map(|d| d.join("backups/kobo"))
            else {
                return;
            };
            let (this, plan, root, title) =
                (this.clone(), plan.clone(), root.clone(), item.title.clone());
            this.start_progress(&format!("Sending “{title}”…"));
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || {
                    let lib = hoard_core::Library::open(&db)?;
                    let writer = hoard_core::kobo::writer::DeviceWriter::open(&root, &backups)?;
                    lib.apply_send(&writer, &plan, |_, _| {})
                })
                .await;
                this.set_progress(None);
                match result {
                    Ok(Ok(stats)) if stats.failed.is_empty() => {
                        this.toast(&format!("Sent “{title}”. Eject your {name} to add it."))
                    }
                    Ok(Ok(stats)) => this.error(
                        "Couldn’t Send the Book",
                        stats
                            .failed
                            .iter()
                            .map(|(_, why)| why.as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Ok(Err(err)) => this.error("Couldn’t Send the Book", err),
                    Err(_) => this.error("Couldn’t Send the Book", "Sending stopped unexpectedly."),
                }
            });
        });
        dialog.present(Some(&self.win));
    }
}
