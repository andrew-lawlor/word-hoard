//! Other readers on this computer (Foliate), and reading progress by
//! source.

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::Serialize;

use super::Library;
use crate::Result;
use crate::import::ImportStats;
use crate::kobo::DeviceInfo;

/// How far a book has been read in one place.
#[derive(Debug, Clone, Serialize)]
pub struct SourceProgress {
    /// "Foliate", or the Kobo's name.
    pub source: String,
    pub percent_read: i64,
    pub last_read: Option<DateTime<Utc>>,
}

/// An EPUB identifier as compared: no `urn:uuid:` prefix, lower case.
fn plain_uid(uid: &str) -> String {
    let uid = uid.trim();
    uid.strip_prefix("urn:uuid:")
        .unwrap_or(uid)
        .to_ascii_lowercase()
}

impl Library {
    /// Imports Foliate's highlights and progress from its data folder `dir`
    /// (installed as `kind`: flatpak, system or snap). Books are matched to
    /// library books by EPUB identifier, else by title and author as usual;
    /// library books keep their own title and author. Highlights that go
    /// from Foliate are handled as ones deleted on a Kobo.
    pub fn import_foliate(&mut self, dir: &Path, kind: &str) -> Result<ImportStats> {
        let books = crate::foliate::read_dir(dir);
        let device = DeviceInfo {
            serial: format!("{}{kind}", crate::foliate::ID_PREFIX),
            firmware: None,
            model_id: None,
        };
        // Library books by EPUB identifier.
        let mut by_uid: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT epub_uid, book_id FROM book_file WHERE epub_uid IS NOT NULL")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (uid, book) = row?;
                by_uid.entry(plain_uid(&uid)).or_insert(book);
            }
        }
        let device_id = self.upsert_device(&device)?;
        let mut titles = std::collections::HashMap::new();
        for b in &books {
            let Some(&book_id) = by_uid.get(&plain_uid(&b.identifier)) else {
                continue;
            };
            let volume = format!("{}{}", crate::foliate::ID_PREFIX, b.identifier);
            self.conn.execute(
                "INSERT INTO book_source (device_id, volume_id, book_id) VALUES (?1, ?2, ?3)
                 ON CONFLICT (device_id, volume_id) DO UPDATE SET book_id = excluded.book_id",
                params![device_id, volume, book_id],
            )?;
            if let Some(book) = self.book(book_id)? {
                titles.insert(b.identifier.clone(), (book.title, book.author));
            }
        }
        let snapshot = crate::foliate::snapshot(&books, |b| titles.get(&b.identifier).cloned());
        self.import(&snapshot, &device, false)
    }

    /// How far a book has been read in each place that says, latest first.
    pub fn reading_progress(&self, book_id: i64) -> Result<Vec<SourceProgress>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.serial, d.model_id, d.firmware, s.percent_read, s.last_read_at
             FROM book_source s JOIN device d ON d.id = s.device_id
             WHERE s.book_id = ?1 AND s.percent_read IS NOT NULL
             ORDER BY s.last_read_at IS NULL, s.last_read_at DESC",
        )?;
        let rows = stmt.query_map([book_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<DateTime<Utc>>>(4)?,
            ))
        })?;
        let mut out: Vec<SourceProgress> = Vec::new();
        for row in rows {
            let (serial, model_id, firmware, percent_read, last_read) = row?;
            let source = if serial.starts_with(crate::foliate::ID_PREFIX) {
                "Foliate".to_owned()
            } else {
                DeviceInfo {
                    serial,
                    firmware,
                    model_id,
                }
                .model_name()
                .to_owned()
            };
            // One line per source (a book can have two files on one Kobo).
            if !out.iter().any(|p| p.source == source) {
                out.push(SourceProgress {
                    source,
                    percent_read,
                    last_read,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::books::AddMode;

    #[test]
    fn imports_foliate_highlights_onto_the_library_book() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        let src = dir.path().join("albion.epub");
        crate::epub::tests::write_epub(
            &src,
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Albion's Seed</dc:title><dc:creator>David Hackett Fischer</dc:creator><dc:identifier id="id">urn:uuid:11AC094D-a237-4bd2-b8d8-b83d06d8cfbc</dc:identifier></metadata></package>"#,
        );
        let book = lib.add_file(&src, AddMode::Copy).unwrap().book_id;
        lib.conn
            .execute(
                "UPDATE book SET title = 'Albion’s Seed (Library)' WHERE id = ?1",
                [book],
            )
            .unwrap();

        let foliate = dir.path().join("foliate");
        std::fs::create_dir_all(&foliate).unwrap();
        let file = foliate.join("11ac094d-a237-4bd2-b8d8-b83d06d8cfbc.json");
        let write = |annotations: &str| {
            std::fs::write(
                &file,
                format!(
                    r#"{{"metadata": {{"identifier": "11ac094d-a237-4bd2-b8d8-b83d06d8cfbc", "title": "Albion's Seed: Four British Folkways"}},
                       "progress": [8, 100], "annotations": [{annotations}]}}"#
                ),
            )
            .unwrap();
        };
        write(
            r#"{"value": "epubcfi(/6/14!/4/30/2,/5:133,/7:11)", "color": "yellow", "text": "Its problematiques", "note": "test", "created": "2026-11-27T04:53:10.779Z"}"#,
        );

        let stats = lib.import_foliate(&foliate, "flatpak").unwrap();
        assert_eq!(stats.annotations_new, 1);
        let found = lib.book(book).unwrap().unwrap();
        assert_eq!(
            found.title, "Albion’s Seed (Library)",
            "the library's title stays"
        );
        assert_eq!(found.annotation_count, 1);
        assert_eq!(found.percent_read, Some(8));
        let progress = lib.reading_progress(book).unwrap();
        assert_eq!(progress.len(), 1);
        assert_eq!(
            (progress[0].source.as_str(), progress[0].percent_read),
            ("Foliate", 8)
        );
        let a = &lib.annotations_for_book(book).unwrap()[0];
        assert_eq!(a.reader.as_deref(), Some("Foliate"));

        // Again: nothing new. Then the highlight goes in Foliate.
        assert_eq!(
            lib.import_foliate(&foliate, "flatpak")
                .unwrap()
                .annotations_new,
            0
        );
        write("");
        lib.import_foliate(&foliate, "flatpak").unwrap();
        assert!(
            lib.annotations_for_book(book).unwrap()[0]
                .removed_on_device_at
                .is_some()
        );
    }
}
