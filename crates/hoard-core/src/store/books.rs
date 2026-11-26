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

fn hash_file(path: &Path) -> Result<(String, u64)> {
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
fn relative_dest(meta: &EpubMetadata, title: &str, format: &str) -> PathBuf {
    let author = meta
        .authors
        .first()
        .map_or_else(|| "Unknown author".to_owned(), |a| a.display_name());
    let folder = match meta.year() {
        Some(year) => format!("{title} ({year})"),
        None => title.to_owned(),
    };
    let ext = if format == "kepub" {
        "kepub.epub"
    } else {
        "epub"
    };
    PathBuf::from(safe_component(&author))
        .join(safe_component(&folder))
        .join(format!("{}.{ext}", safe_component(title)))
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

    /// Adds an EPUB to the library. A file already inside the books folder
    /// is adopted where it is; others are copied (or linked) into it.
    pub fn add_file(&self, src: &Path, mode: AddMode) -> Result<AddedFile> {
        let format = format_of(src).ok_or_else(|| Error::UnsupportedFormat(src.to_path_buf()))?;
        let root = self.books_root()?;
        let (hash, size) = hash_file(src)?;

        let known: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT book_id, path FROM book_file WHERE hash = ?1",
                [&hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((book_id, path)) = known {
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
        let author = meta.author_line();

        // Where the file goes: adopted in place inside the books folder, else
        // a free `Author/Title (Year)/` folder.
        let inside = src
            .canonicalize()
            .ok()
            .zip(root.canonicalize().ok())
            .and_then(|(s, r)| s.strip_prefix(&r).ok().map(Path::to_path_buf));
        let relative = match inside {
            Some(rel) => rel,
            None => {
                let mut rel = relative_dest(&meta, &title, format);
                let mut n = 2;
                while root.join(&rel).exists() {
                    let base = relative_dest(&meta, &title, format);
                    let folder = base.parent().unwrap_or(Path::new(""));
                    let named = format!(
                        "{} ({n})",
                        folder.file_name().unwrap_or_default().to_string_lossy()
                    );
                    rel = folder
                        .with_file_name(named)
                        .join(base.file_name().unwrap_or_default());
                    n += 1;
                }
                let dest = root.join(&rel);
                crate::kobo::ensure_not_on_kobo(&dest)?;
                std::fs::create_dir_all(dest.parent().unwrap_or(&root))?;
                let linked = mode == AddMode::Link && std::fs::hard_link(src, &dest).is_ok();
                if !linked {
                    std::fs::copy(src, &dest)?;
                }
                rel
            }
        };
        let dest = root.join(&relative);

        let result = self.record_file(
            &meta,
            &title,
            author.as_deref(),
            &relative,
            format,
            size,
            &hash,
        );
        if result.is_err() && created_by_add(&dest, src) {
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

    /// Records a file and its book in one transaction.
    #[allow(clippy::too_many_arguments)]
    fn record_file(
        &self,
        meta: &EpubMetadata,
        title: &str,
        author: Option<&str>,
        relative: &Path,
        format: &str,
        size: u64,
        hash: &str,
    ) -> Result<(i64, AddOutcome)> {
        let now = Utc::now();
        let fingerprint = book_fingerprint(title, author);
        let tx = self.conn.unchecked_transaction()?;
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
                     VALUES (?1, ?2, ?3, ?4, ?4) RETURNING id",
                    params![fingerprint, title, author, now],
                    |r| r.get(0),
                )?;
                (id, AddOutcome::NewBook)
            }
        };
        // Fill in what the library doesn't know yet; never overwrite what a
        // device or the user recorded.
        let identifiers = (!meta.identifiers.is_empty())
            .then(|| serde_json::to_string(&meta.identifiers))
            .transpose()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        tx.execute(
            "UPDATE book SET publisher = coalesce(publisher, ?2), isbn = coalesce(isbn, ?3),
                    language = coalesce(language, ?4), series = coalesce(series, ?5),
                    series_number = coalesce(series_number, ?6),
                    description = coalesce(description, ?7), published = coalesce(published, ?8),
                    identifiers = coalesce(identifiers, ?9), updated_at = ?10
             WHERE id = ?1",
            params![
                book_id,
                meta.publisher,
                meta.isbn,
                meta.language,
                meta.series,
                meta.series_index,
                meta.description,
                meta.published,
                identifiers,
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO book_file (book_id, path, format, size, hash, epub_uid, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                book_id,
                relative.to_string_lossy(),
                format,
                size as i64,
                hash,
                meta.unique_id,
                now
            ],
        )?;
        tx.commit()?;
        Ok((book_id, outcome))
    }

    /// Gives a book without a cover the one inside its EPUB. Best effort: a
    /// book without a readable cover just has none.
    fn store_cover(&self, book_id: i64, epub: &Path) {
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

/// Whether `dest` was created by this add (not the source adopted in place).
fn created_by_add(dest: &Path, src: &Path) -> bool {
    dest.canonicalize().ok() != src.canonicalize().ok()
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
