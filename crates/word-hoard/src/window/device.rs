//! Importing, and noticing, importing from and ejecting a connected Kobo.

use super::*;

impl Window {
    pub(super) fn choose_import_folder(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose Your Kobo")
            .modal(true)
            .build();
        let media = PathBuf::from("/media").join(std::env::var("USER").unwrap_or_default());
        if media.is_dir() {
            dialog.set_initial_folder(Some(&gio::File::for_path(media)));
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(folder) = dialog.select_folder_future(Some(&this.win)).await
                && let Some(path) = folder.path()
            {
                this.import_from(path);
            }
        });
    }

    /// Imports from a Kobo mount point (or a folder/DB file) in the
    /// background: reads a copy of the database and copies covers and
    /// markup images, then merges everything into the library.
    pub(super) fn import_from(self: &Rc<Self>, path: PathBuf) {
        if self.importing.replace(true) {
            return;
        }
        let this = self.clone();
        let is_connected = self.kobo.borrow().as_ref().is_some_and(|k| k.root == path);
        let path_for_details = path.clone();
        if is_connected {
            self.set_banner(&format!("Importing from your {}…", self.kobo_name()), None);
        } else {
            self.show_toast(adw::Toast::builder().title("Importing…").timeout(0).build());
        }
        let assets_dir = self.lib.borrow().assets_dir();
        glib::spawn_future_local(async move {
            let read = gio::spawn_blocking(move || -> hoard_core::Result<_> {
                let device = DeviceInfo::identify(&path)?;
                let mut snapshot = KoboDb::open_copy(&find_kobo_db(&path)?)?.snapshot()?;
                if path.is_dir() {
                    hoard_core::kobo::notebook::add_notebook_pages(&path, &mut snapshot);
                    hoard_core::koreader::add_koreader(&path, &mut snapshot);
                    for (file, why) in &snapshot.koreader_unread {
                        eprintln!("word-hoard: couldn’t read KOReader’s {file}: {why}");
                    }
                }
                // Asset copying is best-effort; a failure never blocks the import.
                let assets = match (&assets_dir, path.is_dir()) {
                    (Some(dir), true) => {
                        copy_assets(&path, &snapshot, dir).map_err(|e| e.to_string())
                    }
                    _ => Ok(CopiedAssets::default()),
                };
                // Context sentences come from the books themselves.
                let (contexts, markup_words) = if path.is_dir() {
                    (
                        find_word_contexts(&path, &snapshot, 5),
                        markup_contexts(&path, &snapshot),
                    )
                } else {
                    (Vec::new(), Vec::new())
                };
                Ok((device, snapshot, assets, contexts, markup_words))
            })
            .await;
            this.importing.set(false);
            if let Some(toast) = this.last_toast.borrow().as_ref() {
                toast.dismiss();
            }
            let outcome = match read {
                Ok(Ok((device, snapshot, assets, contexts, markup_words))) => {
                    let mut lib = this.lib.borrow_mut();
                    lib.import(&snapshot, &device, false)
                        .and_then(|stats| {
                            if let Ok(assets) = &assets {
                                lib.attach_assets(&device, assets)?;
                            }
                            lib.set_word_contexts(&device, &contexts)?;
                            lib.set_markup_contexts(&device, &markup_words)?;
                            if is_connected {
                                lib.forget_missing_copies(&path_for_details)?;
                            }
                            Ok((stats, assets.err(), device, snapshot.db_version))
                        })
                        .map_err(|e| e.to_string())
                }
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("The import stopped unexpectedly.".to_owned()),
            };
            match outcome {
                Ok((stats, asset_error, device, db_version)) => {
                    this.enrich_vocab();
                    this.rebuild_sidebar();
                    this.reload();
                    this.refresh_on_device();
                    this.import_toast(&stats);
                    this.warn_if_untested(&device, db_version);
                    this.auto_sync_obsidian();
                    this.transcribe_pending();
                    if is_connected {
                        this.update_kobo_details(path_for_details.clone());
                    }
                    if let Some(err) = asset_error {
                        this.toast(&format!("Some images couldn’t be copied: {err}"));
                    }
                }
                Err(err) => this.error("Import Failed", err),
            }
            if is_connected {
                this.show_connected_banner();
            }
        });
    }

    /// Imports from Kobo database versions Word Hoard hasn't been tested with
    /// go ahead, with a one-time toast (per version) asking for a report.
    /// It's queued behind the import summary rather than replacing it.
    fn warn_if_untested(self: &Rc<Self>, device: &DeviceInfo, db_version: i64) {
        const KEY: &str = "warned_db_version";
        let lib = self.lib.borrow();
        if is_tested_db_version(db_version)
            || lib.setting(KEY).ok().flatten() == Some(db_version.to_string())
        {
            return;
        }
        let _ = lib.set_setting(KEY, &db_version.to_string());
        let toast = adw::Toast::builder()
            .title(format!(
                "This Kobo’s database (version {db_version}) hasn’t been tested yet. Please check your highlights."
            ))
            .button_label("Report")
            .timeout(20)
            .build();
        let url = compatibility_report_url(device, db_version);
        let weak = Rc::downgrade(self);
        toast.connect_button_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                gtk::UriLauncher::new(&url).launch(Some(&this.win), gio::Cancellable::NONE, |_| {});
            }
        });
        self.toasts.add_toast(toast);
    }

    // ---- Device ------------------------------------------------------------

    pub(super) fn kobo_name(&self) -> &'static str {
        self.kobo.borrow().as_ref().map_or("Kobo", |k| k.name)
    }

    fn set_banner(&self, title: &str, action: Option<BannerAction>) {
        self.banner.set_title(title);
        self.banner.set_button_label(action.map(|a| match a {
            BannerAction::Import => "Import",
            BannerAction::Eject => "Eject",
        }));
        if let Some(action) = action {
            self.banner_action.set(action);
        }
        self.banner.set_revealed(true);
    }

    fn show_connected_banner(&self) {
        let title = format!("Your {} is connected", self.kobo_name());
        self.set_banner(&title, Some(BannerAction::Eject));
    }

    pub(super) fn on_connect_setting(&self) -> OnConnect {
        OnConnect::from_key(
            self.lib
                .borrow()
                .setting("on_connect")
                .ok()
                .flatten()
                .as_deref(),
        )
    }

    pub(super) fn setup_device_monitor(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.banner.connect_button_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            match this.banner_action.get() {
                BannerAction::Import => {
                    if let Some(root) = this.kobo.borrow().as_ref().map(|k| k.root.clone()) {
                        this.import_from(root);
                    }
                }
                BannerAction::Eject => this.eject(),
            }
        });

        // A Kobo that's plugged in but not mounted (KDE, for one, doesn't
        // mount drives until they're opened) is mounted here, as the desktop
        // would, so it can be imported from and sent to.
        let weak = Rc::downgrade(self);
        self.monitor.connect_volume_added(move |_, volume| {
            if let Some(this) = weak.upgrade() {
                this.mount_kobo_volume(volume);
            }
        });
        for volume in self.monitor.volumes() {
            self.mount_kobo_volume(&volume);
        }
        // Without gvfs (KDE, for one) GIO doesn't see unmounted drives at
        // all, so UDisks2 is asked directly as well.
        self.watch_udisks();

        let weak = Rc::downgrade(self);
        self.monitor.connect_mount_added(move |_, mount| {
            if let Some(this) = weak.upgrade() {
                this.mount_added(mount);
            }
        });
        let weak = Rc::downgrade(self);
        self.monitor.connect_mount_removed(move |_, mount| {
            let Some(this) = weak.upgrade() else { return };
            let root = mount.root().path();
            let ours = this
                .kobo
                .borrow()
                .as_ref()
                .is_some_and(|k| Some(&k.root) == root.as_ref());
            if ours {
                this.kobo.replace(None);
                this.update_send_action();
                this.refresh_on_device();
                this.banner.set_revealed(false);
            }
        });
        for mount in self.monitor.mounts() {
            self.mount_added(&mount);
        }
    }

    /// Mounts `volume` if it's an unmounted Kobo (every Kobo's storage is
    /// labelled KOBOeReader). Mounting it reports it through `mount_added`.
    fn mount_kobo_volume(self: &Rc<Self>, volume: &gio::Volume) {
        if volume.get_mount().is_some() {
            return;
        }
        let label = volume
            .identifier(gio::VOLUME_IDENTIFIER_KIND_LABEL)
            .map(|l| l.to_string())
            .unwrap_or_else(|| volume.name().to_string());
        if !label.eq_ignore_ascii_case("KOBOeReader") {
            return;
        }
        let this = self.clone();
        let volume = volume.clone();
        glib::spawn_future_local(async move {
            if let Err(err) = volume
                .mount_future(gio::MountMountFlags::NONE, None::<&gio::MountOperation>)
                .await
            {
                eprintln!("word-hoard: couldn’t mount the Kobo: {err}");
                this.toast("Your Kobo is plugged in, but couldn’t be opened. Open it in your file manager.");
            }
        });
    }

    /// Mounts unmounted Kobos through UDisks2, now and whenever a drive is
    /// added. Mounting one reports it through `mount_added`.
    fn watch_udisks(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Ok(bus) = gio::bus_get_future(gio::BusType::System).await else {
                return;
            };
            mount_udisks_kobos(&bus).await;
            let weak = Rc::downgrade(&this);
            let subscription = bus.subscribe_to_signal(
                Some("org.freedesktop.UDisks2"),
                Some("org.freedesktop.DBus.ObjectManager"),
                Some("InterfacesAdded"),
                Some("/org/freedesktop/UDisks2"),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    if weak.upgrade().is_none() {
                        return;
                    }
                    let bus = signal.connection.clone();
                    glib::spawn_future_local(async move {
                        // Give the new drive a moment to be probed for its label.
                        glib::timeout_future(std::time::Duration::from_millis(500)).await;
                        mount_udisks_kobos(&bus).await;
                    });
                },
            );
            // Keep watching for as long as the window is open.
            this.win.connect_destroy(move |_| {
                let _ = &subscription;
            });
        });
    }

    fn mount_added(self: &Rc<Self>, mount: &gio::Mount) {
        let Some(root) = mount.root().path() else {
            return;
        };
        if !is_kobo_mount(&root) || self.kobo.borrow().is_some() {
            return;
        }
        let name = DeviceInfo::read(&root).map_or("Kobo", |info| info.model_name());
        self.kobo.replace(Some(Connected {
            mount: mount.clone(),
            root: root.clone(),
            name,
        }));
        self.update_send_action();
        self.refresh_on_device();
        match self.on_connect_setting() {
            OnConnect::Import => self.import_from(root),
            OnConnect::Ask => self.set_banner(
                &format!("Your {name} is connected"),
                Some(BannerAction::Import),
            ),
            OnConnect::Nothing => {}
        }
    }

    pub(super) fn eject(self: &Rc<Self>) {
        if self.importing.get() {
            return;
        }
        if self.sending.get() {
            return self.toast(
                "Word Hoard is still sending books to your Kobo. Eject it when that’s done.",
            );
        }
        let Some((mount, root)) = self
            .kobo
            .borrow()
            .as_ref()
            .map(|k| (k.mount.clone(), k.root.clone()))
        else {
            return;
        };
        let name = self.kobo_name();
        self.set_banner(&format!("Ejecting your {name}…"), None);
        let this = self.clone();
        glib::spawn_future_local(async move {
            // UDisks2 first: GIO without gvfs runs `umount`, which a Flatpak
            // doesn't have.
            // UDisks2's own error is shown when it fails; GIO is only tried
            // when UDisks2 can't be reached or doesn't know the mount.
            let result = match udisks_eject(&root).await {
                Ok(Some(())) => Ok(()),
                Ok(None) => this.gio_eject(&mount).await,
                Err(err) => Err(err),
            };
            match result {
                Ok(()) => {
                    this.kobo.replace(None);
                    this.update_send_action();
                    this.refresh_on_device();
                    this.banner.set_revealed(false);
                    this.toast(&format!("You can unplug your {name}"));
                }
                Err(err) => {
                    this.show_connected_banner();
                    this.error("Couldn’t Eject", err.message());
                }
            }
        });
    }

    async fn gio_eject(&self, mount: &gio::Mount) -> Result<(), glib::Error> {
        {
            let operation = gtk::MountOperation::new(Some(&self.win));
            if mount.can_eject() {
                mount
                    .eject_with_operation_future(gio::MountUnmountFlags::NONE, Some(&operation))
                    .await
            } else {
                mount
                    .unmount_with_operation_future(gio::MountUnmountFlags::NONE, Some(&operation))
                    .await
            }
        }
    }

    fn import_toast(self: &Rc<Self>, s: &ImportStats) {
        let (title, target) = import_summary(s);
        let toast = adw::Toast::builder().title(title).timeout(6).build();
        if let Some((label, view)) = target {
            toast.set_button_label(Some(label));
            let weak = Rc::downgrade(self);
            toast.connect_button_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.select_nav(Nav::Annotations(view));
                }
            });
        }
        self.show_toast(toast);
    }
}

/// A new "Kobo compatibility" issue with the device details filled in.
fn compatibility_report_url(device: &DeviceInfo, db_version: i64) -> String {
    let esc = |s: &str| glib::Uri::escape_string(s, None, false).to_string();
    let model = match device.model_name() {
        "Kobo" => device.model_id.clone().unwrap_or_default(),
        name => name.to_owned(),
    };
    format!(
        "{ISSUES_URL}/new?template=compatibility.yml&model={}&firmware={}&db_version={db_version}",
        esc(&model),
        esc(device.firmware.as_deref().unwrap_or("")),
    )
}

const ISSUES_URL: &str = "https://github.com/andrew-lawlor/word-hoard/issues";

/// The import toast's text, and which view its button opens (if any).
fn import_summary(s: &ImportStats) -> (String, Option<(&'static str, View)>) {
    let mut parts = Vec::new();
    if s.annotations_new > 0 {
        parts.push(plural(s.annotations_new, "new highlight", "new highlights"));
    }
    if s.words_new > 0 {
        parts.push(plural(s.words_new, "new word", "new words"));
    }
    if s.annotations_updated > 0 {
        parts.push(format!("{} updated", s.annotations_updated));
    }
    if s.annotations_removed > 0 {
        parts.push(if s.annotations_trashed == s.annotations_removed {
            format!("{} deleted on Kobo, moved to Trash", s.annotations_removed)
        } else {
            format!("{} deleted on Kobo", s.annotations_removed)
        });
    }
    if s.annotations_restored > 0 {
        parts.push(format!("{} back on Kobo", s.annotations_restored));
    }
    let title = if parts.is_empty() {
        "Nothing new on your Kobo".to_owned()
    } else {
        parts.join(" · ")
    };
    // New highlights are the most useful thing to jump to; otherwise show
    // where the deleted ones went.
    let target = if s.annotations_new > 0 {
        Some(("Review", View::Inbox))
    } else if s.annotations_removed > s.annotations_trashed {
        Some(("Show", View::RemovedOnDevice))
    } else if s.annotations_trashed > 0 {
        Some(("Show", View::Trash))
    } else {
        None
    };
    (title, target)
}

const UDISKS: &str = "org.freedesktop.UDisks2";

/// A filesystem UDisks2 knows about.
struct UdisksFilesystem {
    path: String,
    label: Option<String>,
    mount_points: Vec<PathBuf>,
    /// The drive it's on (`/` when none).
    drive: Option<String>,
}

/// Every filesystem UDisks2 manages, with its label, mount points and drive.
async fn udisks_filesystems(
    bus: &gio::DBusConnection,
) -> Result<Vec<UdisksFilesystem>, glib::Error> {
    let reply = bus
        .call_future(
            Some(UDISKS),
            "/org/freedesktop/UDisks2",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            None,
            Some(glib::VariantTy::new("(a{oa{sa{sv}}})").expect("valid type")),
            gio::DBusCallFlags::NONE,
            5000,
        )
        .await?;
    let objects = reply.child_value(0);
    let mut out = Vec::new();
    for i in 0..objects.n_children() {
        let entry = objects.child_value(i);
        let Some(path) = entry.child_value(0).str().map(str::to_owned) else {
            continue;
        };
        // a{sa{sv}}: each interface's properties.
        let interfaces = entry.child_value(1);
        let properties = |name: &str| -> Option<glib::VariantDict> {
            (0..interfaces.n_children())
                .map(|j| interfaces.child_value(j))
                .find(|i| i.child_value(0).str() == Some(name))
                .map(|i| glib::VariantDict::new(Some(&i.child_value(1))))
        };
        let Some(filesystem) = properties("org.freedesktop.UDisks2.Filesystem") else {
            continue;
        };
        let block = properties("org.freedesktop.UDisks2.Block");
        // MountPoints is aay: NUL-terminated byte strings.
        let mount_points = filesystem
            .lookup_value("MountPoints", None)
            .map(|points| {
                (0..points.n_children())
                    .map(|k| {
                        let bytes = points.child_value(k).data().to_vec();
                        let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes).to_vec();
                        PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(UdisksFilesystem {
            path,
            label: block
                .as_ref()
                .and_then(|b| b.lookup_value("IdLabel", None))
                .and_then(|l| l.str().map(str::to_owned)),
            mount_points,
            drive: block
                .as_ref()
                .and_then(|b| b.lookup_value("Drive", None))
                .and_then(|d| d.str().map(str::to_owned)),
        });
    }
    Ok(out)
}

/// Calls a UDisks2 method that takes only an options dictionary.
async fn udisks_call(
    bus: &gio::DBusConnection,
    path: &str,
    interface: &str,
    method: &str,
) -> Result<(), glib::Error> {
    let options = glib::VariantDict::new(None).end();
    bus.call_future(
        Some(UDISKS),
        path,
        interface,
        method,
        Some(&glib::Variant::tuple_from_iter([options])),
        None,
        gio::DBusCallFlags::NONE,
        30000,
    )
    .await
    .map(|_| ())
}

/// Asks UDisks2 to mount every filesystem labelled KOBOeReader that isn't
/// mounted yet (as the desktop would, read-write under /run/media).
async fn mount_udisks_kobos(bus: &gio::DBusConnection) {
    let Ok(filesystems) = udisks_filesystems(bus).await else {
        return;
    };
    for fs in filesystems {
        let kobo = fs
            .label
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case("KOBOeReader"));
        if !kobo || !fs.mount_points.is_empty() {
            continue;
        }
        if let Err(err) =
            udisks_call(bus, &fs.path, "org.freedesktop.UDisks2.Filesystem", "Mount").await
        {
            eprintln!("word-hoard: couldn’t mount the Kobo through UDisks2: {err}");
        }
    }
}

/// Ejects the drive mounted at `root` through UDisks2: unmounts it, then
/// powers it off so the Kobo knows it's free. `Ok(None)` when UDisks2 can't
/// be reached or doesn't know the mount (then GIO is tried instead).
async fn udisks_eject(root: &std::path::Path) -> Result<Option<()>, glib::Error> {
    let Ok(bus) = gio::bus_get_future(gio::BusType::System).await else {
        return Ok(None);
    };
    let target = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let Ok(filesystems) = udisks_filesystems(&bus).await else {
        return Ok(None);
    };
    let Some(fs) = filesystems
        .into_iter()
        .find(|fs| fs.mount_points.iter().any(|m| *m == target || m == root))
    else {
        return Ok(None);
    };
    udisks_call(
        &bus,
        &fs.path,
        "org.freedesktop.UDisks2.Filesystem",
        "Unmount",
    )
    .await?;
    if let Some(drive) = fs.drive.filter(|d| d != "/") {
        // Not every drive can be powered off; it's unmounted either way.
        if let Err(err) =
            udisks_call(&bus, &drive, "org.freedesktop.UDisks2.Drive", "PowerOff").await
        {
            eprintln!("word-hoard: couldn’t power off the Kobo: {err}");
        }
    }
    Ok(Some(()))
}

/// Setting: "1" turns off keeping book details in step on the Kobo.
pub(super) const KOBO_DETAILS_OFF: &str = "kobo_details_off";

impl Window {
    /// Brings the details the connected Kobo shows for books from the library
    /// into step with it (title, authors, series, description, publisher), in
    /// the background, unless turned off in Preferences. The Kobo's database
    /// is backed up first, and only when something changes.
    fn update_kobo_details(self: &Rc<Self>, mount: PathBuf) {
        if self.flag(KOBO_DETAILS_OFF) || self.sending.get() {
            self.sync_positions();
            return;
        }
        let Some(db) = self.lib.borrow().path().map(std::path::Path::to_path_buf) else {
            return;
        };
        let backups = hoard_core::local_dir().join("backups/kobo");
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                hoard_core::Library::open(&db)?.update_kobo_details(&mount, &backups)
            })
            .await;
            match result {
                Ok(Ok(0)) => {}
                Ok(Ok(n)) => this.toast(&format!(
                    "Updated the details of {} on your {}",
                    plural(n, "book", "books"),
                    this.kobo_name()
                )),
                Ok(Err(hoard_core::Error::UntestedKoboDb(_))) => {}
                Ok(Err(err)) => this.error("Couldn’t Update Book Details on the Kobo", err),
                Err(_) => {}
            }
            // Then the reading position, one write to the Kobo at a time.
            this.sync_positions();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_report_url_prefills_the_form() {
        let device = DeviceInfo::parse(
            "N0000000000001,4.20.14622,4.45.23697,4.20.14622,4.20.14622,00000000-0000-0000-0000-000000000390",
        )
        .unwrap();
        assert_eq!(
            compatibility_report_url(&device, 180),
            "https://github.com/andrew-lawlor/word-hoard/issues/new?template=compatibility.yml\
             &model=Kobo%20Libra%20Colour&firmware=4.45.23697&db_version=180"
        );
    }

    #[test]
    fn import_summary_mentions_deletions() {
        let s = ImportStats {
            annotations_removed: 2,
            ..Default::default()
        };
        assert_eq!(
            import_summary(&s),
            (
                "2 deleted on Kobo".to_owned(),
                Some(("Show", View::RemovedOnDevice))
            )
        );

        let s = ImportStats {
            annotations_removed: 1,
            annotations_trashed: 1,
            ..Default::default()
        };
        assert_eq!(
            import_summary(&s),
            (
                "1 deleted on Kobo, moved to Trash".to_owned(),
                Some(("Show", View::Trash))
            )
        );

        let s = ImportStats {
            annotations_new: 3,
            annotations_removed: 1,
            annotations_restored: 1,
            ..Default::default()
        };
        assert_eq!(
            import_summary(&s),
            (
                "3 new highlights · 1 deleted on Kobo · 1 back on Kobo".to_owned(),
                Some(("Review", View::Inbox))
            )
        );

        assert_eq!(
            import_summary(&ImportStats::default()),
            ("Nothing new on your Kobo".to_owned(), None)
        );
    }
}
