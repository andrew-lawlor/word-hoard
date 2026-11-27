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
    /// Where the Kobo's database was backed up before anything was written.
    pub backup: PathBuf,
}

impl Library {
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
                plan.skipped.push(skip("already on this Kobo"));
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
            let dest = PathBuf::from(safe_component(&folder))
                .join(safe_component(&author_folder))
                .join(format!("{}.{ext}", safe_component(&book.title)));
            let dest = existing.clone().filter(|_| replace).unwrap_or(dest);
            let replacing = replace && mount.join(&dest).is_file();
            if mount.join(&dest).exists() && !replacing {
                plan.skipped
                    .push(skip("a file by that name is already on the Kobo"));
                continue;
            }
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
        let device_id = self.upsert_device(writer.device())?;
        for (n, item) in plan.items.iter().enumerate() {
            let sent = (|| -> Result<()> {
                let file = match item.action {
                    SendAction::Convert => {
                        let kepub = work.path().join(format!("{n}.kepub.epub"));
                        let cover = self.own_cover(item.book_id);
                        crate::kobo::kepub::convert_with_cover(
                            &item.source,
                            &kepub,
                            cover.as_deref(),
                        )?;
                        kepub
                    }
                    SendAction::Copy => item.source.clone(),
                };
                // A cover the Kobo saved for an earlier copy at this path
                // would be shown instead of the new one.
                writer.forget_cover(&item.dest)?;
                if item.replace {
                    writer.replace_book(&file, &item.dest)?;
                } else {
                    writer.copy_book(&file, &item.dest)?;
                }
                self.conn.execute(
                    "INSERT INTO book_source (device_id, volume_id, book_id) VALUES (?1, ?2, ?3)
                     ON CONFLICT (device_id, volume_id) DO UPDATE SET book_id = excluded.book_id",
                    params![device_id, DeviceWriter::volume_id(&item.dest), item.book_id],
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
        Ok(stats)
    }

    fn upsert_device(&self, d: &DeviceInfo) -> Result<i64> {
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
