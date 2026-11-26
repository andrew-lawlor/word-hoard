use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Utc};
use hoard_core::text::clean_opt;
use hoard_core::{
    Annotation, AnnotationKind, BookAnnotations, BookInfo, BookOrigin, Position, ReadStatus,
};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OpenFlags};

use crate::chapters::{ChapterIndex, Resolution, position_key};
use crate::paths::book_from_path;
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
    /// Stylus notebooks (`.nebo`), not exported yet.
    pub notebooks: usize,
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
    "MimeType",
];

const CHAPTER_COLUMNS: &[&str] = &["ContentID", "ContentType", "Title", "VolumeIndex", "BookID"];

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

/// Stylus notebooks are listed as books with a MyScript MIME type.
const NOTEBOOK_MIME: &str = "application/vnd.myscript.nebo";

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

    /// Every book on the device (`content` rows with `ContentType = 6`),
    /// not counting stylus notebooks.
    pub fn books(&self) -> Result<Vec<BookInfo>> {
        Ok(self.book_rows()?.into_iter().filter(|(_, notebook)| !notebook).map(|(b, _)| b).collect())
    }

    /// Books paired with whether each is a notebook.
    fn book_rows(&self) -> Result<Vec<(BookInfo, bool)>> {
        let rows = self.select("content", BOOK_COLUMNS, "WHERE CAST(ContentType AS INTEGER) = 6")?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let id = r.str(0)?;
                let notebook = r.str(10).is_some_and(|m| m.starts_with(NOTEBOOK_MIME));
                let book = BookInfo {
                    origin: origin_of(&id),
                    title: r.text(1),
                    authors: r.text(2).map(|a| split_authors(&a)).unwrap_or_default(),
                    isbn: r.text(3),
                    publisher: r.text(4),
                    series: r.text(5),
                    series_index: r.text(6),
                    percent_read: r.int(7),
                    read_status: r.int(8).and_then(read_status),
                    last_read: parse_kobo_date(r.str(9).as_deref()),
                    on_device: true,
                    device_id: id,
                };
                Some((book, notebook))
            })
            .collect())
    }

    /// Annotations grouped by book, each book's list in reading order, books
    /// sorted by title. Books without annotations are left out. Hidden
    /// (deleted-on-device) annotations are only included when asked for.
    pub fn annotations(&self, source: &str, include_hidden: bool) -> Result<Vec<BookAnnotations>> {
        let books = self.books()?;
        let by_id: HashMap<&str, &BookInfo> = books.iter().map(|b| (b.device_id.as_str(), b)).collect();
        let mut by_file_name: HashMap<&str, Vec<&BookInfo>> = HashMap::new();
        for b in &books {
            by_file_name.entry(file_name(&b.device_id)).or_default().push(b);
        }
        let indexes = self.chapter_indexes()?;

        // (annotation, chapter resolved exactly?) per book.
        let mut by_book: HashMap<String, Vec<(Annotation, bool)>> = HashMap::new();
        for r in self.select("Bookmark", BOOKMARK_COLUMNS, "")? {
            let (Some(id), Some(volume)) = (r.str(0), r.str(1)) else {
                continue;
            };
            let hidden = r.flag(12);
            if hidden && !include_hidden {
                continue;
            }
            let text = r.text(7);
            let note = r.text(8);
            let kind = classify(r.str(13).as_deref(), text.is_some(), note.is_some());
            let content_id = r.str(2);
            let index = indexes.get(&volume);
            let resolved = content_id.as_deref().and_then(|c| index?.resolve(c));
            let color = match kind {
                AnnotationKind::Highlight | AnnotationKind::Note => {
                    Some(kobo_color(r.int(14).unwrap_or(0)).to_string())
                }
                _ => None,
            };
            let annotation = Annotation {
                source: source.to_string(),
                source_id: id,
                kind,
                text,
                note,
                color,
                chapter: resolved.map(|(title, _)| title.to_string()),
                position: Position {
                    chapter_index: content_id.as_deref().and_then(|c| index?.spine_index(c)),
                    content_id,
                    chapter_progress: r.float(9),
                    start_path: r.str(3),
                    start_offset: r.int(4),
                    end_path: r.str(5),
                    end_offset: r.int(6),
                },
                created: parse_kobo_date(r.str(10).as_deref()),
                modified: parse_kobo_date(r.str(11).as_deref()),
                hidden,
            };
            let exact = resolved.is_none_or(|(_, how)| how == Resolution::Exact);
            by_book.entry(volume).or_default().push((annotation, exact));
        }

        let mut out: Vec<BookAnnotations> = by_book
            .into_iter()
            .map(|(volume, mut entries)| {
                entries.sort_by_key(|(a, _)| reading_key(a));
                carry_chapters_forward(&mut entries);
                let book = find_book(&volume, &by_id, &by_file_name);
                BookAnnotations { book, annotations: entries.into_iter().map(|(a, _)| a).collect() }
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
        let rows = self.book_rows()?;
        let books: Vec<&BookInfo> = rows.iter().filter(|(_, nb)| !nb).map(|(b, _)| b).collect();
        let annotated = self.annotations("", true)?;
        let mut stats = Stats {
            books: books.len(),
            sideloaded: books.iter().filter(|b| b.origin == BookOrigin::Sideloaded).count(),
            store: books.iter().filter(|b| b.origin == BookOrigin::Store).count(),
            notebooks: rows.len() - books.len(),
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

    /// Spine files and TOC entries, per book.
    fn chapter_indexes(&self) -> Result<HashMap<String, ChapterIndex>> {
        let rows = self.select(
            "content",
            CHAPTER_COLUMNS,
            "WHERE CAST(ContentType AS INTEGER) IN (9, 899)",
        )?;
        let mut indexes: HashMap<String, ChapterIndex> = HashMap::new();
        for r in rows {
            let (Some(id), Some(book)) = (r.str(0), r.str(4)) else {
                continue;
            };
            let ix = indexes.entry(book).or_default();
            let index = r.int(3).unwrap_or(0);
            match r.int(1) {
                Some(9) => ix.add_spine_file(&id, index),
                _ => ix.add_toc_entry(&id, &r.text(2).unwrap_or_default(), index),
            }
        }
        Ok(indexes)
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
                (0..wanted.len()).map(|i| r.get_ref(i).map(owned)).collect::<rusqlite::Result<Vec<_>>>()
            })?
            .map(|r| r.map(Row))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Copies a value out of SQLite. Kobo sometimes stores bytes that are not
/// valid UTF-8 in text columns, so text is decoded lossily.
fn owned(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Integer(i),
        ValueRef::Real(f) => Value::Real(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
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
    /// Raw text value; blank strings count as missing.
    fn str(&self, i: usize) -> Option<String> {
        match &self.0[i] {
            Value::Text(t) if !t.trim().is_empty() => Some(t.clone()),
            Value::Integer(n) => Some(n.to_string()),
            Value::Real(f) => Some(f.to_string()),
            _ => None,
        }
    }

    /// Text for display: Unicode-normalised, whitespace tidied.
    fn text(&self, i: usize) -> Option<String> {
        clean_opt(self.str(i).as_deref())
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

/// Kobo's mixed timestamp formats (`2026-08-13T21:24:36.152`,
/// `2026-08-13T21:24:36Z`, `2026-08-13 21:24:36`), all UTC. The
/// `0000-00-00T00:00:00.000` placeholder reads as no date.
fn parse_kobo_date(s: Option<&str>) -> Option<DateTime<Utc>> {
    let s = s?.trim().trim_end_matches('Z');
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|fmt| NaiveDateTime::parse_from_str(s, fmt).ok())
        .map(|dt| dt.and_utc())
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

/// `Bookmark.Color` (verified on a Libra Colour). Kobos without a colour
/// screen have no Color column; their highlights are the default, yellow.
fn kobo_color(code: i64) -> &'static str {
    match code {
        1 => "pink",
        2 => "blue",
        3 => "green",
        _ => "yellow",
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

/// `Attribution` holds either several names separated by commas (store
/// books: "Andy Weir, Jürgen Langowski") or one name surname-first as calibre
/// writes it ("Shaw, Gregory", "Lewis, C.S."). Two parts whose first is a
/// single word are read as the latter and turned round.
fn split_authors(attribution: &str) -> Vec<String> {
    let parts: Vec<&str> = attribution
        .split(" & ")
        .flat_map(|part| part.split(','))
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .collect();
    match parts.as_slice() {
        [last, first] if !last.contains(' ') && !attribution.contains(" & ") => {
            vec![format!("{first} {last}")]
        }
        _ => parts.into_iter().map(String::from).collect(),
    }
}

fn file_name(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// The book a bookmark's `VolumeID` belongs to: an exact match; else a book
/// moved on the device (calibre re-sends to another folder but keeps the file
/// name), when exactly one book has that file name; else a book removed from
/// the device, described from its path.
fn find_book(
    volume: &str,
    by_id: &HashMap<&str, &BookInfo>,
    by_file_name: &HashMap<&str, Vec<&BookInfo>>,
) -> BookInfo {
    if let Some(book) = by_id.get(volume) {
        return (*book).clone();
    }
    if volume.starts_with("file://")
        && let Some([book]) = by_file_name.get(file_name(volume)).map(Vec::as_slice)
    {
        return (*book).clone();
    }
    let (title, author) = match volume.starts_with("file://") {
        true => {
            let (title, author) = book_from_path(volume);
            (Some(title).filter(|t| !t.is_empty()), author)
        }
        false => (None, None),
    };
    BookInfo {
        origin: origin_of(volume),
        device_id: volume.to_string(),
        title,
        authors: author.map(|a| a.split(" & ").map(String::from).collect()).unwrap_or_default(),
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

/// Reading order within a book: spine position of the chapter file, then
/// position within it, then creation time.
fn reading_key(a: &Annotation) -> (i64, String, Option<DateTime<Utc>>) {
    let p = &a.position;
    (
        p.chapter_index.unwrap_or(i64::MAX),
        position_key(
            p.start_path.as_deref().unwrap_or(""),
            p.start_offset.unwrap_or(0),
            p.chapter_progress.unwrap_or(0.0),
        ),
        a.created,
    )
}

/// For annotations whose chapter was only guessed from the start of a file
/// holding several chapters, use the chapter of the closest preceding
/// annotation in the same file whose chapter is known exactly. `entries`
/// must already be in reading order.
fn carry_chapters_forward(entries: &mut [(Annotation, bool)]) {
    let file = |a: &Annotation| {
        a.position.content_id.as_deref().unwrap_or("").split('#').next().unwrap_or("").to_string()
    };
    let mut last_exact: Option<(String, Option<String>)> = None;
    for (annotation, exact) in entries.iter_mut() {
        let f = file(annotation);
        if *exact {
            last_exact = Some((f, annotation.chapter.clone()));
        } else if let Some((last_file, title)) = &last_exact
            && *last_file == f
        {
            annotation.chapter = title.clone();
        }
    }
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
        assert_eq!(split_authors("Andy Weir, Jürgen Langowski"), vec!["Andy Weir", "Jürgen Langowski"]);
        assert_eq!(split_authors("Shaw, Gregory"), vec!["Gregory Shaw"]);
        assert_eq!(split_authors("Lewis, C.S."), vec!["C.S. Lewis"]);
        assert_eq!(split_authors("Fitzgerald, Francis Scott"), vec!["Francis Scott Fitzgerald"]);
        assert_eq!(split_authors("Homer & Caroline Alexander"), vec!["Homer", "Caroline Alexander"]);
        assert_eq!(
            split_authors("Marcella Hazan, Lidia Matticchio Bastianich, Victor Hazan"),
            vec!["Marcella Hazan", "Lidia Matticchio Bastianich", "Victor Hazan"]
        );
    }

    #[test]
    fn parses_mixed_kobo_dates() {
        let a = parse_kobo_date(Some("2026-08-13T21:24:36.152")).unwrap();
        let b = parse_kobo_date(Some("2026-08-13T21:24:36Z")).unwrap();
        assert_eq!(a.timestamp(), b.timestamp());
        assert!(parse_kobo_date(Some("2026-08-13 21:24:36")).is_some());
        assert!(parse_kobo_date(Some("0000-00-00T00:00:00.000")).is_none());
        assert!(parse_kobo_date(None).is_none());
    }

    #[test]
    fn names_colours() {
        let names: Vec<_> = (0..5).map(kobo_color).collect();
        assert_eq!(names, ["yellow", "pink", "blue", "green", "yellow"]);
    }
}
