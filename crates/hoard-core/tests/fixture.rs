//! Tests against a real Kobo Libra Colour database (DbVersion 176).

use std::path::PathBuf;

use hoard_core::kobo::{AnnotationKind, KoboDb, find_kobo_db};

fn snapshot() -> hoard_core::kobo::KoboSnapshot {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/KoboReader.sqlite");
    KoboDb::open_copy(&find_kobo_db(&path).unwrap())
        .unwrap()
        .snapshot()
        .unwrap()
}

#[test]
fn reads_all_items() {
    let s = snapshot();
    assert_eq!(s.db_version, 176);
    assert_eq!(s.bookmarks.len(), 52);
    assert_eq!(s.hidden_count, 0);
    assert_eq!(s.words.len(), 12);
    assert_eq!(s.books.len(), 5);
    assert!(
        s.books
            .iter()
            .all(|b| b.is_sideloaded() && b.author.is_some())
    );
}

#[test]
fn classifies_kinds_and_cleans_text() {
    let s = snapshot();
    let count = |k: AnnotationKind| s.bookmarks.iter().filter(|b| b.kind == k).count();
    assert_eq!(count(AnnotationKind::Highlight), 49);
    assert_eq!(count(AnnotationKind::Note), 2);
    assert_eq!(count(AnnotationKind::Markup), 1);

    let markup = s
        .bookmarks
        .iter()
        .find(|b| b.kind == AnnotationKind::Markup)
        .unwrap();
    assert!(markup.text.is_none() && markup.extra_data.is_some());

    let note = s
        .bookmarks
        .iter()
        .find(|b| b.note.as_deref() == Some("Nice simile"))
        .unwrap();
    assert_eq!(
        note.text.as_deref(),
        Some("The moon began to rise, lean and haggard, like a skull among the stars.")
    );
    for b in s.bookmarks.iter().filter_map(|b| b.text.as_deref()) {
        assert_eq!(b, b.trim());
    }
}

#[test]
fn resolves_chapter_titles() {
    let s = snapshot();
    let titled = s
        .bookmarks
        .iter()
        .filter(|b| b.chapter_title.is_some())
        .count();
    assert_eq!(
        titled,
        s.bookmarks.len(),
        "every bookmark should resolve a chapter"
    );
    let ash = s
        .bookmarks
        .iter()
        .find(|b| b.content_id.contains("chapter009.xhtml#Ref_16114"))
        .unwrap();
    assert_eq!(ash.chapter_title.as_deref(), Some("9. Inroads"));
    let kjv = s
        .bookmarks
        .iter()
        .find(|b| b.content_id.ends_with("#pgepubid00042"))
        .unwrap();
    assert_eq!(
        kjv.chapter_title.as_deref(),
        Some("The Gospel According to Saint Matthew")
    );
    // The markup has no anchor, but follows Matthew highlights in the same file.
    let markup = s
        .bookmarks
        .iter()
        .find(|b| b.kind == AnnotationKind::Markup)
        .unwrap();
    assert_eq!(
        markup.chapter_title.as_deref(),
        Some("The Gospel According to Saint Matthew")
    );
}

#[test]
fn reads_vocab() {
    let s = snapshot();
    let w = s.words.iter().find(|w| w.word == "séances").unwrap();
    assert_eq!(w.language.as_deref(), Some("en"));
    assert!(w.volume_id.as_deref().unwrap().contains("Hellenic Tantra"));
    assert!(w.created.is_some());
}

/// A writable copy of the fixture, changed by `sql`.
fn modified_fixture(sql: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("KoboReader.sqlite");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/KoboReader.sqlite"),
        &path,
    )
    .unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
    (dir, path)
}

#[test]
fn reads_untested_versions() {
    assert!(hoard_core::kobo::is_tested_db_version(176));
    let (_dir, path) = modified_fixture("UPDATE DbVersion SET version = 999");
    let s = KoboDb::open_copy(&path).unwrap().snapshot().unwrap();
    assert_eq!(s.db_version, 999);
    assert!(!hoard_core::kobo::is_tested_db_version(s.db_version));
    assert_eq!(s.bookmarks.len(), 52);
}

#[test]
fn explains_failures_on_untested_versions() {
    let (_dir, path) = modified_fixture(
        "UPDATE DbVersion SET version = 999; ALTER TABLE WordList RENAME COLUMN Text TO Word",
    );
    let err = KoboDb::open_copy(&path).unwrap().snapshot().unwrap_err();
    assert!(matches!(
        err,
        hoard_core::Error::UntestedDb { version: 999, .. }
    ));
    assert!(err.to_string().contains("version 999"), "{err}");

    // On a tested version the plain error comes through, naming the column.
    let (_dir, path) = modified_fixture("ALTER TABLE WordList RENAME COLUMN Text TO Word");
    let err = KoboDb::open_copy(&path).unwrap().snapshot().unwrap_err();
    assert!(
        matches!(&err, hoard_core::Error::MissingColumn { table, column }
            if table == "WordList" && column == "Text"),
        "{err}"
    );
}

#[test]
fn reads_kobos_without_colour() {
    // Like a Clara 2E: DbVersion 174, and no Color column in Bookmark.
    let (_dir, path) = modified_fixture(
        "UPDATE DbVersion SET version = 174; ALTER TABLE Bookmark DROP COLUMN Color",
    );
    let s = KoboDb::open_copy(&path).unwrap().snapshot().unwrap();
    assert_eq!(s.db_version, 174);
    assert!(hoard_core::kobo::is_tested_db_version(174));
    assert_eq!(s.bookmarks.len(), 52);
    assert!(s.bookmarks.iter().all(|b| b.color == "yellow"));
    assert_eq!(s.words.len(), 12);
}

#[test]
fn reads_databases_missing_optional_columns() {
    // Columns a future (or older) firmware might lack. Kinds come from the
    // rows themselves when there's no Type column.
    let (_dir, path) = modified_fixture(
        "ALTER TABLE Bookmark DROP COLUMN Type;
         ALTER TABLE Bookmark DROP COLUMN DateModified;
         ALTER TABLE Bookmark DROP COLUMN ChapterProgress;
         ALTER TABLE content DROP COLUMN ImageId;
         ALTER TABLE content DROP COLUMN Language;
         ALTER TABLE WordList DROP COLUMN DictSuffix;",
    );
    let s = KoboDb::open_copy(&path).unwrap().snapshot().unwrap();
    assert_eq!(s.bookmarks.len(), 52);
    let count = |k: AnnotationKind| s.bookmarks.iter().filter(|b| b.kind == k).count();
    assert_eq!(
        (
            count(AnnotationKind::Highlight),
            count(AnnotationKind::Note),
            count(AnnotationKind::Markup)
        ),
        (49, 2, 1)
    );
    assert!(s.bookmarks.iter().all(|b| b.chapter_title.is_some()));
    assert_eq!(s.books.len(), 5);
    assert!(s.books.iter().all(|b| b.image_id.is_none()));
    assert_eq!(s.words.len(), 12);
    assert!(s.words.iter().all(|w| w.language.is_none()));
}

#[test]
fn reads_databases_without_optional_tables() {
    let (_dir, path) = modified_fixture("DROP TABLE WordList");
    let s = KoboDb::open_copy(&path).unwrap().snapshot().unwrap();
    assert_eq!(s.bookmarks.len(), 52);
    assert!(s.words.is_empty());
}

#[test]
fn schema_report_has_tables_but_no_data() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/KoboReader.sqlite");
    let db = KoboDb::open_copy(&path).unwrap();
    let device = hoard_core::kobo::DeviceInfo::parse(
        "N000000000123,4.9.77,4.45.23697,4.9.77,4.9.77,00000000-0000-0000-0000-000000000390",
    )
    .unwrap();
    let report = db.schema_report(Some(&device)).unwrap();
    assert!(report.contains("CREATE TABLE Bookmark"));
    assert!(report.contains("-- DbVersion: 176"));
    assert!(report.contains("-- model: Kobo Libra Colour (00000000-0000-0000-0000-000000000390)"));
    assert!(report.contains("-- firmware: 4.45.23697"));
    assert!(!report.contains("N000000000123"));
    assert!(!report.contains("Nice simile"));
    assert!(!report.contains("haggard"));
}
