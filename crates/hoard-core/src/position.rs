//! Reading positions that mean the same place in every reader.
//!
//! A [`Spot`] is a chapter (its index in the book's spine) and how far into
//! that chapter's body text it is, in UTF-16 code units (as EPUB CFIs count).
//! The Kobo's copy of a book only wraps the text in `koboSpan`s, so the same
//! spot is the same words in the Kobo's copy and in the library's EPUB:
//!
//! - the Kobo saves a position as `chapter.xhtml#kobo.P.S` (a span in its
//!   copy): [`BookText::from_kobo`] and [`BookText::to_kobo`];
//! - Foliate saves an EPUB CFI against the EPUB it opened:
//!   [`BookText::from_cfi`] and [`BookText::to_cfi`].
//!
//! Open the Kobo's copy for the Kobo's positions and the library's EPUB for
//! Foliate's, and check [`BookText::same_text`] before carrying a spot from
//! one to the other.

use std::io::Read;
use std::path::Path;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::Result;
use crate::kobo::epub::{attr, html_entity, io_err, join_zip_path};

/// A place in a book: spine index, and UTF-16 units into that chapter's
/// body text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Spot {
    pub spine: usize,
    pub offset: usize,
}

#[derive(Debug)]
enum Node {
    Element {
        id: Option<String>,
        children: Vec<Node>,
    },
    Text {
        text: String,
        /// Body-text offset where it starts (`None` outside the body).
        start: Option<usize>,
    },
}

#[derive(Debug)]
struct Chapter {
    /// The zip path of the chapter.
    path: String,
    /// The root element (`html`).
    root: Node,
    /// Length of the body text, in UTF-16 units.
    len: usize,
}

/// The text of a book's chapters, for finding positions in it.
#[derive(Debug)]
pub struct BookText {
    /// By spine index; `None` for an item that couldn't be read.
    chapters: Vec<Option<Chapter>>,
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The byte index in `s` that is `units` UTF-16 units in.
fn byte_at_utf16(s: &str, units: usize) -> usize {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        if n >= units {
            return i;
        }
        n += c.len_utf16();
    }
    s.len()
}

/// Parses a chapter into a tree of elements and text, the way a browser
/// sees an XHTML document for CFIs: comments and processing instructions
/// dropped, adjacent text merged.
fn parse_chapter(path: &str, xhtml: &str) -> Option<Chapter> {
    let mut reader = Reader::from_str(xhtml);
    let config = reader.config_mut();
    config.trim_text(false);
    config.check_end_names = false;
    // (id, is body, children)
    let mut stack: Vec<(Option<String>, bool, Vec<Node>)> = Vec::new();
    let mut root = None;
    let mut body_depth: Option<usize> = None;
    let mut offset = 0usize;
    let push_text = |stack: &mut Vec<(Option<String>, bool, Vec<Node>)>,
                     text: &str,
                     in_body: bool,
                     offset: &mut usize| {
        let Some((_, _, children)) = stack.last_mut() else {
            return;
        };
        if let Some(Node::Text { text: t, .. }) = children.last_mut() {
            t.push_str(text);
        } else {
            children.push(Node::Text {
                text: text.to_owned(),
                start: in_body.then_some(*offset),
            });
        }
        if in_body {
            *offset += utf16_len(text);
        }
    };
    loop {
        let in_body = body_depth.is_some_and(|d| stack.len() > d);
        match reader.read_event().ok()? {
            Event::Start(e) => {
                let is_body = e.local_name().as_ref().eq_ignore_ascii_case("body");
                if is_body && body_depth.is_none() {
                    body_depth = Some(stack.len());
                }
                stack.push((attr(&e, "id"), is_body, Vec::new()));
            }
            Event::Empty(e) => {
                let node = Node::Element {
                    id: attr(&e, "id"),
                    children: Vec::new(),
                };
                match stack.last_mut() {
                    Some((_, _, children)) => children.push(node),
                    None => root = Some(node),
                }
            }
            Event::End(_) => {
                let Some((id, is_body, children)) = stack.pop() else {
                    continue;
                };
                if is_body && body_depth == Some(stack.len()) {
                    body_depth = None;
                }
                let node = Node::Element { id, children };
                match stack.last_mut() {
                    Some((_, _, siblings)) => siblings.push(node),
                    None => root = Some(node),
                }
            }
            Event::Text(t) => push_text(&mut stack, &t.xml10_content(), in_body, &mut offset),
            Event::CData(t) => push_text(&mut stack, &t.xml10_content(), in_body, &mut offset),
            Event::GeneralRef(r) => {
                let text = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    _ => {
                        let name: &str = &r;
                        match name {
                            "quot" => "\"",
                            "apos" => "'",
                            "amp" => "&",
                            "lt" => "<",
                            "gt" => ">",
                            other => html_entity(other),
                        }
                        .to_owned()
                    }
                };
                push_text(&mut stack, &text, in_body, &mut offset);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Some(Chapter {
        path: path.to_owned(),
        root: root?,
        len: offset,
    })
}

/// A node's children as a CFI indexes them: elements at even indices
/// (2, 4, …), the text between them at odd ones (1, 3, …).
fn cfi_child(children: &[Node], index: usize) -> Option<&Node> {
    if index == 0 {
        return None;
    }
    let mut i = 0usize; // CFI index of the last slot passed
    let mut last_was_text = false;
    for child in children {
        match child {
            Node::Element { .. } => {
                // The next even index.
                i = if i.is_multiple_of(2) { i + 2 } else { i + 1 };
                last_was_text = false;
            }
            Node::Text { .. } => {
                if last_was_text {
                    continue;
                }
                i += 1;
                last_was_text = true;
            }
        }
        if i == index {
            return Some(child);
        }
        if i > index {
            return None;
        }
    }
    None
}

/// The first body-text offset at or after the start of `node`.
fn first_offset(node: &Node) -> Option<usize> {
    match node {
        Node::Text { start, .. } => *start,
        Node::Element { children, .. } => children.iter().find_map(first_offset),
    }
}

/// Visits the text nodes in document order with their CFI path (from the
/// root element) and their body offset.
fn each_text<'a>(
    node: &'a Node,
    path: &mut Vec<usize>,
    f: &mut impl FnMut(&[usize], &'a str, usize) -> bool,
) -> bool {
    let Node::Element { children, .. } = node else {
        return false;
    };
    let mut i = 0usize;
    for child in children {
        match child {
            Node::Element { .. } => {
                i = if i.is_multiple_of(2) { i + 2 } else { i + 1 };
                path.push(i);
                let stop = each_text(child, path, f);
                path.pop();
                if stop {
                    return true;
                }
            }
            Node::Text { text, start } => {
                i += 1;
                if let Some(start) = start {
                    path.push(i);
                    let stop = f(path, text, *start);
                    path.pop();
                    if stop {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// The `koboSpan` IDs in document order with the body offset each starts at.
fn kobo_spans<'a>(node: &'a Node, out: &mut Vec<(&'a str, usize)>) {
    if let Node::Element { id, children } = node {
        if let Some(id) = id.as_deref().filter(|id| id.starts_with("kobo."))
            && let Some(start) = first_offset(node)
        {
            out.push((id, start));
        }
        for child in children {
            kobo_spans(child, out);
        }
    }
}

fn find_id<'a>(node: &'a Node, want: &str) -> Option<&'a Node> {
    let Node::Element { id, children } = node else {
        return None;
    };
    if id.as_deref() == Some(want) {
        return Some(node);
    }
    children.iter().find_map(|c| find_id(c, want))
}

impl BookText {
    /// Reads an EPUB (or KEPUB): every spine item, in order.
    pub fn open(path: &Path) -> Result<Self> {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?).map_err(io_err)?;
        let mut read = |name: &str| -> Option<String> {
            let mut bytes = Vec::new();
            zip.by_name(name).ok()?.read_to_end(&mut bytes).ok()?;
            Some(String::from_utf8_lossy(&bytes).into_owned())
        };
        let container = read("META-INF/container.xml").ok_or_else(|| io_err("no container.xml"))?;
        let mut opf_path = None;
        let mut reader = Reader::from_str(&container);
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e))
                    if e.local_name().as_ref() == "rootfile" =>
                {
                    opf_path = attr(&e, "full-path");
                    break;
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
        let opf_path = opf_path.ok_or_else(|| io_err("no rootfile in container.xml"))?;
        let opf = read(&opf_path).ok_or_else(|| io_err("no package document"))?;
        let mut manifest = std::collections::HashMap::new();
        let mut spine = Vec::new();
        let mut reader = Reader::from_str(&opf);
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                    "item" => {
                        if let (Some(id), Some(href)) = (attr(&e, "id"), attr(&e, "href")) {
                            manifest.insert(id, join_zip_path(&opf_path, &href));
                        }
                    }
                    "itemref" => spine.push(attr(&e, "idref")),
                    _ => {}
                },
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
        let chapters = spine
            .into_iter()
            .map(|idref| {
                let path = manifest.get(&idref?)?;
                parse_chapter(path, &read(path)?)
            })
            .collect();
        Ok(Self { chapters })
    }

    fn chapter(&self, spine: usize) -> Option<&Chapter> {
        self.chapters.get(spine)?.as_ref()
    }

    /// Whether `other` has the same text, chapter by chapter (within a few
    /// characters each, as tidying a chapter may change its spacing), so a
    /// spot in one is the same words in the other.
    pub fn same_text(&self, other: &BookText) -> bool {
        self.chapters.len() == other.chapters.len()
            && self
                .chapters
                .iter()
                .zip(&other.chapters)
                .all(|(a, b)| match (a, b) {
                    (Some(a), Some(b)) => a.len.abs_diff(b.len) <= 16 + a.len / 200,
                    (None, None) => true,
                    _ => false,
                })
    }

    /// How far through the book `spot` is, 0.0–1.0, by text.
    pub fn fraction(&self, spot: Spot) -> f64 {
        let lens: Vec<usize> = self
            .chapters
            .iter()
            .map(|c| c.as_ref().map_or(0, |c| c.len))
            .collect();
        let total: usize = lens.iter().sum();
        if total == 0 {
            return 0.0;
        }
        let before: usize = lens.iter().take(spot.spine).sum();
        let here = spot.offset.min(lens.get(spot.spine).copied().unwrap_or(0));
        (before + here) as f64 / total as f64
    }

    /// Up to `n` characters of text from `spot` on (for showing where a
    /// position is).
    pub fn text_at(&self, spot: Spot, n: usize) -> String {
        let Some(chapter) = self.chapter(spot.spine) else {
            return String::new();
        };
        let mut out = String::new();
        each_text(&chapter.root, &mut Vec::new(), &mut |_, text, start| {
            let len = utf16_len(text);
            if start + len <= spot.offset {
                return false;
            }
            let from = byte_at_utf16(text, spot.offset.saturating_sub(start));
            out.push_str(&text[from..]);
            out.chars().count() >= n
        });
        let squeezed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
        squeezed.chars().take(n).collect()
    }

    /// The spot an EPUB CFI points to (for a range, its start).
    pub fn from_cfi(&self, cfi: &str) -> Option<Spot> {
        let inner = cfi.trim().strip_prefix("epubcfi(")?.strip_suffix(')')?;
        // A range is `parent,start,end`; the start is parent + start.
        let mut parts = inner.split(',');
        let path = match (parts.next(), parts.next()) {
            (Some(parent), Some(start)) => format!("{parent}{start}"),
            (Some(whole), None) => whole.to_owned(),
            _ => return None,
        };
        let (package, content) = path.split_once('!')?;
        let steps = |s: &str| -> Option<Vec<(usize, Option<usize>)>> {
            s.split('/')
                .skip(1)
                .map(|step| {
                    // Drop an ID assertion ([…]) and side bias etc.
                    let step = step.split('[').next()?;
                    let (index, offset) = match step.split_once(':') {
                        Some((i, o)) => {
                            let o: String = o.chars().take_while(char::is_ascii_digit).collect();
                            (i, o.parse().ok())
                        }
                        None => (step, None),
                    };
                    Some((index.parse().ok()?, offset))
                })
                .collect()
        };
        let package = steps(package)?;
        // /6 is the spine; the next step is the item.
        let &[(6, None), (item, _)] = package.as_slice() else {
            return None;
        };
        let spine = (item / 2).checked_sub(1)?;
        let chapter = self.chapter(spine)?;
        let mut node = &chapter.root;
        let steps = steps(content)?;
        let mut offset_in_text = 0;
        for (n, &(index, offset)) in steps.iter().enumerate() {
            let Node::Element { children, .. } = node else {
                break;
            };
            let Some(child) = cfi_child(children, index) else {
                break;
            };
            node = child;
            if n + 1 == steps.len() {
                offset_in_text = offset.unwrap_or(0);
            }
        }
        let offset = match node {
            Node::Text { text, start } => {
                let start = (*start)?;
                start + offset_in_text.min(utf16_len(text))
            }
            element => first_offset(element).unwrap_or(0),
        };
        Some(Spot { spine, offset })
    }

    /// An EPUB CFI for `spot`, pointing into the text node it's in.
    pub fn to_cfi(&self, spot: Spot) -> Option<String> {
        let chapter = self.chapter(spot.spine)?;
        let mut found = None;
        let mut last = None;
        each_text(&chapter.root, &mut Vec::new(), &mut |path, text, start| {
            // Blank text (between paragraphs) is never the place: the spot
            // goes to the words after it.
            if text.trim().is_empty() {
                return false;
            }
            let len = utf16_len(text);
            if start + len <= spot.offset {
                last = Some((path.to_vec(), len));
                return false;
            }
            found = Some((path.to_vec(), spot.offset.saturating_sub(start)));
            true
        });
        // Past the last words: the end of them.
        let (path, local) = found.or(last)?;
        let steps: String = path.iter().map(|i| format!("/{i}")).collect();
        Some(format!(
            "epubcfi(/6/{}!{steps}:{local})",
            (spot.spine + 1) * 2
        ))
    }

    /// The spot a Kobo position (`chapter.xhtml#kobo.P.S`, as in
    /// `ChapterIDBookmarked`) points to, in the Kobo's copy.
    pub fn from_kobo(&self, location: &str) -> Option<Spot> {
        let (path, span) = location.split_once('#')?;
        let spine = self.chapters.iter().position(|c| {
            c.as_ref()
                .is_some_and(|c| c.path == path || c.path.ends_with(&format!("/{path}")))
        })?;
        let chapter = self.chapter(spine)?;
        let offset = find_id(&chapter.root, span).and_then(first_offset)?;
        Some(Spot { spine, offset })
    }

    /// The Kobo position for `spot`: the span it's in, in the Kobo's copy.
    pub fn to_kobo(&self, spot: Spot) -> Option<String> {
        let chapter = self.chapter(spot.spine)?;
        let mut spans = Vec::new();
        kobo_spans(&chapter.root, &mut spans);
        let (id, _) = spans
            .iter()
            .rev()
            .find(|(_, start)| *start <= spot.offset)
            .or_else(|| spans.first())?;
        Some(format!("{}#{id}", chapter.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub::tests::write_epub_with;

    const CHAPTER: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Ignored</title></head>
<body>
  <h1>Chapter One</h1>
  <p>It was a dark night. The wind <em>howled</em>&#160;loud.</p>
  <!-- a comment -->
  <p>Second paragraph.</p>
</body>
</html>"#;

    fn book(dir: &Path, name: &str, chapter: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        write_epub_with(
            &path,
            &[("OEBPS/intro.xhtml", CHAPTER), ("OEBPS/ch1.xhtml", chapter)],
        );
        path
    }

    #[test]
    fn cfis_and_kobo_spans_meet_at_the_same_words() {
        let dir = tempfile::tempdir().unwrap();
        let epub = BookText::open(&book(dir.path(), "a.epub", CHAPTER)).unwrap();
        let (kepub, _) = crate::kobo::kepub::convert_document(CHAPTER).unwrap();
        let kobo = BookText::open(&book(dir.path(), "a.kepub.epub", &kepub)).unwrap();
        assert!(epub.same_text(&kobo));

        // Kobo → spot → the same words in the EPUB → CFI → back.
        let spot = kobo.from_kobo("ch1.xhtml#kobo.2.2").unwrap();
        assert_eq!(spot.spine, 1);
        assert!(
            epub.text_at(spot, 30).starts_with("The wind howled"),
            "{}",
            epub.text_at(spot, 30)
        );
        let cfi = epub.to_cfi(spot).unwrap();
        assert_eq!(epub.from_cfi(&cfi), Some(spot), "{cfi}");
        assert_eq!(
            kobo.to_kobo(spot).as_deref(),
            Some("OEBPS/ch1.xhtml#kobo.2.2")
        );

        // A CFI into the middle of a sentence lands in that sentence's span.
        // /4 body, /4 the <p> (h1 is /2), /1 its first text, 25 chars in.
        let spot = epub.from_cfi("epubcfi(/6/4!/4/4/1:25)").unwrap();
        assert!(
            epub.text_at(spot, 10).starts_with("wind"),
            "{}",
            epub.text_at(spot, 10)
        );
        assert_eq!(
            kobo.to_kobo(spot).as_deref(),
            Some("OEBPS/ch1.xhtml#kobo.2.2")
        );

        // Foliate's range CFIs (parent,start,end) use the start.
        let spot = epub.from_cfi("epubcfi(/6/4!/4,/6,/6/1:5)").unwrap();
        assert!(epub.text_at(spot, 6).starts_with("Second"));
        assert_eq!(
            kobo.to_kobo(spot).as_deref(),
            Some("OEBPS/ch1.xhtml#kobo.3.1")
        );

        // After the comment, the text nodes either side of it are one.
        assert!(epub.from_cfi("epubcfi(/6/4!/4/5:3)").is_some());
        // Nonsense finds nothing.
        assert!(epub.from_cfi("epubcfi(/6/40!/4/2)").is_none());
        assert!(kobo.from_kobo("ch1.xhtml#kobo.99.1").is_none());
    }

    #[test]
    fn fraction_counts_text_across_chapters() {
        let dir = tempfile::tempdir().unwrap();
        let epub = BookText::open(&book(dir.path(), "a.epub", CHAPTER)).unwrap();
        assert_eq!(
            epub.fraction(Spot {
                spine: 0,
                offset: 0
            }),
            0.0
        );
        assert!(
            (epub.fraction(Spot {
                spine: 1,
                offset: 0
            }) - 0.5)
                .abs()
                < 1e-9
        );
        assert_eq!(
            epub.fraction(Spot {
                spine: 1,
                offset: usize::MAX
            }),
            1.0
        );
    }
}
