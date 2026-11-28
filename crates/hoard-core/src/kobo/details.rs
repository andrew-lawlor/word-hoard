//! The details a Kobo shows for sideloaded books (title, authors, series,
//! description, publisher), kept in step with the library.
//!
//! A Kobo reads a sideloaded book's details once, when it first adds it,
//! and ignores series in the book's own metadata (checked on a Libra Colour,
//! firmware 4.45, DbVersion 176: books whose OPF named their series got none).
//! So, as Calibre's Kobo driver does, Word Hoard writes them into the
//! `content` table of the Kobo's database, for sideloaded books that match
//! library books, through [`super::writer::DeviceWriter::update_details`].

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::Result;

/// A sideloaded book's details on the Kobo (`content` row, `ContentType` 6).
/// In an update, `None` leaves the field as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KoboDetails {
    /// `file:///mnt/onboard/…`
    pub content_id: String,
    pub title: Option<String>,
    /// Authors as the Kobo lists them (`Attribution`).
    pub attribution: Option<String>,
    pub description: Option<String>,
    pub publisher: Option<String>,
    pub series: Option<String>,
    /// As shown (`1`, `2.5`).
    pub series_number: Option<String>,
}

impl KoboDetails {
    /// The fields of `self` (what the library has) that differ from `on_kobo`,
    /// or `None` when there's nothing to change.
    pub fn changes_from(&self, on_kobo: &KoboDetails) -> Option<KoboDetails> {
        fn differs(want: &Option<String>, have: &Option<String>) -> Option<String> {
            let want = want.as_deref().map(str::trim).filter(|w| !w.is_empty())?;
            (have.as_deref().map(str::trim) != Some(want)).then(|| want.to_owned())
        }
        let change = KoboDetails {
            content_id: self.content_id.clone(),
            title: differs(&self.title, &on_kobo.title),
            attribution: differs(&self.attribution, &on_kobo.attribution),
            description: differs(&self.description, &on_kobo.description),
            publisher: differs(&self.publisher, &on_kobo.publisher),
            series: differs(&self.series, &on_kobo.series),
            series_number: differs(&self.series_number, &on_kobo.series_number),
        };
        let nothing = KoboDetails {
            content_id: self.content_id.clone(),
            ..Default::default()
        };
        (change != nothing).then_some(change)
    }
}

/// Sideloaded books' details in a Kobo database (a copy, or the device's,
/// opened read-only), by `ContentID`.
pub fn read_details(db: &Path) -> Result<HashMap<String, KoboDetails>> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT ContentID, Title, Attribution, Description, Publisher, Series, SeriesNumber
         FROM content WHERE ContentType = 6 AND ContentID LIKE 'file:///mnt/onboard/%'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(KoboDetails {
            content_id: r.get(0)?,
            title: r.get(1)?,
            attribution: r.get(2)?,
            description: r.get(3)?,
            publisher: r.get(4)?,
            series: r.get(5)?,
            series_number: r.get(6)?,
        })
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let row = row?;
        out.insert(row.content_id.clone(), row);
    }
    Ok(out)
}

/// The database's `DbVersion`, if it has one.
pub(crate) fn db_version(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT version FROM DbVersion", [], |r| r.get(0))
        .optional()?)
}

/// Writes `updates` into an open Kobo database, in one transaction: only
/// sideloaded books' `content` rows, only the fields given. Series also sets
/// `SeriesNumberFloat` (which the Kobo sorts by) and `SeriesID` (which it
/// groups by; Calibre uses the series name). Returns how many books changed.
pub(crate) fn write(conn: &mut Connection, updates: &[KoboDetails]) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut changed = 0;
    for u in updates {
        if !u.content_id.starts_with("file:///mnt/onboard/") {
            continue;
        }
        let number_float: Option<f64> = u.series_number.as_deref().and_then(|n| n.parse().ok());
        changed += tx.execute(
            "UPDATE content SET
                Title = coalesce(?2, Title),
                Attribution = coalesce(?3, Attribution),
                Description = coalesce(?4, Description),
                Publisher = coalesce(?5, Publisher),
                Series = coalesce(?6, Series),
                SeriesID = coalesce(?6, SeriesID),
                SeriesNumber = coalesce(?7, SeriesNumber),
                SeriesNumberFloat = coalesce(?8, SeriesNumberFloat)
             WHERE ContentID = ?1 AND ContentType = 6",
            rusqlite::params![
                u.content_id,
                u.title,
                u.attribution,
                u.description,
                u.publisher,
                u.series,
                u.series_number,
                number_float,
            ],
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Removes sideloaded books (by `ContentID`) from an open Kobo database, in
/// one transaction, as Calibre's Kobo driver does: the book's `content` row,
/// its chapters' rows (`BookID`), its cached page covers
/// (`volume_shortcovers`), tabs (`volume_tabs`) and shelf entries
/// (`ShelfContent`). Highlights (`Bookmark`) and reading events are kept,
/// so a book sent again gets its highlights back. Store books are never
/// touched. Returns how many books were removed.
pub(crate) fn remove(conn: &mut Connection, content_ids: &[String]) -> Result<usize> {
    let has = |table: &str| -> Result<bool> {
        Ok(conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    };
    let (covers, tabs, shelves) = (
        has("volume_shortcovers")?,
        has("volume_tabs")?,
        has("ShelfContent")?,
    );
    let tx = conn.transaction()?;
    let mut removed = 0;
    for id in content_ids {
        if !id.starts_with("file:///mnt/onboard/") {
            continue;
        }
        tx.execute("DELETE FROM content WHERE BookID = ?1", [id])?;
        removed += tx.execute(
            "DELETE FROM content WHERE ContentID = ?1 AND ContentType = 6",
            [id],
        )?;
        if covers {
            tx.execute("DELETE FROM volume_shortcovers WHERE volumeId = ?1", [id])?;
        }
        if tabs {
            tx.execute("DELETE FROM volume_tabs WHERE volumeid = ?1", [id])?;
        }
        if shelves {
            tx.execute("DELETE FROM ShelfContent WHERE ContentId = ?1", [id])?;
        }
    }
    tx.commit()?;
    Ok(removed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A Kobo database with the tables and columns this module uses.
    pub(crate) fn fake_kobo_db(path: &Path, version: i64) {
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE DbVersion (version INTEGER);
             INSERT INTO DbVersion VALUES ({version});
             CREATE TABLE content (
                ContentID TEXT, ContentType INTEGER, Title TEXT, Attribution TEXT,
                Description TEXT, Publisher TEXT, Series TEXT, SeriesNumber TEXT,
                SeriesNumberFloat REAL, SeriesID TEXT, BookID TEXT,
                ChapterIDBookmarked TEXT, ___PercentRead INTEGER DEFAULT 0,
                DateLastRead TEXT, ReadStatus INTEGER DEFAULT 0,
                FirstTimeReading TEXT DEFAULT 'true');
             CREATE TABLE volume_shortcovers (volumeId TEXT, shortcoverId TEXT);
             INSERT INTO volume_shortcovers VALUES ('file:///mnt/onboard/Books/H/Hammer.kepub.epub', 'ch1');
             CREATE TABLE ShelfContent (ShelfName TEXT, ContentId TEXT);
             INSERT INTO ShelfContent VALUES ('Vikings', 'file:///mnt/onboard/Books/H/Hammer.kepub.epub');
             CREATE TABLE Bookmark (BookmarkID TEXT, VolumeID TEXT);
             INSERT INTO Bookmark VALUES ('b1', 'file:///mnt/onboard/Books/H/Hammer.kepub.epub');
             INSERT INTO content (ContentID, ContentType, Title, Attribution, SeriesNumberFloat)
             VALUES ('file:///mnt/onboard/Books/H/Hammer.kepub.epub', 6, 'The Hammer & the Cross', 'H. Harrison', 0),
                    ('store-book-uuid', 6, 'Store Book', 'Someone', 0);
             INSERT INTO content (ContentID, ContentType, Title, BookID)
             VALUES ('file:///mnt/onboard/Books/H/Hammer.kepub.epub!!ch1.xhtml', 9, 'ch1.xhtml',
                     'file:///mnt/onboard/Books/H/Hammer.kepub.epub');"
        ))
        .unwrap();
    }

    #[test]
    fn finds_and_writes_only_whats_different() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("KoboReader.sqlite");
        fake_kobo_db(&db, 176);
        let have = read_details(&db).unwrap();
        assert_eq!(have.len(), 1, "sideloaded books only");
        let id = "file:///mnt/onboard/Books/H/Hammer.kepub.epub";
        let want = KoboDetails {
            content_id: id.into(),
            title: Some("The Hammer and the Cross".into()),
            attribution: Some("H. Harrison".into()),
            series: Some("The Hammer and the Cross".into()),
            series_number: Some("1".into()),
            ..Default::default()
        };
        let change = want.changes_from(&have[id]).unwrap();
        assert_eq!(change.attribution, None, "same already");
        assert_eq!(change.title.as_deref(), Some("The Hammer and the Cross"));

        let mut conn = Connection::open(&db).unwrap();
        // A store book is never touched, even if asked.
        let store = KoboDetails {
            content_id: "store-book-uuid".into(),
            title: Some("Changed".into()),
            ..Default::default()
        };
        assert_eq!(write(&mut conn, &[change, store]).unwrap(), 1);
        let row: (String, String, String, String, f64) = conn
            .query_row(
                "SELECT Title, Series, SeriesID, SeriesNumber, SeriesNumberFloat FROM content WHERE ContentID = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "The Hammer and the Cross".into(),
                "The Hammer and the Cross".into(),
                "The Hammer and the Cross".into(),
                "1".into(),
                1.0
            )
        );
        let store_title: String = conn
            .query_row(
                "SELECT Title FROM content WHERE ContentID = 'store-book-uuid'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(store_title, "Store Book");

        // Now in step: nothing more to change.
        let have = read_details(&db).unwrap();
        assert!(want.changes_from(&have[id]).is_none());
    }

    #[test]
    fn removes_a_sideloaded_book_and_its_records_but_keeps_highlights() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("KoboReader.sqlite");
        fake_kobo_db(&db, 176);
        let mut conn = Connection::open(&db).unwrap();
        let id = "file:///mnt/onboard/Books/H/Hammer.kepub.epub".to_owned();
        let removed = remove(&mut conn, &[id.clone(), "store-book-uuid".into()]).unwrap();
        assert_eq!(removed, 1, "store books are never removed");
        let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(
            count("SELECT count(*) FROM content WHERE ContentID LIKE 'file://%'"),
            0
        );
        assert_eq!(count("SELECT count(*) FROM volume_shortcovers"), 0);
        assert_eq!(count("SELECT count(*) FROM ShelfContent"), 0);
        assert_eq!(count("SELECT count(*) FROM Bookmark"), 1, "highlights stay");
        assert_eq!(
            count("SELECT count(*) FROM content WHERE ContentID = 'store-book-uuid'"),
            1
        );
    }
}
