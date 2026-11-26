//! Synthetic Kobo databases for tests. Columns follow the real schema but only
//! a subset of them; `Legacy` drops the columns older firmware lacks.
//!
//! Contents:
//! - "The Dispossessed" (sideloaded KEPUB): 3 highlights/notes out of order,
//!   1 dogear, 1 hidden highlight, across two chapters.
//! - "Piranesi" (store book, UUID id): 1 highlight.
//! - A highlight whose book has no `content` row (removed from device).
//! - "Unread Book" with no annotations.

use std::fs;
use std::path::Path;

use rusqlite::{Connection, params};

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schema {
    Modern,
    /// No `Type`, `Color` or `DateModified` columns on `Bookmark`.
    Legacy,
}

pub const VERSION_LINE: &str =
    "N418170000001,4.1.15,4.38.21908,4.1.15,4.1.15,00000000-0000-0000-0000-000000000390";
pub const SIDELOADED_ID: &str = "file:///mnt/onboard/Books/Le Guin/The Dispossessed.kepub.epub";
pub const STORE_ID: &str = "3f2a9c1e-5b7d-4e8a-9c21-7d4e5f6a8b90";
pub const REMOVED_ID: &str = "file:///mnt/onboard/Books/Gone Book.epub";

/// Creates `<root>/.kobo/version` and `<root>/.kobo/KoboReader.sqlite`.
pub fn create_device(root: &Path) -> Result<()> {
    let kobo = root.join(".kobo");
    fs::create_dir_all(&kobo)?;
    fs::write(kobo.join("version"), format!("{VERSION_LINE}\n"))?;
    create_db(&kobo.join("KoboReader.sqlite"), Schema::Modern)
}

pub fn create_db(path: &Path, schema: Schema) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE content (
            ContentID TEXT NOT NULL, ContentType TEXT NOT NULL, MimeType TEXT,
            BookID TEXT, BookTitle TEXT, Title TEXT, Attribution TEXT, ISBN TEXT,
            Publisher TEXT, Series TEXT, SeriesNumber TEXT, ___PercentRead INTEGER,
            ReadStatus INTEGER, DateLastRead TEXT, VolumeIndex INTEGER,
            PRIMARY KEY (ContentID));
         CREATE TABLE DbVersion (version INTEGER);
         INSERT INTO DbVersion VALUES (174);",
    )?;
    let modern_columns = match schema {
        Schema::Modern => ", DateModified TEXT, Type TEXT, Color INTEGER",
        Schema::Legacy => "",
    };
    conn.execute_batch(&format!(
        "CREATE TABLE Bookmark (
            BookmarkID TEXT NOT NULL, VolumeID TEXT NOT NULL, ContentID TEXT NOT NULL,
            StartContainerPath TEXT NOT NULL, StartContainerChildIndex INTEGER NOT NULL DEFAULT -99,
            StartOffset INTEGER NOT NULL, EndContainerPath TEXT NOT NULL,
            EndContainerChildIndex INTEGER NOT NULL DEFAULT -99, EndOffset INTEGER NOT NULL,
            Text TEXT, Annotation TEXT, DateCreated TEXT,
            ChapterProgress REAL NOT NULL DEFAULT 0, Hidden BOOL NOT NULL DEFAULT 0{modern_columns},
            PRIMARY KEY (BookmarkID));"
    ))?;

    let book = |id: &str, title: &str, author: &str, percent: i64, status: i64| {
        conn.execute(
            "INSERT INTO content (ContentID, ContentType, Title, Attribution, ___PercentRead, ReadStatus, DateLastRead, ISBN)
             VALUES (?1, '6', ?2, ?3, ?4, ?5, '2026-11-20T21:14:03Z', ?6)",
            params![id, title, author, percent, status, (id == SIDELOADED_ID).then_some("9780060512750")],
        )
    };
    book(SIDELOADED_ID, "The Dispossessed", "Ursula K. Le Guin", 87, 1)?;
    book(STORE_ID, "Piranesi", "Susanna Clarke", 100, 2)?;
    book("file:///mnt/onboard/Books/Unread.epub", "Unread Book", "Nobody", 0, 0)?;

    // KEPUB chapters use ContentType 899, EPUB chapters 9.
    let chapter = |id: String, kind: &str, title: &str, index: i64, volume: &str| {
        conn.execute(
            "INSERT INTO content (ContentID, ContentType, BookID, Title, VolumeIndex) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, kind, volume, title, index],
        )
    };
    let ch1 = format!("{SIDELOADED_ID}!OEBPS!ch01.xhtml");
    let ch2 = format!("{SIDELOADED_ID}!OEBPS!ch02.xhtml");
    let piranesi_ch = format!("{STORE_ID}!part1.xhtml");
    chapter(format!("{ch1}-1"), "899", "Chapter One", 0, SIDELOADED_ID)?;
    chapter(ch2.clone(), "9", "Chapter Two", 1, SIDELOADED_ID)?;
    chapter(piranesi_ch.clone(), "899", "Part 1", 0, STORE_ID)?;

    struct Mark<'a> {
        id: &'a str,
        volume: &'a str,
        chapter: &'a str,
        path: &'a str,
        offset: i64,
        text: Option<&'a str>,
        note: Option<&'a str>,
        progress: f64,
        created: &'a str,
        hidden: &'a str,
        kind: &'a str,
        color: i64,
    }
    let insert = |m: Mark| -> rusqlite::Result<usize> {
        match schema {
            Schema::Modern => conn.execute(
                "INSERT INTO Bookmark (BookmarkID, VolumeID, ContentID, StartContainerPath, StartOffset,
                    EndContainerPath, EndOffset, Text, Annotation, DateCreated, ChapterProgress, Hidden,
                    DateModified, Type, Color)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?4, ?5 + 20, ?6, ?7, ?8, ?9, ?10, ?8, ?11, ?12)",
                params![m.id, m.volume, m.chapter, m.path, m.offset, m.text, m.note, m.created,
                        m.progress, m.hidden, m.kind, m.color],
            ),
            Schema::Legacy => conn.execute(
                "INSERT INTO Bookmark (BookmarkID, VolumeID, ContentID, StartContainerPath, StartOffset,
                    EndContainerPath, EndOffset, Text, Annotation, DateCreated, ChapterProgress, Hidden)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?4, ?5 + 20, ?6, ?7, ?8, ?9, ?10)",
                params![m.id, m.volume, m.chapter, m.path, m.offset, m.text, m.note, m.created,
                        m.progress, m.hidden],
            ),
        }
    };
    #[allow(clippy::too_many_arguments)]
    fn mark<'a>(
        id: &'a str, volume: &'a str, chapter: &'a str, path: &'a str, offset: i64,
        text: Option<&'a str>, note: Option<&'a str>, created: &'a str, hidden: &'a str,
        kind: &'a str, color: i64,
    ) -> Mark<'a> {
        Mark { id, volume, chapter, path, offset, text, note, progress: 0.0, created, hidden, kind, color }
    }

    // Inserted out of reading order on purpose.
    insert(mark("b1", SIDELOADED_ID, &ch2, r"span#kobo\.3\.1", 0, Some("True journey is return."),
        None, "2026-11-02T09:00:00.000", "false", "highlight", 0))?;
    insert(mark("b2", SIDELOADED_ID, &ch1, r"span#kobo\.12\.2", 5, Some("You cannot buy the revolution."),
        Some("Compare with Graeber."), "2026-10-30T22:10:00.000", "false", "note", 2))?;
    insert(mark("b3", SIDELOADED_ID, &ch1, r"span#kobo\.2\.1", 0, Some("There was a wall."),
        None, "2026-10-30T21:00:00.000", "false", "highlight", 0))?;
    insert(Mark { progress: 0.5, ..mark("b4", SIDELOADED_ID, &ch1, "", 0, None, None,
        "2026-10-31T08:00:00.000", "false", "dogear", 0) })?;
    insert(mark("b5", SIDELOADED_ID, &ch1, r"span#kobo\.5\.1", 0, Some("Deleted highlight"),
        None, "2026-10-30T21:30:00.000", "true", "highlight", 0))?;
    insert(mark("p1", STORE_ID, &piranesi_ch, r"span#kobo\.1\.1", 0,
        Some("The Beauty of the House is immeasurable; its Kindness infinite."),
        None, "2026-09-01T12:00:00.000", "false", "highlight", 3))?;
    insert(mark("g1", REMOVED_ID, &format!("{REMOVED_ID}!ch1.xhtml"), "/1/4/2", 0,
        Some("Orphaned highlight"), None, "2026-08-01T12:00:00.000", "false", "highlight", 0))?;
    Ok(())
}
