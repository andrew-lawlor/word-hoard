//! Tests against a real Kobo Libra Colour database (DbVersion 176, firmware
//! 4.45), taken from Kollate's test suite.

use std::path::PathBuf;

use hoard_core::AnnotationKind;
use hoard_kobo::{KoboDb, Snapshot};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libra-colour-176.sqlite")
}

fn open() -> (Snapshot, KoboDb) {
    let snapshot = Snapshot::take(&fixture()).unwrap();
    let db = snapshot.open().unwrap();
    (snapshot, db)
}

/// A changed copy of the fixture.
fn modified(sql: &str) -> (Snapshot, KoboDb) {
    let snapshot = Snapshot::take(&fixture()).unwrap();
    rusqlite::Connection::open(snapshot.path()).unwrap().execute_batch(sql).unwrap();
    let db = snapshot.open().unwrap();
    (snapshot, db)
}

#[test]
fn reads_all_annotations() {
    let (_s, db) = open();
    assert_eq!(db.db_version().unwrap(), Some(176));
    let books = db.annotations("kobo:test", false).unwrap();
    assert_eq!(books.len(), 5);
    assert!(books.iter().all(|b| b.book.on_device && !b.book.authors.is_empty()));

    let all: Vec<_> = books.iter().flat_map(|b| &b.annotations).collect();
    assert_eq!(all.len(), 52);
    let count = |k| all.iter().filter(|a| a.kind == k).count();
    assert_eq!(count(AnnotationKind::Highlight), 49);
    assert_eq!(count(AnnotationKind::Note), 2);
    assert_eq!(count(AnnotationKind::Markup), 1);

    let stats = db.stats().unwrap();
    assert_eq!((stats.highlights, stats.notes, stats.markups, stats.hidden), (49, 2, 1, 0));
}

#[test]
fn cleans_text() {
    let (_s, db) = open();
    let books = db.annotations("s", false).unwrap();
    let all: Vec<_> = books.iter().flat_map(|b| &b.annotations).collect();
    let note = all.iter().find(|a| a.note.as_deref() == Some("Nice simile")).unwrap();
    assert_eq!(
        note.text.as_deref(),
        Some("The moon began to rise, lean and haggard, like a skull among the stars.")
    );
    for text in all.iter().filter_map(|a| a.text.as_deref()) {
        assert_eq!(text, text.trim());
    }
    let markup = all.iter().find(|a| a.kind == AnnotationKind::Markup).unwrap();
    assert!(markup.text.is_none());
}

#[test]
fn resolves_every_chapter_title() {
    let (_s, db) = open();
    let books = db.annotations("s", false).unwrap();
    let all: Vec<_> = books.iter().flat_map(|b| &b.annotations).collect();
    assert!(all.iter().all(|a| a.chapter.is_some()), "every annotation should have a chapter");

    let by_content = |suffix: &str| {
        all.iter()
            .find(|a| a.position.content_id.as_deref().is_some_and(|c| c.contains(suffix)))
            .unwrap()
    };
    assert_eq!(by_content("chapter009.xhtml#Ref_16114").chapter.as_deref(), Some("9. Inroads"));
    assert_eq!(
        by_content("#pgepubid00042").chapter.as_deref(),
        Some("The Gospel According to Saint Matthew")
    );
    // The markup has no fragment, but follows Matthew highlights in its file.
    let markup = all.iter().find(|a| a.kind == AnnotationKind::Markup).unwrap();
    assert_eq!(markup.chapter.as_deref(), Some("The Gospel According to Saint Matthew"));
}

#[test]
fn annotations_are_in_reading_order() {
    let (_s, db) = open();
    for book in db.annotations("s", false).unwrap() {
        let spine: Vec<i64> = book.annotations.iter().filter_map(|a| a.position.chapter_index).collect();
        assert!(spine.windows(2).all(|w| w[0] <= w[1]), "{}", book.book.display_title());
    }
}

#[test]
fn reads_kobos_without_colour() {
    // Like a Clara 2E: DbVersion 174, and no Color column on Bookmark.
    let (_s, db) =
        modified("UPDATE DbVersion SET version = 174; ALTER TABLE Bookmark DROP COLUMN Color");
    assert_eq!(db.db_version().unwrap(), Some(174));
    let books = db.annotations("s", false).unwrap();
    let all: Vec<_> = books.iter().flat_map(|b| &b.annotations).collect();
    assert_eq!(all.len(), 52);
    assert!(
        all.iter()
            .filter(|a| a.kind != AnnotationKind::Markup)
            .all(|a| a.color.as_deref() == Some("yellow"))
    );
}

#[test]
fn finds_books_moved_on_the_device() {
    // calibre re-sent a book into another folder; old bookmarks keep the old path.
    let (_s, db) = modified(
        "UPDATE Bookmark SET VolumeID = replace(VolumeID, 'file:///mnt/onboard/', 'file:///mnt/onboard/old/')
         WHERE VolumeID LIKE '%Children of Ash and Elm%'",
    );
    let books = db.annotations("s", false).unwrap();
    let ash = books.iter().find(|b| b.book.title.as_deref() == Some("Children of Ash and Elm")).unwrap();
    assert!(ash.book.on_device);
    assert!(!ash.annotations.is_empty());
}
