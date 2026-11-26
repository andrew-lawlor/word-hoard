use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no Kobo database found at {0}")]
    NotAKobo(PathBuf),
    #[error(
        "{0} is on a Kobo e-reader. Word Hoard never writes to your Kobo; choose a location on this computer instead."
    )]
    OnKobo(PathBuf),
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
