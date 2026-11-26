use crate::model::BookAnnotations;

/// Serialises books and their annotations as pretty-printed JSON.
pub fn render(books: &[BookAnnotations]) -> serde_json::Result<String> {
    serde_json::to_string_pretty(books)
}
