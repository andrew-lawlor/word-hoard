use hoard_core::{AnnotationKind, BookOrigin, ReadStatus};
use hoard_kobo::fixture::{self, REMOVED_ID, SIDELOADED_ID, STORE_ID, Schema};
use hoard_kobo::{KoboDb, KoboDevice, Snapshot};

fn open(schema: Schema) -> (tempfile::TempDir, KoboDb) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("KoboReader.sqlite");
    fixture::create_db(&path, schema).unwrap();
    let db = KoboDb::open(&path).unwrap();
    (dir, db)
}

#[test]
fn reads_books_with_metadata() {
    let (_dir, db) = open(Schema::Modern);
    let books = db.books().unwrap();
    assert_eq!(books.len(), 3);
    let book = books.iter().find(|b| b.device_id == SIDELOADED_ID).unwrap();
    assert_eq!(book.title.as_deref(), Some("The Dispossessed"));
    assert_eq!(book.authors, vec!["Ursula K. Le Guin"]);
    assert_eq!(book.isbn.as_deref(), Some("9780060512750"));
    assert_eq!(book.percent_read, Some(87));
    assert_eq!(book.read_status, Some(ReadStatus::Reading));
    assert_eq!(book.origin, BookOrigin::Sideloaded);
    let store = books.iter().find(|b| b.device_id == STORE_ID).unwrap();
    assert_eq!(store.origin, BookOrigin::Store);
    assert_eq!(db.db_version().unwrap(), Some(174));
}

#[test]
fn annotations_are_grouped_ordered_and_classified() {
    let (_dir, db) = open(Schema::Modern);
    let books = db.annotations("kobo:N1", false).unwrap();
    let titles: Vec<_> = books.iter().map(|b| b.book.display_title()).collect();
    assert_eq!(titles, vec!["Gone Book", "Piranesi", "The Dispossessed"]);

    let dispossessed = &books[2];
    let ids: Vec<_> = dispossessed.annotations.iter().map(|a| a.source_id.as_str()).collect();
    // b4 is a dogear with no span path, so it sorts first within chapter one.
    assert_eq!(ids, vec!["b4", "b3", "b2", "b1"]);

    let note = &dispossessed.annotations[2];
    assert_eq!(note.kind, AnnotationKind::Note);
    assert_eq!(note.note.as_deref(), Some("Compare with Graeber."));
    assert_eq!(note.color.as_deref(), Some("blue"));
    // Title from the 899 TOC row, not the file-name title of the ContentType 9 row.
    assert_eq!(note.chapter.as_deref(), Some("Chapter One"));
    assert_eq!(dispossessed.annotations[3].chapter.as_deref(), Some("Chapter Two"));
    assert_eq!(dispossessed.annotations[0].kind, AnnotationKind::Bookmark);
    assert!(dispossessed.annotations.iter().all(|a| a.source == "kobo:N1"));

    let gone = &books[0];
    assert_eq!(gone.book.device_id, REMOVED_ID);
    assert!(!gone.book.on_device);
    assert_eq!(books[1].annotations[0].color.as_deref(), Some("green"));
}

#[test]
fn hidden_annotations_only_on_request() {
    let (_dir, db) = open(Schema::Modern);
    let count = |include: bool| -> usize {
        db.annotations("s", include).unwrap().iter().map(|b| b.annotations.len()).sum()
    };
    assert_eq!(count(false), 6);
    assert_eq!(count(true), 7);
    let stats = db.stats().unwrap();
    assert_eq!(
        (stats.books, stats.sideloaded, stats.store, stats.annotated_books),
        (3, 2, 1, 3)
    );
    assert_eq!((stats.highlights, stats.notes, stats.bookmarks, stats.hidden), (4, 1, 1, 1));
}

#[test]
fn legacy_schema_without_type_or_color() {
    let (_dir, db) = open(Schema::Legacy);
    let books = db.annotations("s", false).unwrap();
    let dispossessed = books.iter().find(|b| b.book.device_id == SIDELOADED_ID).unwrap();
    let kinds: Vec<_> = dispossessed.annotations.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        vec![
            AnnotationKind::Bookmark,
            AnnotationKind::Highlight,
            AnnotationKind::Note,
            AnnotationKind::Highlight
        ]
    );
    assert!(dispossessed.annotations.iter().all(|a| a.color.is_none()));
}

#[test]
fn schema_report_has_no_rows_or_serial() {
    let (_dir, db) = open(Schema::Modern);
    let version = hoard_kobo::VersionInfo::parse(fixture::VERSION_LINE).unwrap();
    let report = db.schema_report(Some(&version)).unwrap();
    assert!(report.contains("CREATE TABLE Bookmark"));
    assert!(report.contains("-- DbVersion: 174"));
    assert!(report.contains("-- firmware: 4.38.21908"));
    assert!(!report.contains("N418170000001"));
    assert!(!report.contains("True journey"));
}

#[test]
fn snapshot_leaves_device_untouched() {
    let dir = tempfile::tempdir().unwrap();
    fixture::create_device(dir.path()).unwrap();
    let device = KoboDevice::at(dir.path()).unwrap();
    assert_eq!(device.source_tag(), "kobo:N418170000001");

    let before = std::fs::read(device.db_path()).unwrap();
    let snapshot = Snapshot::take(&device.db_path()).unwrap();
    assert_ne!(snapshot.path(), device.db_path());
    let db = snapshot.open().unwrap();
    assert_eq!(db.annotations("s", false).unwrap().len(), 3);
    drop(db);
    assert_eq!(std::fs::read(device.db_path()).unwrap(), before);

    let entries: Vec<_> = std::fs::read_dir(dir.path().join(".kobo")).unwrap().collect();
    assert_eq!(entries.len(), 2, "no WAL or other files created on the device");
}

#[test]
fn rejects_non_kobo_paths() {
    let dir = tempfile::tempdir().unwrap();
    assert!(KoboDevice::at(dir.path()).is_err());
    let not_db = dir.path().join("x.sqlite");
    std::fs::write(&not_db, b"not a database").unwrap();
    assert!(KoboDb::open(&not_db).is_err());
}
