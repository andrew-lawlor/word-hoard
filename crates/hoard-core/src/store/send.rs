//! Sending books to a Kobo (Word Hoard SPEC, "Sync model"): first a plan of
//! what would be written, shown to the user; then, once they approve it,
//! the plan is applied through a [`DeviceWriter`].

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::params;
use serde::Serialize;

use super::Library;
use super::books::safe_component;
use crate::kobo::DeviceInfo;
use crate::kobo::writer::DeviceWriter;
use crate::{Error, Result};

/// Why a book already on the Kobo isn't sent (unless replacing).
pub const ALREADY_ON_KOBO: &str = "already on this Kobo";

/// Setting: the folder on the Kobo books are sent into (default `Books`).
pub const KOBO_FOLDER_SETTING: &str = "kobo_books_folder";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SendAction {
    /// Converted to KEPUB on the way (an EPUB).
    Convert,
    /// Copied as it is (a KEPUB, or a PDF).
    Copy,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendItem {
    pub book_id: i64,
    pub title: String,
    pub author: Option<String>,
    /// The library file sent.
    pub source: PathBuf,
    /// Where it goes, relative to the Kobo's storage.
    pub dest: PathBuf,
    pub size: u64,
    pub action: SendAction,
    /// Replaces the copy already on the Kobo (re-sending).
    pub replace: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub book_id: i64,
    pub title: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SendPlan {
    pub serial: String,
    pub items: Vec<SendItem>,
    pub skipped: Vec<Skipped>,
}

impl SendPlan {
    pub fn total_size(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SendStats {
    pub sent: usize,
    pub converted: usize,
    /// (title, why) for books that couldn't be sent.
    pub failed: Vec<(String, String)>,
    /// (title, why) for books sent as their original EPUB, because the copy
    /// converted for the Kobo failed its check.
    pub unconverted: Vec<(String, String)>,
    /// Where the Kobo's database was backed up before anything was written.
    pub backup: PathBuf,
}

/// Books to remove from a Kobo, and those that can't be.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RemovePlan {
    pub serial: String,
    /// (book ID, title, path on the Kobo relative to its storage).
    pub items: Vec<(i64, String, PathBuf)>,
    pub skipped: Vec<Skipped>,
}

impl Library {
    /// Which of `book_ids` can be removed from the Kobo at `mount`: books
    /// sideloaded there (sent by Word Hoard or otherwise) whose files are
    /// present. Store books, and books not on it, are skipped. Writes nothing.
    pub fn plan_remove(&self, mount: &Path, book_ids: &[i64]) -> Result<RemovePlan> {
        let device = DeviceInfo::identify(mount)?;
        let mut plan = RemovePlan {
            serial: device.serial.clone(),
            ..Default::default()
        };
        for &id in book_ids {
            let Some(book) = self.book(id)? else { continue };
            let volumes: Vec<String> = self
                .conn
                .prepare(
                    "SELECT s.volume_id FROM book_source s JOIN device d ON d.id = s.device_id
                     WHERE d.serial = ?1 AND s.book_id = ?2",
                )?
                .query_map(params![device.serial, id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let files: Vec<PathBuf> = volumes
                .iter()
                .filter_map(|v| v.strip_prefix("file:///mnt/onboard/"))
                .map(PathBuf::from)
                .filter(|rel| mount.join(rel).is_file())
                .collect();
            let store = volumes.iter().any(|v| !v.starts_with("file://"));
            if files.is_empty() {
                plan.skipped.push(Skipped {
                    book_id: id,
                    title: book.title.clone(),
                    reason: if store {
                        "bought from the Kobo store (remove it on the Kobo)"
                    } else {
                        "not on this Kobo"
                    }
                    .to_owned(),
                });
                continue;
            }
            for rel in files {
                plan.items.push((id, book.title.clone(), rel));
            }
        }
        Ok(plan)
    }

    /// Removes the books in an approved plan from the Kobo through `writer`
    /// (which backed up its database). Returns how many files were deleted.
    pub fn apply_remove(&self, writer: &DeviceWriter, plan: &RemovePlan) -> Result<usize> {
        if writer.device().serial != plan.serial {
            return Err(Error::NotAKobo(PathBuf::from(&plan.serial)));
        }
        let paths: Vec<PathBuf> = plan.items.iter().map(|(_, _, rel)| rel.clone()).collect();
        writer.remove_books(&paths)
    }

    /// What sending `book_ids` to the Kobo at `mount` would do. Writes
    /// nothing anywhere.
    pub fn plan_send(&self, mount: &Path, book_ids: &[i64]) -> Result<SendPlan> {
        self.plan_send_with(mount, book_ids, false)
    }

    /// [`Self::plan_send`]; with `replace`, books already on the Kobo are
    /// sent again over their copy there instead of being skipped.
    pub fn plan_send_with(
        &self,
        mount: &Path,
        book_ids: &[i64],
        replace: bool,
    ) -> Result<SendPlan> {
        let device = DeviceInfo::identify(mount)?;
        let folder = self
            .setting(KOBO_FOLDER_SETTING)?
            .filter(|f| !f.trim().is_empty())
            .unwrap_or_else(|| "Books".to_owned());
        let books_root = self.books_root()?;
        let mut plan = SendPlan {
            serial: device.serial.clone(),
            ..Default::default()
        };
        // Destinations taken by earlier books in this plan (the Kobo's
        // FAT storage ignores case).
        let mut planned = std::collections::HashSet::new();
        let dest_key = |d: &Path| d.to_string_lossy().to_lowercase();
        for &id in book_ids {
            let Some(book) = self.book(id)? else { continue };
            let skip = |reason: &str| Skipped {
                book_id: id,
                title: book.title.clone(),
                reason: reason.to_owned(),
            };
            // On the device if a copy recorded for it is still there: a
            // store book, or a sideloaded file that exists. A record whose
            // file is gone (deleted, or the Kobo not yet ejected since) doesn't
            // count.
            let volumes: Vec<String> = self
                .conn
                .prepare(
                    "SELECT s.volume_id FROM book_source s JOIN device d ON d.id = s.device_id
                     WHERE d.serial = ?1 AND s.book_id = ?2",
                )?
                .query_map(params![device.serial, id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let on_device = volumes
                .iter()
                .any(|v| match v.strip_prefix("file:///mnt/onboard/") {
                    Some(rel) => mount.join(rel).is_file(),
                    None => true,
                });
            // The sideloaded copy to replace, when re-sending.
            let existing: Option<PathBuf> = volumes
                .iter()
                .filter_map(|v| v.strip_prefix("file:///mnt/onboard/"))
                .map(PathBuf::from)
                .find(|rel| mount.join(rel).is_file());
            if on_device && !(replace && existing.is_some()) {
                plan.skipped.push(skip(ALREADY_ON_KOBO));
                continue;
            }
            let files = self.book_files(id)?;
            let rank = |f: &str| match f {
                "kepub" => 0,
                "epub" => 1,
                "pdf" => 2,
                _ => 3,
            };
            let Some(file) = files
                .iter()
                .filter(|f| rank(&f.format) < 3 && f.path.is_file())
                .min_by_key(|f| rank(&f.format))
            else {
                plan.skipped.push(skip(if files.is_empty() {
                    "no file in the library"
                } else {
                    "no file the Kobo can read"
                }));
                continue;
            };
            // The same author folder the book has in the books folder.
            let author_folder = file
                .path
                .strip_prefix(&books_root)
                .ok()
                .and_then(|rel| rel.components().next())
                .filter(|_| {
                    file.path
                        .strip_prefix(&books_root)
                        .is_ok_and(|r| r.components().count() > 1)
                })
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .or_else(|| book.author.clone())
                .unwrap_or_else(|| "Unknown author".to_owned());
            let ext = if file.format == "pdf" {
                "pdf"
            } else {
                "kepub.epub"
            };
            let dir = PathBuf::from(safe_component(&folder)).join(safe_component(&author_folder));
            let replacing = replace && existing.is_some();
            let dest = match existing.clone().filter(|_| replace) {
                Some(dest) => dest,
                None => {
                    // Another book of the same title (a second translation,
                    // say) gets its own name: its library folder's
                    // ("The Iliad (2023)"), or a number.
                    let title = safe_component(&book.title);
                    let library_name = file
                        .path
                        .parent()
                        .and_then(Path::file_name)
                        .map(|n| safe_component(&n.to_string_lossy()))
                        .filter(|n| n != &title && n.starts_with(&title));
                    let names = std::iter::once(title.clone())
                        .chain(library_name)
                        .chain((2..100).map(|n| format!("{title} ({n})")));
                    let free = names
                        .map(|name| dir.join(format!("{name}.{ext}")))
                        .find(|d| !mount.join(d).exists() && !planned.contains(&dest_key(d)));
                    let Some(dest) = free else {
                        plan.skipped
                            .push(skip("a file by that name is already on the Kobo"));
                        continue;
                    };
                    dest
                }
            };
            planned.insert(dest_key(&dest));
            plan.items.push(SendItem {
                book_id: id,
                title: book.title.clone(),
                author: book.author.clone(),
                source: file.path.clone(),
                dest,
                size: file.size,
                action: if file.format == "epub" {
                    SendAction::Convert
                } else {
                    SendAction::Copy
                },
                replace: replacing,
            });
        }
        Ok(plan)
    }

    /// Applies an approved plan through `writer`: converts and copies each
    /// book, and records it as on the device so later imports and plans know.
    /// A book that fails (DRM, say) is reported and the rest still go.
    pub fn apply_send(
        &self,
        writer: &DeviceWriter,
        plan: &SendPlan,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<SendStats> {
        if writer.device().serial != plan.serial {
            return Err(Error::NotAKobo(PathBuf::from(&plan.serial)));
        }
        let work = tempfile::Builder::new()
            .prefix("word-hoard-send-")
            .tempdir()?;
        let mut stats = SendStats {
            backup: writer.backup().to_path_buf(),
            ..Default::default()
        };
        let mut unconverted = Vec::new();
        let device_id = self.upsert_device(writer.device())?;
        for (n, item) in plan.items.iter().enumerate() {
            let sent = (|| -> Result<()> {
                // EPUBs and KEPUBs go with the library's cover and details;
                // PDFs as they are.
                let is_pdf = item
                    .source
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
                let mut dest = item.dest.clone();
                let file = if is_pdf {
                    item.source.clone()
                } else {
                    let kepub = work.path().join(format!("{n}.kepub.epub"));
                    let cover = self.own_cover(item.book_id);
                    let metadata = self.send_metadata(item.book_id)?;
                    crate::kobo::kepub::kobo_copy(
                        &item.source,
                        &kepub,
                        &crate::kobo::kepub::KoboCopy {
                            cover: cover.as_deref(),
                            metadata: Some(&metadata),
                            already_kepub: item.action == SendAction::Copy,
                        },
                    )?;
                    // Never send a copy the Kobo might choke on.
                    match crate::kobo::tidy::check_copy(&kepub) {
                        Ok(()) => kepub,
                        Err(why) if item.action == SendAction::Convert && !item.replace => {
                            // The book as it came, which the Kobo reads as a
                            // plain EPUB.
                            let name = dest
                                .file_name()
                                .map(|n| n.to_string_lossy().replace(".kepub.epub", ".epub"))
                                .unwrap_or_default();
                            dest.set_file_name(name);
                            if writer.exists(&dest)? {
                                return Err(Error::Library(format!(
                                    "the copy made for the Kobo failed a check ({why}), and a file is already where the original would go"
                                )));
                            }
                            unconverted.push((item.title.clone(), why));
                            item.source.clone()
                        }
                        Err(why) => {
                            return Err(Error::Library(format!(
                                "the copy made for the Kobo failed a check ({why}), so the one there was left as it is"
                            )));
                        }
                    }
                };
                // A cover the Kobo saved for an earlier copy at this path
                // would be shown instead of the new one.
                writer.forget_cover(&dest)?;
                if item.replace {
                    writer.replace_book(&file, &dest)?;
                } else {
                    writer.copy_book(&file, &dest)?;
                }
                self.conn.execute(
                    "INSERT INTO book_source (device_id, volume_id, book_id) VALUES (?1, ?2, ?3)
                     ON CONFLICT (device_id, volume_id) DO UPDATE SET book_id = excluded.book_id",
                    params![device_id, DeviceWriter::volume_id(&dest), item.book_id],
                )?;
                Ok(())
            })();
            match sent {
                Ok(()) => {
                    stats.sent += 1;
                    if item.action == SendAction::Convert {
                        stats.converted += 1;
                    }
                }
                Err(err) => stats.failed.push((item.title.clone(), err.to_string())),
            }
            progress(n + 1, plan.items.len());
        }
        stats.unconverted = unconverted;
        Ok(stats)
    }

    /// The library's details for a book, to write into the copy sent.
    pub fn send_metadata(&self, book_id: i64) -> Result<crate::kobo::opf_meta::SendMetadata> {
        let Some(book) = self.book(book_id)? else {
            return Ok(Default::default());
        };
        let details = self.book_details(book_id)?;
        let author_sort: Option<String> = self.conn.query_row(
            "SELECT author_sort FROM book WHERE id = ?1",
            [book_id],
            |r| r.get(0),
        )?;
        let text = |s: Option<String>| s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        Ok(crate::kobo::opf_meta::SendMetadata {
            title: text(Some(book.title.clone())),
            authors: crate::kobo::opf_meta::SendMetadata::authors_from(
                book.author.as_deref(),
                author_sort.as_deref(),
            ),
            series: text(book.series.clone()),
            series_index: text(book.series_number.clone()),
            description: text(details.description),
            publisher: text(book.publisher.clone()),
            language: text(details.language),
            published: text(details.published),
        })
    }

    /// What the Kobo should show for each sideloaded book on it that's in
    /// the library (see [`crate::kobo::details`]), compared with what it
    /// shows now (`on_kobo`): only the books and fields that differ.
    pub fn kobo_detail_changes(
        &self,
        mount: &Path,
        on_kobo: &std::collections::HashMap<String, crate::kobo::details::KoboDetails>,
    ) -> Result<Vec<crate::kobo::details::KoboDetails>> {
        use crate::kobo::details::KoboDetails;
        use crate::kobo::opf_meta::{description_html, series_number};
        let device = DeviceInfo::identify(mount)?;
        let pairs: Vec<(String, i64)> = self
            .conn
            .prepare(
                "SELECT s.volume_id, s.book_id FROM book_source s JOIN device d ON d.id = s.device_id
                 WHERE d.serial = ?1 AND s.volume_id LIKE 'file:///mnt/onboard/%'",
            )?
            .query_map([&device.serial], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut changes = Vec::new();
        for (content_id, book_id) in pairs {
            let Some(have) = on_kobo.get(&content_id) else {
                continue; // Not added by the Kobo yet (it adds books on eject).
            };
            let m = self.send_metadata(book_id)?;
            let names: Vec<&str> = m.authors.iter().map(|(n, _)| n.as_str()).collect();
            let want = KoboDetails {
                content_id,
                title: m.title,
                // Several authors as Calibre lists them for the Kobo.
                attribution: (!names.is_empty()).then(|| names.join(" & ")),
                description: m.description.as_deref().map(description_html),
                publisher: m.publisher,
                series_number: m
                    .series
                    .as_ref()
                    .and(m.series_index.as_deref())
                    .map(series_number),
                series: m.series,
            };
            if let Some(change) = want.changes_from(have) {
                changes.push(change);
            }
        }
        Ok(changes)
    }

    /// Brings the details the Kobo at `mount` shows for sideloaded books in
    /// the library into step with it: reads a copy of the Kobo's database,
    /// and only if something differs, backs it up (into `backups`) and
    /// writes the differences. Returns how many books changed.
    pub fn update_kobo_details(&self, mount: &Path, backups: &Path) -> Result<usize> {
        let (_work, copy) = super::positions::copy_kobo_db(mount)?;
        let on_kobo = crate::kobo::details::read_details(&copy)?;
        let changes = self.kobo_detail_changes(mount, &on_kobo)?;
        if changes.is_empty() {
            return Ok(0);
        }
        DeviceWriter::open(mount, backups)?.update_details(&changes)
    }

    /// The books on the Kobo mounted at `mount`: those with a copy recorded
    /// there that still exists (a store book, or a sideloaded file that's
    /// present), as [`Self::plan_send`] judges it.
    pub fn books_on_device(&self, mount: &Path) -> Result<std::collections::HashSet<i64>> {
        let device = DeviceInfo::identify(mount)?;
        let mut stmt = self.conn.prepare(
            "SELECT s.book_id, s.volume_id FROM book_source s JOIN device d ON d.id = s.device_id
             WHERE d.serial = ?1",
        )?;
        let rows = stmt.query_map([&device.serial], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut on = std::collections::HashSet::new();
        for row in rows {
            let (id, volume) = row?;
            let present = match volume.strip_prefix("file:///mnt/onboard/") {
                Some(rel) => mount.join(rel).is_file(),
                None => true,
            };
            if present {
                on.insert(id);
            }
        }
        Ok(on)
    }

    /// Forgets the copies recorded on the Kobo mounted at `mount` whose file
    /// is gone (deleted on the Kobo, or replaced by a copy elsewhere). The
    /// book stays in the library, with its highlights. Returns how many were
    /// forgotten; nothing is forgotten unless the Kobo's storage is readable.
    pub fn forget_missing_copies(&self, mount: &Path) -> Result<usize> {
        let device = DeviceInfo::identify(mount)?;
        if std::fs::read_dir(mount).is_err() {
            return Ok(0);
        }
        let volumes: Vec<String> = self
            .conn
            .prepare(
                "SELECT s.volume_id FROM book_source s JOIN device d ON d.id = s.device_id
                 WHERE d.serial = ?1",
            )?
            .query_map([&device.serial], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut forgotten = 0;
        for volume in volumes {
            let Some(rel) = volume.strip_prefix("file:///mnt/onboard/") else {
                continue;
            };
            if mount.join(rel).exists() {
                continue;
            }
            forgotten += self.conn.execute(
                "DELETE FROM book_source WHERE volume_id = ?2
                 AND device_id = (SELECT id FROM device WHERE serial = ?1)",
                params![device.serial, volume],
            )?;
        }
        Ok(forgotten)
    }

    pub(crate) fn upsert_device(&self, d: &DeviceInfo) -> Result<i64> {
        Ok(self.conn.query_row(
            "INSERT INTO device (serial, model_id, firmware, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT (serial) DO UPDATE SET
                model_id = coalesce(excluded.model_id, model_id),
                firmware = coalesce(excluded.firmware, firmware),
                last_seen_at = excluded.last_seen_at
             RETURNING id",
            params![d.serial, d.model_id, d.firmware, Utc::now()],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub::tests::write_epub;
    use crate::store::books::AddMode;

    fn fake_kobo(root: &Path) {
        std::fs::create_dir_all(root.join(".kobo")).unwrap();
        std::fs::write(root.join(".kobo/KoboReader.sqlite"), b"db").unwrap();
        std::fs::write(
            root.join(".kobo/version"),
            "N000000000777,4.9.77,4.45.23697,4.9.77,4.9.77,00000000-0000-0000-0000-000000000390",
        )
        .unwrap();
    }

    fn epub(path: &Path, title: &str, author: &str) {
        let chapter = format!(
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{title}</dc:title><dc:creator>{author}</dc:creator><dc:identifier id="id">urn:uuid:{title}</dc:identifier></metadata></package>"#
        );
        write_epub(path, &chapter);
    }

    /// The OPF of a book sent to the Kobo.
    fn sent_opf(path: &Path) -> String {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        let name = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_owned())
            .find(|n| n.ends_with(".opf"))
            .unwrap();
        let mut opf = String::new();
        std::io::Read::read_to_string(&mut zip.by_name(&name).unwrap(), &mut opf).unwrap();
        opf
    }

    #[test]
    fn brings_the_kobos_details_into_step_with_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let kobo = dir.path().join("KOBOeReader");
        fake_kobo(&kobo);
        crate::kobo::details::tests::fake_kobo_db(&kobo.join(".kobo/KoboReader.sqlite"), 176);
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        let src = dir.path().join("hammer.epub");
        epub(&src, "The Hammer and the Cross", "Harry Harrison");
        let book = lib.add_file(&src, AddMode::Copy).unwrap().book_id;
        lib.conn
            .execute(
                "UPDATE book SET series = 'The Hammer and the Cross', series_number = '1.0',
                 publisher = 'Tor' WHERE id = ?1",
                [book],
            )
            .unwrap();
        let device = lib
            .upsert_device(&DeviceInfo::identify(&kobo).unwrap())
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO book_source (device_id, volume_id, book_id) VALUES (?1, ?2, ?3)",
                params![
                    device,
                    "file:///mnt/onboard/Books/H/Hammer.kepub.epub",
                    book
                ],
            )
            .unwrap();
        let backups = dir.path().join("backups");

        assert_eq!(lib.update_kobo_details(&kobo, &backups).unwrap(), 1);
        assert!(backups.exists(), "backed up first");
        let conn = rusqlite::Connection::open(kobo.join(".kobo/KoboReader.sqlite")).unwrap();
        let (title, author, series, number, publisher): (String, String, String, String, String) =
            conn.query_row(
                "SELECT Title, Attribution, Series, SeriesNumber, Publisher FROM content
                 WHERE ContentType = 6 AND ContentID LIKE 'file://%'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            (
                title.as_str(),
                author.as_str(),
                series.as_str(),
                number.as_str(),
                publisher.as_str()
            ),
            (
                "The Hammer and the Cross",
                "Harry Harrison",
                "The Hammer and the Cross",
                "1",
                "Tor"
            )
        );
        drop(conn);
        // In step now: no backup, no write.
        std::fs::remove_dir_all(&backups).unwrap();
        assert_eq!(lib.update_kobo_details(&kobo, &backups).unwrap(), 0);
        assert!(!backups.exists());

        // An unverified database version is left alone.
        lib.conn
            .execute("UPDATE book SET publisher = 'Other' WHERE id = ?1", [book])
            .unwrap();
        rusqlite::Connection::open(kobo.join(".kobo/KoboReader.sqlite"))
            .unwrap()
            .execute("UPDATE DbVersion SET version = 999", [])
            .unwrap();
        assert!(matches!(
            lib.update_kobo_details(&kobo, &backups),
            Err(Error::UntestedKoboDb(999))
        ));
    }

    #[test]
    fn removes_books_from_the_kobo_and_can_send_them_again() {
        let dir = tempfile::tempdir().unwrap();
        let kobo = dir.path().join("KOBOeReader");
        fake_kobo(&kobo);
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        let src = dir.path().join("dune.epub");
        epub(&src, "Dune", "Frank Herbert");
        let dune = lib.add_file(&src, AddMode::Copy).unwrap().book_id;
        let writer = DeviceWriter::open(&kobo, &dir.path().join("backups")).unwrap();
        lib.apply_send(&writer, &lib.plan_send(&kobo, &[dune]).unwrap(), |_, _| {})
            .unwrap();
        // The Kobo adds it on eject: a stand-in database with its row.
        crate::kobo::details::tests::fake_kobo_db(&kobo.join(".kobo/KoboReader.sqlite"), 176);
        rusqlite::Connection::open(kobo.join(".kobo/KoboReader.sqlite"))
            .unwrap()
            .execute(
                "INSERT INTO content (ContentID, ContentType, Title) VALUES (?1, 6, 'Dune')",
                ["file:///mnt/onboard/Books/Frank Herbert/Dune.kepub.epub"],
            )
            .unwrap();

        let plan = lib.plan_remove(&kobo, &[dune]).unwrap();
        assert_eq!(plan.items.len(), 1);
        let writer = DeviceWriter::open(&kobo, &dir.path().join("backups")).unwrap();
        assert_eq!(lib.apply_remove(&writer, &plan).unwrap(), 1);
        assert!(
            !kobo.join("Books/Frank Herbert").exists(),
            "file and empty folder gone"
        );
        assert!(kobo.join("Books").is_dir(), "the books folder stays");
        let rows: i64 = rusqlite::Connection::open(kobo.join(".kobo/KoboReader.sqlite"))
            .unwrap()
            .query_row(
                "SELECT count(*) FROM content WHERE Title = 'Dune'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        assert!(lib.books_on_device(&kobo).unwrap().is_empty());
        // Not on it any more: nothing to remove; and it can be sent again.
        assert_eq!(
            lib.plan_remove(&kobo, &[dune]).unwrap().skipped[0].reason,
            "not on this Kobo"
        );
        assert_eq!(lib.plan_send(&kobo, &[dune]).unwrap().items.len(), 1);
    }

    #[test]
    fn books_with_the_same_title_get_their_own_names() {
        let dir = tempfile::tempdir().unwrap();
        let kobo = dir.path().join("KOBOeReader");
        fake_kobo(&kobo);
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        let translation = |n: u32| {
            let src = dir.path().join(format!("iliad{n}.epub"));
            write_epub(
                &src,
                &format!(
                    r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>The Iliad</dc:title><dc:creator>Homer</dc:creator><dc:identifier id="id">urn:uuid:iliad-{n}</dc:identifier></metadata></package>"#
                ),
            );
            let added = lib.add_file(&src, AddMode::Copy).unwrap();
            // The library would take a second Iliad by the same author as
            // another file of the first; make each its own book, as a
            // Calibre import with different authors does.
            if n > 1 {
                lib.conn
                    .execute(
                        "INSERT INTO book (fingerprint, title, author, created_at, updated_at)
                         VALUES (?1, 'The Iliad', 'Homer', '2026-01-01', '2026-01-01')",
                        [format!("iliad-{n}")],
                    )
                    .unwrap();
                let id = lib.conn.last_insert_rowid();
                lib.conn
                    .execute(
                        "UPDATE book_file SET book_id = ?1 WHERE path LIKE ?2",
                        rusqlite::params![
                            id,
                            format!(
                                "%{}",
                                added
                                    .path
                                    .strip_prefix(dir.path().join("Books"))
                                    .unwrap()
                                    .display()
                            )
                        ],
                    )
                    .unwrap();
                return id;
            }
            added.book_id
        };
        let (first, second, third) = (translation(1), translation(2), translation(3));
        assert!(first != second && second != third);
        let writer = DeviceWriter::open(&kobo, &dir.path().join("backups")).unwrap();
        let plan = lib.plan_send(&kobo, &[first]).unwrap();
        assert_eq!(
            plan.items[0].dest,
            Path::new("Books/Homer/The Iliad.kepub.epub")
        );
        lib.apply_send(&writer, &plan, |_, _| {}).unwrap();

        // Later, two more of the same title in one send: neither takes the
        // first's name, nor each other's.
        let plan = lib.plan_send(&kobo, &[second, third]).unwrap();
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
        let dests: Vec<_> = plan.items.iter().map(|i| i.dest.clone()).collect();
        assert_eq!(dests.len(), 2);
        assert_ne!(dests[0], dests[1]);
        for d in &dests {
            assert_ne!(d, Path::new("Books/Homer/The Iliad.kepub.epub"));
            let name = d.file_name().unwrap().to_string_lossy();
            assert!(
                name.starts_with("The Iliad (") && name.ends_with(").kepub.epub"),
                "{name}"
            );
        }
        lib.apply_send(&writer, &plan, |_, _| {}).unwrap();
        assert_eq!(
            lib.books_on_device(&kobo).unwrap(),
            [first, second, third].into()
        );
        // Updating keeps each book's own name.
        let plan = lib.plan_send_with(&kobo, &[second], true).unwrap();
        assert_eq!(plan.items[0].dest, dests[0]);
        assert!(plan.items[0].replace);
    }

    #[test]
    fn forgets_copies_whose_file_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let kobo = dir.path().join("KOBOeReader");
        fake_kobo(&kobo);
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        let src = dir.path().join("dune.epub");
        epub(&src, "Dune", "Frank Herbert");
        let dune = lib.add_file(&src, AddMode::Copy).unwrap().book_id;
        let plan = lib.plan_send(&kobo, &[dune]).unwrap();
        let writer = DeviceWriter::open(&kobo, &dir.path().join("backups")).unwrap();
        lib.apply_send(&writer, &plan, |_, _| {}).unwrap();
        // An older copy, sideloaded before and since deleted from the Kobo.
        let device = lib
            .upsert_device(&DeviceInfo::identify(&kobo).unwrap())
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO book_source (device_id, volume_id, book_id)
                 VALUES (?1, 'file:///mnt/onboard/Herbert, Frank/Dune - Frank Herbert.kepub.epub', ?2)",
                params![device, dune],
            )
            .unwrap();
        let count = |lib: &Library| -> i64 {
            lib.conn
                .query_row("SELECT count(*) FROM book_source", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count(&lib), 2);
        assert_eq!(lib.forget_missing_copies(&kobo).unwrap(), 1);
        assert_eq!(count(&lib), 1, "the copy that's there is kept");
        assert_eq!(lib.books_on_device(&kobo).unwrap(), [dune].into());
        assert_eq!(lib.forget_missing_copies(&kobo).unwrap(), 0);
    }

    #[test]
    fn sends_the_original_when_the_converted_copy_fails_its_check() {
        let dir = tempfile::tempdir().unwrap();
        let kobo = dir.path().join("KOBOeReader");
        fake_kobo(&kobo);
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        // A package document with a prefix it never declares: conversion
        // keeps it, so the check fails.
        let src = dir.path().join("odd.epub");
        write_epub(
            &src,
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Odd Book</dc:title><dc:creator>Some One</dc:creator><dc:identifier id="id">urn:uuid:odd</dc:identifier><mystery:thing/></metadata></package>"#,
        );
        let book = lib.add_file(&src, AddMode::Copy).unwrap().book_id;
        let plan = lib.plan_send(&kobo, &[book]).unwrap();
        assert_eq!(plan.items[0].action, SendAction::Convert);
        let writer = DeviceWriter::open(&kobo, &dir.path().join("backups")).unwrap();
        let stats = lib.apply_send(&writer, &plan, |_, _| {}).unwrap();
        assert_eq!(stats.sent, 1);
        assert_eq!(stats.unconverted.len(), 1, "{stats:?}");
        assert!(stats.unconverted[0].1.contains("package document"));
        let sent = kobo.join("Books/Some One/Odd Book.epub");
        assert!(sent.is_file() && !kobo.join("Books/Some One/Odd Book.kepub.epub").exists());
        assert_eq!(
            std::fs::read(&sent).unwrap(),
            std::fs::read(&lib.book_files(book).unwrap()[0].path).unwrap(),
            "the library's file, as it is"
        );
        assert_eq!(lib.books_on_device(&kobo).unwrap(), [book].into());
    }

    #[test]
    fn plans_then_sends_and_remembers() {
        let dir = tempfile::tempdir().unwrap();
        let kobo = dir.path().join("KOBOeReader");
        fake_kobo(&kobo);
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();

        let src = dir.path().join("dune.epub");
        epub(&src, "Dune", "Frank Herbert");
        let dune = lib.add_file(&src, AddMode::Copy).unwrap().book_id;
        let pdf_only = lib
            .conn
            .query_row(
                "INSERT INTO book (fingerprint, title, author, created_at, updated_at)
                 VALUES ('x', 'No Files', 'Nobody', ?1, ?1) RETURNING id",
                [Utc::now()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();

        let plan = lib.plan_send(&kobo, &[dune, pdf_only]).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].action, SendAction::Convert);
        assert_eq!(
            plan.items[0].dest,
            Path::new("Books/Frank Herbert/Dune.kepub.epub")
        );
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, "no file in the library");
        // Planning wrote nothing.
        assert!(!kobo.join("Books").exists());

        let writer = DeviceWriter::open(&kobo, &dir.path().join("backups")).unwrap();
        let mut calls = 0;
        let stats = lib.apply_send(&writer, &plan, |_, _| calls += 1).unwrap();
        assert_eq!((stats.sent, stats.converted, calls), (1, 1, 1));
        assert!(stats.failed.is_empty());
        assert!(stats.backup.join("KoboReader.sqlite").is_file());
        let sent = kobo.join("Books/Frank Herbert/Dune.kepub.epub");
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&sent).unwrap()).unwrap();
        assert_eq!(zip.by_index(0).unwrap().name(), "mimetype");
        drop(zip);

        assert_eq!(lib.books_on_device(&kobo).unwrap(), [dune].into());

        // Updating it: the library's details go into the fresh copy (the
        // library file itself is untouched).
        lib.conn
            .execute(
                "UPDATE book SET title = 'Dune (Library)', series = 'Dune Chronicles',
                 series_number = '1.0' WHERE id = ?1",
                [dune],
            )
            .unwrap();
        let update = lib.plan_send_with(&kobo, &[dune], true).unwrap();
        assert!(update.items[0].replace);
        assert_eq!(
            update.items[0].dest,
            Path::new("Books/Frank Herbert/Dune.kepub.epub")
        );
        lib.apply_send(&writer, &update, |_, _| {}).unwrap();
        let opf = sent_opf(&sent);
        assert!(opf.contains(">Dune (Library)</dc:title>"), "{opf}");
        assert!(opf.contains(r#"<meta name="calibre:series" content="Dune Chronicles"/>"#));
        assert!(opf.contains(r#"<meta name="calibre:series_index" content="1"/>"#));
        assert!(opf.contains(">Frank Herbert</dc:creator>"));
        let library_file = &lib.book_files(dune).unwrap()[0].path;
        let mut original = String::new();
        std::io::Read::read_to_string(
            &mut zip::ZipArchive::new(std::fs::File::open(library_file).unwrap())
                .unwrap()
                .by_name("OEBPS/content.opf")
                .unwrap(),
            &mut original,
        )
        .unwrap();
        assert!(!original.contains("Library") && !original.contains("Chronicles"));

        // Remembered: the next plan skips it.
        let again = lib.plan_send(&kobo, &[dune]).unwrap();
        assert!(again.items.is_empty());
        assert_eq!(again.skipped[0].reason, "already on this Kobo");
        let volume: String = lib
            .conn
            .query_row(
                "SELECT volume_id FROM book_source WHERE book_id = ?1",
                [dune],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            volume,
            "file:///mnt/onboard/Books/Frank Herbert/Dune.kepub.epub"
        );

        // Re-sending on purpose replaces the copy on the Kobo, at the same path.
        let resend = lib.plan_send_with(&kobo, &[dune], true).unwrap();
        assert_eq!(resend.items.len(), 1);
        assert!(resend.items[0].replace);
        assert_eq!(
            resend.items[0].dest,
            Path::new("Books/Frank Herbert/Dune.kepub.epub")
        );
        let writer = DeviceWriter::open(&kobo, &dir.path().join("backups")).unwrap();
        let again_stats = lib.apply_send(&writer, &resend, |_, _| {}).unwrap();
        assert_eq!(again_stats.sent, 1, "{:?}", again_stats.failed);

        // Deleted from the Kobo (but still recorded): it can be sent again.
        std::fs::remove_file(&sent).unwrap();
        assert!(lib.books_on_device(&kobo).unwrap().is_empty());
        let after_delete = lib.plan_send(&kobo, &[dune]).unwrap();
        assert_eq!(after_delete.items.len(), 1, "{:?}", after_delete.skipped);
    }

    #[test]
    fn reports_books_that_fail_and_sends_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let kobo = dir.path().join("KOBOeReader");
        fake_kobo(&kobo);
        let lib = Library::open(&dir.path().join("data/library.db")).unwrap();
        lib.set_books_root(&dir.path().join("Books")).unwrap();
        let a = dir.path().join("a.epub");
        epub(&a, "Alpha", "Ann");
        let b = dir.path().join("b.epub");
        epub(&b, "Beta", "Bob");
        let alpha = lib.add_file(&a, AddMode::Copy).unwrap();
        let beta = lib.add_file(&b, AddMode::Copy).unwrap();
        // Lock Alpha with DRM after it's in the library.
        {
            use std::io::Write;
            let mut zip = zip::ZipWriter::new_append(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&alpha.path)
                    .unwrap(),
            )
            .unwrap();
            zip.start_file(
                "META-INF/rights.xml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(b"<rights/>").unwrap();
            zip.finish().unwrap();
        }
        let plan = lib
            .plan_send(&kobo, &[alpha.book_id, beta.book_id])
            .unwrap();
        let writer = DeviceWriter::open(&kobo, &dir.path().join("backups")).unwrap();
        let stats = lib.apply_send(&writer, &plan, |_, _| {}).unwrap();
        assert_eq!(stats.sent, 1);
        assert_eq!(stats.failed.len(), 1);
        assert!(stats.failed[0].1.contains("DRM"), "{:?}", stats.failed);
        assert!(kobo.join("Books/Bob/Beta.kepub.epub").is_file());
        assert!(!kobo.join("Books/Ann/Alpha.kepub.epub").exists());
    }
}
