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
}
