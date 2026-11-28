//! Keeping the reading position in step between the Kobo and Foliate.
//!
//! For each book both have, the position a reader moved since the last sync
//! goes to the other; if both moved, the one read later wins. A position
//! counts as moved only when it's away (more than about a page) from where
//! Word Hoard last left that reader and from where it was just before, so
//! going back to re-read is a move, but Foliate saving the page it had open
//! before the sync isn't. Positions are carried by text (see
//! [`crate::position`]): the Kobo's from its copy of the book, Foliate's
//! from the library's EPUB, only when the two have the same text.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

use super::Library;
use super::readers::plain_uid;
use crate::Result;
use crate::kobo::DeviceInfo;
use crate::kobo::writer::DeviceWriter;
use crate::kobo::writer::KoboPosition;
use crate::position::{BookText, Spot};

/// How close two positions are to count as the same place (UTF-16 units;
/// about a page).
const SAME_PLACE: usize = 1500;

fn near(a: Spot, b: Spot) -> bool {
    a.spine == b.spine && a.offset.abs_diff(b.offset) <= SAME_PLACE
}

/// Which reader a position went to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    Kobo,
    Foliate,
}

/// A position carried from one reader to the other.
#[derive(Debug, Clone)]
pub struct PositionMove {
    pub book_id: i64,
    pub title: String,
    pub to: Reader,
    /// How far through, 0.0–1.0.
    pub fraction: f64,
    /// The words there.
    pub text: String,
}

#[derive(Debug, Default)]
pub struct PositionSync {
    pub moves: Vec<PositionMove>,
    /// Books both readers have whose copies differ in text, so their
    /// positions can't be carried over.
    pub different_text: Vec<String>,
    /// The Kobo database backup, if the Kobo was written.
    pub backup: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct Synced {
    kobo: Option<String>,
    kobo_before: Option<String>,
    foliate: Option<String>,
    foliate_before: Option<String>,
}

/// A copy of the Kobo's database (with its write-ahead log) to read.
pub(crate) fn copy_kobo_db(mount: &Path) -> Result<(tempfile::TempDir, PathBuf)> {
    let work = tempfile::Builder::new()
        .prefix("word-hoard-kobo-")
        .tempdir()?;
    let copy = work.path().join("KoboReader.sqlite");
    std::fs::copy(mount.join(crate::kobo::device::DB_RELATIVE_PATH), &copy)?;
    for suffix in ["-wal", "-shm"] {
        let side = mount.join(format!("{}{suffix}", crate::kobo::device::DB_RELATIVE_PATH));
        if side.is_file() {
            std::fs::copy(
                &side,
                work.path().join(format!("KoboReader.sqlite{suffix}")),
            )?;
        }
    }
    Ok((work, copy))
}

fn kobo_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|t| t.and_utc())
        })
}

impl Library {
    fn synced(&self, book_id: i64) -> Result<Synced> {
        Ok(self
            .conn
            .query_row(
                "SELECT kobo, kobo_before, foliate, foliate_before FROM position_sync WHERE book_id = ?1",
                [book_id],
                |r| {
                    Ok(Synced {
                        kobo: r.get(0)?,
                        kobo_before: r.get(1)?,
                        foliate: r.get(2)?,
                        foliate_before: r.get(3)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    fn record_synced(&self, book_id: i64, s: &Synced) -> Result<()> {
        self.conn.execute(
            "INSERT INTO position_sync (book_id, kobo, kobo_before, foliate, foliate_before, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (book_id) DO UPDATE SET kobo = excluded.kobo,
                kobo_before = excluded.kobo_before, foliate = excluded.foliate,
                foliate_before = excluded.foliate_before, synced_at = excluded.synced_at",
            params![
                book_id,
                s.kobo,
                s.kobo_before,
                s.foliate,
                s.foliate_before,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Brings the reading position of books on the Kobo mounted at `mount`
    /// and in Foliate's data folder `foliate_dir` into step (see the module
    /// notes). The Kobo's database is backed up into `backups` before it's
    /// written, and only if a position goes to it. With `dry_run`, nothing
    /// is written and nothing recorded.
    pub fn sync_positions(
        &self,
        mount: &Path,
        backups: &Path,
        foliate_dir: &Path,
        dry_run: bool,
    ) -> Result<PositionSync> {
        let device = DeviceInfo::identify(mount)?;
        let mut out = PositionSync::default();

        // Foliate's books, by EPUB identifier.
        let foliate: std::collections::HashMap<String, crate::foliate::FoliateBook> =
            crate::foliate::read_dir(foliate_dir)
                .into_iter()
                .map(|b| (plain_uid(&b.identifier), b))
                .collect();
        if foliate.is_empty() {
            return Ok(out);
        }

        // The Kobo's sideloaded books in the library, with where each is open.
        let (_work, copy) = copy_kobo_db(mount)?;
        let kobo_rows: Vec<(String, Option<String>, Option<String>)> = {
            let conn = rusqlite::Connection::open(&copy)?;
            let mut stmt = conn.prepare(
                "SELECT ContentID, ChapterIDBookmarked, DateLastRead FROM content
                 WHERE ContentType = 6 AND ContentID LIKE 'file:///mnt/onboard/%'",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let books_root = self.books_root()?;

        let mut to_kobo: Vec<(KoboPosition, i64, Synced)> = Vec::new();
        let mut to_foliate: Vec<(PathBuf, String, f64, i64, Synced)> = Vec::new();
        for (content_id, location, last_read) in kobo_rows {
            let Some(book_id): Option<i64> = self
                .conn
                .query_row(
                    "SELECT s.book_id FROM book_source s JOIN device d ON d.id = s.device_id
                     WHERE d.serial = ?1 AND s.volume_id = ?2",
                    params![device.serial, content_id],
                    |r| r.get(0),
                )
                .optional()?
            else {
                continue;
            };
            // The library's EPUB that Foliate has, by identifier.
            let files: Vec<(String, Option<String>)> = self
                .conn
                .prepare(
                    "SELECT path, epub_uid FROM book_file WHERE book_id = ?1 AND format = 'epub'",
                )?
                .query_map([book_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let Some((epub_path, fol)) = files.iter().find_map(|(path, uid)| {
                let uid = plain_uid(uid.as_deref()?);
                foliate.get(&uid).map(|f| (books_root.join(path), f))
            }) else {
                continue;
            };
            let Some(rel) = content_id.strip_prefix("file:///mnt/onboard/") else {
                continue;
            };
            let (Ok(kepub), Ok(epub)) =
                (BookText::open(&mount.join(rel)), BookText::open(&epub_path))
            else {
                continue;
            };
            let title = self.book(book_id)?.map(|b| b.title).unwrap_or_default();
            if !kepub.same_text(&epub) {
                out.different_text.push(title);
                continue;
            }
            let last = self.synced(book_id)?;
            let kobo_now = location
                .as_deref()
                .filter(|l| !l.is_empty())
                .and_then(|l| kepub.from_kobo(l));
            let fol_now = fol.last_location.as_deref().and_then(|c| epub.from_cfi(c));
            let moved =
                |now: Option<Spot>, was: [&Option<String>; 2], text: &BookText, cfi: bool| {
                    now.is_some_and(|now| {
                        !was.iter().filter_map(|w| w.as_deref()).any(|w| {
                            let spot = if cfi {
                                text.from_cfi(w)
                            } else {
                                text.from_kobo(w)
                            };
                            spot.is_some_and(|s| near(s, now))
                        })
                    })
                };
            let kobo_moved = moved(kobo_now, [&last.kobo, &last.kobo_before], &kepub, false);
            let fol_moved = moved(fol_now, [&last.foliate, &last.foliate_before], &epub, true);
            let kobo_time = last_read.as_deref().and_then(kobo_time);
            let winner = match (kobo_moved, fol_moved) {
                (false, false) => continue,
                (true, false) => Reader::Kobo,
                (false, true) => Reader::Foliate,
                (true, true) => {
                    if kobo_time >= fol.last_read {
                        Reader::Kobo
                    } else {
                        Reader::Foliate
                    }
                }
            };
            let kobo_loc = location.clone().filter(|l| !l.is_empty());
            let fol_loc = fol.last_location.clone();
            match winner {
                Reader::Kobo => {
                    let spot = kobo_now.expect("moved");
                    if fol_now.is_some_and(|f| near(f, spot)) {
                        to_foliate_noop(&mut to_foliate, book_id, kobo_loc, fol_loc);
                        continue;
                    }
                    let Some(cfi) = epub.to_cfi(spot) else {
                        continue;
                    };
                    out.moves.push(PositionMove {
                        book_id,
                        title,
                        to: Reader::Foliate,
                        fraction: epub.fraction(spot),
                        text: epub.text_at(spot, 80),
                    });
                    to_foliate.push((
                        fol.path.clone(),
                        cfi.clone(),
                        epub.fraction(spot),
                        book_id,
                        Synced {
                            kobo: kobo_loc,
                            kobo_before: None,
                            foliate: Some(cfi),
                            foliate_before: fol_loc,
                        },
                    ));
                }
                Reader::Foliate => {
                    let spot = fol_now.expect("moved");
                    if kobo_now.is_some_and(|k| near(k, spot)) {
                        to_foliate_noop(&mut to_foliate, book_id, kobo_loc, fol_loc);
                        continue;
                    }
                    let Some(loc) = kepub.to_kobo(spot) else {
                        continue;
                    };
                    let fraction = kepub.fraction(spot);
                    out.moves.push(PositionMove {
                        book_id,
                        title,
                        to: Reader::Kobo,
                        fraction,
                        text: kepub.text_at(spot, 80),
                    });
                    let percent = (fraction * 100.0).round() as i64;
                    to_kobo.push((
                        KoboPosition {
                            content_id,
                            location: loc.clone(),
                            percent_read: percent.clamp(1, 100),
                            read_at: fol.last_read.unwrap_or_else(Utc::now),
                        },
                        book_id,
                        Synced {
                            kobo: Some(loc),
                            kobo_before: kobo_loc,
                            foliate: fol_loc,
                            foliate_before: None,
                        },
                    ));
                }
            }
        }
        if dry_run {
            return Ok(out);
        }
        if !to_kobo.is_empty() {
            let writer = DeviceWriter::open(mount, backups)?;
            let positions: Vec<KoboPosition> = to_kobo.iter().map(|(p, ..)| p.clone()).collect();
            writer.set_positions(&positions)?;
            out.backup = Some(writer.backup().to_path_buf());
            for (_, book_id, synced) in &to_kobo {
                self.record_synced(*book_id, synced)?;
            }
        }
        for (file, cfi, fraction, book_id, synced) in &to_foliate {
            if !cfi.is_empty() {
                crate::foliate::set_position(file, cfi, *fraction)?;
            }
            self.record_synced(*book_id, synced)?;
        }
        Ok(out)
    }
}

/// Both readers are already at the same place: just remember it (an entry
/// with no CFI writes nothing).
fn to_foliate_noop(
    list: &mut Vec<(PathBuf, String, f64, i64, Synced)>,
    book_id: i64,
    kobo: Option<String>,
    foliate: Option<String>,
) {
    list.push((
        PathBuf::new(),
        String::new(),
        0.0,
        book_id,
        Synced {
            kobo,
            kobo_before: None,
            foliate,
            foliate_before: None,
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::books::AddMode;

    const CH1: &str = r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>1</title></head><body><h1>One</h1><p>The first chapter begins here. It goes on for a while.</p></body></html>"#;

    fn ch2() -> String {
        let para = "A long sentence that fills the second chapter with words. ".repeat(200);
        format!(
            r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>2</title></head><body><h1>Two</h1><p>{para}</p><p>Near the end of the book now.</p></body></html>"#
        )
    }

    struct World {
        _dir: tempfile::TempDir,
        lib: Library,
        kobo: PathBuf,
        backups: PathBuf,
        foliate: PathBuf,
        content_id: String,
    }

    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let kobo = dir.path().join("KOBOeReader");
        std::fs::create_dir_all(kobo.join(".kobo")).unwrap();
        std::fs::write(
            kobo.join(".kobo/version"),
            "N000000000777,4.9.77,4.45.23697,4.9.77,4.9.77,00000000-0000-0000-0000-000000000390",
        )
        .unwrap();
        crate::kobo::details::tests::fake_kobo_db(&kobo.join(".kobo/KoboReader.sqlite"), 176);
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        let src = dir.path().join("t.epub");
        crate::epub::tests::write_epub_with(
            &src,
            &[("OEBPS/ch1.xhtml", CH1), ("OEBPS/ch2.xhtml", &ch2())],
        );
        let book = lib.add_file(&src, AddMode::Copy).unwrap().book_id;
        let backups = dir.path().join("backups");
        let plan = lib.plan_send(&kobo, &[book]).unwrap();
        let writer = DeviceWriter::open(&kobo, &backups).unwrap();
        lib.apply_send(&writer, &plan, |_, _| {}).unwrap();
        let content_id = format!("file:///mnt/onboard/{}", plan.items[0].dest.display());
        // The Kobo has taken the book in.
        rusqlite::Connection::open(kobo.join(".kobo/KoboReader.sqlite"))
            .unwrap()
            .execute(
                "INSERT INTO content (ContentID, ContentType, Title) VALUES (?1, 6, 'T')",
                [&content_id],
            )
            .unwrap();
        let foliate = dir.path().join("foliate");
        std::fs::create_dir_all(&foliate).unwrap();
        World {
            _dir: dir,
            lib,
            kobo,
            backups,
            foliate,
            content_id,
        }
    }

    impl World {
        fn foliate_at(&self, cfi: &str) {
            std::fs::write(
                self.foliate.join("t.json"),
                format!(
                    r#"{{"metadata":{{"identifier":"urn:uuid:t","title":"T"}},"progress":[1,100],"lastLocation":"{cfi}","annotations":[]}}"#
                ),
            )
            .unwrap();
        }
        fn foliate_cfi(&self) -> String {
            let json: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(self.foliate.join("t.json")).unwrap(),
            )
            .unwrap();
            json["lastLocation"].as_str().unwrap().to_owned()
        }
        fn kobo(&self) -> (Option<String>, i64, String) {
            rusqlite::Connection::open(self.kobo.join(".kobo/KoboReader.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT ChapterIDBookmarked, ___PercentRead, FirstTimeReading FROM content WHERE ContentID = ?1",
                    [&self.content_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap()
        }
        fn kobo_at(&self, location: &str, read_at: &str) {
            rusqlite::Connection::open(self.kobo.join(".kobo/KoboReader.sqlite"))
                .unwrap()
                .execute(
                    "UPDATE content SET ChapterIDBookmarked = ?2, DateLastRead = ?3 WHERE ContentID = ?1",
                    params![self.content_id, location, read_at],
                )
                .unwrap();
        }
        fn sync(&self) -> PositionSync {
            self.lib
                .sync_positions(&self.kobo, &self.backups, &self.foliate, false)
                .unwrap()
        }
    }

    #[test]
    fn carries_the_position_a_reader_moved_to_the_other() {
        let w = world();
        // Read near the end of chapter two in Foliate; never opened on the Kobo.
        w.foliate_at("epubcfi(/6/4!/4/4/1:9000)");
        let dry = w
            .lib
            .sync_positions(&w.kobo, &w.backups, &w.foliate, true)
            .unwrap();
        assert_eq!(dry.moves.len(), 1);
        assert_eq!(w.kobo().0, None, "a dry run writes nothing");

        let s = w.sync();
        assert_eq!(s.moves.len(), 1, "{s:?}");
        assert_eq!(s.moves[0].to, Reader::Kobo);
        assert!(s.backup.is_some(), "backed up first");
        let (loc, percent, first) = w.kobo();
        let loc = loc.unwrap();
        assert!(loc.starts_with("OEBPS/ch2.xhtml#kobo.2."), "{loc}");
        assert!(percent > 50, "{percent}");
        assert_eq!(first, "false");

        // Nothing moved since: nothing to do.
        assert!(w.sync().moves.is_empty());

        // Back to chapter one on the Kobo, to re-read: Foliate follows.
        w.kobo_at("OEBPS/ch1.xhtml#kobo.2.1", "2030-01-01T10:00:00Z");
        let s = w.sync();
        assert_eq!(s.moves.len(), 1);
        assert_eq!(s.moves[0].to, Reader::Foliate);
        assert!(
            s.moves[0].text.starts_with("The first chapter"),
            "{}",
            s.moves[0].text
        );
        assert!(
            w.foliate_cfi().starts_with("epubcfi(/6/2!"),
            "{}",
            w.foliate_cfi()
        );

        // Foliate, still open on the old page, saves it over the sync: not a
        // move, so the Kobo stays where it was read.
        w.foliate_at("epubcfi(/6/4!/4/4/1:9000)");
        assert!(w.sync().moves.is_empty());
        assert!(w.kobo().0.unwrap().starts_with("OEBPS/ch1.xhtml"));
    }

    #[test]
    fn when_both_moved_the_later_read_wins() {
        let w = world();
        w.foliate_at("epubcfi(/6/4!/4/4/1:9000)");
        w.kobo_at("OEBPS/ch1.xhtml#kobo.2.1", "2000-01-01T10:00:00Z");
        // Foliate's file is newer than 2000: Foliate wins.
        let s = w.sync();
        assert_eq!(s.moves[0].to, Reader::Kobo);
        // Both move again; the Kobo was read later.
        w.foliate_at("epubcfi(/6/2!/4/4/1:0)");
        w.kobo_at("OEBPS/ch2.xhtml#kobo.3.1", "2099-01-01T10:00:00Z");
        let s = w.sync();
        assert_eq!(s.moves.len(), 1);
        assert_eq!(s.moves[0].to, Reader::Foliate);
        assert!(
            s.moves[0].text.starts_with("Near the end"),
            "{}",
            s.moves[0].text
        );
    }
}
