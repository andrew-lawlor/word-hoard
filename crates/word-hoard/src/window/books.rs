//! Adding EPUB files to the Library.

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
}
