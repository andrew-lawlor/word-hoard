use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "this Kobo's database (version {0}) hasn't been checked with Word Hoard, so it won't change it"
    )]
    UntestedKoboDb(i64),
    /// Opening, moving or choosing a library folder.
    #[error("{0}")]
    Library(String),
    #[error("no Kobo database found at {0}")]
    NotAKobo(PathBuf),
    #[error(
        "{0} is on a Kobo e-reader. Word Hoard doesn't save files onto your Kobo; choose a location on this computer instead."
    )]
    OnKobo(PathBuf),
    #[error("{0} is mounted read-only; mount your Kobo normally to send books to it")]
    ReadOnlyKobo(PathBuf),
    #[error("{0} is already on the Kobo")]
    AlreadyOnKobo(PathBuf),
    #[error("{0} isn't a place on the Kobo where books can go")]
    BadKoboPath(PathBuf),
    #[error("{0} is locked with DRM, so it can't be converted for the Kobo")]
    Drm(PathBuf),
    #[error("{0} isn't a Calibre library (it has no metadata.db)")]
    NotACalibreLibrary(PathBuf),
    #[error("{0} isn't an EPUB; Word Hoard adds EPUB and KEPUB files for now")]
    UnsupportedFormat(PathBuf),
    #[error("this Kobo's database has no {0} table")]
    MissingTable(String),
    #[error("this Kobo's database has no {column} column in its {table} table")]
    MissingColumn { table: String, column: String },
    #[error(
        "this Kobo's database (version {version}) hasn't been tested with Word Hoard, and reading it failed: {source}"
    )]
    UntestedDb {
        version: i64,
        #[source]
        source: Box<Error>,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
