//! Foliate on this computer: its highlights and progress are imported when
//! Word Hoard starts and whenever Foliate saves (a watch on its data folder),
//! unless turned off in Preferences.

use super::*;

/// Setting: "1" turns off importing from Foliate.
pub(super) const FOLIATE_OFF: &str = "foliate_off";

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Foliate's data folders here, with how it's installed.
pub(super) fn foliate_dirs() -> Vec<(PathBuf, &'static str)> {
    hoard_core::foliate::data_dirs(&home())
}

impl Window {
    /// Imports from Foliate now and watches for more (or stops watching,
    /// when turned off).
    pub(super) fn start_foliate(self: &Rc<Self>) {
        self.foliate_monitors.borrow_mut().clear();
        if self.flag(FOLIATE_OFF) {
            return;
        }
        for (dir, kind) in foliate_dirs() {
            self.import_foliate(dir.clone(), kind);
            let Ok(monitor) = gio::File::for_path(&dir)
                .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
            else {
                continue;
            };
            let weak = Rc::downgrade(self);
            monitor.connect_changed(move |_, file, _, event| {
                let Some(this) = weak.upgrade() else { return };
                let is_book = file
                    .path()
                    .is_some_and(|p| p.extension().is_some_and(|e| e == "json"));
                let saved = matches!(
                    event,
                    gio::FileMonitorEvent::ChangesDoneHint
                        | gio::FileMonitorEvent::Created
                        | gio::FileMonitorEvent::Deleted
                        | gio::FileMonitorEvent::MovedIn
                        | gio::FileMonitorEvent::Renamed
                );
                if is_book && saved {
                    this.import_foliate_soon(dir.clone(), kind);
                }
            });
            self.foliate_monitors.borrow_mut().push(monitor);
        }
    }

    /// Foliate saves often while you read: import once things settle.
    fn import_foliate_soon(self: &Rc<Self>, dir: PathBuf, kind: &'static str) {
        if let Some(pending) = self.foliate_pending.borrow_mut().take() {
            pending.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
            if let Some(this) = weak.upgrade() {
                this.foliate_pending.borrow_mut().take();
                this.import_foliate(dir, kind);
            }
        });
        *self.foliate_pending.borrow_mut() = Some(id);
    }

    fn import_foliate(self: &Rc<Self>, dir: PathBuf, kind: &'static str) {
        let Some(db) = self.lib.borrow().path().map(std::path::Path::to_path_buf) else {
            return;
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                hoard_core::Library::open(&db)?.import_foliate(&dir, kind)
            })
            .await;
            match result {
                Ok(Ok(stats)) if !stats.is_empty() => {
                    this.rebuild_sidebar();
                    if this.current.get() == Nav::Library {
                        this.refresh_library_books();
                    } else {
                        this.reload();
                    }
                    if stats.annotations_new > 0 {
                        this.toast(&format!(
                            "{} from Foliate",
                            plural(stats.annotations_new, "new highlight", "new highlights")
                        ));
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(err)) => eprintln!("word-hoard: couldn’t import from Foliate: {err}"),
                Err(_) => {}
            }
        });
    }
}
