//! Chapter titles and reading order for bookmarks (ported from Kollate,
//! verified on a Libra Colour, Clara 2E and Elipsa 2E).
//!
//! Bookmark `ContentID`s name a chapter file plus an optional fragment:
//! `<book path>!OEBPS!chapter009.xhtml#Ref_16114`. In `content`:
//! - ContentType 9 rows are spine files (no fragment); `VolumeIndex` is
//!   reading order, and `Title` is just the file name.
//! - ContentType 899 rows are table-of-contents entries: the file, maybe a
//!   fragment, and a `-N` suffix (`…#Ref_16114-1`), with the real title.

use std::collections::HashMap;

#[derive(Debug, Clone)]
struct TocEntry {
    fragment: Option<String>,
    title: String,
    toc_index: i64,
}

/// One book's table of contents and spine.
#[derive(Debug, Default)]
pub(crate) struct ChapterIndex {
    /// Chapter file → its TOC entries, in TOC order.
    toc: HashMap<String, Vec<TocEntry>>,
    /// Chapter file → spine index.
    spine: HashMap<String, i64>,
}

/// How confidently a chapter was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolution {
    Exact,
    /// The file holds several TOC entries and none matched the bookmark's
    /// fragment, so the file's first entry was used. Callers may replace it
    /// with the chapter of the preceding exactly-resolved bookmark.
    FileStart,
}

fn split_id(id: &str) -> (&str, Option<&str>) {
    match id.split_once('#') {
        Some((file, fragment)) => (file, Some(fragment)),
        None => (id, None),
    }
}

/// Strips the `-N` Kobo appends to TOC content ids.
fn strip_toc_suffix(id: &str) -> &str {
    match id.rsplit_once('-') {
        Some((head, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => id,
    }
}

impl ChapterIndex {
    pub fn add_toc_entry(&mut self, content_id: &str, title: &str, toc_index: i64) {
        let (file, fragment) = split_id(strip_toc_suffix(content_id));
        let entries = self.toc.entry(file.to_owned()).or_default();
        entries.push(TocEntry {
            fragment: fragment.map(str::to_owned),
            title: title.to_owned(),
            toc_index,
        });
        entries.sort_by_key(|e| e.toc_index);
    }

    pub fn add_spine_file(&mut self, content_id: &str, spine_index: i64) {
        self.spine.insert(content_id.to_owned(), spine_index);
    }

    pub fn spine_index(&self, content_id: &str) -> Option<i64> {
        self.spine.get(split_id(content_id).0).copied()
    }

    /// Chapter title for a bookmark's `ContentID`:
    /// 1. the TOC entry with the same file and fragment;
    /// 2. otherwise the first TOC entry in the same file;
    /// 3. otherwise the last TOC entry of the nearest preceding spine file
    ///    (a chapter split across several files).
    pub fn resolve(&self, content_id: &str) -> Option<(&str, Resolution)> {
        let (file, fragment) = split_id(content_id);
        if let Some(entries) = self.toc.get(file) {
            if let Some(e) =
                fragment.and_then(|f| entries.iter().find(|e| e.fragment.as_deref() == Some(f)))
            {
                return Some((&e.title, Resolution::Exact));
            }
            let kind = if entries.len() > 1 { Resolution::FileStart } else { Resolution::Exact };
            return entries.first().map(|e| (e.title.as_str(), kind));
        }
        let here = self.spine.get(file)?;
        self.toc
            .iter()
            .filter_map(|(f, entries)| Some((*self.spine.get(f)?, entries)))
            .filter(|(index, _)| index < here)
            .max_by_key(|(index, _)| *index)
            .and_then(|(_, entries)| entries.last())
            .map(|e| (e.title.as_str(), Resolution::Exact))
    }
}

/// Orders positions within one chapter file. Kobo positions are a span path
/// (`span#kobo\.153\.2`) plus a character offset inside that span, compared
/// numerically; positions without Kobo spans fall back to chapter progress.
pub(crate) fn position_key(start_path: &str, start_offset: i64, chapter_progress: f64) -> String {
    let spans = kobo_span_numbers(start_path);
    if spans.is_empty() {
        return format!("p{:012}", (chapter_progress.clamp(0.0, 1.0) * 1e11) as u64);
    }
    let mut key = String::from("s");
    for n in spans {
        key.push_str(&format!("{n:08}."));
    }
    key.push_str(&format!("{:08}", start_offset.max(0)));
    key
}

/// Parses `span#kobo\.10\.4` into `[10, 4]`.
fn kobo_span_numbers(path: &str) -> Vec<i64> {
    path.rsplit_once("kobo")
        .map(|(_, rest)| {
            rest.split(|c: char| !c.is_ascii_digit())
                .filter_map(|n| n.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> ChapterIndex {
        let mut ix = ChapterIndex::default();
        ix.add_toc_entry("b!OEBPS!a.xhtml#p1-1", "Part One", 1);
        ix.add_toc_entry("b!OEBPS!a.xhtml#p2-1", "Chapter 1", 2);
        ix.add_toc_entry("b!OEBPS!c.xhtml-1", "Chapter 2", 3);
        ix.add_spine_file("b!OEBPS!a.xhtml", 0);
        ix.add_spine_file("b!OEBPS!a_split.xhtml", 1);
        ix.add_spine_file("b!OEBPS!c.xhtml", 2);
        ix
    }

    #[test]
    fn resolves_by_fragment_file_and_preceding_spine() {
        let ix = index();
        let title = |id: &str| ix.resolve(id).map(|(t, _)| t);
        assert_eq!(title("b!OEBPS!a.xhtml#p2"), Some("Chapter 1"));
        assert_eq!(title("b!OEBPS!a.xhtml#zzz"), Some("Part One"));
        assert_eq!(title("b!OEBPS!c.xhtml"), Some("Chapter 2"));
        assert_eq!(title("b!OEBPS!a_split.xhtml#x"), Some("Chapter 1"));
        assert_eq!(ix.spine_index("b!OEBPS!c.xhtml#frag"), Some(2));
        assert_eq!(title("b!OEBPS!unknown.xhtml"), None);
        assert_eq!(ix.resolve("b!OEBPS!a.xhtml").unwrap().1, Resolution::FileStart);
        assert_eq!(ix.resolve("b!OEBPS!c.xhtml").unwrap().1, Resolution::Exact);
    }

    #[test]
    fn position_key_orders_spans_before_offsets() {
        // Real positions from four highlights in one paragraph group.
        let mut keys = [
            ("green", position_key(r"span#kobo\.153\.3", 27, 0.1)),
            ("blue", position_key(r"span#kobo\.153\.2", 30, 0.1)),
            ("yellow", position_key(r"span#kobo\.153\.1", 309, 0.1)),
            ("pink", position_key(r"span#kobo\.153\.1", 356, 0.1)),
            ("later", position_key(r"span#kobo\.1000\.1", 0, 0.1)),
        ];
        keys.sort_by(|a, b| a.1.cmp(&b.1));
        let order: Vec<_> = keys.iter().map(|k| k.0).collect();
        assert_eq!(order, ["yellow", "pink", "blue", "green", "later"]);
    }
}
