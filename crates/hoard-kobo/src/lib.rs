//! Read-only access to Kobo e-readers.
//!
//! The device's `KoboReader.sqlite` is never opened in place: [`Snapshot`]
//! copies it (plus any WAL/SHM files) to a temporary directory first, and
//! [`KoboDb`] reads from that copy. Nothing in this crate writes to a device.
//!
//! Kobo's schema changes between firmware versions, so every query checks
//! which columns exist and treats missing ones as empty.

mod chapters;
mod db;
mod device;
#[doc(hidden)]
pub mod fixture;
mod paths;
mod snapshot;

use std::path::PathBuf;

pub use db::{KoboDb, Stats};
pub use device::{
    KoboDevice, TESTED_DB_VERSIONS, VersionInfo, detect, ensure_not_on_kobo, is_tested_db_version,
    kobo_root_containing,
};
pub use snapshot::Snapshot;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{} does not look like a Kobo (no .kobo/KoboReader.sqlite)", .0.display())]
    NotAKobo(PathBuf),
    #[error("{} is on a Kobo; Word Hoard doesn't write to the device", .0.display())]
    OnKobo(PathBuf),
    #[error("{} is not a Kobo database (it has no content table)", .0.display())]
    NotKoboDb(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
