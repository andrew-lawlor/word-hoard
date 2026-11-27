//! Importing a Calibre library (SPEC, "Calibre import"): its `metadata.db`
//! is read from a copy, never in place, so Calibre can keep running and
//! nothing in its library changes.
//!
//! Calibre's quirks, handled here: a comma inside an author's name is stored
//! as `|`; an unknown publication date is the placeholder year 101; a book's
//! files are `<library>/<books.path>/<data.name>.<format>` and its cover is
//! `cover.jpg` beside them; `comments` is HTML.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::store::books::{
    AddMode, AddOutcome, BookFacts, find_or_create_book, hash_file, insert_file,
};
use crate::{Error, Library, Result};

/// One book as Calibre knows it.
#[derive(Debug, Clone, Default)]
pub struct CalibreBook {
    pub id: i64,
    pub title: String,
    /// In order, commas restored.
    pub authors: Vec<String>,
    pub author_sort: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<f64>,
    pub tags: Vec<String>,
    pub publisher: Option<String>,
    pub published: Option<DateTime<Utc>>,
    pub added: Option<DateTime<Utc>>,
    pub identifiers: Vec<(String, String)>,
    /// HTML.
    pub comments: Option<String>,
    pub languages: Vec<String>,
    /// 0–10.
    pub rating: Option<i64>,
    pub uuid: Option<String>,
    pub cover: Option<PathBuf>,
    /// (format lower-cased, file), EPUB first.
    pub files: Vec<(String, PathBuf)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CalibreStats {
    pub books_new: usize,
    /// Books the library already had (from a Kobo, or added before).
    pub books_matched: usize,
    pub files_added: usize,
    pub files_already: usize,
    /// Listed in Calibre but missing from its folder.
    pub files_missing: usize,
    pub books_without_files: usize,
}

/// Calibre's timestamps: `2023-01-05 12:34:56.789012+00:00` (or with `T`).
fn parse_date(s: Option<String>) -> Option<DateTime<Utc>> {
    let s = s?;
    let parsed = DateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f%:z")
        .or_else(|_| DateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f%:z"))
        .or_else(|_| DateTime::parse_from_rfc3339(&s))
        .ok()?
        .with_timezone(&Utc);
    // Calibre's "unknown" date is the year 101.
    (chrono::Datelike::year(&parsed) > 101).then_some(parsed)
}

/// Calibre libraries on this computer, most likely first: the ones Calibre
/// itself remembers (its regular and Flatpak settings), `~/Calibre Library`,
/// then any `metadata.db` within a few folders of home (a library kept in a
/// synced folder, say, on a computer without Calibre). Hidden folders aren't
/// searched.
pub fn find_libraries(home: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        if p.join("metadata.db").is_file() && !found.contains(&p) {
            found.push(p);
        }
    };
    for config in [
        home.join(".config/calibre"),
        home.join(".var/app/com.calibre_ebook.calibre/config/calibre"),
    ] {
        for path in remembered_libraries(&config) {
            add(path);
        }
    }
    add(home.join("Calibre Library"));
    let mut searched = Vec::new();
    search_for_libraries(home, 3, &mut searched);
    for path in searched {
        add(path);
    }
    found
}

/// The libraries in Calibre's settings: the current one, then recent ones.
fn remembered_libraries(config: &Path) -> Vec<PathBuf> {
    let read = |name: &str| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(config.join(name)).ok()?).ok()
    };
    let mut paths = Vec::new();
    if let Some(path) = read("global.py.json")
        .as_ref()
        .and_then(|v| v.get("library_path"))
        .and_then(|v| v.as_str())
    {
        paths.push(PathBuf::from(path));
    }
    if let Some(stats) = read("gui.json")
        .as_ref()
        .and_then(|v| v.get("library_usage_stats"))
        .and_then(|v| v.as_object())
    {
        let mut recent: Vec<(&String, f64)> = stats
            .iter()
            .map(|(k, v)| (k, v.as_f64().unwrap_or(0.0)))
            .collect();
        recent.sort_by(|a, b| b.1.total_cmp(&a.1));
        paths.extend(recent.into_iter().map(|(k, _)| PathBuf::from(k)));
    }
    paths
}

fn search_for_libraries(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for dir in dirs {
        if dir.join("metadata.db").is_file() {
            found.push(dir);
        } else if depth > 1 {
            search_for_libraries(&dir, depth - 1, found);
        }
    }
}

/// How many books a Calibre library has (read-only; `None` if unreadable).
pub fn count_books(root: &Path) -> Option<usize> {
    let conn = Connection::open_with_flags(
        root.join("metadata.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row("SELECT count(*) FROM books", [], |r| r.get(0))
        .ok()
}

/// Reads every book in a Calibre library folder.
pub fn read_library(root: &Path) -> Result<Vec<CalibreBook>> {
    let db_path = root.join("metadata.db");
    if !db_path.is_file() {
        return Err(Error::NotACalibreLibrary(root.to_path_buf()));
    }
    let copy = tempfile::Builder::new()
        .prefix("word-hoard-calibre-")
        .tempdir()?;
    let db = copy.path().join("metadata.db");
    std::fs::copy(&db_path, &db)?;
    let conn = Connection::open(&db)?;
    conn.pragma_update(None, "query_only", true)?;

    let mut books: Vec<CalibreBook> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, title, author_sort, series_index, pubdate, timestamp, uuid, has_cover, path
             FROM books ORDER BY id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let path: String = r.get(8)?;
            let dir = root.join(&path);
            let has_cover: bool = r.get::<_, Option<bool>>(7)?.unwrap_or(false);
            books.push(CalibreBook {
                id: r.get(0)?,
                title: r
                    .get::<_, Option<String>>(1)?
                    .unwrap_or_else(|| "Unknown".into()),
                author_sort: r.get(2)?,
                series_index: r.get(3)?,
                published: parse_date(r.get(4)?),
                added: parse_date(r.get(5)?),
                uuid: r.get(6)?,
                cover: Some(dir.join("cover.jpg")).filter(|c| has_cover && c.is_file()),
                ..Default::default()
            });
        }
    }
    let index: std::collections::HashMap<i64, usize> =
        books.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
    let mut each = |sql: &str,
                    f: &mut dyn FnMut(&mut CalibreBook, &rusqlite::Row) -> rusqlite::Result<()>|
     -> Result<()> {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            if let Some(&i) = index.get(&id) {
                f(&mut books[i], r)?;
            }
        }
        Ok(())
    };
    each(
        "SELECT l.book, a.name FROM books_authors_link l JOIN authors a ON a.id = l.author ORDER BY l.book, l.id",
        &mut |b, r| {
            b.authors.push(r.get::<_, String>(1)?.replace('|', ","));
            Ok(())
        },
    )?;
    each(
        "SELECT l.book, s.name FROM books_series_link l JOIN series s ON s.id = l.series",
        &mut |b, r| {
            b.series = r.get(1)?;
            Ok(())
        },
    )?;
    each(
        "SELECT l.book, t.name FROM books_tags_link l JOIN tags t ON t.id = l.tag ORDER BY l.book, t.name",
        &mut |b, r| {
            b.tags.push(r.get(1)?);
            Ok(())
        },
    )?;
    each(
        "SELECT l.book, p.name FROM books_publishers_link l JOIN publishers p ON p.id = l.publisher",
        &mut |b, r| {
            b.publisher = r.get(1)?;
            Ok(())
        },
    )?;
    each(
        "SELECT l.book, x.rating FROM books_ratings_link l JOIN ratings x ON x.id = l.rating",
        &mut |b, r| {
            b.rating = r.get::<_, Option<i64>>(1)?.filter(|r| *r > 0);
            Ok(())
        },
    )?;
    each(
        "SELECT book, type, val FROM identifiers ORDER BY book, type",
        &mut |b, r| {
            b.identifiers
                .push((r.get::<_, String>(1)?.to_ascii_lowercase(), r.get(2)?));
            Ok(())
        },
    )?;
    each("SELECT book, text FROM comments", &mut |b, r| {
        b.comments = r.get(1)?;
        Ok(())
    })?;
    each(
        "SELECT l.book, g.lang_code FROM books_languages_link l JOIN languages g ON g.id = l.lang_code
         ORDER BY l.book, l.item_order",
        &mut |b, r| {
            b.languages.push(r.get(1)?);
            Ok(())
        },
    )?;
    each(
        "SELECT d.book, d.format, d.name, b.path FROM data d JOIN books b ON b.id = d.book",
        &mut |b, r| {
            let format = r.get::<_, String>(1)?.to_ascii_lowercase();
            let file = root
                .join(r.get::<_, String>(3)?)
                .join(format!("{}.{format}", r.get::<_, String>(2)?));
            b.files.push((format, file));
            Ok(())
        },
    )?;
    for b in &mut books {
        // EPUB first (its metadata and cover are read), then KEPUB, then the rest.
        b.files.sort_by_key(|(f, _)| match f.as_str() {
            "epub" => 0,
            "kepub" => 1,
            _ => 2,
        });
    }
    Ok(books)
}

impl CalibreBook {
    fn facts(&self) -> BookFacts {
        let mut identifiers = self.identifiers.clone();
        identifiers.push(("calibre".into(), self.id.to_string()));
        if let Some(uuid) = &self.uuid {
            identifiers.push(("uuid".into(), uuid.clone()));
        }
        let isbn = self
            .identifiers
            .iter()
            .find(|(t, _)| t == "isbn")
            .map(|(_, v)| v.replace(['-', ' '], ""));
        let series_number = self.series.as_ref().and(self.series_index).map(|n| {
            if n.fract() == 0.0 {
                format!("{}", n as i64)
            } else {
                n.to_string()
            }
        });
        BookFacts {
            title: self.title.clone(),
            author: (!self.authors.is_empty()).then(|| self.authors.join(", ")),
            folder_author: self.authors.first().map(|name| {
                crate::epub::Author {
                    name: name.clone(),
                    file_as: None,
                }
                .display_name()
            }),
            author_sort: self.author_sort.clone(),
            publisher: self.publisher.clone(),
            isbn,
            language: self.languages.first().cloned(),
            series: self.series.clone(),
            series_number,
            description: self
                .comments
                .as_deref()
                .and_then(|c| crate::normalize::clean_opt(Some(&crate::epub::strip_tags(c)))),
            published: self.published.map(|d| d.format("%Y-%m-%d").to_string()),
            identifiers,
            rating: self.rating,
            tags: self.tags.clone(),
            added: self.added,
        }
    }
}

impl Library {
    /// Imports a Calibre library: every book with Calibre's metadata (it
    /// wins over what's inside the files), its tags and rating, and all its
    /// files, copied or linked into the books folder. Books already in the
    /// library (same title and author) get the files and metadata added.
    /// Importing again adds only what's new. `progress` is called with
    /// (done, total) after each book.
    pub fn import_calibre(
        &self,
        root: &Path,
        mode: AddMode,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<CalibreStats> {
        let books = read_library(root)?;
        let books_root = self.books_root()?;
        let mut stats = CalibreStats::default();
        for (n, book) in books.iter().enumerate() {
            self.import_calibre_book(book, &books_root, mode, &mut stats)?;
            progress(n + 1, books.len());
        }
        Ok(stats)
    }

    fn import_calibre_book(
        &self,
        book: &CalibreBook,
        books_root: &Path,
        mode: AddMode,
        stats: &mut CalibreStats,
    ) -> Result<()> {
        let facts = book.facts();
        let tx = self.conn.unchecked_transaction()?;
        let (book_id, outcome) = find_or_create_book(&tx, &facts, true)?;
        match outcome {
            AddOutcome::NewBook => stats.books_new += 1,
            _ => stats.books_matched += 1,
        }
        tx.commit()?;

        let existing = book.files.iter().filter(|(_, f)| f.is_file()).count();
        stats.files_missing += book.files.len() - existing;
        if book.files.is_empty() {
            stats.books_without_files += 1;
        }
        for (format, file) in book.files.iter().filter(|(_, f)| f.is_file()) {
            let (hash, size) = hash_file(file)?;
            if self.file_by_hash(&hash)?.is_some() {
                stats.files_already += 1;
                continue;
            }
            let epub_uid = matches!(format.as_str(), "epub" | "kepub")
                .then(|| {
                    crate::epub::read_metadata(file)
                        .ok()
                        .and_then(|m| m.unique_id)
                })
                .flatten();
            let placed = self.place_file(file, books_root, &facts, format, mode)?;
            let tx = self.conn.unchecked_transaction()?;
            let recorded = insert_file(
                &tx,
                book_id,
                &placed.relative,
                format,
                size,
                &hash,
                epub_uid.as_deref(),
            )
            .and_then(|()| Ok(tx.commit()?));
            if let Err(err) = recorded {
                if placed.created {
                    let _ = std::fs::remove_file(books_root.join(&placed.relative));
                }
                return Err(err);
            }
            stats.files_added += 1;
        }

        // Calibre's cover, else the one in the EPUB, for a book without one.
        let has_cover: bool = self
            .conn
            .query_row(
                "SELECT cover_path IS NOT NULL FROM book WHERE id = ?1",
                [book_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(true);
        if !has_cover {
            match (&book.cover, self.assets_dir()) {
                (Some(cover), Some(dir)) => {
                    let dest = dir.join("covers").join(format!("book-{book_id}.jpg"));
                    std::fs::create_dir_all(dest.parent().unwrap_or(&dir))?;
                    std::fs::copy(cover, &dest)?;
                    self.conn.execute(
                        "UPDATE book SET cover_path = ?2 WHERE id = ?1",
                        params![book_id, dest.to_string_lossy()],
                    )?;
                }
                _ => {
                    if let Some((_, epub)) =
                        book.files.iter().find(|(f, p)| f == "epub" && p.is_file())
                    {
                        self.store_cover(book_id, epub);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::epub::tests::write_epub;

    /// A small Calibre library with Calibre's own table layout (the tables
    /// and columns Word Hoard reads).
    pub(crate) fn calibre_library(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        let conn = Connection::open(root.join("metadata.db")).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE books (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL DEFAULT 'Unknown' COLLATE NOCASE,
                sort TEXT COLLATE NOCASE, timestamp TIMESTAMP DEFAULT CURRENT_TIMESTAMP, pubdate TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                series_index REAL NOT NULL DEFAULT 1.0, author_sort TEXT COLLATE NOCASE, isbn TEXT DEFAULT "" COLLATE NOCASE,
                lccn TEXT DEFAULT "" COLLATE NOCASE, path TEXT NOT NULL DEFAULT "", flags INTEGER NOT NULL DEFAULT 1,
                uuid TEXT, has_cover BOOL DEFAULT 0, last_modified TIMESTAMP NOT NULL DEFAULT "2000-01-01 00:00:00+00:00");
            CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL COLLATE NOCASE, sort TEXT COLLATE NOCASE, link TEXT NOT NULL DEFAULT "");
            CREATE TABLE books_authors_link (id INTEGER PRIMARY KEY, book INTEGER NOT NULL, author INTEGER NOT NULL);
            CREATE TABLE series (id INTEGER PRIMARY KEY, name TEXT NOT NULL COLLATE NOCASE, sort TEXT COLLATE NOCASE);
            CREATE TABLE books_series_link (id INTEGER PRIMARY KEY, book INTEGER NOT NULL, series INTEGER NOT NULL);
            CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL COLLATE NOCASE);
            CREATE TABLE books_tags_link (id INTEGER PRIMARY KEY, book INTEGER NOT NULL, tag INTEGER NOT NULL);
            CREATE TABLE publishers (id INTEGER PRIMARY KEY, name TEXT NOT NULL COLLATE NOCASE, sort TEXT COLLATE NOCASE);
            CREATE TABLE books_publishers_link (id INTEGER PRIMARY KEY, book INTEGER NOT NULL, publisher INTEGER NOT NULL);
            CREATE TABLE ratings (id INTEGER PRIMARY KEY, rating INTEGER CHECK(rating > -1 AND rating < 11));
            CREATE TABLE books_ratings_link (id INTEGER PRIMARY KEY, book INTEGER NOT NULL, rating INTEGER NOT NULL);
            CREATE TABLE identifiers (id INTEGER PRIMARY KEY, book INTEGER NOT NULL, type TEXT NOT NULL DEFAULT "isbn" COLLATE NOCASE, val TEXT NOT NULL COLLATE NOCASE);
            CREATE TABLE comments (id INTEGER PRIMARY KEY, book INTEGER NOT NULL, text TEXT NOT NULL COLLATE NOCASE);
            CREATE TABLE languages (id INTEGER PRIMARY KEY, lang_code TEXT NOT NULL COLLATE NOCASE);
            CREATE TABLE books_languages_link (id INTEGER PRIMARY KEY, book INTEGER NOT NULL, lang_code INTEGER NOT NULL, item_order INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE data (id INTEGER PRIMARY KEY, book INTEGER NOT NULL, format TEXT NOT NULL COLLATE NOCASE, uncompressed_size INTEGER NOT NULL, name TEXT NOT NULL);

            INSERT INTO books (id, title, sort, timestamp, pubdate, series_index, author_sort, path, uuid, has_cover) VALUES
              (1, 'The Eye of the World', 'Eye of the World, The', '2023-01-05 12:34:56.789012+00:00', '1990-01-15 00:00:00+00:00', 1.0, 'Jordan, Robert', 'Robert Jordan/The Eye of the World (1)', 'aaaaaaaa-0000-4000-8000-000000000001', 1),
              (2, 'Good Omens', 'Good Omens', '2023-02-01 09:00:00+00:00', '0101-01-01 00:00:00+00:00', 1.0, 'Pratchett, Terry & Gaiman, Neil', 'Terry Pratchett/Good Omens (2)', NULL, 0),
              (3, 'A Paper-Only Book', 'Paper-Only Book, A', '2023-03-01 09:00:00+00:00', '2001-01-01 00:00:00+00:00', 1.0, 'Doe, Jane', 'Jane Doe/A Paper-Only Book (3)', NULL, 0);
            INSERT INTO authors VALUES (1, 'Robert Jordan', 'Jordan, Robert', ''), (2, 'Terry Pratchett', 'Pratchett, Terry', ''),
              (3, 'Neil Gaiman', 'Gaiman, Neil', ''), (4, 'Jane Doe| Jr.', 'Doe, Jane', '');
            INSERT INTO books_authors_link VALUES (1, 1, 1), (2, 2, 2), (3, 2, 3), (4, 3, 4);
            INSERT INTO series VALUES (1, 'The Wheel of Time', 'Wheel of Time, The');
            INSERT INTO books_series_link VALUES (1, 1, 1);
            INSERT INTO tags VALUES (1, 'Fantasy'), (2, 'Epic'), (3, 'Humour');
            INSERT INTO books_tags_link VALUES (1, 1, 1), (2, 1, 2), (3, 2, 1), (4, 2, 3);
            INSERT INTO publishers VALUES (1, 'Tor', 'Tor');
            INSERT INTO books_publishers_link VALUES (1, 1, 1);
            INSERT INTO ratings VALUES (1, 8);
            INSERT INTO books_ratings_link VALUES (1, 1, 1);
            INSERT INTO identifiers VALUES (1, 1, 'isbn', '978-0-312-85009-6'), (2, 1, 'goodreads', '228665');
            INSERT INTO comments VALUES (1, 1, '<div><p>The Wheel of Time turns.</p></div>');
            INSERT INTO languages VALUES (1, 'eng');
            INSERT INTO books_languages_link VALUES (1, 1, 1, 0), (2, 2, 1, 0);
            INSERT INTO data VALUES (1, 1, 'EPUB', 100, 'The Eye of the World - Robert Jordan'),
              (2, 2, 'EPUB', 100, 'Good Omens - Terry Pratchett'), (3, 2, 'PDF', 100, 'Good Omens - Terry Pratchett'),
              (4, 2, 'MOBI', 100, 'Good Omens - Terry Pratchett');
            "#,
        )
        .unwrap();
        let opf = |title: &str, author: &str| {
            format!(
                r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{title} (file title)</dc:title><dc:creator>{author}</dc:creator><dc:identifier id="id">urn:uuid:{title}</dc:identifier></metadata></package>"#
            )
        };
        let eye = root.join("Robert Jordan/The Eye of the World (1)");
        std::fs::create_dir_all(&eye).unwrap();
        write_epub(
            &eye.join("The Eye of the World - Robert Jordan.epub"),
            &opf("Eye", "Robert Jordan"),
        );
        std::fs::write(eye.join("cover.jpg"), b"\xff\xd8\xff\xe0 not really a jpeg").unwrap();
        let omens = root.join("Terry Pratchett/Good Omens (2)");
        std::fs::create_dir_all(&omens).unwrap();
        write_epub(
            &omens.join("Good Omens - Terry Pratchett.epub"),
            &opf("Omens", "Terry Pratchett"),
        );
        std::fs::write(omens.join("Good Omens - Terry Pratchett.pdf"), b"%PDF-1.4").unwrap();
        // The MOBI is listed but missing from the folder.
    }

    #[test]
    fn reads_calibre_metadata() {
        let dir = tempfile::tempdir().unwrap();
        calibre_library(dir.path());
        let books = read_library(dir.path()).unwrap();
        assert_eq!(books.len(), 3);
        let eye = &books[0];
        assert_eq!(eye.title, "The Eye of the World");
        assert_eq!(eye.authors, ["Robert Jordan"]);
        assert_eq!(eye.series.as_deref(), Some("The Wheel of Time"));
        assert_eq!(eye.tags, ["Epic", "Fantasy"]);
        assert_eq!(eye.rating, Some(8));
        assert_eq!(
            eye.published.map(|d| d.format("%Y").to_string()).as_deref(),
            Some("1990")
        );
        assert!(eye.cover.is_some());
        assert_eq!(eye.files[0].0, "epub");
        let omens = &books[1];
        assert_eq!(omens.authors, ["Terry Pratchett", "Neil Gaiman"]);
        assert_eq!(omens.published, None, "year 101 means unknown");
        assert_eq!(
            omens
                .files
                .iter()
                .map(|(f, _)| f.as_str())
                .collect::<Vec<_>>(),
            ["epub", "pdf", "mobi"]
        );
        assert_eq!(books[2].authors, ["Jane Doe, Jr."]);
        assert!(books[2].files.is_empty());
    }

    #[test]
    fn imports_books_files_and_metadata_once() {
        let dir = tempfile::tempdir().unwrap();
        let calibre = dir.path().join("Calibre Library");
        calibre_library(&calibre);
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        // A book already known from a Kobo, by the same title and author.
        let fp =
            crate::normalize::book_fingerprint("Good Omens", Some("Terry Pratchett, Neil Gaiman"));
        lib.conn
            .execute(
                "INSERT INTO book (fingerprint, title, author, created_at, updated_at) VALUES (?1, 'Good Omens', 'Terry Pratchett, Neil Gaiman', ?2, ?2)",
                params![fp, Utc::now()],
            )
            .unwrap();

        let mut calls = 0;
        let stats = lib
            .import_calibre(&calibre, AddMode::Copy, |_, total| {
                calls += 1;
                assert_eq!(total, 3);
            })
            .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(
            stats,
            CalibreStats {
                books_new: 2,
                books_matched: 1,
                files_added: 3,
                files_already: 0,
                files_missing: 1,
                books_without_files: 1,
            }
        );

        let (id, title, series, number, rating, isbn, language, description, published, author_sort): (
            i64, String, String, String, i64, String, String, String, String, String,
        ) = lib
            .conn
            .query_row(
                "SELECT id, title, series, series_number, rating, isbn, language, description, published, author_sort
                 FROM book WHERE title = 'The Eye of the World'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
            )
            .unwrap();
        // Calibre's title wins over the file's "(file title)".
        assert_eq!(title, "The Eye of the World");
        assert_eq!(
            (series.as_str(), number.as_str(), rating),
            ("The Wheel of Time", "1", 8)
        );
        assert_eq!((isbn.as_str(), language.as_str()), ("9780312850096", "eng"));
        assert_eq!(description, "The Wheel of Time turns.");
        assert_eq!(
            (published.as_str(), author_sort.as_str()),
            ("1990-01-15", "Jordan, Robert")
        );
        let files = lib.book_files(id).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].path,
            dir.path()
                .join("Books/Robert Jordan/The Eye of the World (1990)/The Eye of the World.epub")
        );
        let tags: Vec<String> = lib
            .conn
            .prepare("SELECT name FROM book_tag WHERE book_id = ?1 ORDER BY name")
            .unwrap()
            .query_map([id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(tags, ["Epic", "Fantasy"]);
        assert!(lib.book(id).unwrap().unwrap().cover.is_some());

        // The Kobo-known book got Calibre's files (EPUB and PDF).
        let omens: i64 = lib
            .conn
            .query_row("SELECT id FROM book WHERE title = 'Good Omens'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let formats: Vec<String> = lib
            .book_files(omens)
            .unwrap()
            .into_iter()
            .map(|f| f.format)
            .collect();
        assert_eq!(formats, ["epub", "pdf"]);

        // Again: nothing new.
        let again = lib
            .import_calibre(&calibre, AddMode::Copy, |_, _| {})
            .unwrap();
        assert_eq!(
            (again.books_new, again.files_added, again.files_already),
            (0, 0, 3)
        );
        assert_eq!(lib.books().unwrap().len(), 3);
    }

    #[test]
    fn refuses_folders_without_a_calibre_library() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_library(dir.path()),
            Err(Error::NotACalibreLibrary(_))
        ));
    }

    #[test]
    fn finds_calibre_libraries() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let lib = |p: &str| {
            let p = home.join(p);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("metadata.db"), b"").unwrap();
            p
        };
        let remembered = lib("Elsewhere/Remembered");
        let usual = lib("Calibre Library");
        let synced = lib("Nextcloud/books");
        lib(".hidden/Library");
        lib("a/b/c/too deep");
        std::fs::create_dir_all(home.join(".config/calibre")).unwrap();
        std::fs::write(
            home.join(".config/calibre/global.py.json"),
            format!(r#"{{"library_path": "{}"}}"#, remembered.display()),
        )
        .unwrap();
        assert_eq!(find_libraries(home), [remembered, usual, synced]);
    }
}
