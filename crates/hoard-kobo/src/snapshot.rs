use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::{KoboDb, Result};

/// A private copy of a Kobo database, deleted when dropped.
pub struct Snapshot {
    _dir: TempDir,
    path: PathBuf,
}

impl Snapshot {
    /// Copies `db` and its `-wal`/`-shm` companions (if present) to a new
    /// temporary directory. The source files are only read.
    pub fn take(db: &Path) -> Result<Self> {
        let dir = tempfile::Builder::new().prefix("word-hoard-kobo-").tempdir()?;
        let path = dir.path().join("KoboReader.sqlite");
        fs::copy(db, &path)?;
        for suffix in ["-wal", "-shm"] {
            let src = with_suffix(db, suffix);
            if src.is_file() {
                fs::copy(&src, with_suffix(&path, suffix))?;
            }
        }
        Ok(Self { _dir: dir, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn open(&self) -> Result<KoboDb> {
        KoboDb::open(&self.path)
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}
