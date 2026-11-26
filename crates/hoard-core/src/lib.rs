//! Word Hoard's domain model and exporters.
//!
//! Device crates (such as `hoard-kobo`) translate what they read into these
//! types; exporters turn them into Markdown or JSON.

pub mod export;
pub mod model;

pub use model::*;
