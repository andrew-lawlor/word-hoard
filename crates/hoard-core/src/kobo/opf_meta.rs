//! Writing the library's details into a book's package document (OPF) as
//! it's sent, so the Kobo shows the title, authors, series, description and
//! publisher from the library rather than whatever the file came with.
//!
//! Only the copy sent is changed; library files are never rewritten. The
//! fields replaced are removed from the OPF (with any `<meta refines>` that
//! describe them) and written again at the end of `<metadata>`: EPUB 3 style
//! (`belongs-to-collection`, `refines`) or EPUB 2 style (`opf:` attributes)
//! to match the book, plus Calibre's `calibre:series` metadata, which Kobo
//! firmware reads for series on sideloaded books.

use std::collections::HashSet;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// What the library knows about a book, to write into the copy sent.
/// `None` (or empty) fields leave the book's own value alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SendMetadata {
    pub title: Option<String>,
    /// Display name and sort name ("Ursula K. Le Guin", "Le Guin, Ursula K.").
    pub authors: Vec<(String, Option<String>)>,
    pub series: Option<String>,
    /// As stored (`2`, `2.5`, `2.0`).
    pub series_index: Option<String>,
    pub description: Option<String>,
    pub publisher: Option<String>,
    pub language: Option<String>,
    pub published: Option<String>,
}

impl SendMetadata {
    /// Authors from the library's joined author name and Calibre-style sort
    /// (`"George Orwell, Eric Arthur Blair"`, `"Orwell, George & Blair, Eric
    /// Arthur"`). Names are split on commas only when that gives as many
    /// names as the sort has: `"Fitzgerald, Francis Scott"` is one author.
    pub fn authors_from(author: Option<&str>, sort: Option<&str>) -> Vec<(String, Option<String>)> {
        let Some(author) = author.map(str::trim).filter(|a| !a.is_empty()) else {
            return Vec::new();
        };
        let sorts: Vec<&str> = sort
            .map(|s| {
                s.split(" & ")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let names: Vec<&str> = author.split(", ").map(str::trim).collect();
        if sorts.len() > 1 && names.len() == sorts.len() {
            names
                .into_iter()
                .zip(sorts)
                .map(|(n, s)| (n.to_owned(), Some(s.to_owned())))
                .collect()
        } else {
            vec![(
                author.to_owned(),
                sorts
                    .first()
                    .filter(|_| sorts.len() == 1)
                    .map(|s| s.to_string()),
            )]
        }
    }
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// An attribute's value by name (`refines`, `property`, `name`, `id`,
/// `version`); `opf:`-prefixed forms count too, except for `name`.
fn attr(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| {
            let k = a.key.as_ref();
            k == key || (key != "name" && local(k) == key)
        })
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// Which OPF elements `meta` replaces.
struct Replaced<'a> {
    meta: &'a SendMetadata,
}

impl Replaced<'_> {
    /// A Dublin Core element inside `<metadata>` that's written again.
    fn dc(&self, local_name: &str) -> bool {
        let m = self.meta;
        match local_name {
            "title" => m.title.is_some(),
            "creator" => !m.authors.is_empty(),
            "description" => m.description.is_some(),
            "publisher" => m.publisher.is_some(),
            "language" => m.language.is_some(),
            "date" => m.published.is_some(),
            _ => false,
        }
    }

    /// A `<meta>` that's written again (series), whatever it refines aside.
    fn meta(&self, e: &BytesStart) -> bool {
        if self.meta.series.is_none() {
            return false;
        }
        let name = attr(e, "name");
        matches!(
            name.as_deref(),
            Some("calibre:series" | "calibre:series_index")
        ) || attr(e, "property").as_deref() == Some("belongs-to-collection")
    }
}

/// `opf` with `meta` written into its `<metadata>`, or `None` when it can't
/// be read as XML (the book is then sent as it was).
pub fn apply(opf: &str, meta: &SendMetadata) -> Option<String> {
    let replaced = Replaced { meta };
    // First pass: the IDs of elements being replaced, so the `<meta
    // refines="#id">` describing them go too; the package's version; and
    // the prefix Dublin Core elements use.
    let mut dropped_ids: HashSet<String> = HashSet::new();
    let mut epub3 = false;
    let mut dc_prefix = String::from("dc");
    let mut in_metadata = false;
    let mut reader = Reader::from_str(opf);
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) | Event::Empty(e) => {
                let full = e.name().as_ref().to_owned();
                let l = local(&full);
                if l == "package" {
                    epub3 = attr(&e, "version").is_some_and(|v| v.starts_with('3'));
                } else if l == "metadata" {
                    in_metadata = true;
                } else if in_metadata {
                    if l == "title"
                        && let Some((prefix, _)) = full.split_once(':')
                    {
                        dc_prefix = prefix.to_owned();
                    }
                    let gone = if l == "meta" {
                        replaced.meta(&e)
                    } else {
                        replaced.dc(l)
                    };
                    if gone && let Some(id) = attr(&e, "id") {
                        dropped_ids.insert(format!("#{id}"));
                    }
                }
            }
            Event::End(e) if local(e.name().as_ref()) == "metadata" => in_metadata = false,
            Event::Eof => break,
            _ => {}
        }
    }

    // Second pass: copy everything but the replaced elements, and write the
    // new ones before </metadata>.
    let mut out = String::with_capacity(opf.len() + 1024);
    let mut reader = Reader::from_str(opf);
    let mut skip_depth = 0usize;
    let mut in_metadata = false;
    let mut last = 0usize;
    loop {
        let before = reader.buffer_position() as usize;
        let event = reader.read_event().ok()?;
        let after = reader.buffer_position() as usize;
        let raw = &opf[before..after];
        match &event {
            Event::Eof => break,
            _ if skip_depth > 0 => {
                match event {
                    Event::Start(_) => skip_depth += 1,
                    Event::End(_) => skip_depth -= 1,
                    _ => {}
                }
                last = after;
                continue;
            }
            Event::Start(e) | Event::Empty(e) if in_metadata => {
                let l = local(e.name().as_ref()).to_owned();
                let refines_dropped = attr(e, "refines").is_some_and(|r| dropped_ids.contains(&r));
                let gone = refines_dropped
                    || if l == "meta" {
                        replaced.meta(e)
                    } else {
                        replaced.dc(&l)
                    };
                if gone {
                    // Drop its line: the indentation and line break before
                    // it (the text after it starts the next line).
                    let kept = out.trim_end_matches([' ', '\t']).len();
                    out.truncate(kept);
                    if out.ends_with('\n') {
                        out.pop();
                        if out.ends_with('\r') {
                            out.pop();
                        }
                    }
                    if matches!(event, Event::Start(_)) {
                        skip_depth = 1;
                    }
                    last = after;
                    // A newline after it would leave a blank line.
                    continue;
                }
            }
            Event::Start(e) if local(e.name().as_ref()) == "metadata" => in_metadata = true,
            Event::End(e) if local(e.name().as_ref()) == "metadata" => {
                in_metadata = false;
                out.push_str(&new_elements(meta, epub3, &dc_prefix));
            }
            _ => {}
        }
        out.push_str(raw);
        last = after;
    }
    out.push_str(&opf[last..]);
    // Collapse the blank lines left where elements were removed.
    let mut tidy = String::with_capacity(out.len());
    let mut blank_run = false;
    for line in out.split_inclusive('\n') {
        let blank = line.trim().is_empty();
        if !(blank && blank_run) {
            tidy.push_str(line);
        }
        blank_run = blank;
    }
    Some(tidy)
}

fn esc(s: &str) -> String {
    quick_xml::escape::escape(s).into_owned()
}

/// `2.0` → `2`; other numbers as they are.
pub(crate) fn series_number(n: &str) -> String {
    n.trim().strip_suffix(".0").unwrap_or(n.trim()).to_owned()
}

/// A description as the Kobo shows it: HTML, as Calibre sends it. The
/// library keeps descriptions as plain text with blank lines between
/// paragraphs; each paragraph becomes a `<p>`. One that's HTML already is
/// left as it is.
pub(crate) fn description_html(d: &str) -> String {
    let t = d.trim();
    if t.starts_with('<') && t.ends_with('>') {
        return t.to_owned();
    }
    t.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| format!("<p>{}</p>", esc(&p.replace('\n', " "))))
        .collect()
}

const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const OPF_NS: &str = "http://www.idpf.org/2007/opf";

/// `xml` with `xmlns:prefix="ns"` added to every `<prefix:…>` start tag.
fn declare_prefix(xml: &str, prefix: &str, ns: &str) -> String {
    let open = format!("<{prefix}:");
    let decl = format!(" xmlns:{prefix}=\"{ns}\"");
    let mut out = String::with_capacity(xml.len() + 256);
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let tag = &rest[i..];
        let name_end = tag[open.len()..]
            .find([' ', '>', '/'])
            .map_or(tag.len(), |e| e + open.len());
        out.push_str(&rest[..i]);
        out.push_str(&tag[..name_end]);
        out.push_str(&decl);
        rest = &tag[name_end..];
    }
    out.push_str(rest);
    out
}

fn new_elements(m: &SendMetadata, epub3: bool, dc: &str) -> String {
    let mut s = String::from("  ");
    let mut push = |line: String| {
        s.push_str(&line);
        s.push_str("\n    ");
    };
    if let Some(title) = &m.title {
        push(format!(
            "<{dc}:title id=\"wh-title\">{}</{dc}:title>",
            esc(title)
        ));
    }
    for (i, (name, sort)) in m.authors.iter().enumerate() {
        let id = format!("wh-creator{}", i + 1);
        if epub3 {
            push(format!(
                "<{dc}:creator id=\"{id}\">{}</{dc}:creator>",
                esc(name)
            ));
            push(format!(
                "<meta refines=\"#{id}\" property=\"role\" scheme=\"marc:relators\">aut</meta>"
            ));
            if let Some(sort) = sort {
                push(format!(
                    "<meta refines=\"#{id}\" property=\"file-as\">{}</meta>",
                    esc(sort)
                ));
            }
        } else {
            let file_as = sort
                .as_ref()
                .map(|s| format!(" opf:file-as=\"{}\"", esc(s)))
                .unwrap_or_default();
            push(format!(
                "<{dc}:creator id=\"{id}\" xmlns:opf=\"{OPF_NS}\" opf:role=\"aut\"{file_as}>{}</{dc}:creator>",
                esc(name)
            ));
        }
    }
    if let Some(series) = &m.series {
        if epub3 {
            push(format!(
                "<meta property=\"belongs-to-collection\" id=\"wh-series\">{}</meta>",
                esc(series)
            ));
            push("<meta refines=\"#wh-series\" property=\"collection-type\">series</meta>".into());
            if let Some(n) = &m.series_index {
                push(format!(
                    "<meta refines=\"#wh-series\" property=\"group-position\">{}</meta>",
                    esc(&series_number(n))
                ));
            }
        }
        push(format!(
            "<meta name=\"calibre:series\" content=\"{}\"/>",
            esc(series)
        ));
        if let Some(n) = &m.series_index {
            push(format!(
                "<meta name=\"calibre:series_index\" content=\"{}\"/>",
                esc(&series_number(n))
            ));
        }
    }
    if let Some(d) = &m.description {
        push(format!(
            "<{dc}:description>{}</{dc}:description>",
            esc(&description_html(d))
        ));
    }
    if let Some(p) = &m.publisher {
        push(format!("<{dc}:publisher>{}</{dc}:publisher>", esc(p)));
    }
    if let Some(l) = &m.language {
        push(format!("<{dc}:language>{}</{dc}:language>", esc(l)));
    }
    if let Some(d) = &m.published {
        push(format!("<{dc}:date>{}</{dc}:date>", esc(d)));
    }
    // Each new Dublin Core element declares its namespace itself: some
    // books declare it on each of their own elements rather than once on
    // <metadata>, and an undeclared prefix makes the OPF unreadable.
    let s = declare_prefix(&s, dc, DC_NS);
    let mut s = s;
    // Leave </metadata> where it was on its line.
    let trimmed = s.trim_end_matches([' ', '\n']).len();
    s.truncate(trimmed);
    s.push('\n');
    s.push_str("  ");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPUB2: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>Hammer &amp; Cross (ebook)</dc:title>
    <dc:creator opf:role="aut" opf:file-as="Harrison, H">H. Harrison</dc:creator>
    <dc:identifier id="uid">urn:uuid:1234</dc:identifier>
    <dc:language>en</dc:language>
    <meta name="calibre:series" content="Old"/>
    <meta name="cover" content="cover-img"/>
  </metadata>
  <manifest><item id="cover-img" href="c.jpg" media-type="image/jpeg"/></manifest>
</package>"##;

    const EPUB3: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title id="t1">Wrong</dc:title>
    <meta refines="#t1" property="title-type">main</meta>
    <dc:creator id="c1">Someone</dc:creator>
    <meta refines="#c1" property="role" scheme="marc:relators">aut</meta>
    <dc:identifier id="uid">urn:isbn:978</dc:identifier>
    <meta property="belongs-to-collection" id="s1">Old Series</meta>
    <meta refines="#s1" property="group-position">9</meta>
    <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
  </metadata>
</package>"##;

    fn meta() -> SendMetadata {
        SendMetadata {
            title: Some("The Hammer & the Cross".into()),
            authors: vec![("Harry Harrison".into(), Some("Harrison, Harry".into()))],
            series: Some("Hammer and the Cross".into()),
            series_index: Some("1.0".into()),
            description: Some("<p>Vikings.</p>".into()),
            publisher: Some("Tor".into()),
            ..Default::default()
        }
    }

    fn reparses(xml: &str) {
        let mut r = Reader::from_str(xml);
        loop {
            match r.read_event() {
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("not XML: {e}\n{xml}"),
            }
        }
    }

    #[test]
    fn writes_library_details_into_an_epub2_opf() {
        let out = apply(EPUB2, &meta()).unwrap();
        reparses(&out);
        assert!(out.contains(">The Hammer &amp; the Cross</dc:title>"));
        assert!(!out.contains("Hammer &amp; Cross (ebook)"));
        assert!(out.contains(
            r##"<dc:creator xmlns:dc="http://purl.org/dc/elements/1.1/" id="wh-creator1" xmlns:opf="http://www.idpf.org/2007/opf" opf:role="aut" opf:file-as="Harrison, Harry">Harry Harrison</dc:creator>"##
        ));
        assert!(!out.contains("H. Harrison"));
        assert!(out.contains(r##"<meta name="calibre:series" content="Hammer and the Cross"/>"##));
        assert!(out.contains(r##"<meta name="calibre:series_index" content="1"/>"##));
        assert!(!out.contains("\"Old\""));
        assert!(out.contains(">&lt;p&gt;Vikings.&lt;/p&gt;</dc:description>"));
        assert!(!out.contains("\n\n"), "no blank lines where elements were");
        // Left alone: identifier, language (no library value), cover, manifest.
        assert!(out.contains(r##"<dc:identifier id="uid">urn:uuid:1234</dc:identifier>"##));
        assert!(out.contains("<dc:language>en</dc:language>"));
        assert!(out.contains(r##"<meta name="cover" content="cover-img"/>"##));
        assert!(out.contains(r##"<item id="cover-img" href="c.jpg""##));
        assert!(!out.contains("\n\n\n"));
    }

    #[test]
    fn writes_epub3_style_and_drops_what_described_the_old_values() {
        let out = apply(EPUB3, &meta()).unwrap();
        reparses(&out);
        assert!(!out.contains("Wrong") && !out.contains("Someone") && !out.contains("Old Series"));
        assert!(!out.contains("#t1") && !out.contains("#c1") && !out.contains("#s1"));
        assert!(out.contains(
            r##"<meta refines="#wh-creator1" property="file-as">Harrison, Harry</meta>"##
        ));
        assert!(out.contains(
            r##"<meta property="belongs-to-collection" id="wh-series">Hammer and the Cross</meta>"##
        ));
        assert!(out.contains(r##"<meta refines="#wh-series" property="group-position">1</meta>"##));
        assert!(!out.contains("opf:role"));
        assert!(out.contains("dcterms:modified"));
        assert!(out.contains(r##"<dc:identifier id="uid">urn:isbn:978</dc:identifier>"##));
    }

    #[test]
    fn leaves_fields_the_library_lacks() {
        let only_title = SendMetadata {
            title: Some("New".into()),
            ..Default::default()
        };
        let out = apply(EPUB2, &only_title).unwrap();
        assert!(out.contains("H. Harrison") && out.contains("content=\"Old\""));
        assert!(out.contains(">New</dc:title>"));
    }

    #[test]
    fn declares_namespaces_the_book_only_declares_per_element() {
        // As in a real book: no xmlns:dc or xmlns:opf on <package> or
        // <metadata>, only on each element.
        let opf = r##"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata>
    <dc:identifier xmlns:dc="http://purl.org/dc/elements/1.1/" id="bookid">bm:x</dc:identifier>
    <dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">Old</dc:title>
    <meta name="cover" content="c"/>
  </metadata>
</package>"##;
        let out = apply(opf, &meta()).unwrap();
        // A namespace-aware parse: every prefix must be declared.
        let mut r = quick_xml::NsReader::from_str(&out);
        loop {
            match r.read_resolved_event() {
                Ok((ns, Event::Start(e) | Event::Empty(e))) => {
                    let has_prefix = e.name().as_ref().contains(':');
                    assert!(
                        !has_prefix || !matches!(ns, quick_xml::name::ResolveResult::Unknown(_)),
                        "undeclared prefix in {out}"
                    );
                }
                Ok((_, Event::Eof)) => break,
                Ok(_) => {}
                Err(e) => panic!("{e}\n{out}"),
            }
        }
        assert!(!out.contains(">Old<"));
    }

    #[test]
    fn descriptions_become_paragraphs() {
        assert_eq!(
            description_html("First & one.\n\nSecond\nline."),
            "<p>First &amp; one.</p><p>Second line.</p>"
        );
        assert_eq!(
            description_html("<div><p>Kept</p></div>"),
            "<div><p>Kept</p></div>"
        );
    }

    #[test]
    fn splits_authors_only_when_the_sort_agrees() {
        assert_eq!(
            SendMetadata::authors_from(
                Some("George Orwell, Eric Arthur Blair"),
                Some("Orwell, George & Blair, Eric Arthur")
            ),
            vec![
                ("George Orwell".into(), Some("Orwell, George".into())),
                (
                    "Eric Arthur Blair".into(),
                    Some("Blair, Eric Arthur".into())
                )
            ]
        );
        assert_eq!(
            SendMetadata::authors_from(
                Some("Fitzgerald, Francis Scott"),
                Some("Fitzgerald, Francis Scott")
            ),
            vec![(
                "Fitzgerald, Francis Scott".into(),
                Some("Fitzgerald, Francis Scott".into())
            )]
        );
        assert_eq!(
            SendMetadata::authors_from(
                Some("Homer, Caroline Alexander"),
                Some("Homer & Alexander, Caroline")
            ),
            vec![
                ("Homer".into(), Some("Homer".into())),
                (
                    "Caroline Alexander".into(),
                    Some("Alexander, Caroline".into())
                )
            ]
        );
        assert!(SendMetadata::authors_from(None, None).is_empty());
    }
}
