//! Read-only access to a Kobo e-reader's data.

pub mod assets;
pub mod chapters;
pub mod details;
pub mod device;
pub mod epub;
pub mod kepub;
pub mod model;
pub mod notebook;
pub mod opf_meta;
pub mod qvariant;
pub mod reader;
pub mod writer;

pub use device::{
    DeviceInfo, ensure_not_on_kobo, find_kobo_db, find_mounted_kobos, is_kobo_mount,
    kobo_root_containing,
};
pub use model::*;
pub use reader::KoboDb;
