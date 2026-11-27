//! Where the library lives.
//!
//! Like a Calibre library, a Word Hoard library is one folder: the books, in
//! `Author/Title (Year)/` folders, and a hidden `.word-hoard` folder beside
//! them holding the database, covers and markup images. Moving, syncing or
//! backing up that folder takes the whole library with it.
//!
//! What belongs to this computer rather than the library stays in the app's
//! data folder ([`local_dir`]): Kobo database backups, handwriting models,
//! downloaded dictionaries, and a note of which library folder is open.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use crate::{Error, Result};

/// The hidden folder inside a library folder.
pub const LIBRARY_DATA_DIR: &str = ".word-hoard";
const DB_NAME: &str = "library.db";
/// In [`local_dir`]: the library folder that's open.
const POINTER: &str = "library-folder";
/// In [`local_dir`]: kept there when an old library is brought into its
/// library folder (they belong to this computer).
const LOCAL_ONLY: &[&str] = &["backups", "models", "dictionaries", POINTER];

/// `$XDG_DATA_HOME/word-hoard`: this computer's own files.
/// `WORD_HOARD_LOCAL_DIR` overrides it (development builds use the
/// Flatpak's, so both open the same library).
pub fn local_dir() -> PathBuf {
    std::env::var_os("WORD_HOARD_LOCAL_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| super::data_home().join("word-hoard"))
}

/// The database of the library in `folder`.
pub fn library_db(folder: &Path) -> PathBuf {
    folder.join(LIBRARY_DATA_DIR).join(DB_NAME)
}

/// Whether `folder` holds a Word Hoard library.
pub fn is_library(folder: &Path) -> bool {
    library_db(folder).is_file()
}

/// The library folder a database belongs to, when it's kept the usual way
/// (`<folder>/.word-hoard/library.db`).
pub fn folder_of(db: &Path) -> Option<&Path> {
    let data = db.parent()?;
    (data.file_name()? == LIBRARY_DATA_DIR).then(|| data.parent())?
}

/// The open library's database. The first time, that's the library in the
/// default books folder; a library from before libraries were folders (its
/// database in [`local_dir`]) is moved into its books folder first.
pub fn current_library() -> Result<PathBuf> {
    current_library_in(&local_dir(), super::books::default_books_root)
}

fn current_library_in(local: &Path, default_folder: impl FnOnce() -> PathBuf) -> Result<PathBuf> {
    if let Ok(folder) = std::fs::read_to_string(local.join(POINTER)) {
        let folder = PathBuf::from(folder.trim_end_matches('\n'));
        if folder.is_absolute() {
            return Ok(library_db(&folder));
        }
    }
    let old_db = local.join(DB_NAME);
    let folder = if old_db.is_file() {
        // Its books folder setting, if it had one.
        let conn = Connection::open(&old_db)?;
        let chosen: Option<String> = conn
            .query_row(
                "SELECT value FROM setting WHERE key = ?1",
                [super::BOOKS_ROOT_SETTING],
                |r| r.get(0),
            )
            .ok();
        drop(conn);
        let folder = chosen.map(PathBuf::from).unwrap_or_else(default_folder);
        adopt_old_library(local, &folder)?;
        folder
    } else {
        default_folder()
    };
    set_current_library_in(local, &folder)?;
    Ok(library_db(&folder))
}

/// Opens the library in `folder` from now on.
pub fn set_current_library(folder: &Path) -> Result<()> {
    set_current_library_in(&local_dir(), folder)
}

fn set_current_library_in(local: &Path, folder: &Path) -> Result<()> {
    crate::kobo::ensure_not_on_kobo(folder)?;
    std::fs::create_dir_all(local)?;
    let tmp = local.join(format!("{POINTER}.tmp"));
    std::fs::write(&tmp, format!("{}\n", folder.display()))?;
    std::fs::rename(tmp, local.join(POINTER))?;
    Ok(())
}

/// Moves a library kept in `local` (database, covers, markup images) into
/// `folder/.word-hoard`, leaving this computer's own files where they are.
fn adopt_old_library(local: &Path, folder: &Path) -> Result<()> {
    crate::kobo::ensure_not_on_kobo(folder)?;
    let data = folder.join(LIBRARY_DATA_DIR);
    if data.join(DB_NAME).exists() {
        return Err(Error::Library(format!(
            "{} already holds a library",
            folder.display()
        )));
    }
    checkpoint(&local.join(DB_NAME))?;
    std::fs::create_dir_all(&data)?;
    for entry in std::fs::read_dir(local)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if LOCAL_ONLY.contains(&name.as_ref()) || name.starts_with(DB_NAME) && name != DB_NAME {
            continue;
        }
        move_path(&entry.path(), &data.join(&*name))?;
    }
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(local.join(format!("{DB_NAME}{suffix}")));
    }
    rewrite_asset_paths(&data.join(DB_NAME), local, &data)
}

/// Moves the library whose database is `db` (kept the usual way, in its
/// library folder) to `new_folder`: its book files, then its `.word-hoard`
/// folder. Other files in the old folder are left alone. Nothing is
/// overwritten; if a book can't be moved, the ones already moved go back.
/// Returns the database's new path. The database must not be open.
pub fn move_library(db: &Path, new_folder: &Path) -> Result<PathBuf> {
    let old_folder = folder_of(db)
        .ok_or_else(|| Error::Library(format!("{} isn't in a library folder", db.display())))?
        .to_path_buf();
    crate::kobo::ensure_not_on_kobo(new_folder)?;
    std::fs::create_dir_all(new_folder)?;
    let (old_c, new_c) = (old_folder.canonicalize()?, new_folder.canonicalize()?);
    if old_c == new_c {
        return Ok(db.to_path_buf());
    }
    if new_c.starts_with(&old_c) || old_c.starts_with(&new_c) {
        return Err(Error::Library(
            "A library can’t be moved into its own folder, or into a folder that contains it"
                .into(),
        ));
    }
    if new_folder.join(LIBRARY_DATA_DIR).exists() {
        return Err(Error::Library(format!(
            "{} already holds a library",
            new_folder.display()
        )));
    }
    checkpoint(db)?;
    let files: Vec<String> = {
        let conn = Connection::open(db)?;
        let mut stmt = conn.prepare("SELECT path FROM book_file ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let files: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|rel| old_folder.join(rel).is_file())
        .collect();
    if let Some(clash) = files.iter().find(|rel| new_folder.join(rel).exists()) {
        return Err(Error::Library(format!(
            "{} already has a file at {clash}",
            new_folder.display()
        )));
    }
    let mut moved: Vec<&str> = Vec::new();
    for rel in &files {
        let (from, to) = (old_folder.join(rel), new_folder.join(rel));
        let result = to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(Error::from)
            .and_then(|()| move_path(&from, &to));
        if let Err(err) = result {
            for rel in moved.iter().rev() {
                let _ = move_path(&new_folder.join(rel), &old_folder.join(rel));
            }
            return Err(err);
        }
        moved.push(rel);
    }
    let (old_data, new_data) = (
        old_folder.join(LIBRARY_DATA_DIR),
        new_folder.join(LIBRARY_DATA_DIR),
    );
    if let Err(err) = move_path(&old_data, &new_data) {
        for rel in moved.iter().rev() {
            let _ = move_path(&new_folder.join(rel), &old_folder.join(rel));
        }
        return Err(err);
    }
    // Tidy the folders the books left empty.
    for rel in &files {
        let mut dir = old_folder.join(rel);
        while dir.pop() && dir != old_folder && std::fs::remove_dir(&dir).is_ok() {}
    }
    let new_db = new_data.join(DB_NAME);
    rewrite_asset_paths(&new_db, &old_data, &new_data)?;
    Ok(new_db)
}

/// Folds the write-ahead log into the database, so the file alone is
/// complete and can be moved.
fn checkpoint(db: &Path) -> Result<()> {
    let conn = Connection::open(db)?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    Ok(())
}

/// Updates the absolute paths of covers and markup images, which live in
/// the library's data folder, after it moved from `old` to `new`.
pub(crate) fn rewrite_asset_paths(db: &Path, old: &Path, new: &Path) -> Result<()> {
    let conn = Connection::open(db)?;
    let from = format!("{}/", old.display());
    let to = format!("{}/", new.display());
    for (table, column) in [
        ("book", "cover_path"),
        ("annotation", "markup_svg_path"),
        ("annotation", "markup_jpg_path"),
    ] {
        conn.execute(
            &format!(
                "UPDATE {table} SET {column} = ?2 || substr({column}, length(?1) + 1)
                 WHERE substr({column}, 1, length(?1)) = ?1"
            ),
            params![from, to],
        )?;
    }
    Ok(())
}

/// Renames, or copies and then deletes when `to` is on another drive.
fn move_path(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        return Err(Error::Library(format!("{} already exists", to.display())));
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_recursive(from, to).inspect_err(|_| {
        let _ = if to.is_dir() {
            std::fs::remove_dir_all(to)
        } else {
            std::fs::remove_file(to)
        };
    })?;
    if from.is_dir() {
        std::fs::remove_dir_all(from)?;
    } else {
        std::fs::remove_file(from)?;
    }
    Ok(())
}

fn copy_recursive(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)?.flatten() {
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to)?;
        std::fs::File::open(to)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Library;

    /// A library in `folder` with one book file, a cover and a markup image.
    fn make_library(folder: &Path) -> PathBuf {
        let db = library_db(folder);
        let lib = Library::open(&db).unwrap();
        let data = folder.join(LIBRARY_DATA_DIR);
        std::fs::create_dir_all(data.join("covers")).unwrap();
        std::fs::write(data.join("covers/book-1.jpg"), b"jpg").unwrap();
        std::fs::create_dir_all(folder.join("Ann Author/Book (2020)")).unwrap();
        std::fs::write(folder.join("Ann Author/Book (2020)/Book.epub"), b"epub").unwrap();
        std::fs::write(folder.join("unrelated.txt"), b"mine").unwrap();
        lib.conn
            .execute_batch(&format!(
                "INSERT INTO book (id, fingerprint, title, created_at, updated_at, cover_path) VALUES (1, 'f', 'Book', '2026-01-01', '2026-01-01', '{}/covers/book-1.jpg');
                 INSERT INTO book_file (book_id, path, format, size, hash, added_at)
                 VALUES (1, 'Ann Author/Book (2020)/Book.epub', 'epub', 4, 'h', '2026-01-01');",
                data.display()
            ))
            .unwrap();
        db
    }

    fn cover(db: &Path) -> String {
        Connection::open(db)
            .unwrap()
            .query_row("SELECT cover_path FROM book WHERE id = 1", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn moves_a_library_with_its_books_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("Books"), dir.path().join("Nextcloud/Books"));
        let db = make_library(&old);
        assert_eq!(folder_of(&db), Some(old.as_path()));

        let moved = move_library(&db, &new).unwrap();
        assert_eq!(moved, library_db(&new));
        assert!(is_library(&new) && !is_library(&old));
        assert!(new.join("Ann Author/Book (2020)/Book.epub").is_file());
        assert!(
            !old.join("Ann Author").exists(),
            "emptied folders are tidied"
        );
        assert!(old.join("unrelated.txt").is_file(), "other files stay");
        assert_eq!(
            cover(&moved),
            format!("{}/.word-hoard/covers/book-1.jpg", new.display())
        );
        assert!(std::path::Path::new(&cover(&moved)).is_file());
        Library::open(&moved).unwrap();

        // Not into a folder that already has a library, nor into itself.
        let other = dir.path().join("Other");
        make_library(&other);
        assert!(move_library(&moved, &other).is_err());
        assert!(move_library(&moved, &new.join("sub")).is_err());
        assert!(is_library(&new));
    }

    #[test]
    fn brings_an_old_library_into_its_books_folder() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("data/word-hoard");
        let books = dir.path().join("Documents/Books");
        // The old layout: the database and covers in the data folder.
        std::fs::create_dir_all(local.join("covers")).unwrap();
        std::fs::create_dir_all(local.join("backups/kobo")).unwrap();
        std::fs::write(local.join("covers/book-1.jpg"), b"jpg").unwrap();
        {
            let lib = Library::open(&local.join(DB_NAME)).unwrap();
            lib.set_books_root(&books).unwrap();
            lib.conn
                .execute(
                    "INSERT INTO book (id, fingerprint, title, created_at, updated_at, cover_path) VALUES (1, 'f', 'Book', '2026-01-01', '2026-01-01', ?1)",
                    [format!("{}/covers/book-1.jpg", local.display())],
                )
                .unwrap();
        }
        let db = current_library_in(&local, || unreachable!("its setting is used")).unwrap();
        assert_eq!(db, library_db(&books));
        assert!(!local.join(DB_NAME).exists() && !local.join("covers").exists());
        assert!(
            local.join("backups/kobo").is_dir(),
            "this computer's files stay"
        );
        assert_eq!(
            cover(&db),
            format!("{}/.word-hoard/covers/book-1.jpg", books.display())
        );
        // Remembered.
        assert_eq!(current_library_in(&local, || unreachable!()).unwrap(), db);
    }

    #[test]
    fn starts_in_the_default_folder() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("data/word-hoard");
        let books = dir.path().join("Books");
        let db = current_library_in(&local, || books.clone()).unwrap();
        assert_eq!(db, library_db(&books));
        set_current_library_in(&local, &dir.path().join("Elsewhere")).unwrap();
        assert_eq!(
            current_library_in(&local, || unreachable!()).unwrap(),
            library_db(&dir.path().join("Elsewhere"))
        );
    }
}
