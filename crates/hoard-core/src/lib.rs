//! Core library for Word Hoard: reading Kobo data, normalizing it, and storing
//! and merging it into the local library. Contains no GTK code.

pub mod color;
pub mod dict;
pub mod epub;
pub mod error;
pub mod export;
pub mod import;
pub mod kobo;
pub mod koreader;
pub mod markup;
pub mod normalize;
pub mod store;

pub use error::{Error, Result};
pub use import::ImportStats;
pub use store::{Library, default_library_path, migrate_from_kollate};
