//! Searching the whole library at once: books and highlights, best matches
//! first, with the matching words marked in a snippet.

use rusqlite::params;
use serde::Serialize;

use super::{Library, query::fts_query};
use crate::Result;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BookHit {
    pub id: i64,
    pub title: String,
    pub author: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnnotationHit {
    pub id: i64,
    pub book_id: i64,
    pub book_title: String,
    pub chapter: Option<String>,
    /// The matching passage (or note), trimmed around the match, with the
    /// matched words between `[` and `]`.
    pub snippet: String,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SearchResults {
    pub books: Vec<BookHit>,
    pub annotations: Vec<AnnotationHit>,
}

impl Library {
    /// Books (by title, author, series, publisher, tags and description) and
    /// highlights (by text, note and chapter) matching every word of `text`,
    /// the last as a prefix; at most `limit` of each, best first. Trashed
    /// highlights are left out.
    pub fn search(&self, text: &str, limit: usize) -> Result<SearchResults> {
        let Some(query) = fts_query(text, None) else {
            return Ok(SearchResults::default());
        };
        let mut stmt = self.conn.prepare(
            "SELECT b.id, coalesce(b.user_title, b.title), coalesce(b.user_author, b.author)
             FROM book_fts f JOIN book b ON b.id = f.rowid
             WHERE book_fts MATCH ?1 AND NOT b.hidden
             ORDER BY bm25(book_fts, 10.0, 6.0, 4.0, 1.0, 3.0, 1.0)
             LIMIT ?2",
        )?;
        let books = stmt
            .query_map(params![query, limit as i64], |r| {
                Ok(BookHit {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    author: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut stmt = self.conn.prepare(
            "SELECT a.id, a.book_id, coalesce(b.user_title, b.title), a.chapter_title,
                    snippet(annotation_fts, 0, '[', ']', '…', 24),
                    snippet(annotation_fts, 1, '[', ']', '…', 24)
             FROM annotation_fts f JOIN annotation a ON a.id = f.rowid JOIN book b ON b.id = a.book_id
             WHERE annotation_fts MATCH ?1 AND a.status != 'trashed'
             ORDER BY bm25(annotation_fts, 4.0, 3.0, 1.0)
             LIMIT ?2",
        )?;
        let annotations = stmt
            .query_map(params![query, limit as i64], |r| {
                Ok(AnnotationHit {
                    id: r.get(0)?,
                    book_id: r.get(1)?,
                    book_title: r.get(2)?,
                    chapter: r.get(3)?,
                    // The passage, unless only the note matched.
                    snippet: {
                        let text: String = r.get(4)?;
                        let note: String = r.get(5)?;
                        if !text.contains('[') && note.contains('[') {
                            note
                        } else {
                            text
                        }
                    },
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(SearchResults { books, annotations })
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::store::BookSort;
    use crate::store::query::fts_query;

    fn book(
        lib: &Library,
        title: &str,
        author: &str,
        series: Option<&str>,
        description: Option<&str>,
    ) -> i64 {
        lib.conn
            .query_row(
                "INSERT INTO book (fingerprint, title, author, series, description, created_at, updated_at)
                 VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?5) RETURNING id",
                params![title, author, series, description, Utc::now()],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn highlight(lib: &Library, book_id: i64, text: &str, note: Option<&str>) -> i64 {
        lib.conn
            .query_row(
                "INSERT INTO annotation (book_id, kind, device_text, device_note, chapter_title, content_id,
                    start_path, start_offset, end_path, end_offset, imported_at, updated_at)
                 VALUES (?1, 'highlight', ?2, ?3, 'Chapter One', 'c', 'p', 0, 'p', 1, ?4, ?4) RETURNING id",
                params![book_id, text, note, Utc::now()],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn builds_safe_fts_queries() {
        assert_eq!(
            fts_query("dune her", None).as_deref(),
            Some(r#""dune" "her"*"#)
        );
        assert_eq!(
            fts_query("the \"AND* -x", Some(&["title", "author"])).as_deref(),
            Some(r#"{title author} : ("the" "AND" "x"*)"#)
        );
        assert_eq!(fts_query("  -- ", None), None);
    }

    #[test]
    fn library_search_matches_words_series_tags_and_accents() {
        let lib = Library::open_in_memory().unwrap();
        let wot = book(
            &lib,
            "The Eye of the World",
            "Robert Jordan",
            Some("The Wheel of Time"),
            None,
        );
        let asatru = book(
            &lib,
            "Ásatrú for Beginners",
            "Mathias Nordvig",
            None,
            Some("Norse gods"),
        );
        book(
            &lib,
            "Dune",
            "Frank Herbert",
            None,
            Some("A wheel turns in the desert"),
        );
        let found = |q: &str| -> Vec<i64> {
            lib.library_books(BookSort::Title, Some(q))
                .unwrap()
                .into_iter()
                .map(|b| b.id)
                .collect()
        };
        assert_eq!(found("wheel"), [wot], "series matches; descriptions don't");
        assert_eq!(found("jord"), [wot], "the last word is a prefix");
        assert_eq!(found("asatru"), [asatru], "accents are ignored");
        assert!(
            found("antra").is_empty(),
            "whole words, not the middle of one"
        );
        assert!(found("***").is_empty());
        lib.conn
            .execute(
                "INSERT INTO book_tag (book_id, name) VALUES (?1, 'Mythology')",
                [asatru],
            )
            .unwrap();
        assert_eq!(found("mytho"), [asatru], "tags are indexed when added");
        lib.conn
            .execute(
                "UPDATE book SET user_title = 'Heathen Ways' WHERE id = ?1",
                [asatru],
            )
            .unwrap();
        assert_eq!(found("heathen"), [asatru], "edits are indexed");
        lib.conn
            .execute("DELETE FROM book_tag WHERE book_id = ?1", [asatru])
            .unwrap();
        assert!(found("mytho").is_empty());
    }

    #[test]
    fn whole_library_search_ranks_and_marks_matches() {
        let lib = Library::open_in_memory().unwrap();
        let dune = book(&lib, "Dune", "Frank Herbert", None, Some("Spice and sand"));
        let other = book(
            &lib,
            "Gardening",
            "Someone",
            None,
            Some("All about sand in soil"),
        );
        highlight(
            &lib,
            dune,
            "I must not fear. Fear is the mind-killer.",
            None,
        );
        let noted = highlight(
            &lib,
            other,
            "Loam holds water.",
            Some("Compare with desert sand"),
        );
        let results = lib.search("sand", 10).unwrap();
        assert_eq!(
            results.books.len(),
            2,
            "descriptions count in a library-wide search"
        );
        assert_eq!(results.annotations.len(), 1);
        assert_eq!(results.annotations[0].id, noted);
        assert!(
            results.annotations[0].snippet.contains("[sand]"),
            "{}",
            results.annotations[0].snippet
        );

        let fear = lib.search("fear mind", 10).unwrap();
        assert_eq!(fear.annotations[0].book_title, "Dune");
        assert!(
            fear.annotations[0].snippet.contains("[Fear]")
                || fear.annotations[0].snippet.contains("[fear]")
        );

        lib.conn
            .execute("UPDATE annotation SET status = 'trashed'", [])
            .unwrap();
        assert!(lib.search("fear", 10).unwrap().annotations.is_empty());
        lib.conn.execute("DELETE FROM annotation", []).unwrap();
        let rows: i64 = lib
            .conn
            .query_row("SELECT count(*) FROM annotation_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "deletes leave the index");
    }
}
