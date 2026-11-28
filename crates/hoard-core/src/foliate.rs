//! Reading Foliate's highlights and progress (Foliate 3: one JSON file per
//! book in its data folder, named by the book's identifier).
//!
//! Each file has the book's metadata (`identifier`, the EPUB's unique ID,
//! which Calibre sets and Word Hoard records per file), `progress`
//! (`[current, total]`), `lastLocation` (an EPUB CFI) and `annotations`
//! (CFI, colour, text, note, created and changed times). Bookmarks have no
//! text, so, like the Kobo's dog-ears, they aren't imported.
//!
//! Foliate is found as a Flatpak, a system install, or a Snap. Its files are
//! only read.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::kobo::model::{AnnotationKind, KoboBook, KoboBookmark, KoboSnapshot, Position};

/// Prefix of the IDs Foliate highlights get in Word Hoard, and of the
/// volume IDs its books are recorded under.
pub const ID_PREFIX: &str = "foliate:";

/// Foliate's data folders that exist, with how it's installed.
pub fn data_dirs(home: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut dirs = vec![
        (
            home.join(
                ".var/app/com.github.johnfactotum.Foliate/data/com.github.johnfactotum.Foliate",
            ),
            "flatpak",
        ),
        (
            home.join(".local/share/com.github.johnfactotum.Foliate"),
            "system",
        ),
        (
            home.join("snap/foliate/current/.local/share/com.github.johnfactotum.Foliate"),
            "snap",
        ),
    ];
    dirs.retain(|(d, _)| d.is_dir());
    dirs
}

/// A book as Foliate knows it.
#[derive(Debug, Clone)]
pub struct FoliateBook {
    /// Foliate's file for the book.
    pub path: PathBuf,
    /// The EPUB's unique identifier (Foliate's key for the book).
    pub identifier: String,
    pub title: Option<String>,
    pub author: Option<String>,
    /// How far through, 0–100.
    pub percent_read: Option<i64>,
    /// When Foliate last saved the book's file (it saves as you read).
    pub last_read: Option<DateTime<Utc>>,
    pub last_location: Option<String>,
    pub annotations: Vec<KoboBookmark>,
}

/// Every book in a Foliate data folder. Files that can't be read are skipped.
pub fn read_dir(dir: &Path) -> Vec<FoliateBook> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut books: Vec<FoliateBook> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| read_book(&p))
        .collect();
    books.sort_by(|a, b| a.identifier.cmp(&b.identifier));
    books
}

/// Text from a metadata value that may be a string, a language map
/// (`{"en": "…"}`), an object with a `name`, or a list of those.
fn text(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m
            .get("name")
            .and_then(text)
            .or_else(|| m.values().find_map(text)),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(text).collect();
            (!parts.is_empty()).then(|| parts.join(" & "))
        }
        _ => None,
    }?;
    let s = s.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

fn time(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let s = v?.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Foliate's colours as Word Hoard's; underlines and the like show as the
/// default colour; custom colours are kept as they are.
fn color(c: Option<&str>) -> String {
    match c.unwrap_or("yellow") {
        "magenta" => "pink",
        "aqua" => "cyan",
        "lime" => "green",
        "underline" | "squiggly" | "strikethrough" => crate::color::DEFAULT_COLOR,
        other => other,
    }
    .to_owned()
}

/// The spine index an EPUB CFI points into (`epubcfi(/6/14!…)`: the 7th
/// item, index 6).
fn spine_index(cfi: &str) -> Option<i64> {
    let inner = cfi.strip_prefix("epubcfi(")?;
    let step = inner.strip_prefix("/6/")?;
    let n: i64 = step
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    (n >= 2).then(|| n / 2 - 1)
}

fn read_book(path: &Path) -> Option<FoliateBook> {
    let json: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let meta = json.get("metadata")?;
    let identifier = meta
        .get("identifier")
        .and_then(text)
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))?;
    let percent_read = json.get("progress").and_then(|p| {
        let a = p.as_array()?;
        let (cur, total) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
        // A book just begun shows as 1%, not 0% (unread).
        (total > 0.0).then(|| {
            let pct = ((cur / total) * 100.0).round().clamp(0.0, 100.0) as i64;
            if cur > 0.0 { pct.max(1) } else { pct }
        })
    });
    let last_read = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<Utc>::from);
    let volume_id = format!("{ID_PREFIX}{identifier}");
    let annotations = json
        .get("annotations")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| annotation(a, &volume_id, &identifier))
                .collect()
        })
        .unwrap_or_default();
    Some(FoliateBook {
        path: path.to_path_buf(),
        title: meta.get("title").and_then(text),
        author: meta.get("author").and_then(text),
        identifier,
        percent_read,
        last_read,
        last_location: json.get("lastLocation").and_then(text),
        annotations,
    })
}

fn annotation(a: &Value, volume_id: &str, identifier: &str) -> Option<KoboBookmark> {
    let cfi = a.get("value")?.as_str()?;
    let highlighted = a.get("text").and_then(text)?;
    let note = a.get("note").and_then(text);
    let created = time(a.get("created"));
    let id = blake3::hash(format!("{identifier}\n{cfi}").as_bytes()).to_hex();
    Some(KoboBookmark {
        bookmark_id: format!("{ID_PREFIX}{}", &id[..32]),
        volume_id: volume_id.to_owned(),
        content_id: volume_id.to_owned(),
        kind: if note.is_some() {
            AnnotationKind::Note
        } else {
            AnnotationKind::Highlight
        },
        text: Some(highlighted),
        note,
        color: color(a.get("color").and_then(Value::as_str)),
        start: Position {
            container_path: cfi.to_owned(),
            child_index: 0,
            offset: 0,
        },
        end: Position {
            container_path: cfi.to_owned(),
            child_index: 0,
            offset: 0,
        },
        chapter_progress: 0.0,
        chapter_title: None,
        spine_index: spine_index(cfi),
        created,
        modified: time(a.get("modified")).or(created),
        extra_data: None,
    })
}

/// Sets where Foliate opens a book: its `lastLocation` (a CFI) and its
/// progress (`[current, total]`, kept in Foliate's own units) to
/// `fraction` of the way. Everything else in the file stays as it is; the
/// file is replaced whole, so Foliate never reads half of it.
pub fn set_position(file: &Path, cfi: &str, fraction: f64) -> crate::Result<()> {
    let mut json: Value = serde_json::from_str(&std::fs::read_to_string(file)?)
        .map_err(|e| crate::kobo::epub::io_err(format!("Foliate file: {e}")))?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| crate::kobo::epub::io_err("Foliate file isn't an object"))?;
    obj.insert("lastLocation".into(), Value::String(cfi.to_owned()));
    if let Some(total) = obj
        .get("progress")
        .and_then(Value::as_array)
        .and_then(|p| p.get(1))
        .and_then(Value::as_f64)
        .filter(|t| *t > 0.0)
    {
        let current = (fraction.clamp(0.0, 1.0) * total).round();
        obj.insert(
            "progress".into(),
            serde_json::json!([current as i64, total as i64]),
        );
    }
    let tmp = file.with_extension("json.word-hoard");
    std::fs::write(
        &tmp,
        serde_json::to_vec(&json).map_err(crate::kobo::epub::io_err)?,
    )?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

/// The books as a snapshot for [`crate::Library::import`]. `title_for`
/// gives the library's title and author for a book it has (so Foliate never
/// renames library books); others use Foliate's.
pub fn snapshot(
    books: &[FoliateBook],
    title_for: impl Fn(&FoliateBook) -> Option<(String, Option<String>)>,
) -> KoboSnapshot {
    let mut snap = KoboSnapshot::default();
    for b in books {
        let (title, author) = title_for(b).unwrap_or_else(|| {
            (
                b.title.clone().unwrap_or_else(|| b.identifier.clone()),
                b.author.clone(),
            )
        });
        snap.books.push(KoboBook {
            volume_id: format!("{ID_PREFIX}{}", b.identifier),
            title,
            author,
            publisher: None,
            isbn: None,
            language: None,
            series: None,
            series_number: None,
            image_id: None,
            percent_read: b.percent_read,
            last_read: b.last_read,
        });
        snap.bookmarks.extend(b.annotations.iter().cloned());
    }
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK: &str = r##"{
      "metadata": {
        "identifier": "11ac094d-a237-4bd2-b8d8-b83d06d8cfbc",
        "title": "Albion's Seed",
        "author": {"name": "David Hackett Fischer", "sortAs": "Fischer, David Hackett"}
      },
      "progress": [8, 3362],
      "lastLocation": "epubcfi(/6/14!/4,/22,/30/2/9:544)",
      "annotations": [
        {"value": "epubcfi(/6/14!/4/30/2,/5:133,/7:11)", "color": "yellow",
         "text": "Its problematiques were about", "note": "test",
         "created": "2026-11-27T04:53:10.779Z", "modified": "2026-11-27T04:53:15.449Z"},
        {"value": "epubcfi(/6/16!/4/2,/1:0,/1:20)", "color": "magenta", "text": "A second one"},
        {"value": "epubcfi(/6/16!/4/4)", "color": "yellow", "text": ""}
      ],
      "bookmarks": ["epubcfi(/6/18!/4/2)"]
    }"##;

    #[test]
    fn reads_foliates_books_and_highlights() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("11ac094d-a237-4bd2-b8d8-b83d06d8cfbc.json"),
            BOOK,
        )
        .unwrap();
        std::fs::write(dir.path().join("broken.json"), "{").unwrap();
        std::fs::create_dir(dir.path().join("library")).unwrap();
        let books = read_dir(dir.path());
        assert_eq!(books.len(), 1);
        let b = &books[0];
        assert_eq!(b.title.as_deref(), Some("Albion's Seed"));
        assert_eq!(b.author.as_deref(), Some("David Hackett Fischer"));
        assert_eq!(b.percent_read, Some(1), "just begun");
        assert!(b.last_read.is_some());
        assert_eq!(
            b.annotations.len(),
            2,
            "empty highlights and bookmarks are skipped"
        );
        let first = &b.annotations[0];
        assert_eq!(first.kind, AnnotationKind::Note);
        assert_eq!(first.note.as_deref(), Some("test"));
        assert_eq!(first.spine_index, Some(6));
        assert!(first.bookmark_id.starts_with(ID_PREFIX));
        assert_eq!(b.annotations[1].color, "pink");
        // Stable IDs: reading again gives the same.
        assert_eq!(
            read_dir(dir.path())[0].annotations[0].bookmark_id,
            first.bookmark_id
        );

        let snap = snapshot(&books, |_| Some(("Library Title".into(), None)));
        assert_eq!(snap.books[0].title, "Library Title");
        assert_eq!(
            snap.books[0].volume_id,
            "foliate:11ac094d-a237-4bd2-b8d8-b83d06d8cfbc"
        );
        assert_eq!(snap.bookmarks.len(), 2);
    }

    #[test]
    fn finds_foliate_where_it_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(data_dirs(dir.path()).is_empty());
        let flatpak = dir
            .path()
            .join(".var/app/com.github.johnfactotum.Foliate/data/com.github.johnfactotum.Foliate");
        std::fs::create_dir_all(&flatpak).unwrap();
        assert_eq!(data_dirs(dir.path()), [(flatpak, "flatpak")]);
    }
}
