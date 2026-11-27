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

    /// Adds books to the library in a background thread (copying them into
    /// the books folder), then shows what happened. Folders are searched for
    /// EPUBs; other files are counted as skipped.
    pub(crate) fn add_book_files(self: &Rc<Self>, paths: &[std::path::PathBuf]) {
        let (books, skipped) = collect_books(paths);
        if books.is_empty() {
            return self.toast(if skipped > 0 {
                "Only EPUB books can be added"
            } else {
                "No books to add"
            });
        }
        if self.adding_books.get() {
            return self.toast("Still adding books. Add more when it’s done.");
        }
        let Some(db) = self.lib.borrow().path().map(Path::to_path_buf) else {
            return;
        };
        self.adding_books.set(true);
        let total = books.len();
        self.start_progress(&format!("Adding {}…", plural(total, "book", "books")));
        let done = std::sync::Arc::new(AtomicUsize::new(0));
        let done_view = done.clone();
        self.tick_while_adding(move || {
            let n = done_view.load(Ordering::Relaxed);
            Some(format!("Adding books… {n} of {total}"))
        });
        let this = self.clone();
        let job_done = done.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                let lib = hoard_core::Library::open(&db)?;
                let (mut added, mut known, mut failed) = (0usize, 0usize, Vec::new());
                for (i, path) in books.iter().enumerate() {
                    job_done.store(i, Ordering::Relaxed);
                    match lib.add_file(path, AddMode::Copy) {
                        Ok(a) if a.outcome == AddOutcome::AlreadyInLibrary => known += 1,
                        Ok(_) => added += 1,
                        Err(err) => failed.push(format!(
                            "{}: {err}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        )),
                    }
                }
                hoard_core::Result::Ok((added, known, failed))
            })
            .await;
            this.adding_books.set(false);
            this.set_progress(None);
            let (added, known, failed) = match result {
                Ok(Ok(r)) => r,
                Ok(Err(err)) => return this.error("Couldn’t Add Books", err),
                Err(_) => return this.error("Couldn’t Add Books", "Adding stopped unexpectedly."),
            };
            this.rebuild_sidebar();
            this.reload();
            this.refresh_on_device();
            if !failed.is_empty() {
                this.error(
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
            if skipped > 0 {
                parts.push(format!(
                    "{} skipped (not EPUB)",
                    plural(skipped, "file", "files")
                ));
            }
            if !parts.is_empty() {
                this.toast(&parts.join(" · "));
            }
        });
    }

    /// Asks for a Calibre library folder and imports it in the background.
    pub(super) fn import_calibre(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose Your Calibre Library")
            .accept_label("Import")
            .modal(true)
            .build();
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        if let Some(found) = hoard_core::calibre::find_libraries(&home).first() {
            dialog.set_initial_folder(Some(&gio::File::for_path(found)));
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Ok(folder) = dialog.select_folder_future(Some(&this.win)).await else {
                return;
            };
            let Some(path) = folder.path() else { return };
            this.import_calibre_from(path);
        });
    }

    /// Imports the Calibre library at `path` in the background, with a
    /// running count and the Library filling in as books arrive. Books are
    /// hard-linked where they can be (same drive: no extra space; Calibre's
    /// files aren't changed), copied otherwise.
    pub(crate) fn import_calibre_from(self: &Rc<Self>, path: PathBuf) {
        if !hoard_core::calibre::is_calibre_library(&path) {
            return self.error(
                "Not a Calibre Library",
                "Choose the folder that holds Calibre’s metadata.db (usually “Calibre Library” in your home folder).",
            );
        }
        if self.adding_books.get() {
            return self.toast("Still adding books. Import when it’s done.");
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Some(db) = this.lib.borrow().path().map(Path::to_path_buf) else {
                return;
            };
            let progress = std::sync::Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
            this.adding_books.set(true);
            this.start_progress("Importing your Calibre library…");
            {
                let progress = progress.clone();
                this.tick_while_adding(move || {
                    let (done, total) = (
                        progress.0.load(Ordering::Relaxed),
                        progress.1.load(Ordering::Relaxed),
                    );
                    (total > 0).then(|| format!("Importing from Calibre… {done} of {total} books"))
                });
            }
            let job_progress = progress.clone();
            let result = gio::spawn_blocking(move || {
                let lib = hoard_core::Library::open(&db)?;
                lib.import_calibre(&path, AddMode::Link, |done, total| {
                    job_progress.0.store(done, Ordering::Relaxed);
                    job_progress.1.store(total, Ordering::Relaxed);
                })
            })
            .await;
            this.adding_books.set(false);
            this.set_progress(None);
            this.rebuild_sidebar();
            this.reload();
            this.refresh_on_device();
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

    /// Sends the chosen books to the connected Kobo: works out what would be
    /// written, shows it and asks, then sends in the background with a
    /// running count. Planning and sending both run off the UI thread.
    pub(super) fn send_books(self: &Rc<Self>, ids: Vec<i64>) {
        if ids.is_empty() {
            return;
        }
        if self.sending.get() {
            return self.toast("Still sending to your Kobo. Send more when it’s done.");
        }
        let Some(root) = self.kobo.borrow().as_ref().map(|k| k.root.clone()) else {
            return self.error(
                "No Kobo Connected",
                "Connect your Kobo with a USB cable to send books to it.",
            );
        };
        let Some(db) = self.lib.borrow().path().map(Path::to_path_buf) else {
            return;
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            // Two plans: sending only what's new, and also updating the books
            // already on the Kobo (offered when there are any).
            let plans = {
                let (db, root) = (db.clone(), root.clone());
                gio::spawn_blocking(move || {
                    let lib = hoard_core::Library::open(&db)?;
                    Ok::<_, hoard_core::Error>((
                        lib.plan_send(&root, &ids)?,
                        lib.plan_send_with(&root, &ids, true)?,
                    ))
                })
                .await
            };
            let plan = match plans {
                Ok(Ok(plans)) => plans,
                Ok(Err(err)) => return this.error("Couldn’t Send to the Kobo", err),
                Err(_) => {
                    return this.error(
                        "Couldn’t Send to the Kobo",
                        "Planning stopped unexpectedly.",
                    );
                }
            };
            this.confirm_send(plan.0, plan.1, db, root);
        });
    }

    /// Asks before sending: what goes, how, and what's left out. When some
    /// of the books are already on the Kobo, offers to update them too
    /// (`with_updates`): a fresh copy, with the library's cover and details,
    /// over the one there.
    fn confirm_send(
        self: &Rc<Self>,
        plan: hoard_core::store::send::SendPlan,
        with_updates: hoard_core::store::send::SendPlan,
        db: std::path::PathBuf,
        root: std::path::PathBuf,
    ) {
        use hoard_core::store::send::{ALREADY_ON_KOBO, SendAction};
        let name = self.kobo_name();
        let updates = with_updates.items.iter().filter(|i| i.replace).count();
        let others: Vec<_> = plan
            .skipped
            .iter()
            .filter(|s| !(updates > 0 && s.reason == ALREADY_ON_KOBO))
            .cloned()
            .collect();
        let skipped = summarize_skipped(&others);
        if plan.items.is_empty() && updates == 0 {
            return self.toast(&format!(
                "Nothing to send: {}",
                summarize_skipped(&plan.skipped)
            ));
        }
        let new = plan.items.len();
        let converted = plan
            .items
            .iter()
            .filter(|i| i.action == SendAction::Convert)
            .count();
        let copied = new - converted;
        let mut body = match plan.items.as_slice() {
            [] => String::new(),
            [one] => format!(
                "“{}” will be {} to {}.",
                one.title,
                if converted == 1 {
                    "converted for the Kobo and copied"
                } else {
                    "copied"
                },
                one.dest.display()
            ),
            items => {
                let mut how = Vec::new();
                if converted > 0 {
                    how.push(format!("{converted} converted for the Kobo"));
                }
                if copied > 0 {
                    how.push(format!("{copied} copied as they are"));
                }
                format!(
                    "{} ({}).",
                    plural(items.len(), "book", "books"),
                    how.join(", ")
                )
            }
        };
        if new > 0 {
            body.push_str(&format!(
                " {} in all.",
                glib::format_size(plan.total_size())
            ));
        }
        if updates > 0 {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            let which = match with_updates.items.iter().find(|i| i.replace) {
                Some(one) if updates == 1 => format!("“{}” is", one.title),
                _ => format!("{} are", plural(updates, "book", "books")),
            };
            body.push_str(&format!(
                "{which} already on your Kobo. Updating replaces {} with a fresh copy carrying your library’s cover and details. Highlights and reading position on the Kobo stay with the book.",
                if updates == 1 { "it" } else { "them" }
            ));
        }
        if !others.is_empty() {
            body.push_str(&format!("\n\nNot sent: {skipped}."));
        }
        body.push_str("\n\nWord Hoard backs up the Kobo’s database first and changes nothing else on it. Eject the Kobo afterwards and it adds the books.");
        let heading = match (new, updates) {
            (0, 1) => format!("Update a Book on Your {name}?"),
            (0, n) => format!("Update {n} Books on Your {name}?"),
            (1, _) => format!("Send a Book to Your {name}?"),
            (n, _) => format!("Send {n} Books to Your {name}?"),
        };
        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        dialog.add_response("cancel", "Cancel");
        let default = match (new, updates) {
            (0, _) => {
                dialog.add_response("update", "Update");
                "update"
            }
            (_, 0) => {
                dialog.add_response("send", "Send");
                "send"
            }
            _ => {
                dialog.add_response("update", "Send and Update");
                dialog.add_response("send", "Send New Only");
                "send"
            }
        };
        dialog.set_response_appearance(default, adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some(default));
        dialog.set_close_response("cancel");
        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            let chosen = match response {
                "send" => plan.clone(),
                "update" => with_updates.clone(),
                _ => return,
            };
            // The choice is made: leave select mode now, so more books can
            // be chosen while these go.
            this.set_selecting(false);
            this.apply_send(chosen, db.clone(), root.clone());
        });
        dialog.present(Some(&self.win));
    }

    fn apply_send(
        self: &Rc<Self>,
        plan: hoard_core::store::send::SendPlan,
        db: std::path::PathBuf,
        root: std::path::PathBuf,
    ) {
        // Backups belong to this computer, not the library folder.
        let backups = hoard_core::local_dir().join("backups/kobo");
        if self.sending.replace(true) {
            return self.toast("Still sending to your Kobo. Send more when it’s done.");
        }
        self.update_send_action();
        let name = self.kobo_name();
        let total = plan.items.len();
        self.start_progress(&format!("Sending {}…", plural(total, "book", "books")));
        let done = std::sync::Arc::new(AtomicUsize::new(0));
        let ticking = Rc::new(Cell::new(true));
        {
            let (this, done, ticking) = (self.clone(), done.clone(), ticking.clone());
            glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                if !ticking.get() {
                    return glib::ControlFlow::Break;
                }
                let n = done.load(Ordering::Relaxed);
                this.set_progress(Some(&format!("Sending to your {name}… {n} of {total}")));
                glib::ControlFlow::Continue
            });
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let job_done = done.clone();
            let result = gio::spawn_blocking(move || {
                let lib = hoard_core::Library::open(&db)?;
                let writer = hoard_core::kobo::writer::DeviceWriter::open(&root, &backups)?;
                lib.apply_send(&writer, &plan, |n, _| job_done.store(n, Ordering::Relaxed))
            })
            .await;
            ticking.set(false);
            this.set_progress(None);
            this.sending.set(false);
            this.update_send_action();
            match result {
                Ok(Ok(stats)) => {
                    if !stats.failed.is_empty() {
                        this.error(
                            &format!(
                                "Couldn’t Send {}",
                                plural(stats.failed.len(), "Book", "Books")
                            ),
                            stats
                                .failed
                                .iter()
                                .map(|(title, why)| format!("{title}: {why}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                    }
                    this.refresh_on_device();
                    if stats.sent > 0 {
                        this.toast(&format!(
                            "Sent {}. Eject your {name} to add them.",
                            plural(stats.sent, "book", "books")
                        ));
                    }
                }
                Ok(Err(err)) => this.error("Couldn’t Send to the Kobo", err),
                Err(_) => this.error("Couldn’t Send to the Kobo", "Sending stopped unexpectedly."),
            }
        });
    }
}

impl Window {
    /// While books are being added: shows `status()` as the progress text and
    /// refreshes the Library every couple of seconds, so books appear as
    /// they arrive. Stops once adding is done.
    fn tick_while_adding(self: &Rc<Self>, status: impl Fn() -> Option<String> + 'static) {
        let weak = Rc::downgrade(self);
        let mut ticks = 0u32;
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !this.adding_books.get() {
                return glib::ControlFlow::Break;
            }
            if let Some(text) = status() {
                this.set_progress(Some(&text));
            }
            ticks += 1;
            if ticks.is_multiple_of(7)
                && this.current.get() == Nav::Library
                && this.search_text().is_none()
            {
                this.refresh_library_books();
            }
            glib::ControlFlow::Continue
        });
    }
}

/// "2 already on the Kobo, 1 with no file" from the reasons books were skipped.
fn summarize_skipped(skipped: &[hoard_core::store::send::Skipped]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for s in skipped {
        match counts.iter_mut().find(|(r, _)| *r == s.reason) {
            Some((_, n)) => *n += 1,
            None => counts.push((&s.reason, 1)),
        }
    }
    match skipped {
        [one] => format!("“{}”, {}", one.title, one.reason),
        _ => counts
            .iter()
            .map(|(reason, n)| format!("{n} {reason}"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// The EPUBs among `paths`, looking inside folders (not hidden ones), and
/// how many other files were left out.
pub(crate) fn collect_books(paths: &[std::path::PathBuf]) -> (Vec<std::path::PathBuf>, usize) {
    fn is_epub(path: &Path) -> bool {
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("epub"))
    }
    fn walk(path: &Path, books: &mut Vec<std::path::PathBuf>, skipped: &mut usize, depth: usize) {
        if path.is_dir() {
            if depth > 8 {
                return;
            }
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            let mut children: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    !p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                })
                .collect();
            children.sort();
            for child in children {
                walk(&child, books, skipped, depth + 1);
            }
        } else if is_epub(path) {
            books.push(path.to_path_buf());
        } else if path.is_file() {
            *skipped += 1;
        }
    }
    let (mut books, mut skipped) = (Vec::new(), 0);
    for path in paths {
        walk(path, &mut books, &mut skipped, 0);
    }
    books.dedup();
    (books, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_epubs_from_files_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("Shelf/Author")).unwrap();
        std::fs::create_dir_all(d.join("Shelf/.hidden")).unwrap();
        for f in [
            "a.epub",
            "notes.txt",
            "Shelf/Author/b.EPUB",
            "Shelf/cover.jpg",
            "Shelf/.hidden/c.epub",
        ] {
            std::fs::write(d.join(f), b"").unwrap();
        }
        let (books, skipped) =
            collect_books(&[d.join("a.epub"), d.join("notes.txt"), d.join("Shelf")]);
        assert_eq!(books, [d.join("a.epub"), d.join("Shelf/Author/b.EPUB")]);
        assert_eq!(skipped, 2);
    }
}
