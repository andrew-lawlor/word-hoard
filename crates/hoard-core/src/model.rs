use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What kind of mark the reader left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationKind {
    /// Highlighted text with no note.
    Highlight,
    /// A note, usually attached to highlighted text.
    Note,
    /// A position bookmark (Kobo "dogear"); no text.
    Bookmark,
    /// Handwritten stylus markup.
    Markup,
}

/// Where a book on a device came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookOrigin {
    /// Copied onto the device as a file.
    Sideloaded,
    /// Bought from the device vendor's store (usually DRM-protected).
    Store,
    Unknown,
}

impl BookOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            BookOrigin::Sideloaded => "sideloaded",
            BookOrigin::Store => "store",
            BookOrigin::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadStatus {
    Unread,
    Reading,
    Finished,
}

impl ReadStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadStatus::Unread => "unread",
            ReadStatus::Reading => "reading",
            ReadStatus::Finished => "finished",
        }
    }
}

/// Device-specific position of an annotation, kept raw so it can be anchored
/// to the library's EPUB later (CFI + text-quote selector, see SPEC.md).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// The device's id for the chapter file (Kobo `ContentID`, with any
    /// `#fragment`).
    pub content_id: Option<String>,
    /// Reading-order (spine) index of the chapter file, when known.
    pub chapter_index: Option<i64>,
    /// Device-reported progress value for the annotation (0.0–1.0).
    pub chapter_progress: Option<f64>,
    pub start_path: Option<String>,
    pub start_offset: Option<i64>,
    pub end_path: Option<String>,
    pub end_offset: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    /// Where it came from, e.g. `kobo:N418170000001`.
    pub source: String,
    /// The device's own id for it; (`source`, `source_id`) is unique.
    pub source_id: String,
    pub kind: AnnotationKind,
    /// The highlighted passage.
    pub text: Option<String>,
    /// The reader's own note.
    pub note: Option<String>,
    pub color: Option<String>,
    /// Chapter title, when the device knows it.
    pub chapter: Option<String>,
    pub position: Position,
    pub created: Option<DateTime<Utc>>,
    pub modified: Option<DateTime<Utc>>,
    /// Deleted or hidden on the device but still present in its database.
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookInfo {
    /// The device's id for the book (Kobo `ContentID`).
    pub device_id: String,
    pub title: Option<String>,
    pub authors: Vec<String>,
    pub isbn: Option<String>,
    pub publisher: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    /// 0–100.
    pub percent_read: Option<i64>,
    pub read_status: Option<ReadStatus>,
    pub last_read: Option<DateTime<Utc>>,
    pub origin: BookOrigin,
    /// False when annotations remain for a book that is no longer on the device.
    pub on_device: bool,
}

impl BookInfo {
    pub fn display_title(&self) -> String {
        self.title.clone().unwrap_or_else(|| "Untitled".to_string())
    }

    pub fn sort_key(&self) -> String {
        self.display_title().to_lowercase()
    }
}

/// One book and its annotations, in reading order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookAnnotations {
    pub book: BookInfo,
    pub annotations: Vec<Annotation>,
}

impl BookAnnotations {
    pub fn count(&self, kind: AnnotationKind) -> usize {
        self.annotations.iter().filter(|a| a.kind == kind).count()
    }
}
