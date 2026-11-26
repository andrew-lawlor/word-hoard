use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use hoard_core::{
    Annotation, AnnotationKind, BookAnnotations, BookInfo, BookOrigin, Position, ReadStatus,
};
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};

use crate::{Error, Result, VersionInfo};

/// Reader over a Kobo database snapshot.
pub struct KoboDb {
    conn: Connection,
}

/// Counts for `word-hoard kobo info`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub books: usize,
    pub sideloaded: usize,
    pub store: usize,
    pub annotated_books: usize,
    pub highlights: usize,
    pub notes: usize,
    pub bookmarks: usize,
    pub markups: usize,
    pub hidden: usize,
}

const BOOK_COLUMNS: &[&str] = &[
    "ContentID",
    "Title",
    "Attribution",
    "ISBN",
    "Publisher",
    "Series",
    "SeriesNumber",
    "___PercentRead",
    "ReadStatus",
    "DateLastRead",
];

const CHAPTER_COLUMNS: &[&str] = &["ContentID", "Title", "VolumeIndex", "ContentType"];

/// Table-of-contents entries for KEPUBs. Their ids are the chapter file's id
/// plus `-1`, `-2`, …; their titles are the real chapter titles.
const TOC_TYPE: i64 = 899;

const BOOKMARK_COLUMNS: &[&str] = &[
    "BookmarkID",
    "VolumeID",
    "ContentID",
    "StartContainerPath",
    "StartOffset",
    "EndContainerPath",
    "EndOffset",
    "Text",
    "Annotation",
    "ChapterProgress",
    "DateCreated",
    "DateModified",
    "Hidden",
    "Type",
    "Color",
];

struct ChapterRow {
    title: Option<String>,
    index: Option<i64>,
    content_type: Option<i64>,
}

impl ChapterRow {
    fn is_toc(&self) -> bool {
        self.content_type == Some(TOC_TYPE)
    }
}

/// A resolved chapter: title for display, index for ordering.
struct Chapter {
    title: Option<String>,
    index: Option<i64>,
}

impl KoboDb {
    /// Opens a database file. Call this on a [`crate::Snapshot`], never on the
    /// live file on a device: SQLite may write to the WAL while reading.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        if table_columns(&conn, "content")?.is_empty() {
            return Err(Error::NotKoboDb(path.to_path_buf()));
        }
        Ok(Self { conn })
    }

    /// The schema version in the `DbVersion` table, if present.
    pub fn db_version(&self) -> Result<Option<i64>> {
        let rows = self.select("DbVersion", &["version"], "LIMIT 1")?;
        Ok(rows.first().and_then(|r| r.int(0)))
    }

    /// Every book on the device (`content` rows with `ContentType = 6`).
    pub fn books(&self) -> Result<Vec<BookInfo>> {
        let rows = self.select("content", BOOK_COLUMNS, "WHERE CAST(ContentType AS INTEGER) = 6")?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let id = r.str(0)?;
                Some(BookInfo {
                    origin: origin_of(&id),
                    title: r.str(1),
                    authors: r.str(2).map(|a| split_authors(&a)).unwrap_or_default(),
                    isbn: r.str(3),
                    publisher: r.str(4),
                    series: r.str(5),
                    series_index: r.str(6),
                    percent_read: r.int(7),
                    read_status: r.int(8).and_then(read_status),
                    last_read: r.str(9),
                    on_device: true,
                    device_id: id,
                })
            })
            .collect())
    }

    /// Annotations grouped by book, each book's list in reading order, books
    /// sorted by title. Books without annotations are left out. Hidden
    /// (deleted-on-device) annotations are only included when asked for.
    pub fn annotations(&self, source: &str, include_hidden: bool) -> Result<Vec<BookAnnotations>> {
        let books: HashMap<String, BookInfo> =
            self.books()?.into_iter().map(|b| (b.device_id.clone(), b)).collect();
        let chapters = self.chapters()?;

        let mut by_book: HashMap<String, Vec<Annotation>> = HashMap::new();
        for r in self.select("Bookmark", BOOKMARK_COLUMNS, "")? {
            let (Some(id), Some(volume)) = (r.str(0), r.str(1)) else {
                continue;
            };
            let hidden = r.flag(12);
            if hidden && !include_hidden {
                continue;
            }
            let text = r.str(7);
            let note = r.str(8);
            let kind = classify(r.str(13).as_deref(), text.is_some(), note.is_some());
            let chapter = r.str(2).map(|cid| find_chapter(&chapters, &cid));
            let chapter = chapter.as_ref();
            let color = match kind {
                AnnotationKind::Highlight | AnnotationKind::Note => r.int(14).map(color_name),
                _ => None,
            };
            by_book.entry(volume).or_default().push(Annotation {
                source: source.to_string(),
                source_id: id,
                kind,
                text,
                note,
                color,
                chapter: chapter.and_then(|c| c.title.clone()),
                position: Position {
                    chapter_index: chapter.and_then(|c| c.index),
                    chapter_progress: r.float(9),
                    start_path: r.str(3),
                    start_offset: r.int(4),
                    end_path: r.str(5),
                    end_offset: r.int(6),
                },
                created: r.str(10),
                modified: r.str(11),
                hidden,
            });
        }

        let mut out: Vec<BookAnnotations> = by_book
            .into_iter()
            .map(|(volume, mut annotations)| {
                annotations.sort_by(reading_order);
                let book = books.get(&volume).cloned().unwrap_or_else(|| missing_book(volume));
                BookAnnotations { book, annotations }
            })
            .collect();
        out.sort_by(|a, b| {
            a.book
                .sort_key()
                .cmp(&b.book.sort_key())
                .then_with(|| a.book.device_id.cmp(&b.book.device_id))
        });
        Ok(out)
    }

    pub fn stats(&self) -> Result<Stats> {
        let books = self.books()?;
        let annotated = self.annotations("", true)?;
        let mut stats = Stats {
            books: books.len(),
            sideloaded: books.iter().filter(|b| b.origin == BookOrigin::Sideloaded).count(),
            store: books.iter().filter(|b| b.origin == BookOrigin::Store).count(),
            annotated_books: annotated.len(),
            ..Stats::default()
        };
        for ann in annotated.iter().flat_map(|b| &b.annotations) {
            if ann.hidden {
                stats.hidden += 1;
                continue;
            }
            match ann.kind {
                AnnotationKind::Highlight => stats.highlights += 1,
                AnnotationKind::Note => stats.notes += 1,
                AnnotationKind::Bookmark => stats.bookmarks += 1,
                AnnotationKind::Markup => stats.markups += 1,
            }
        }
        Ok(stats)
    }

    /// Table and index definitions only (no rows, no serial number), for
    /// testers to share so write support can be verified per `DbVersion`.
    pub fn schema_report(&self, version: Option<&VersionInfo>) -> Result<String> {
        let mut out = String::from(
            "-- Word Hoard Kobo schema report\n\
             -- Table definitions only: no rows and no serial number.\n",
        );
        let firmware = version.and_then(|v| v.firmware.as_deref()).unwrap_or("unknown");
        let model = version.and_then(|v| v.model_id.as_deref()).unwrap_or("unknown");
        let db_version = self.db_version()?.map_or("unknown".to_string(), |v| v.to_string());
        out.push_str(&format!(
            "-- firmware: {firmware}\n-- model id: {model}\n-- DbVersion: {db_version}\n\n"
        ));
        let mut stmt = self.conn.prepare(
            "SELECT sql FROM sqlite_master \
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
             ORDER BY type DESC, name",
        )?;
        let statements = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for sql in statements {
            out.push_str(&sql?);
            out.push_str(";\n\n");
        }
        Ok(out)
    }

    fn chapters(&self) -> Result<BTreeMap<String, ChapterRow>> {
        let rows = self.select("content", CHAPTER_COLUMNS, "WHERE CAST(ContentType AS INTEGER) <> 6")?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let row = ChapterRow { title: r.str(1), index: r.int(2), content_type: r.int(3) };
                Some((r.str(0)?, row))
            })
            .collect())
    }

    /// Selects `wanted` columns from `table`, substituting NULL for columns
    /// this firmware's schema lacks. A missing table yields no rows.
    fn select(&self, table: &str, wanted: &[&str], filter: &str) -> Result<Vec<Row>> {
        let have = table_columns(&self.conn, table)?;
        if have.is_empty() {
            return Ok(Vec::new());
        }
        let columns: Vec<String> = wanted
            .iter()
            .map(|c| {
                if have.contains(&c.to_ascii_lowercase()) {
                    format!("\"{c}\"")
                } else {
                    format!("NULL AS \"{c}\"")
                }
            })
            .collect();
        let sql = format!("SELECT {} FROM \"{table}\" {filter}", columns.join(", "));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |r| {
                (0..wanted.len()).map(|i| r.get::<_, Value>(i)).collect::<rusqlite::Result<Vec<_>>>()
            })?
            .map(|r| r.map(Row))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Lower-cased column names of `table`; empty if the table does not exist.
fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .map(|n| n.map(|n| n.to_ascii_lowercase()))
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(names)
}

struct Row(Vec<Value>);

impl Row {
    /// Text value; blank strings count as missing.
    fn str(&self, i: usize) -> Option<String> {
        match &self.0[i] {
            Value::Text(t) if !t.trim().is_empty() => Some(t.clone()),
            Value::Integer(n) => Some(n.to_string()),
            Value::Real(f) => Some(f.to_string()),
            _ => None,
        }
    }

    fn int(&self, i: usize) -> Option<i64> {
        match &self.0[i] {
            Value::Integer(n) => Some(*n),
            Value::Real(f) => Some(*f as i64),
            Value::Text(t) => t.trim().parse().ok(),
            _ => None,
        }
    }

    fn float(&self, i: usize) -> Option<f64> {
        match &self.0[i] {
            Value::Integer(n) => Some(*n as f64),
            Value::Real(f) => Some(*f),
            Value::Text(t) => t.trim().parse().ok(),
            _ => None,
        }
    }

    /// Kobo stores booleans as 0/1 or as the strings 'true'/'false'.
    fn flag(&self, i: usize) -> bool {
        match &self.0[i] {
            Value::Integer(n) => *n != 0,
            Value::Text(t) => matches!(t.trim().to_ascii_lowercase().as_str(), "true" | "1"),
            _ => false,
        }
    }
}

fn classify(kind: Option<&str>, has_text: bool, has_note: bool) -> AnnotationKind {
    match kind.map(str::to_ascii_lowercase).as_deref() {
        Some("dogear") => AnnotationKind::Bookmark,
        Some("markup") => AnnotationKind::Markup,
        Some("note") => AnnotationKind::Note,
        Some("highlight") if has_note => AnnotationKind::Note,
        Some("highlight") => AnnotationKind::Highlight,
        // Older firmware has no Type column: infer from content.
        _ if has_note => AnnotationKind::Note,
        _ if has_text => AnnotationKind::Highlight,
        _ => AnnotationKind::Bookmark,
    }
}

/// Kobo colour codes as seen on colour models. To verify on a Libra Colour.
fn color_name(code: i64) -> String {
    match code {
        0 => "yellow".into(),
        1 => "pink".into(),
        2 => "blue".into(),
        3 => "green".into(),
        n => format!("colour {n}"),
    }
}

fn read_status(code: i64) -> Option<ReadStatus> {
    match code {
        0 => Some(ReadStatus::Unread),
        1 => Some(ReadStatus::Reading),
        2 => Some(ReadStatus::Finished),
        _ => None,
    }
}

fn origin_of(content_id: &str) -> BookOrigin {
    if content_id.starts_with("file://") {
        BookOrigin::Sideloaded
    } else if is_uuid(content_id) {
        BookOrigin::Store
    } else {
        BookOrigin::Unknown
    }
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn split_authors(attribution: &str) -> Vec<String> {
    attribution
        .split(" & ")
        .flat_map(|part| part.split(", "))
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(String::from)
        .collect()
}

/// A book that has annotations in the database but no `content` row: it was
/// removed from the device. Sideloaded ones get their file name as a title.
fn missing_book(volume: String) -> BookInfo {
    let title = volume.strip_prefix("file://").and_then(|path| {
        let name = path.rsplit('/').next()?;
        let stem = name
            .strip_suffix(".kepub.epub")
            .or_else(|| name.strip_suffix(".epub"))
            .unwrap_or(name);
        (!stem.is_empty()).then(|| stem.to_string())
    });
    BookInfo {
        origin: origin_of(&volume),
        device_id: volume,
        title,
        authors: Vec::new(),
        isbn: None,
        publisher: None,
        series: None,
        series_index: None,
        percent_read: None,
        read_status: None,
        last_read: None,
        on_device: false,
    }
}

/// Resolves a bookmark's `ContentID` (a chapter file) to a chapter.
///
/// Kobo keeps two kinds of rows per KEPUB chapter file: the file itself
/// (ContentType 9, titled with its file name, `VolumeIndex` in spine order)
/// and table-of-contents entries (ContentType 899, id + `-1`, real titles).
/// Ordering comes from the file row; the title from the first TOC entry, or
/// from the file row when its title is not just the file name.
fn find_chapter(chapters: &BTreeMap<String, ChapterRow>, content_id: &str) -> Chapter {
    let base = content_id.split('#').next().unwrap_or(content_id);
    let file_row = chapters.get(base).filter(|c| !c.is_toc());
    let toc_prefix = format!("{base}-");
    let toc_row = chapters
        .range(toc_prefix.clone()..)
        .take_while(|(id, _)| id.starts_with(&toc_prefix))
        .map(|(_, c)| c)
        .find(|c| c.is_toc() && c.title.is_some())
        .or_else(|| chapters.get(base).filter(|c| c.is_toc()));

    let file_name = base.rsplit(['!', '/']).next().unwrap_or(base);
    let title = toc_row
        .and_then(|c| c.title.clone())
        .or_else(|| file_row.and_then(|c| c.title.clone()).filter(|t| t != file_name));
    let index = file_row.and_then(|c| c.index).or_else(|| toc_row.and_then(|c| c.index));
    Chapter { title, index }
}

/// Chapter order, then position within the chapter (koboSpan ids such as
/// `span#kobo\.12\.3` compare by their numbers), then offset, then time.
fn reading_order(a: &Annotation, b: &Annotation) -> Ordering {
    let chapter = |x: &Annotation| x.position.chapter_index.unwrap_or(i64::MAX);
    let path = |x: &Annotation| x.position.start_path.as_deref().map(digit_runs).unwrap_or_default();
    chapter(a)
        .cmp(&chapter(b))
        .then_with(|| path(a).cmp(&path(b)))
        .then_with(|| a.position.start_offset.unwrap_or(0).cmp(&b.position.start_offset.unwrap_or(0)))
        .then_with(|| {
            let p = |x: &Annotation| x.position.chapter_progress.unwrap_or(0.0);
            p(a).total_cmp(&p(b))
        })
        .then_with(|| a.created.cmp(&b.created))
}

fn digit_runs(s: &str) -> Vec<u64> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter_map(|run| run.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_kinds() {
        assert_eq!(classify(Some("highlight"), true, false), AnnotationKind::Highlight);
        assert_eq!(classify(Some("highlight"), true, true), AnnotationKind::Note);
        assert_eq!(classify(Some("dogear"), false, false), AnnotationKind::Bookmark);
        assert_eq!(classify(Some("markup"), false, false), AnnotationKind::Markup);
        assert_eq!(classify(None, true, false), AnnotationKind::Highlight);
        assert_eq!(classify(None, false, false), AnnotationKind::Bookmark);
    }

    #[test]
    fn recognises_origins() {
        assert_eq!(origin_of("file:///mnt/onboard/a.epub"), BookOrigin::Sideloaded);
        assert_eq!(origin_of("3f2a9c1e-5b7d-4e8a-9c21-7d4e5f6a8b90"), BookOrigin::Store);
        assert_eq!(origin_of("something-else"), BookOrigin::Unknown);
    }

    #[test]
    fn splits_authors() {
        assert_eq!(split_authors("Ursula K. Le Guin"), vec!["Ursula K. Le Guin"]);
        assert_eq!(split_authors("A One, B Two & C Three"), vec!["A One", "B Two", "C Three"]);
    }

    #[test]
    fn span_paths_sort_numerically() {
        assert!(digit_runs(r"span#kobo\.2\.1") < digit_runs(r"span#kobo\.12\.1"));
    }
}
