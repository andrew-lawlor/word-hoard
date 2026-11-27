//! Turning a book's HTML into well-formed XHTML for the Kobo, and checking
//! a converted copy before it's sent.
//!
//! Some EPUBs carry HTML5-style chapters (`<meta charset="UTF-8">`, `<br>`
//! never closed) that aren't valid XML. The KEPUB converter works on XML,
//! so such chapters are first parsed as HTML (html5ever, the parser browsers
//! use) and written out as XHTML, as kepubify does. Chapters that are
//! already well-formed are left exactly as they are.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use quick_xml::Reader;
use quick_xml::events::Event;

const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const SVG_NS: &str = "http://www.w3.org/2000/svg";
const MATHML_NS: &str = "http://www.w3.org/1998/Math/MathML";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
const EPUB_NS: &str = "http://www.idpf.org/2007/ops";

/// Whether `xml` parses as XML: tags balanced and properly nested.
pub(crate) fn is_well_formed(xml: &str) -> bool {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = true;
    let mut depth = 0i64;
    let mut root_seen = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => {
                depth += 1;
                root_seen = true;
            }
            Ok(Event::Empty(_)) => root_seen = true,
            Ok(Event::End(_)) => depth -= 1,
            // XML defines only five named entities; HTML ones like &copy;
            // or &nbsp; make a strict parser give up.
            Ok(Event::GeneralRef(r)) => {
                let name = r.as_ref();
                let known =
                    matches!(name, "amp" | "lt" | "gt" | "quot" | "apos") || name.starts_with('#');
                if !known {
                    return false;
                }
            }
            Ok(Event::Eof) => return depth == 0 && root_seen,
            Ok(_) => {}
            Err(_) => return false,
        }
        if depth < 0 {
            return false;
        }
    }
}

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// `html`, parsed as HTML and written as well-formed XHTML.
pub(crate) fn to_xhtml(html: &str) -> String {
    let dom: RcDom = html5ever::parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .expect("reading from memory can't fail");
    let mut out = String::with_capacity(html.len() + html.len() / 8);
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE html>\n");
    for child in dom.document.children.borrow().iter() {
        write_node(child, &mut out, None);
    }
    out.push('\n');
    out
}

/// Writes `node`; `parent_ns` is the namespace of the enclosing element.
fn write_node(node: &Handle, out: &mut String, parent_ns: Option<&str>) {
    match &node.data {
        NodeData::Text { contents } => escape_text(&contents.borrow(), out),
        NodeData::Comment { contents } => {
            out.push_str("<!--");
            out.push_str(&contents.replace("--", "- -"));
            out.push_str("-->");
        }
        NodeData::Element { name, attrs, .. } => {
            let local: &str = &name.local;
            let ns: &str = &name.ns;
            out.push('<');
            out.push_str(local);
            // Each namespace is declared where it begins.
            if parent_ns != Some(ns) && [XHTML_NS, SVG_NS, MATHML_NS].contains(&ns) {
                out.push_str(" xmlns=\"");
                out.push_str(ns);
                out.push('"');
            }
            if local == "html" && ns == XHTML_NS {
                out.push_str(" xmlns:epub=\"");
                out.push_str(EPUB_NS);
                out.push('"');
            }
            let mut written: HashSet<String> = HashSet::new();
            for attr in attrs.borrow().iter() {
                let attr_ns: &str = &attr.name.ns;
                let attr_local: &str = &attr.name.local;
                let qualified = if attr_ns == XLINK_NS {
                    format!("xlink:{attr_local}")
                } else if attr_ns == "http://www.w3.org/XML/1998/namespace" {
                    format!("xml:{attr_local}")
                } else {
                    attr_local.to_owned()
                };
                // Declarations are written above; other prefixes would be
                // undeclared (epub:type is declared on <html>).
                if qualified == "xmlns"
                    || (qualified.starts_with("xmlns:") && qualified != "xmlns:xlink")
                    || (qualified.contains(':')
                        && !["xlink:", "xml:", "epub:", "xmlns:xlink"]
                            .iter()
                            .any(|p| qualified.starts_with(p)))
                    || !written.insert(qualified.clone())
                {
                    continue;
                }
                if qualified.starts_with("xlink:") && written.insert("xmlns:xlink".into()) {
                    out.push_str(" xmlns:xlink=\"");
                    out.push_str(XLINK_NS);
                    out.push('"');
                }
                out.push(' ');
                out.push_str(&qualified);
                out.push_str("=\"");
                escape_attr(&attr.value, out);
                out.push('"');
            }
            let children = node.children.borrow();
            if children.is_empty() && (VOID.contains(&local) || ns != XHTML_NS) {
                out.push_str("/>");
                return;
            }
            out.push('>');
            for child in children.iter() {
                write_node(child, out, Some(ns));
            }
            out.push_str("</");
            out.push_str(local);
            out.push('>');
        }
        NodeData::Document => {
            for child in node.children.borrow().iter() {
                write_node(child, out, parent_ns);
            }
        }
        NodeData::Doctype { .. } | NodeData::ProcessingInstruction { .. } => {}
    }
}

/// Checks a copy made for the Kobo before it's sent: a zip with the EPUB
/// `mimetype` first; a package document that's valid XML with every
/// namespace prefix declared; and every XHTML document in its manifest
/// well-formed. Returns what's wrong, for the user.
pub fn check_copy(path: &Path) -> std::result::Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|_| "it isn’t a valid EPUB file".to_owned())?;
    let first_is_mimetype = zip
        .by_index(0)
        .map(|e| e.name() == "mimetype")
        .unwrap_or(false);
    if !first_is_mimetype {
        return Err("its file type marker isn’t first".into());
    }
    let opf_path = super::kepub::package_path(&mut zip).ok_or("it has no package document")?;
    let mut opf = String::new();
    zip.by_name(&opf_path)
        .map_err(|_| "its package document is missing")?
        .read_to_string(&mut opf)
        .map_err(|_| "its package document isn’t text")?;
    check_namespaces(&opf).map_err(|e| format!("its package document is broken ({e})"))?;
    // XHTML documents in the manifest must be well-formed.
    let mut docs = Vec::new();
    let mut reader = Reader::from_str(&opf);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "item" => {
                let get = |k: &str| {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == k)
                        .and_then(|a| {
                            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .ok()
                                .map(|v| v.into_owned())
                        })
                };
                if get("media-type").as_deref() == Some("application/xhtml+xml")
                    && let Some(href) = get("href")
                {
                    docs.push(super::epub::join_zip_path(&opf_path, &href));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    for doc in docs {
        let decoded = percent_decode(&doc);
        let name = if zip.index_for_name(&decoded).is_some() {
            decoded
        } else {
            doc.clone()
        };
        let Ok(mut entry) = zip.by_name(&name) else {
            continue; // A missing file is the book's own problem; the Kobo skips it.
        };
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_err() {
            return Err(format!("{doc} can’t be read"));
        }
        if !is_well_formed(&String::from_utf8_lossy(&bytes)) {
            return Err(format!("{doc} isn’t well-formed XHTML"));
        }
    }
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every element and attribute prefix in `xml` is declared.
fn check_namespaces(xml: &str) -> std::result::Result<(), String> {
    use quick_xml::name::ResolveResult;
    let mut reader = quick_xml::NsReader::from_str(xml);
    reader.config_mut().check_end_names = true;
    loop {
        match reader.read_resolved_event() {
            Ok((ns, Event::Start(e) | Event::Empty(e))) => {
                let name = e.name().as_ref().to_owned();
                if name.contains(':') && matches!(ns, ResolveResult::Unknown(_)) {
                    return Err(format!("undeclared prefix on <{name}>"));
                }
                for a in e.attributes().flatten() {
                    let key = a.key.as_ref().to_owned();
                    if key.contains(':') && !key.starts_with("xmlns") && !key.starts_with("xml:") {
                        let (resolved, _) = reader.resolver().resolve_attribute(a.key);
                        if matches!(resolved, ResolveResult::Unknown(_)) {
                            return Err(format!("undeclared prefix on {key}"));
                        }
                    }
                }
            }
            Ok((_, Event::Eof)) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As in a real Bookmate-made EPUB: HTML5 void elements never closed.
    const HTML5: &str = r#"<!DOCTYPE html>
      <html>
        <head>
          <meta charset="UTF-8">
          <title>Irish Paganism</title>
          <link href="bookmate.css" rel="stylesheet" type="text/css">
          <style>p > span{color:red}</style>
        </head>
        <body>
          <p epub:type="chapter">One<br>two &nbsp;&amp; three <img src="a.jpg" alt="x"></p>
          <p>Unclosed paragraph
          <svg viewBox="0 0 10 10"><image xlink:href="c.jpg" width="10" height="10"></image></svg>
        </body>
      </html>"#;

    #[test]
    fn tidies_html5_into_well_formed_xhtml() {
        assert!(!is_well_formed(HTML5));
        let xhtml = to_xhtml(HTML5);
        assert!(is_well_formed(&xhtml), "{xhtml}");
        check_namespaces(&xhtml).unwrap();
        assert!(xhtml.contains(r#"<html xmlns="http://www.w3.org/1999/xhtml""#));
        assert!(xhtml.contains(r#"<meta charset="UTF-8"/>"#));
        assert!(xhtml.contains("One<br/>two \u{a0}&amp; three"));
        assert!(xhtml.contains(r#"epub:type="chapter""#));
        assert!(xhtml.contains(r#"<svg xmlns="http://www.w3.org/2000/svg""#));
        assert!(xhtml.contains(r#"xlink:href="c.jpg""#));
        assert!(xhtml.contains("p &gt; span{color:red}"));
        // And the KEPUB converter takes it.
        let (kepub, spans) = super::super::kepub::convert_document(&xhtml).unwrap();
        assert!(spans > 0 && is_well_formed(&kepub));
    }

    #[test]
    fn well_formed_documents_are_recognised() {
        assert!(is_well_formed(
            r#"<?xml version="1.0"?><html><body><p>Hi<br/></p></body></html>"#
        ));
        assert!(!is_well_formed("<html><body><p>Hi</body></html>"));
        assert!(!is_well_formed("<html><body>"));
        assert!(!is_well_formed(""));
        assert!(
            !is_well_formed("<p>&copy; 2019</p>"),
            "HTML entities aren't XML"
        );
        assert!(is_well_formed("<p>&#169; &amp; &lt;2019&gt;</p>"));
        assert!(to_xhtml("<p>&copy; 2019</p>").contains("<p>\u{a9} 2019</p>"));
    }
}
