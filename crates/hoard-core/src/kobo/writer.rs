//! The only code in Word Hoard that writes to a Kobo (Word Hoard SPEC,
//! "Writing to the device"). Everything else goes through
//! [`super::ensure_not_on_kobo`], which refuses any path on a device.
//!
//! A [`DeviceWriter`] exists only while a send the user approved is being
//! applied. Opening one backs up the Kobo's database first. It can copy a
//! book file into the Kobo's storage, and nothing else: no database writes
//! (the Kobo indexes new books itself when it's ejected), no deletes, no
//! overwrites, nothing under `.kobo` or other hidden folders.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use chrono::Utc;

use super::device::{DB_RELATIVE_PATH, DeviceInfo, is_kobo_mount};
use crate::{Error, Result};

pub struct DeviceWriter {
    mount: PathBuf,
    info: DeviceInfo,
    backup: PathBuf,
}

/// Whether `/proc/self/mounts` lists `mount` as read-only. Unknown mounts
/// (not in the table, such as a folder used in tests) count as writable.
fn mounted_read_only(mount: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    let target = mount.canonicalize().unwrap_or_else(|_| mount.to_path_buf());
    mounts.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.len() >= 4
            && Path::new(&super::device::unescape_mount(fields[1])) == target
            && fields[3].split(',').any(|o| o == "ro")
    })
}

impl DeviceWriter {
    /// Opens the Kobo mounted at `mount` for writing, after copying its
    /// database (and WAL, if any) to `backups/<serial>/<time>/`.
    pub fn open(mount: &Path, backups: &Path) -> Result<Self> {
        if !is_kobo_mount(mount) {
            return Err(Error::NotAKobo(mount.to_path_buf()));
        }
        if mounted_read_only(mount) {
            return Err(Error::ReadOnlyKobo(mount.to_path_buf()));
        }
        let info = DeviceInfo::identify(mount)?;
        let serial: String = info
            .serial
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let backup = backups
            .join(serial)
            .join(Utc::now().format("%Y-%m-%dT%H-%M-%S").to_string());
        super::ensure_not_on_kobo(&backup)?;
        std::fs::create_dir_all(&backup)?;
        let db = mount.join(DB_RELATIVE_PATH);
        std::fs::copy(&db, backup.join("KoboReader.sqlite"))?;
        let wal = db.with_extension("sqlite-wal");
        if wal.is_file() {
            std::fs::copy(&wal, backup.join("KoboReader.sqlite-wal"))?;
        }
        Ok(Self {
            mount: mount.to_path_buf(),
            info,
            backup,
        })
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.info
    }

    /// Where the database was backed up to.
    pub fn backup(&self) -> &Path {
        &self.backup
    }

    /// The Kobo's own name for a file at `relative`, as its database and
    /// Word Hoard's `book_source` record it.
    pub fn volume_id(relative: &Path) -> String {
        format!("file:///mnt/onboard/{}", relative.to_string_lossy())
    }

    /// Whether a file already exists at `relative` on the Kobo.
    pub fn exists(&self, relative: &Path) -> Result<bool> {
        Ok(self.mount.join(check_relative(relative)?).exists())
    }

    /// Copies `src` to `relative` on the Kobo, through a temporary name and a
    /// rename, so an unplugged cable never leaves half a book behind.
    pub fn copy_book(&self, src: &Path, relative: &Path) -> Result<PathBuf> {
        self.write_book(src, relative, false)
    }

    /// [`Self::copy_book`], replacing the book file already at `relative`
    /// (re-sending a book whose cover or details changed). Only a book file
    /// (`.epub`, `.kepub.epub`, `.pdf`) is ever replaced.
    pub fn replace_book(&self, src: &Path, relative: &Path) -> Result<PathBuf> {
        self.write_book(src, relative, true)
    }

    fn write_book(&self, src: &Path, relative: &Path, replace: bool) -> Result<PathBuf> {
        let relative = check_relative(relative)?;
        let dest = self.mount.join(relative);
        let is_book = {
            let name = relative.to_string_lossy().to_ascii_lowercase();
            [".epub", ".pdf"].iter().any(|e| name.ends_with(e))
        };
        if dest.exists() && !(replace && is_book && dest.is_file()) {
            return Err(Error::AlreadyOnKobo(dest));
        }
        let dir = dest.parent().unwrap_or(&self.mount);
        std::fs::create_dir_all(dir)?;
        let partial = dir.join(format!(
            ".{}.word-hoard-partial",
            dest.file_name().unwrap_or_default().to_string_lossy()
        ));
        let written = (|| -> std::io::Result<()> {
            let mut input = std::fs::File::open(src)?;
            let mut output = std::fs::File::create(&partial)?;
            std::io::copy(&mut input, &mut output)?;
            output.flush()?;
            output.sync_all()?;
            std::fs::rename(&partial, &dest)
        })();
        if let Err(err) = written {
            let _ = std::fs::remove_file(&partial);
            return Err(err.into());
        }
        Ok(dest)
    }
}

/// A relative path inside the Kobo's storage: no `..`, no root, no hidden
/// folders (`.kobo`, `.adds`, …), so a book can't land on the device's own
/// files.
fn check_relative(relative: &Path) -> Result<&Path> {
    let ok = relative.components().all(|c| match c {
        Component::Normal(part) => !part.to_string_lossy().starts_with('.'),
        _ => false,
    }) && relative.components().next().is_some();
    if ok {
        Ok(relative)
    } else {
        Err(Error::BadKoboPath(relative.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_kobo(root: &Path) {
        std::fs::create_dir_all(root.join(".kobo")).unwrap();
        std::fs::write(root.join(DB_RELATIVE_PATH), b"database").unwrap();
        std::fs::write(
            root.join(".kobo/version"),
            "N000000000123,4.9.77,4.45.23697,4.9.77,4.9.77,00000000-0000-0000-0000-000000000390",
        )
        .unwrap();
    }

    #[test]
    fn backs_up_then_copies_books_only_where_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let kobo = dir.path().join("KOBOeReader");
        fake_kobo(&kobo);
        let backups = dir.path().join("backups");
        let writer = DeviceWriter::open(&kobo, &backups).unwrap();
        assert!(writer.backup().starts_with(backups.join("N000000000123")));
        assert_eq!(
            std::fs::read(writer.backup().join("KoboReader.sqlite")).unwrap(),
            b"database"
        );

        let book = dir.path().join("book.kepub.epub");
        std::fs::write(&book, b"kepub").unwrap();
        let rel = Path::new("Books/Ursula K. Le Guin/The Dispossessed.kepub.epub");
        let dest = writer.copy_book(&book, rel).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"kepub");
        assert!(writer.exists(rel).unwrap());
        // No partial files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("partial"))
            .collect();
        assert!(leftovers.is_empty());

        // Never overwrites, never outside the storage, never on the Kobo's own files.
        assert!(matches!(
            writer.copy_book(&book, rel),
            Err(Error::AlreadyOnKobo(_))
        ));
        // Replacing a book file on purpose is allowed; nothing else is.
        std::fs::write(&book, b"kepub v2").unwrap();
        writer.replace_book(&book, rel).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"kepub v2");
        assert!(matches!(
            writer.replace_book(&book, Path::new(".kobo/KoboReader.sqlite")),
            Err(Error::BadKoboPath(_))
        ));
        std::fs::write(kobo.join("notes.txt"), b"mine").unwrap();
        assert!(matches!(
            writer.replace_book(&book, Path::new("notes.txt")),
            Err(Error::AlreadyOnKobo(_))
        ));
        for bad in [
            ".kobo/KoboReader.sqlite",
            "../outside.epub",
            "/abs.epub",
            "Books/.adds/x.epub",
            "",
        ] {
            assert!(
                matches!(
                    writer.copy_book(&book, Path::new(bad)),
                    Err(Error::BadKoboPath(_))
                ),
                "{bad}"
            );
        }
        assert_eq!(
            std::fs::read(kobo.join(DB_RELATIVE_PATH)).unwrap(),
            b"database"
        );
        assert_eq!(
            DeviceWriter::volume_id(rel),
            "file:///mnt/onboard/Books/Ursula K. Le Guin/The Dispossessed.kepub.epub"
        );
    }

    #[test]
    fn refuses_folders_that_are_not_kobos() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            DeviceWriter::open(dir.path(), &dir.path().join("b")),
            Err(Error::NotAKobo(_))
        ));
    }
}
