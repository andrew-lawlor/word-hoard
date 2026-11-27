//! EPUB → KEPUB conversion (Word Hoard SPEC, "Writing to the device"), in
//! the manner of kepubify: Kobo's own reader (Nickel) treats a
//! `.kepub.epub` as a Kobo book (faster, with page and chapter statistics
//! and reliable highlights) when its text is wrapped in `koboSpan`s.
//!
//! Every content document (`.xhtml`, `.html`, `.htm`) is rewritten:
//! - the body's content goes inside `<div id="book-columns"><div
//!   id="book-inner">`, which Nickel lays out pages with;
//! - each sentence of text becomes `<span class="koboSpan"
//!   id="kobo.P.S">`, P counting paragraphs (block elements with text) and S
//!   sentences within one; highlights are anchored to these ids, so the
//!   numbering is deterministic: the same EPUB always converts the same way;
//! - a small style rule keeps `book-inner` from adding margins.
//!
//! Text in `head`, `script`, `style`, `pre`, `svg` and `math`, and text
//! already in a koboSpan, is left alone. Everything else is copied as is.

use std::io::{Read, Write};
use std::path::Path;

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};

use crate::kobo::epub::{html_entity, io_err, is_drm};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KepubStats {
    /// Content documents rewritten.
    pub documents: usize,
    /// koboSpans added.
    pub spans: usize,
    /// Documents that weren't well-formed XHTML, tidied first.
    pub tidied: usize,
}

const STYLE_HACK: &str = "div#book-inner { margin-top: 0; margin-bottom: 0; }";

/// Elements whose text gets its own paragraph number.
const BLOCKS: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "li",
    "dt",
    "dd",
    "td",
    "th",
    "caption",
    "figcaption",
    "blockquote",
    "div",
    "section",
    "article",
    "aside",
    "header",
    "footer",
    "address",
    "center",
];

/// Elements whose text is never wrapped.
const SKIP: &[&str] = &[
    "head", "script", "style", "pre", "svg", "math", "textarea", "title",
];

fn is_content_document(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".xhtml", ".html", ".htm"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// Converts the EPUB at `src` into a KEPUB at `dest`. Refuses DRM-locked
/// books (their text can't be read, let alone rewritten).
pub fn convert(src: &Path, dest: &Path) -> Result<KepubStats> {
    convert_with_cover(src, dest, None)
}

/// [`convert`], putting `cover` (the library's cover for the book) in place
/// of the image the book marks as its cover, as Calibre does when it sends a
/// book. The image keeps its name in the book; it's re-encoded if the
/// formats differ. A book that marks no cover keeps its own images.
pub fn convert_with_cover(src: &Path, dest: &Path, cover: Option<&Path>) -> Result<KepubStats> {
    kobo_copy(
        src,
        dest,
        &KoboCopy {
            cover,
            ..Default::default()
        },
    )
}

/// How to make the copy of a book that goes to the Kobo.
#[derive(Debug, Default, Clone, Copy)]
pub struct KoboCopy<'a> {
    /// The library's cover, in place of the book's own.
    pub cover: Option<&'a Path>,
    /// The library's details, written into the copy's OPF.
    pub metadata: Option<&'a super::opf_meta::SendMetadata>,
    /// Leave the text alone: the book is a KEPUB already.
    pub already_kepub: bool,
}

/// Writes the copy of the EPUB or KEPUB at `src` that goes to the Kobo, at
/// `dest`: converted to KEPUB (unless it is one), with the library's cover
/// and details. `src` is only read.
pub fn kobo_copy(src: &Path, dest: &Path, how: &KoboCopy) -> Result<KepubStats> {
    let cover = how.cover;
    let mut input = zip::ZipArchive::new(std::fs::File::open(src)?).map_err(io_err)?;
    if is_drm(&mut input) {
        return Err(Error::Drm(src.to_path_buf()));
    }
    crate::kobo::ensure_not_on_kobo(dest)?;
    let mut output = zip::ZipWriter::new(std::fs::File::create(dest)?);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // The mimetype entry comes first, uncompressed, as EPUB requires.
    output.start_file("mimetype", stored).map_err(io_err)?;
    output.write_all(b"application/epub+zip")?;

    let opf_path = package_path(&mut input);
    // Where the book's cover image is, when there's a library cover to put there.
    let cover_entry = match (cover, &opf_path) {
        (Some(_), Some(opf_path)) => {
            let mut opf = String::new();
            input
                .by_name(opf_path)
                .map_err(io_err)?
                .read_to_string(&mut opf)?;
            cover_href(&opf).map(|href| crate::kobo::epub::join_zip_path(opf_path, &href))
        }
        _ => None,
    };
    // Documents that aren't well-formed XML (HTML5-style chapters) are
    // tidied into XHTML before converting; found first, so the package
    // document can say so.
    let mut tidy: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !how.already_kepub {
        for i in 0..input.len() {
            let mut entry = input.by_index(i).map_err(io_err)?;
            let name = entry.name().to_owned();
            if !is_content_document(&name) {
                continue;
            }
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes)?;
            if !super::tidy::is_well_formed(&String::from_utf8_lossy(&bytes)) {
                tidy.insert(name);
            }
        }
    }
    let mut stats = KepubStats::default();
    for i in 0..input.len() {
        let mut entry = input.by_index(i).map_err(io_err)?;
        let name = entry.name().to_owned();
        if name == "mimetype" || entry.is_dir() {
            continue;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes)?;
        drop(entry);
        if let (Some(cover), Some(entry)) = (cover, &cover_entry)
            && *entry == name
        {
            bytes = cover_as(cover, &name)?;
        } else if Some(name.as_str()) == opf_path.as_deref() {
            let mut opf = String::from_utf8_lossy(&bytes).into_owned();
            if let Some(fixed) = mark_cover_image(&opf) {
                opf = fixed;
            }
            if !tidy.is_empty() {
                opf = mark_xhtml(&opf, opf_path.as_deref().unwrap_or(""), &tidy);
            }
            if let Some(meta) = how.metadata
                && let Some(with_details) = super::opf_meta::apply(&opf, meta)
            {
                opf = with_details;
            }
            bytes = opf.into_bytes();
        } else if !how.already_kepub && is_content_document(&name) {
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            if tidy.contains(&name) {
                text = super::tidy::to_xhtml(&text);
                stats.tidied += 1;
            }
            let (converted, spans) = convert_document(&text)?;
            bytes = converted.into_bytes();
            stats.documents += 1;
            stats.spans += spans;
        }
        output.start_file(&name, deflated).map_err(io_err)?;
        output.write_all(&bytes)?;
    }
    output.finish().map_err(io_err)?;
    Ok(stats)
}

/// The package document's path, from `META-INF/container.xml`.
pub(crate) fn package_path<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Option<String> {
    let mut container = String::new();
    zip.by_name("META-INF/container.xml")
        .ok()?
        .read_to_string(&mut container)
        .ok()?;
    let mut reader = Reader::from_str(&container);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == "rootfile" => {
                return crate::kobo::epub::attr(&e, "full-path");
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Marks an EPUB 2 book's cover for the Kobo. Such books name their cover
/// only with `<meta name="cover" content="ID">`; Nickel looks for the
/// manifest item with `properties="cover-image"` (EPUB 3), and without one
/// it may show another image from the book. `None` when there's nothing to
/// change (the cover is already marked, or the book names none).
/// `opf` with the manifest items for `tidied` documents (zip paths) marked
/// `application/xhtml+xml`, as they are once tidied.
fn mark_xhtml(opf: &str, opf_path: &str, tidied: &std::collections::HashSet<String>) -> String {
    let mut out = String::with_capacity(opf.len());
    let mut reader = Reader::from_str(opf);
    let mut last = 0usize;
    loop {
        let before = reader.buffer_position() as usize;
        let event = match reader.read_event() {
            Ok(e) => e,
            Err(_) => return opf.to_owned(),
        };
        let after = reader.buffer_position() as usize;
        match event {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "item" => {
                let href = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.as_ref() == "href")
                    .and_then(|a| {
                        a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .ok()
                            .map(|v| v.into_owned())
                    });
                let raw = &opf[before..after];
                if let Some(href) = href
                    && tidied.contains(&crate::kobo::epub::join_zip_path(opf_path, &href))
                    && raw.contains("text/html")
                {
                    out.push_str(&opf[last..before]);
                    out.push_str(&raw.replacen("\"text/html\"", "\"application/xhtml+xml\"", 1));
                    last = after;
                }
            }
            _ => {}
        }
    }
    out.push_str(&opf[last..]);
    out
}

pub(crate) fn mark_cover_image(opf: &str) -> Option<String> {
    // First pass: is a cover already marked, and which item does the meta name?
    let mut reader = Reader::from_str(opf);
    let mut cover_id = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                "item" => {
                    let props = crate::kobo::epub::attr(&e, "properties").unwrap_or_default();
                    if props.split_whitespace().any(|p| p == "cover-image") {
                        return None;
                    }
                }
                "meta" if crate::kobo::epub::attr(&e, "name").as_deref() == Some("cover") => {
                    cover_id = crate::kobo::epub::attr(&e, "content");
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    let cover_id = cover_id?;
    // Second pass: copy the document, adding the property to that item.
    let mut reader = Reader::from_str(opf);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::with_capacity(opf.len() + 32));
    let mut marked = false;
    loop {
        let event = match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(e) => e,
            Err(_) => return None,
        };
        let mark = |e: &BytesStart| {
            e.local_name().as_ref() == "item"
                && crate::kobo::epub::attr(e, "id").as_deref() == Some(cover_id.as_str())
        };
        let written = match event {
            Event::Empty(e) if mark(&e) => {
                marked = true;
                writer.write_event(Event::Empty(with_cover_property(&e)))
            }
            Event::Start(e) if mark(&e) => {
                marked = true;
                writer.write_event(Event::Start(with_cover_property(&e)))
            }
            e => writer.write_event(e),
        };
        written.ok()?;
    }
    marked
        .then(|| String::from_utf8(writer.into_inner()).ok())
        .flatten()
}

/// The element with `cover-image` added to its `properties`.
fn with_cover_property(e: &BytesStart) -> BytesStart<'static> {
    let name = String::from_utf8_lossy(e.name().as_ref().as_bytes()).into_owned();
    let mut out = BytesStart::new(name);
    let mut had = false;
    for a in e.attributes().flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref().as_bytes()).into_owned();
        let value = a
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_default();
        if key == "properties" {
            had = true;
            out.push_attribute((key.as_str(), format!("{value} cover-image").trim()));
        } else {
            out.push_attribute((key.as_str(), value.as_str()));
        }
    }
    if !had {
        out.push_attribute(("properties", "cover-image"));
    }
    out
}

/// The href of the book's cover image: the item marked `cover-image`, else
/// the one EPUB 2's `<meta name="cover">` names.
pub(crate) fn cover_href(opf: &str) -> Option<String> {
    let mut reader = Reader::from_str(opf);
    let (mut marked, mut meta_id) = (None, None);
    let mut items: Vec<(String, String)> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                "item" => {
                    let attr = |n| crate::kobo::epub::attr(&e, n);
                    let (Some(id), Some(href)) = (attr("id"), attr("href")) else {
                        continue;
                    };
                    if attr("properties")
                        .is_some_and(|p| p.split_whitespace().any(|p| p == "cover-image"))
                    {
                        marked = Some(href.clone());
                    }
                    items.push((id, href));
                }
                "meta" if crate::kobo::epub::attr(&e, "name").as_deref() == Some("cover") => {
                    meta_id = crate::kobo::epub::attr(&e, "content");
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    marked.or_else(|| {
        let id = meta_id?;
        items.into_iter().find(|(i, _)| *i == id).map(|(_, h)| h)
    })
}

/// The image at `cover`, in the format the book's file `name` is in.
fn cover_as(cover: &Path, name: &str) -> Result<Vec<u8>> {
    let bytes = std::fs::read(cover)?;
    let lower = name.to_ascii_lowercase();
    let want = if lower.ends_with(".png") {
        image::ImageFormat::Png
    } else {
        image::ImageFormat::Jpeg
    };
    // A baseline JPEG goes in as it is. Anything else is re-encoded, a
    // progressive JPEG included: Kobos can't decode those for covers and
    // fall back to a plain one (found with a cover from Calibre).
    let progressive = bytes.windows(2).any(|w| w == [0xFF, 0xC2]);
    if image::guess_format(&bytes).ok() == Some(want) && !progressive {
        return Ok(bytes);
    }
    let decoded = image::load_from_memory(&bytes).map_err(io_err)?;
    // JPEG has no alpha channel.
    let decoded = image::DynamicImage::ImageRgb8(decoded.to_rgb8());
    let mut out = std::io::Cursor::new(Vec::new());
    decoded.write_to(&mut out, want).map_err(io_err)?;
    Ok(out.into_inner())
}

/// One run of text being gathered across text, entity and CDATA events.
#[derive(Default)]
struct TextRun {
    /// Decoded text; an entity that can't be decoded is kept as a marker.
    text: String,
}

/// Unknown entities (`&foo;`) are carried through as `\u{E000}foo\u{E001}`
/// and written back as they were.
const ENTITY_OPEN: char = '\u{E000}';
const ENTITY_CLOSE: char = '\u{E001}';

fn escape_with_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_entity = false;
    for c in text.chars() {
        match c {
            ENTITY_OPEN => {
                in_entity = true;
                out.push('&');
            }
            ENTITY_CLOSE => {
                in_entity = false;
                out.push(';');
            }
            '&' if !in_entity => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// Splits text into sentences, each keeping its closing punctuation,
/// closing quotes or brackets, and the whitespace after them.
pub(crate) fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (_, c) = chars[i];
        if matches!(c, '.' | '!' | '?' | '…') {
            let mut j = i + 1;
            while j < chars.len() && matches!(chars[j].1, '.' | '!' | '?' | '…') {
                j += 1;
            }
            while j < chars.len() && matches!(chars[j].1, '"' | '\'' | '’' | '”' | ')' | ']' | '»')
            {
                j += 1;
            }
            if j < chars.len() && chars[j].1.is_whitespace() {
                while j < chars.len() && chars[j].1.is_whitespace() {
                    j += 1;
                }
                let end = chars.get(j).map_or(text.len(), |(b, _)| *b);
                out.push(&text[start..end]);
                start = end;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Rewrites one XHTML document; returns it and the number of spans added.
pub(crate) fn convert_document(xhtml: &str) -> Result<(String, usize)> {
    let mut reader = Reader::from_str(xhtml);
    let config = reader.config_mut();
    config.trim_text(false);
    config.check_end_names = false;
    let mut writer = Writer::new(Vec::with_capacity(xhtml.len() + xhtml.len() / 3));

    let mut stack: Vec<String> = Vec::new();
    let mut skip_depth = 0usize;
    let mut in_body = false;
    let mut para = 0usize;
    let mut sentence = 0usize;
    // A block element opened since the last paragraph number was used.
    let mut new_para = true;
    let mut run = TextRun::default();
    let mut spans = 0usize;
    let mut styled = false;

    // Writes the gathered text: wrapped in spans in the body, as is elsewhere.
    let flush = |run: &mut TextRun,
                 writer: &mut Writer<Vec<u8>>,
                 wrap: bool,
                 para: &mut usize,
                 sentence: &mut usize,
                 new_para: &mut bool,
                 spans: &mut usize|
     -> Result<()> {
        if run.text.is_empty() {
            return Ok(());
        }
        let text = std::mem::take(&mut run.text);
        if !wrap || text.trim().is_empty() {
            writer
                .get_mut()
                .write_all(escape_with_entities(&text).as_bytes())?;
            return Ok(());
        }
        if *new_para || *para == 0 {
            *para += 1;
            *sentence = 0;
            *new_para = false;
        }
        // Leading whitespace stays outside the first span.
        let trimmed = text.trim_start();
        let lead = &text[..text.len() - trimmed.len()];
        writer
            .get_mut()
            .write_all(escape_with_entities(lead).as_bytes())?;
        for part in sentences(trimmed) {
            *sentence += 1;
            *spans += 1;
            let id = format!("kobo.{}.{}", *para, *sentence);
            let mut start = BytesStart::new("span");
            start.push_attribute(("class", "koboSpan"));
            start.push_attribute(("id", id.as_str()));
            writer.write_event(Event::Start(start))?;
            writer
                .get_mut()
                .write_all(escape_with_entities(part).as_bytes())?;
            writer.write_event(Event::End(BytesEnd::new("span")))?;
        }
        Ok(())
    };

    loop {
        let event = reader
            .read_event()
            .map_err(|e| io_err(format!("XHTML error: {e}")))?;
        let wrap = in_body && skip_depth == 0;
        match event {
            Event::Text(t) => run.text.push_str(&t.xml10_content()),
            Event::CData(t) => run.text.push_str(&t.xml10_content()),
            Event::GeneralRef(r) => match r.resolve_char_ref() {
                Ok(Some(c)) => run.text.push(c),
                _ => {
                    let name: &str = &r;
                    // XML's own five keep their exact meaning (Kollate's
                    // display table turns &apos; into a curly quote).
                    let known = match name {
                        "quot" => "\"",
                        "apos" => "'",
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        other => html_entity(other),
                    };
                    if known.is_empty() {
                        run.text.push(ENTITY_OPEN);
                        run.text.push_str(name);
                        run.text.push(ENTITY_CLOSE);
                    } else {
                        run.text.push_str(known);
                    }
                }
            },
            other => {
                flush(
                    &mut run,
                    &mut writer,
                    wrap,
                    &mut para,
                    &mut sentence,
                    &mut new_para,
                    &mut spans,
                )?;
                match other {
                    Event::Start(e) => {
                        let name = e.local_name().as_ref().to_ascii_lowercase();
                        let is_kobo_span = name == "span"
                            && e.attributes().flatten().any(|a| {
                                a.key.as_ref() == "class" && a.value.as_ref().contains("koboSpan")
                            });
                        if SKIP.contains(&name.as_str()) || is_kobo_span || skip_depth > 0 {
                            skip_depth += 1;
                        }
                        if BLOCKS.contains(&name.as_str()) {
                            new_para = true;
                        }
                        writer.write_event(Event::Start(e))?;
                        if name == "body" {
                            in_body = true;
                            writer.write_event(Event::Start(
                                BytesStart::new("div").with_attributes([("id", "book-columns")]),
                            ))?;
                            writer.write_event(Event::Start(
                                BytesStart::new("div").with_attributes([("id", "book-inner")]),
                            ))?;
                        }
                        stack.push(name);
                    }
                    Event::End(e) => {
                        let name = e.local_name().as_ref().to_ascii_lowercase();
                        if name == "head" && !styled {
                            let mut style = BytesStart::new("style");
                            style.push_attribute(("type", "text/css"));
                            style.push_attribute(("id", "kobostylehacks"));
                            writer.write_event(Event::Start(style))?;
                            writer.write_event(Event::Text(BytesText::new(STYLE_HACK)))?;
                            writer.write_event(Event::End(BytesEnd::new("style")))?;
                            styled = true;
                        }
                        if name == "body" {
                            writer.write_event(Event::End(BytesEnd::new("div")))?;
                            writer.write_event(Event::End(BytesEnd::new("div")))?;
                            in_body = false;
                        }
                        if BLOCKS.contains(&name.as_str()) {
                            new_para = true;
                        }
                        skip_depth = skip_depth.saturating_sub(1);
                        stack.pop();
                        writer.write_event(Event::End(e))?;
                    }
                    Event::Eof => break,
                    e => writer.write_event(e)?,
                }
            }
        }
    }
    let out = String::from_utf8(writer.into_inner()).map_err(io_err)?;
    Ok((out, spans))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTER: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>One. Two.</title><style>p { margin: 0; }</style></head>
<body>
  <h1>Chapter One</h1>
  <p>It was a dark night. The wind <em>howled!</em> Tom &amp; Jerry ran&#8230; “Where?” she asked.</p>
  <p>Second&nbsp;paragraph &unknown; here, 34&apos; &quot;wide&quot;.</p>
  <pre>Code. Stays.</pre>
  <p><span class="koboSpan" id="kobo.9.9">Already done.</span></p>
  <script>var a = "x. y.";</script>
</body>
</html>"#;

    fn strip_spans(s: &str) -> String {
        let mut out = s.to_owned();
        while let Some(start) = out.find("<span class=\"koboSpan\"") {
            let end = out[start..].find('>').unwrap() + start + 1;
            out.replace_range(start..end, "");
            let close = out[start..].find("</span>").unwrap() + start;
            out.replace_range(close..close + 7, "");
        }
        out
    }

    #[test]
    fn splits_sentences_keeping_punctuation_and_space() {
        assert_eq!(sentences("A b. C d! E?"), ["A b. ", "C d! ", "E?"]);
        assert_eq!(
            sentences("He said “Go.” Then left."),
            ["He said “Go.” ", "Then left."]
        );
        assert_eq!(sentences("Wait… what"), ["Wait… ", "what"]);
        assert_eq!(sentences("3.14 is pi"), ["3.14 is pi"]);
        assert_eq!(sentences("no end"), ["no end"]);
    }

    #[test]
    fn wraps_sentences_in_numbered_kobo_spans() {
        let (out, spans) = convert_document(CHAPTER).unwrap();
        assert!(
            out.contains(r#"<body><div id="book-columns"><div id="book-inner">"#)
                || out.contains(r#"<div id="book-columns"><div id="book-inner">"#)
        );
        assert!(out.contains(r#"</div></div></body>"#));
        assert!(out.contains(r#"<style type="text/css" id="kobostylehacks">div#book-inner"#));
        assert!(
            out.contains(r#"<h1><span class="koboSpan" id="kobo.1.1">Chapter One</span></h1>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<span class="koboSpan" id="kobo.2.1">It was a dark night. </span>"#)
        );
        assert!(out.contains(r#"<span class="koboSpan" id="kobo.2.2">The wind </span><em><span class="koboSpan" id="kobo.2.3">howled!</span></em>"#), "{out}");
        assert!(out.contains("Tom &amp; Jerry ran… "), "{out}");
        assert!(out.contains(r#"<span class="koboSpan" id="kobo.3.1">Second paragraph &unknown; here, 34' "wide".</span>"#), "{out}");
        assert!(out.contains("<pre>Code. Stays.</pre>"));
        assert!(out.contains(r#"<span class="koboSpan" id="kobo.9.9">Already done.</span>"#));
        assert!(!out.contains(r#"id="kobo.4.1">Already"#));
        assert!(out.contains(r#"var a = "x. y.";"#));
        assert!(out.contains("<title>One. Two.</title>"));
        assert_eq!(spans, 8);
    }

    #[test]
    fn keeps_the_text_and_stays_well_formed() {
        let (out, _) = convert_document(CHAPTER).unwrap();
        // Parses cleanly as XML, end tags matched.
        let mut reader = Reader::from_str(&out);
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Err(e) => panic!("not well-formed: {e}\n{out}"),
                _ => {}
            }
        }
        let text = strip_spans(&out)
            .replace(r#"<div id="book-columns"><div id="book-inner">"#, "")
            .replace("</div></div></body>", "</body>");
        assert!(text.contains("It was a dark night. The wind <em>howled!</em> Tom &amp; Jerry ran… “Where?” she asked."));
    }

    #[test]
    fn marks_an_epub2_cover_for_the_kobo() {
        let opf = r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata><meta content="cover-image" name="cover"/></metadata><manifest><item href="thumb.jpg" id="thumb" media-type="image/jpeg"/><item href="cover.jpg" id="cover-image" media-type="image/jpeg"/></manifest></package>"#;
        let fixed = mark_cover_image(opf).unwrap();
        assert!(fixed.contains(r#"<item href="cover.jpg" id="cover-image" media-type="image/jpeg" properties="cover-image"/>"#), "{fixed}");
        assert!(fixed.contains(r#"<item href="thumb.jpg" id="thumb" media-type="image/jpeg"/>"#));
        // Already marked, or no cover named: left alone.
        assert_eq!(mark_cover_image(&fixed), None);
        assert_eq!(
            mark_cover_image(
                r#"<package><manifest><item id="a" href="a.jpg"/></manifest></package>"#
            ),
            None
        );
        // Existing properties are kept.
        let svg = r#"<package><metadata><meta name="cover" content="c"/></metadata><manifest><item id="c" href="c.svg" properties="svg"/></manifest></package>"#;
        assert!(
            mark_cover_image(svg)
                .unwrap()
                .contains(r#"properties="svg cover-image""#)
        );
    }

    #[test]
    fn puts_the_library_cover_in_place_of_the_books_own() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("book.epub");
        // A tiny JPEG as the book's own (placeholder) cover.
        let jpeg = |w, h, shade: u8| {
            let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                w,
                h,
                image::Rgb([shade; 3]),
            ));
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
            out.into_inner()
        };
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&src).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("META-INF/container.xml", opts).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#).unwrap();
            zip.start_file("OEBPS/content.opf", opts).unwrap();
            zip.write_all(br#"<package version="2.0"><metadata><meta name="cover" content="c"/></metadata><manifest><item id="t" href="thumb.jpg" media-type="image/jpeg"/><item id="c" href="images/cover.jpg" media-type="image/jpeg"/></manifest></package>"#).unwrap();
            zip.start_file("OEBPS/images/cover.jpg", opts).unwrap();
            zip.write_all(&jpeg(10, 15, 0)).unwrap();
            zip.start_file("OEBPS/thumb.jpg", opts).unwrap();
            zip.write_all(&jpeg(4, 6, 0)).unwrap();
            zip.finish().unwrap();
        }
        // The library's cover is a PNG; the book's slot is a JPEG.
        let library_cover = dir.path().join("book-1.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 60, image::Rgb([200; 3])))
            .save(&library_cover)
            .unwrap();
        let dest = dir.path().join("book.kepub.epub");
        convert_with_cover(&src, &dest, Some(&library_cover)).unwrap();

        let mut out = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
        let mut cover = Vec::new();
        out.by_name("OEBPS/images/cover.jpg")
            .unwrap()
            .read_to_end(&mut cover)
            .unwrap();
        assert_eq!(
            image::guess_format(&cover).unwrap(),
            image::ImageFormat::Jpeg
        );
        assert!(
            !cover.windows(2).any(|w| w == [0xFF, 0xC2]),
            "baseline, not progressive"
        );
        let img = image::load_from_memory(&cover).unwrap();
        assert_eq!((img.width(), img.height()), (40, 60), "the library's cover");
        let mut thumb = Vec::new();
        out.by_name("OEBPS/thumb.jpg")
            .unwrap()
            .read_to_end(&mut thumb)
            .unwrap();
        assert_eq!(
            image::load_from_memory(&thumb).unwrap().width(),
            4,
            "other images untouched"
        );
        // And the cover is marked for the Kobo.
        let mut opf = String::new();
        out.by_name("OEBPS/content.opf")
            .unwrap()
            .read_to_string(&mut opf)
            .unwrap();
        assert!(opf.contains(r#"properties="cover-image""#));
    }

    #[test]
    fn re_encodes_progressive_jpeg_covers() {
        let dir = tempfile::tempdir().unwrap();
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            96,
            image::Rgb([90; 3]),
        ));
        // A real progressive JPEG isn't something the image crate writes, so
        // a baseline one is made and checked to pass through untouched, and a
        // PNG to be re-encoded as baseline.
        let base = dir.path().join("b.jpg");
        img.save(&base).unwrap();
        assert_eq!(
            cover_as(&base, "cover.jpg").unwrap(),
            std::fs::read(&base).unwrap()
        );
        let png = dir.path().join("c.png");
        img.save(&png).unwrap();
        let out = cover_as(&png, "cover.jpg").unwrap();
        assert_eq!(image::guess_format(&out).unwrap(), image::ImageFormat::Jpeg);
        assert!(!out.windows(2).any(|w| w == [0xFF, 0xC2]));
    }

    #[test]
    fn converts_the_same_way_every_time() {
        assert_eq!(
            convert_document(CHAPTER).unwrap(),
            convert_document(CHAPTER).unwrap()
        );
    }

    #[test]
    fn converts_a_whole_epub() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("book.epub");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&src).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("META-INF/container.xml", opts).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#).unwrap();
            zip.start_file("mimetype", opts).unwrap();
            zip.write_all(b"application/epub+zip").unwrap();
            zip.start_file("OEBPS/content.opf", opts).unwrap();
            zip.write_all(b"<package/>").unwrap();
            zip.start_file("OEBPS/ch1.xhtml", opts).unwrap();
            zip.write_all(CHAPTER.as_bytes()).unwrap();
            zip.start_file("OEBPS/cover.jpg", opts).unwrap();
            zip.write_all(b"\xff\xd8 jpeg").unwrap();
            zip.finish().unwrap();
        }
        let dest = dir.path().join("book.kepub.epub");
        let stats = convert(&src, &dest).unwrap();
        assert_eq!(
            stats,
            KepubStats {
                documents: 1,
                spans: 8,
                // &nbsp; and &unknown; aren't XML: tidied first.
                tidied: 1
            }
        );

        let mut out = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
        let first = out.by_index(0).unwrap();
        assert_eq!(first.name(), "mimetype");
        assert_eq!(first.compression(), zip::CompressionMethod::Stored);
        drop(first);
        let mut chapter = String::new();
        out.by_name("OEBPS/ch1.xhtml")
            .unwrap()
            .read_to_string(&mut chapter)
            .unwrap();
        assert!(chapter.contains("koboSpan"));
        let mut cover = Vec::new();
        out.by_name("OEBPS/cover.jpg")
            .unwrap()
            .read_to_end(&mut cover)
            .unwrap();
        assert_eq!(cover, b"\xff\xd8 jpeg");
        assert_eq!(out.len(), 5);
    }
}
