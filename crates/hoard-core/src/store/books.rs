//! Books as files: EPUBs kept in the books folder, each tied to a library
//! book. Adding a file reads its metadata, finds the book it belongs to (one
//! already known from a Kobo, by the same fingerprint, or a new one) and puts
//! the file at `Author/Title (Year)/Title.epub` under the books folder.

use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::Library;
use crate::epub::{EpubMetadata, read_metadata};
use crate::normalize::book_fingerprint;
use crate::{Error, Result};

/// Setting holding the books folder, when the user has chosen one.
pub const BOOKS_ROOT_SETTING: &str = "books_root";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddMode {
    /// Copy the file into the books folder.
    Copy,
    /// Hard-link it instead (no extra space; falls back to a copy across
    /// filesystems).
    Link,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AddOutcome {
    /// A book the library didn't know.
    NewBook,
    /// A book already in the library (from a Kobo, say) now has its file.
    AttachedToBook,
    /// This exact file is already in the library; nothing changed.
    AlreadyInLibrary,
}

#[derive(Debug, Clone, Serialize)]
pub struct AddedFile {
    pub book_id: i64,
    /// Where the file is now.
    pub path: PathBuf,
    pub outcome: AddOutcome,
}

/// What the book detail page shows beyond [`super::Book`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct BookDetails {
    pub description: Option<String>,
    /// As the book gives it (`1974`, `1974-05-01`, …).
    pub published: Option<String>,
    pub language: Option<String>,
    pub files: Vec<BookFile>,
}

impl BookDetails {
    /// The year of publication, if the date has one.
    pub fn year(&self) -> Option<&str> {
        let date = self.published.as_deref()?;
        date.get(..4)
            .filter(|y| y.bytes().all(|b| b.is_ascii_digit()))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BookFile {
    pub id: i64,
    pub book_id: i64,
    /// Absolute path (the stored path is relative to the books folder).
    pub path: PathBuf,
    pub format: String,
    pub size: u64,
}

/// `$XDG_DOCUMENTS_DIR/Books`, else `~/Documents/Books`, else `~/Books`.
pub fn default_books_root() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let documents = std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|dirs| documents_dir(&dirs, &home))
        .or_else(|| Some(home.join("Documents")).filter(|d| d.is_dir()));
    documents.unwrap_or(home).join("Books")
}

/// `XDG_DOCUMENTS_DIR` from a `user-dirs.dirs` file.
fn documents_dir(user_dirs: &str, home: &Path) -> Option<PathBuf> {
    let value = user_dirs
        .lines()
        .find_map(|l| l.trim().strip_prefix("XDG_DOCUMENTS_DIR="))?
        .trim()
        .trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // A documents dir set to $HOME itself means "none".
    (path != home && path.is_absolute()).then_some(path)
}

/// The file's format from its name: `kepub` or `epub`.
fn format_of(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".kepub.epub") {
        Some("kepub")
    } else if name.ends_with(".epub") {
        Some("epub")
    } else {
        None
    }
}

pub(crate) fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hasher.finalize().to_hex().to_string(), size))
}

/// A path component safe on every filesystem a book might be copied to
/// (FAT included): no `/ \ : * ? " < > |`, no control characters, no
/// leading or trailing dots or spaces, at most 100 characters.
fn safe_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed: String = cleaned
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .chars()
        .take(100)
        .collect();
    let trimmed = trimmed.trim_end().to_owned();
    if trimmed.is_empty() {
        "Untitled".to_owned()
    } else {
        trimmed
    }
}

/// `Author/Title (Year)/Title.ext`, relative to the books folder.
fn relative_dest(author: &str, title: &str, year: Option<&str>, format: &str) -> PathBuf {
    let folder = match year {
        Some(year) => format!("{title} ({year})"),
        None => title.to_owned(),
    };
    PathBuf::from(safe_component(author))
        .join(safe_component(&folder))
        .join(format!("{}.{}", safe_component(title), extension(format)))
}

/// The file extension for a format: `kepub.epub` for KEPUB, else the format.
fn extension(format: &str) -> String {
    match format {
        "kepub" => "kepub.epub".to_owned(),
        other => other.to_ascii_lowercase(),
    }
}

/// What the library records about a book, from whichever source knows it
/// (the EPUB itself, or a Calibre library).
#[derive(Debug, Clone, Default)]
pub(crate) struct BookFacts {
    pub title: String,
    /// As one string, like Kobo's `Attribution`, for the fingerprint.
    pub author: Option<String>,
    /// The first author in reading order, for the folder name.
    pub folder_author: Option<String>,
    pub author_sort: Option<String>,
    pub publisher: Option<String>,
    pub isbn: Option<String>,
    pub language: Option<String>,
    pub series: Option<String>,
    pub series_number: Option<String>,
    pub description: Option<String>,
    pub published: Option<String>,
    pub identifiers: Vec<(String, String)>,
    /// 0–10, as Calibre stores it (two per star).
    pub rating: Option<i64>,
    pub tags: Vec<String>,
    /// When the book entered its previous library.
    pub added: Option<chrono::DateTime<Utc>>,
}

impl BookFacts {
    fn from_epub(meta: &EpubMetadata, title: String) -> Self {
        BookFacts {
            title,
            author: meta.author_line(),
            folder_author: meta.authors.first().map(|a| a.display_name()),
            author_sort: meta.authors.first().and_then(|a| a.file_as.clone()),
            publisher: meta.publisher.clone(),
            isbn: meta.isbn.clone(),
            language: meta.language.clone(),
            series: meta.series.clone(),
            series_number: meta.series_index.clone(),
            description: meta.description.clone(),
            published: meta.published.clone(),
            identifiers: meta.identifiers.clone(),
            ..Default::default()
        }
    }

    fn year(&self) -> Option<&str> {
        let date = self.published.as_deref()?;
        date.get(..4)
            .filter(|y| y.bytes().all(|b| b.is_ascii_digit()))
    }
}

impl Library {
    /// The folder book files are kept in.
    pub fn books_root(&self) -> Result<PathBuf> {
        Ok(self
            .setting(BOOKS_ROOT_SETTING)?
            .map(PathBuf::from)
            .unwrap_or_else(default_books_root))
    }

    /// Changes the books folder. Files already there stay where they are;
    /// stored paths are relative, so moving the folder's contents along
    /// with it keeps them working.
    pub fn set_books_root(&self, root: &Path) -> Result<()> {
        crate::kobo::ensure_not_on_kobo(root)?;
        self.set_setting(BOOKS_ROOT_SETTING, &root.to_string_lossy())
    }

    /// The book and stored path of a file already in the library, by hash.
    pub(crate) fn file_by_hash(&self, hash: &str) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT book_id, path FROM book_file WHERE hash = ?1",
                [hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Adds an EPUB to the library. A file already inside the books folder
    /// is adopted where it is; others are copied (or linked) into it.
    pub fn add_file(&self, src: &Path, mode: AddMode) -> Result<AddedFile> {
        let format = format_of(src).ok_or_else(|| Error::UnsupportedFormat(src.to_path_buf()))?;
        let root = self.books_root()?;
        let (hash, size) = hash_file(src)?;
        if let Some((book_id, path)) = self.file_by_hash(&hash)? {
            return Ok(AddedFile {
                book_id,
                path: root.join(path),
                outcome: AddOutcome::AlreadyInLibrary,
            });
        }

        let meta = read_metadata(src)?;
        let title = meta.title.clone().unwrap_or_else(|| {
            src.file_name()
                .map(|n| {
                    n.to_string_lossy()
                        .trim_end_matches(".epub")
                        .trim_end_matches(".kepub")
                        .to_owned()
                })
                .unwrap_or_else(|| "Untitled".to_owned())
        });
        let facts = BookFacts::from_epub(&meta, title);
        let placed = self.place_file(src, &root, &facts, format, mode)?;
        let dest = root.join(&placed.relative);

        let result = (|| {
            let tx = self.conn.unchecked_transaction()?;
            let (book_id, outcome) = find_or_create_book(&tx, &facts, false)?;
            insert_file(
                &tx,
                book_id,
                &placed.relative,
                format,
                size,
                &hash,
                meta.unique_id.as_deref(),
            )?;
            tx.commit()?;
            Ok::<_, Error>((book_id, outcome))
        })();
        if result.is_err() && placed.created {
            let _ = std::fs::remove_file(&dest);
        }
        let (book_id, outcome) = result?;
        self.store_cover(book_id, src);
        Ok(AddedFile {
            book_id,
            path: dest,
            outcome,
        })
    }

    /// Puts a file in the books folder: adopted in place if it's already
    /// inside it, else copied or linked to a free `Author/Title (Year)/`.
    pub(crate) fn place_file(
        &self,
        src: &Path,
        root: &Path,
        facts: &BookFacts,
        format: &str,
        mode: AddMode,
    ) -> Result<PlacedFile> {
        let inside = src
            .canonicalize()
            .ok()
            .zip(root.canonicalize().ok())
            .and_then(|(s, r)| s.strip_prefix(&r).ok().map(Path::to_path_buf));
        if let Some(relative) = inside {
            return Ok(PlacedFile {
                relative,
                created: false,
            });
        }
        let author = facts.folder_author.as_deref().unwrap_or("Unknown author");
        let base = relative_dest(author, &facts.title, facts.year(), format);
        let mut relative = base.clone();
        let mut n = 2;
        while root.join(&relative).exists() {
            let folder = base.parent().unwrap_or(Path::new(""));
            let named = format!(
                "{} ({n})",
                folder.file_name().unwrap_or_default().to_string_lossy()
            );
            relative = folder
                .with_file_name(named)
                .join(base.file_name().unwrap_or_default());
            n += 1;
        }
        let dest = root.join(&relative);
        crate::kobo::ensure_not_on_kobo(&dest)?;
        std::fs::create_dir_all(dest.parent().unwrap_or(root))?;
        let linked = mode == AddMode::Link && std::fs::hard_link(src, &dest).is_ok();
        if !linked {
            std::fs::copy(src, &dest)?;
        }
        Ok(PlacedFile {
            relative,
            created: true,
        })
    }

    /// Gives a book without a cover the one inside its EPUB. Best effort: a
    /// book without a readable cover just has none.
    pub(crate) fn store_cover(&self, book_id: i64, epub: &Path) {
        let Some(dir) = self.assets_dir() else { return };
        let has_cover: bool = self
            .conn
            .query_row(
                "SELECT cover_path IS NOT NULL FROM book WHERE id = ?1",
                [book_id],
                |r| r.get(0),
            )
            .unwrap_or(true);
        if has_cover {
            return;
        }
        let Ok(Some((bytes, ext))) = crate::kobo::epub::cover_image(epub) else {
            return;
        };
        let dest = dir.join("covers").join(format!("book-{book_id}.{ext}"));
        let written = std::fs::create_dir_all(dest.parent().unwrap_or(&dir))
            .and_then(|_| std::fs::write(&dest, bytes));
        if written.is_ok() {
            let _ = self.conn.execute(
                "UPDATE book SET cover_path = ?2 WHERE id = ?1",
                params![book_id, dest.to_string_lossy()],
            );
        }
    }

    pub fn book_details(&self, book_id: i64) -> Result<BookDetails> {
        let (description, published, language) = self
            .conn
            .query_row(
                "SELECT description, published, language FROM book WHERE id = ?1",
                [book_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .unwrap_or_default();
        Ok(BookDetails {
            description,
            published,
            language,
            files: self.book_files(book_id)?,
        })
    }

    /// A book's files, oldest first.
    pub fn book_files(&self, book_id: i64) -> Result<Vec<BookFile>> {
        let root = self.books_root()?;
        let mut stmt = self.conn.prepare(
            "SELECT id, book_id, path, format, size FROM book_file WHERE book_id = ?1 ORDER BY id",
        )?;
        let files = stmt
            .query_map([book_id], |r| {
                Ok(BookFile {
                    id: r.get(0)?,
                    book_id: r.get(1)?,
                    path: root.join(r.get::<_, String>(2)?),
                    format: r.get(3)?,
                    size: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(files)
    }
}

/// Where [`Library::place_file`] put a file.
pub(crate) struct PlacedFile {
    /// Relative to the books folder.
    pub relative: PathBuf,
    /// A new copy or link (to remove again if recording it fails).
    pub created: bool,
}

/// The book these facts describe: one with the same fingerprint (known from
/// a Kobo, say), or a new one. A found book gets the facts it's missing, or
/// all of them when `overwrite` (a curated source such as Calibre).
pub(crate) fn find_or_create_book(
    tx: &rusqlite::Transaction,
    facts: &BookFacts,
    overwrite: bool,
) -> Result<(i64, AddOutcome)> {
    let now = Utc::now();
    let fingerprint = book_fingerprint(&facts.title, facts.author.as_deref());
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM book WHERE fingerprint = ?1",
            [&fingerprint],
            |r| r.get(0),
        )
        .optional()?;
    let (book_id, outcome) = match existing {
        Some(id) => (id, AddOutcome::AttachedToBook),
        None => {
            let id = tx.query_row(
                "INSERT INTO book (fingerprint, title, author, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
                params![
                    fingerprint,
                    facts.title,
                    facts.author,
                    facts.added.unwrap_or(now),
                    now
                ],
                |r| r.get(0),
            )?;
            (id, AddOutcome::NewBook)
        }
    };
    let identifiers = (!facts.identifiers.is_empty())
        .then(|| serde_json::to_string(&facts.identifiers))
        .transpose()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    // Fill in what the library doesn't know yet; never overwrite what a
    // device or the user recorded, unless the source is curated.
    let merge = if overwrite {
        "coalesce(?, {c})"
    } else {
        "coalesce({c}, ?)"
    };
    let set = |c: &str| format!("{c} = {}", merge.replace("{c}", c));
    let columns = [
        "publisher",
        "isbn",
        "language",
        "series",
        "series_number",
        "description",
        "published",
        "identifiers",
        "author_sort",
        "rating",
    ];
    let assignments: Vec<String> = columns.iter().map(|c| set(c)).collect();
    tx.execute(
        &format!(
            "UPDATE book SET {}, updated_at = ? WHERE id = ?",
            assignments.join(", ")
        ),
        params![
            facts.publisher,
            facts.isbn,
            facts.language,
            facts.series,
            facts.series_number,
            facts.description,
            facts.published,
            identifiers,
            facts.author_sort,
            facts.rating,
            now,
            book_id
        ],
    )?;
    for tag in &facts.tags {
        tx.execute(
            "INSERT OR IGNORE INTO book_tag (book_id, name) VALUES (?1, ?2)",
            params![book_id, tag],
        )?;
    }
    Ok((book_id, outcome))
}

pub(crate) fn insert_file(
    tx: &rusqlite::Transaction,
    book_id: i64,
    relative: &Path,
    format: &str,
    size: u64,
    hash: &str,
    epub_uid: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO book_file (book_id, path, format, size, hash, epub_uid, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            book_id,
            relative.to_string_lossy(),
            format,
            size as i64,
            hash,
            epub_uid,
            Utc::now()
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub::tests::write_epub;

    fn opf(title: &str, author: &str, extra: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
            <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
            <dc:title>{title}</dc:title><dc:creator opf:role="aut">{author}</dc:creator>
            <dc:identifier id="id">urn:uuid:{title}</dc:identifier><dc:date>1974-01-01</dc:date>{extra}
            </metadata></package>"#
        )
    }

    fn library() -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        (dir, lib)
    }

    #[test]
    fn adds_copies_and_deduplicates() {
        let (dir, lib) = library();
        let src = dir.path().join("download.epub");
        write_epub(
            &src,
            &opf(
                "The Dispossessed",
                "Ursula K. Le Guin",
                r#"<dc:publisher>Harper</dc:publisher><meta name="calibre:series" content="Hainish"/>"#,
            ),
        );

        let added = lib.add_file(&src, AddMode::Copy).unwrap();
        assert_eq!(added.outcome, AddOutcome::NewBook);
        assert_eq!(
            added.path,
            dir.path()
                .join("Books/Ursula K. Le Guin/The Dispossessed (1974)/The Dispossessed.epub")
        );
        assert!(added.path.is_file() && src.is_file());
        let book = lib.book(added.book_id).unwrap().unwrap();
        assert_eq!(book.title, "The Dispossessed");
        let files = lib.book_files(added.book_id).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, added.path);
        assert_eq!(files[0].format, "epub");

        // The same file again, from anywhere: nothing new.
        let again = lib.add_file(&added.path, AddMode::Copy).unwrap();
        assert_eq!(again.outcome, AddOutcome::AlreadyInLibrary);
        assert_eq!(again.book_id, added.book_id);
        assert_eq!(lib.book_files(added.book_id).unwrap().len(), 1);
    }

    #[test]
    fn attaches_files_to_books_known_from_a_kobo() {
        let (dir, lib) = library();
        // A book the library knows only from highlights on a Kobo.
        let fp = book_fingerprint("Annihilation", Some("Jeff VanderMeer"));
        lib.conn
            .execute(
                "INSERT INTO book (fingerprint, title, author, isbn, created_at, updated_at)
                 VALUES (?1, 'Annihilation', 'Jeff VanderMeer', '111', ?2, ?2)",
                params![fp, Utc::now()],
            )
            .unwrap();
        let src = dir.path().join("a.epub");
        write_epub(
            &src,
            &opf(
                "Annihilation",
                "Jeff VanderMeer",
                "<dc:language>en</dc:language><dc:identifier>urn:isbn:9780374104092</dc:identifier>",
            ),
        );
        let added = lib.add_file(&src, AddMode::Copy).unwrap();
        assert_eq!(added.outcome, AddOutcome::AttachedToBook);
        let (isbn, language): (String, String) = lib
            .conn
            .query_row(
                "SELECT isbn, language FROM book WHERE id = ?1",
                [added.book_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // The device's ISBN stays; the missing language is filled in.
        assert_eq!((isbn.as_str(), language.as_str()), ("111", "en"));
    }

    #[test]
    fn adopts_files_already_in_the_books_folder_and_keeps_names_apart() {
        let (dir, lib) = library();
        let inside = dir.path().join("Books/Somewhere/mine.kepub.epub");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        write_epub(&inside, &opf("Dune", "Frank Herbert", ""));
        let added = lib.add_file(&inside, AddMode::Copy).unwrap();
        assert_eq!(added.path, inside);
        assert_eq!(lib.book_files(added.book_id).unwrap()[0].format, "kepub");

        // A different edition with the same title and author lands beside it.
        let a = dir.path().join("x.epub");
        let b = dir.path().join("y.epub");
        write_epub(
            &a,
            &opf("Dune", "Frank Herbert", "<dc:subject>one</dc:subject>"),
        );
        write_epub(
            &b,
            &opf("Dune", "Frank Herbert", "<dc:subject>two</dc:subject>"),
        );
        let first = lib.add_file(&a, AddMode::Link).unwrap();
        let second = lib.add_file(&b, AddMode::Copy).unwrap();
        assert_eq!(
            first.path,
            dir.path().join("Books/Frank Herbert/Dune (1974)/Dune.epub")
        );
        assert_eq!(
            second.path,
            dir.path()
                .join("Books/Frank Herbert/Dune (1974) (2)/Dune.epub")
        );
        assert_eq!(lib.book_files(added.book_id).unwrap().len(), 3);
    }

    #[test]
    fn refuses_other_formats() {
        let (dir, lib) = library();
        let pdf = dir.path().join("x.pdf");
        std::fs::write(&pdf, b"%PDF").unwrap();
        assert!(matches!(
            lib.add_file(&pdf, AddMode::Copy),
            Err(Error::UnsupportedFormat(_))
        ));
    }

    #[test]
    fn finds_the_documents_folder() {
        let home = Path::new("/home/me");
        assert_eq!(
            documents_dir("# comment\nXDG_DOCUMENTS_DIR=\"$HOME/Dokumente\"\n", home),
            Some(PathBuf::from("/home/me/Dokumente"))
        );
        assert_eq!(documents_dir("XDG_DOCUMENTS_DIR=\"$HOME/\"", home), None);
        assert_eq!(safe_component(" ..A: Title?/x. "), "A_ Title__x");
        assert_eq!(safe_component("..."), "Untitled");
    }
}
