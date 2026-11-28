//! A book's own metadata, read from its EPUB's package document (OPF):
//! title, authors, language, identifiers, publisher, date, description,
//! subjects and series (calibre's `calibre:series` meta or EPUB 3's
//! `belongs-to-collection`). Becomes `hoard-epub` when the crates split.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use quick_xml::events::{BytesStart, Event};
use serde::Serialize;

use crate::Result;
use crate::kobo::epub::{attr, html_entity, io_err, is_drm};
use crate::normalize::clean_opt;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Author {
    pub name: String,
    /// Sort form, e.g. "Le Guin, Ursula K." (`opf:file-as` / `file-as`).
    pub file_as: Option<String>,
}

impl Author {
    /// The name in reading order. Some books give it surname first
    /// ("Lewis, C.S.", "Blair, Eric Arthur"); two parts whose first is a
    /// single word are read that way and turned round.
    pub fn display_name(&self) -> String {
        match self.name.split_once(", ") {
            Some((last, first))
                if !last.contains(' ') && !first.contains(',') && !first.is_empty() =>
            {
                format!("{first} {last}")
            }
            _ => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EpubMetadata {
    pub title: Option<String>,
    pub authors: Vec<Author>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    /// As written: `1974`, `1974-05-01`, `2011-09-01T00:00:00+00:00`.
    pub published: Option<String>,
    /// Plain text; HTML in the description is reduced to its text.
    pub description: Option<String>,
    /// (scheme, value), scheme lower-cased: `isbn`, `uuid`, `calibre`, …;
    /// empty when the book doesn't say.
    pub identifiers: Vec<(String, String)>,
    /// The identifier the package names as unique.
    pub unique_id: Option<String>,
    pub isbn: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub subjects: Vec<String>,
    /// The text is locked (Adobe DRM); metadata is still readable.
    pub drm: bool,
}

impl EpubMetadata {
    /// Authors as one string, the way Kobo's `Attribution` and Kollate's
    /// libraries record them, so book fingerprints line up.
    pub fn author_line(&self) -> Option<String> {
        let names: Vec<&str> = self.authors.iter().map(|a| a.name.as_str()).collect();
        (!names.is_empty()).then(|| names.join(", "))
    }

    /// The four-digit year of publication, if the date has one.
    pub fn year(&self) -> Option<&str> {
        let date = self.published.as_deref()?;
        date.get(..4)
            .filter(|y| y.bytes().all(|b| b.is_ascii_digit()))
    }
}

/// One `dc:*` element as read, before refinements are applied.
#[derive(Default)]
struct Element {
    name: String,
    id: Option<String>,
    attrs: HashMap<String, String>,
    text: String,
}

pub fn read_metadata(path: &Path) -> Result<EpubMetadata> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?).map_err(io_err)?;
    let drm = is_drm(&mut zip);
    let mut read = |name: &str| -> Result<String> {
        let mut file = zip.by_name(name).map_err(io_err)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    let container = read("META-INF/container.xml")?;
    let opf_path =
        rootfile(&container).ok_or_else(|| io_err("no package document in container.xml"))?;
    let mut meta = parse_opf(&read(&opf_path)?);
    meta.drm = drm;
    Ok(meta)
}

fn rootfile(container: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(container);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == "rootfile" => {
                return attr(&e, "full-path");
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Attributes by local name (`opf:file-as` and `file-as` alike).
fn local_attrs(e: &BytesStart) -> HashMap<String, String> {
    e.attributes()
        .flatten()
        .filter_map(|a| {
            let key = a.key.local_name().as_ref().to_owned();
            let value = a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()?
                .into_owned();
            Some((key, value))
        })
        .collect()
}

pub(crate) fn parse_opf(opf: &str) -> EpubMetadata {
    let mut reader = quick_xml::Reader::from_str(opf);
    reader.config_mut().trim_text(false);
    let mut unique_ref = None;
    let mut elements: Vec<Element> = Vec::new();
    // (property, refines id without '#', text) for EPUB 3 <meta property>.
    let mut refines: Vec<(String, Option<String>, String, Option<String>)> = Vec::new();
    // EPUB 2 <meta name content>.
    let mut named: HashMap<String, String> = HashMap::new();
    let mut in_metadata = false;
    // The element whose text is being read: a dc:* one or an EPUB 3 meta.
    enum Open {
        Dc(Element),
        Meta {
            property: String,
            refines: Option<String>,
            id: Option<String>,
            text: String,
        },
    }
    let mut open: Option<Open> = None;
    let push_text = |open: &mut Option<Open>, s: &str| match open {
        Some(Open::Dc(e)) => e.text.push_str(s),
        Some(Open::Meta { text, .. }) => text.push_str(s),
        None => {}
    };
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name().as_ref().to_owned();
                let attrs = local_attrs(&e);
                match local.as_str() {
                    "package" => unique_ref = attrs.get("unique-identifier").cloned(),
                    "metadata" => in_metadata = true,
                    "meta" if in_metadata => {
                        if let Some(property) = attrs.get("property") {
                            open = Some(Open::Meta {
                                property: property.clone(),
                                refines: attrs
                                    .get("refines")
                                    .map(|r| r.trim_start_matches('#').to_owned()),
                                id: attrs.get("id").cloned(),
                                text: String::new(),
                            });
                        } else if let (Some(n), Some(c)) = (attrs.get("name"), attrs.get("content"))
                        {
                            named.insert(n.clone(), c.clone());
                        }
                    }
                    _ if in_metadata && e.name().as_ref().starts_with("dc:")
                        || in_metadata && is_dc(&e) =>
                    {
                        open = Some(Open::Dc(Element {
                            name: local,
                            id: attrs.get("id").cloned(),
                            attrs,
                            text: String::new(),
                        }));
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if in_metadata && e.local_name().as_ref() == "meta" => {
                let attrs = local_attrs(&e);
                if let (Some(n), Some(c)) = (attrs.get("name"), attrs.get("content")) {
                    named.insert(n.clone(), c.clone());
                }
            }
            Ok(Event::Text(t)) => push_text(&mut open, &t.xml10_content()),
            Ok(Event::CData(t)) => push_text(&mut open, &t.xml10_content()),
            Ok(Event::GeneralRef(r)) => match r.resolve_char_ref() {
                Ok(Some(c)) => push_text(&mut open, c.encode_utf8(&mut [0; 4])),
                _ => push_text(&mut open, html_entity(&r)),
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                "metadata" => in_metadata = false,
                _ => match open.take() {
                    Some(Open::Dc(el)) => elements.push(el),
                    Some(Open::Meta {
                        property,
                        refines: r,
                        id,
                        text,
                    }) => refines.push((property, r, text, id)),
                    None => {}
                },
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    build(elements, refines, named, unique_ref)
}

/// Whether an element is in the Dublin Core namespace under another prefix.
fn is_dc(e: &BytesStart) -> bool {
    matches!(
        e.local_name().as_ref(),
        "title"
            | "creator"
            | "language"
            | "identifier"
            | "publisher"
            | "date"
            | "description"
            | "subject"
    ) && e.name().as_ref().contains(':')
}

fn build(
    elements: Vec<Element>,
    refines: Vec<(String, Option<String>, String, Option<String>)>,
    named: HashMap<String, String>,
    unique_ref: Option<String>,
) -> EpubMetadata {
    let refined = |id: &Option<String>, property: &str| -> Option<String> {
        let id = id.as_deref()?;
        refines
            .iter()
            .find(|(p, r, _, _)| p == property && r.as_deref() == Some(id))
            .and_then(|(_, _, text, _)| clean_opt(Some(text)))
    };
    let text = |e: &Element| clean_opt(Some(&e.text));
    fn of<'a>(elements: &'a [Element], name: &'a str) -> impl Iterator<Item = &'a Element> {
        elements.iter().filter(move |e| e.name == name)
    }

    // The main title: EPUB 3 may mark several with title-type.
    let title = of(&elements, "title")
        .find(|e| refined(&e.id, "title-type").as_deref() == Some("main"))
        .or_else(|| of(&elements, "title").next())
        .and_then(text);
    let mut meta = EpubMetadata {
        title,
        ..Default::default()
    };

    // Authors: creators whose role is "aut" or unstated; everyone if none are.
    let role = |e: &Element| {
        e.attrs
            .get("role")
            .cloned()
            .or_else(|| refined(&e.id, "role"))
    };
    let creators: Vec<&Element> = of(&elements, "creator").collect();
    let authors: Vec<&&Element> = creators
        .iter()
        .filter(|e| role(e).is_none_or(|r| r == "aut"))
        .collect();
    let chosen: Vec<&Element> = if authors.is_empty() {
        creators.clone()
    } else {
        authors.into_iter().copied().collect()
    };
    meta.authors = chosen
        .into_iter()
        .filter_map(|e| {
            Some(Author {
                name: text(e)?,
                file_as: e
                    .attrs
                    .get("file-as")
                    .and_then(|f| clean_opt(Some(f)))
                    .or_else(|| refined(&e.id, "file-as")),
            })
        })
        .collect();

    meta.language = of(&elements, "language").next().and_then(text);
    meta.publisher = of(&elements, "publisher").next().and_then(text);
    meta.published = of(&elements, "date")
        .find(|e| e.attrs.get("event").is_none_or(|ev| ev == "publication"))
        .and_then(text);
    meta.description = of(&elements, "description")
        .next()
        .and_then(|e| clean_opt(Some(&strip_tags(&e.text))));
    meta.subjects = of(&elements, "subject").filter_map(text).collect();

    for e in of(&elements, "identifier") {
        let Some(value) = text(e) else { continue };
        let scheme = e
            .attrs
            .get("scheme")
            .cloned()
            .or_else(|| refined(&e.id, "identifier-type"))
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if unique_ref.is_some() && e.id == unique_ref {
            meta.unique_id = Some(value.clone());
        }
        let (scheme, value) = match value.split_once(':') {
            // urn:isbn:978…, urn:uuid:…
            Some(("urn", rest)) if scheme.is_empty() => match rest.split_once(':') {
                Some((s, v)) => (s.to_ascii_lowercase(), v.to_owned()),
                None => (scheme, value),
            },
            Some((s, v))
                if scheme.is_empty()
                    && matches!(s.to_ascii_lowercase().as_str(), "isbn" | "uuid" | "calibre") =>
            {
                (s.to_ascii_lowercase(), v.to_owned())
            }
            _ => (scheme, value),
        };
        if meta.isbn.is_none() && (scheme == "isbn" || looks_like_isbn(&value)) {
            meta.isbn = Some(value.replace(['-', ' '], ""));
        }
        meta.identifiers.push((scheme, value));
    }

    // Series: calibre's meta, else EPUB 3's collection marked as a series.
    meta.series = named.get("calibre:series").and_then(|s| clean_opt(Some(s)));
    meta.series_index = named
        .get("calibre:series_index")
        .and_then(|s| clean_opt(Some(s)));
    if meta.series.is_none()
        && let Some((_, _, name, id)) = refines.iter().find(|(p, r, _, id)| {
            p == "belongs-to-collection"
                && r.is_none()
                && refined(id, "collection-type").is_none_or(|t| t == "series")
        })
    {
        meta.series = clean_opt(Some(name));
        meta.series_index = refined(id, "group-position");
    }
    meta
}

fn looks_like_isbn(value: &str) -> bool {
    let digits: String = value.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
    (digits.len() == 13 && (digits.starts_with("978") || digits.starts_with("979")))
        && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Text of an HTML fragment: tags dropped, block ends turned into line breaks.
pub(crate) fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = &rest[start..];
            break;
        };
        let tag = rest[start + 1..start + end]
            .trim_start_matches('/')
            .to_ascii_lowercase();
        if ["p", "br", "div", "li"].iter().any(|t| {
            tag == *t || tag.starts_with(&format!("{t} ")) || tag.starts_with(&format!("{t}/"))
        }) {
            out.push('\n');
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    // A paragraph's closing and the next one's opening tag each made a line
    // break: one blank line between paragraphs is enough.
    let decoded = decode_entities(&out);
    let mut text = String::with_capacity(decoded.len());
    let mut blank_run = 0;
    for line in decoded.split('\n') {
        let blank = line.trim().is_empty();
        blank_run = if blank { blank_run + 1 } else { 0 };
        if blank_run <= 1 {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

/// Decodes the HTML entities left in a description once its tags are gone
/// (`&nbsp;`, `&amp;`, `&#8217;`, …).
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| {
            let name = &after[..e];
            match name.strip_prefix('#') {
                Some(num) => {
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => num.parse().ok(),
                    };
                    code.and_then(char::from_u32).map(String::from)
                }
                None => Some(html_entity(name))
                    .filter(|t| !t.is_empty())
                    .map(String::from),
            }
        });
        match (decoded, end) {
            (Some(text), Some(e)) => {
                out.push_str(&text);
                rest = &after[e + 1..];
            }
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const EPUB2: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uuid_id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>The Dispossessed</dc:title>
    <dc:creator opf:file-as="Le Guin, Ursula K." opf:role="aut">Ursula K. Le Guin</dc:creator>
    <dc:contributor opf:role="bkp">calibre (7.0.0)</dc:contributor>
    <dc:creator opf:role="ill">Someone Else</dc:creator>
    <dc:identifier id="uuid_id" opf:scheme="uuid">8f6b1c9e-0000-4000-8000-000000000001</dc:identifier>
    <dc:identifier opf:scheme="ISBN">978-0-06-051275-0</dc:identifier>
    <dc:language>en</dc:language>
    <dc:publisher>Harper &amp; Row</dc:publisher>
    <dc:date>1974-05-01T00:00:00+00:00</dc:date>
    <dc:description>&lt;p&gt;An ambiguous&amp;nbsp;utopia.&lt;/p&gt;&lt;p&gt;Second paragraph.&lt;/p&gt;</dc:description>
    <dc:subject>Science Fiction</dc:subject>
    <dc:subject>Anarchism</dc:subject>
    <meta name="calibre:series" content="Hainish Cycle"/>
    <meta name="calibre:series_index" content="6.0"/>
    <meta name="cover" content="cover"/>
  </metadata>
</package>"#;

    const EPUB3: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="pub-id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="pub-id">urn:uuid:12345678-1234-4234-8234-123456789abc</dc:identifier>
    <dc:identifier id="isbn">urn:isbn:9780374104092</dc:identifier>
    <dc:title id="sub">A Novel</dc:title>
    <meta refines="#sub" property="title-type">subtitle</meta>
    <dc:title id="main">Annihilation</dc:title>
    <meta refines="#main" property="title-type">main</meta>
    <dc:creator id="c1">Jeff VanderMeer</dc:creator>
    <meta refines="#c1" property="role" scheme="marc:relators">aut</meta>
    <meta refines="#c1" property="file-as">VanderMeer, Jeff</meta>
    <dc:creator id="c2">Cover Artist</dc:creator>
    <meta refines="#c2" property="role" scheme="marc:relators">cov</meta>
    <dc:language>en-US</dc:language>
    <dc:date>2014</dc:date>
    <meta property="belongs-to-collection" id="coll">Southern Reach</meta>
    <meta refines="#coll" property="collection-type">series</meta>
    <meta refines="#coll" property="group-position">1</meta>
    <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
  </metadata>
</package>"##;

    #[test]
    fn reads_epub2_calibre_metadata() {
        let m = parse_opf(EPUB2);
        assert_eq!(m.title.as_deref(), Some("The Dispossessed"));
        assert_eq!(
            m.authors,
            [Author {
                name: "Ursula K. Le Guin".into(),
                file_as: Some("Le Guin, Ursula K.".into())
            }]
        );
        assert_eq!(m.author_line().as_deref(), Some("Ursula K. Le Guin"));
        assert_eq!(m.language.as_deref(), Some("en"));
        assert_eq!(m.publisher.as_deref(), Some("Harper & Row"));
        assert_eq!(m.year(), Some("1974"));
        assert_eq!(
            m.description.as_deref(),
            Some("An ambiguous utopia.\n\nSecond paragraph.")
        );
        assert_eq!(
            m.unique_id.as_deref(),
            Some("8f6b1c9e-0000-4000-8000-000000000001")
        );
        assert_eq!(m.isbn.as_deref(), Some("9780060512750"));
        assert!(
            m.identifiers
                .contains(&("uuid".into(), "8f6b1c9e-0000-4000-8000-000000000001".into()))
        );
        assert_eq!(m.series.as_deref(), Some("Hainish Cycle"));
        assert_eq!(m.series_index.as_deref(), Some("6.0"));
        assert_eq!(m.subjects, ["Science Fiction", "Anarchism"]);
        assert_eq!(
            decode_entities("a &amp; b &#8217;c&#x2019; &bogus; & d"),
            "a & b ’c’ &bogus; & d"
        );
    }

    #[test]
    fn reads_epub3_refinements() {
        let m = parse_opf(EPUB3);
        assert_eq!(m.title.as_deref(), Some("Annihilation"));
        assert_eq!(
            m.authors,
            [Author {
                name: "Jeff VanderMeer".into(),
                file_as: Some("VanderMeer, Jeff".into())
            }]
        );
        assert_eq!(
            m.unique_id.as_deref(),
            Some("urn:uuid:12345678-1234-4234-8234-123456789abc")
        );
        assert_eq!(m.isbn.as_deref(), Some("9780374104092"));
        assert!(
            m.identifiers
                .contains(&("uuid".into(), "12345678-1234-4234-8234-123456789abc".into()))
        );
        assert_eq!(m.year(), Some("2014"));
        assert_eq!(m.series.as_deref(), Some("Southern Reach"));
        assert_eq!(m.series_index.as_deref(), Some("1"));
    }

    #[test]
    fn turns_surname_first_names_round() {
        let name = |n: &str| {
            Author {
                name: n.into(),
                file_as: None,
            }
            .display_name()
        };
        assert_eq!(name("Lewis, C.S."), "C.S. Lewis");
        assert_eq!(name("Blair, Eric Arthur"), "Eric Arthur Blair");
        assert_eq!(name("Ursula K. Le Guin"), "Ursula K. Le Guin");
        assert_eq!(
            name("Edward John Moreton Dunsany, Lord"),
            "Edward John Moreton Dunsany, Lord"
        );
    }

    #[test]
    fn reads_a_whole_epub_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.epub");
        write_epub(&path, EPUB2);
        let m = read_metadata(&path).unwrap();
        assert_eq!(m.title.as_deref(), Some("The Dispossessed"));
        assert!(!m.drm);
    }

    pub(crate) fn write_epub(path: &Path, opf: &str) {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("mimetype", stored).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", stored).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#).unwrap();
        zip.start_file("OEBPS/content.opf", stored).unwrap();
        zip.write_all(opf.as_bytes()).unwrap();
        zip.finish().unwrap();
    }

    /// An EPUB whose spine is `chapters` (zip path, XHTML), in order.
    pub(crate) fn write_epub_with(path: &Path, chapters: &[(&str, &str)]) {
        use std::io::Write;
        let mut manifest = String::new();
        let mut spine = String::new();
        for (i, (name, _)) in chapters.iter().enumerate() {
            let href = name.strip_prefix("OEBPS/").unwrap_or(name);
            manifest.push_str(&format!(
                r#"<item id="c{i}" href="{href}" media-type="application/xhtml+xml"/>"#
            ));
            spine.push_str(&format!(r#"<itemref idref="c{i}"/>"#));
        }
        let opf = format!(
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title><dc:identifier id="id">urn:uuid:t</dc:identifier></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
        );
        write_epub(path, &opf);
        let mut zip = zip::ZipWriter::new_append(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap(),
        )
        .unwrap();
        for (name, xhtml) in chapters {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(xhtml.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
}
