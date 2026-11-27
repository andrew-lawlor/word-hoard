//! Converts every EPUB under a folder to KEPUB and checks the results: each
//! rewritten document must still be well-formed XML, and with the koboSpans
//! taken out its text must be unchanged.
//!
//!     cargo run --release -p hoard-core --example kepub-check -- <epub folder> <output folder>

use std::io::Read;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;

fn epubs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            epubs(&p, out);
        } else if p.extension().is_some_and(|x| x == "epub") {
            out.push(p);
        }
    }
}

/// The text content of an XHTML document's body, spans and wrappers ignored.
fn body_text(xhtml: &str) -> Result<String, String> {
    let mut reader = quick_xml::Reader::from_str(xhtml);
    reader.config_mut().check_end_names = true;
    let (mut text, mut in_body) = (String::new(), false);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "body" => in_body = true,
            Ok(Event::Text(t)) if in_body => text.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(r)) if in_body => match r.resolve_char_ref() {
                Ok(Some(c)) => text.push(c),
                _ => text.push_str(match &*r {
                    "quot" => "\"",
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "apos" => "'",
                    "nbsp" | "ensp" | "emsp" | "thinsp" => " ",
                    "mdash" => "—",
                    "ndash" => "–",
                    "hellip" => "…",
                    "ldquo" => "“",
                    "rdquo" => "”",
                    "rsquo" | "lsquo" => "’",
                    other => return Err(format!("unknown entity &{other};")),
                }),
            },
            Ok(Event::Eof) => return Ok(text),
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (src, out) = (Path::new(&args[1]), Path::new(&args[2]));
    std::fs::create_dir_all(out).unwrap();
    let mut files = Vec::new();
    epubs(src, &mut files);
    files.sort();
    let started = std::time::Instant::now();
    let (mut ok, mut drm, mut failed, mut docs, mut spans, mut changed) = (0, 0, 0, 0, 0, 0);
    for (i, file) in files.iter().enumerate() {
        let dest = out.join(format!("{i}.kepub.epub"));
        match hoard_core::kobo::kepub::convert(file, &dest) {
            Ok(stats) => {
                docs += stats.documents;
                spans += stats.spans;
                // Compare each content document before and after.
                let mut before = zip::ZipArchive::new(std::fs::File::open(file).unwrap()).unwrap();
                let mut after = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
                for j in 0..before.len() {
                    let name = before.by_index(j).unwrap().name().to_owned();
                    let lower = name.to_lowercase();
                    if !(lower.ends_with(".xhtml")
                        || lower.ends_with(".html")
                        || lower.ends_with(".htm"))
                    {
                        continue;
                    }
                    let read = |z: &mut zip::ZipArchive<std::fs::File>| {
                        let mut s = Vec::new();
                        z.by_name(&name).unwrap().read_to_end(&mut s).unwrap();
                        String::from_utf8_lossy(&s).into_owned()
                    };
                    let (a, b) = (read(&mut before), read(&mut after));
                    match (body_text(&a), body_text(&b)) {
                        (Ok(x), Ok(y)) if x == y => {}
                        (Ok(x), Ok(y)) => {
                            changed += 1;
                            let at = x.chars().zip(y.chars()).take_while(|(p, q)| p == q).count();
                            let around = |s: &str| {
                                s.chars()
                                    .skip(at.saturating_sub(30))
                                    .take(70)
                                    .collect::<String>()
                            };
                            if changed <= 6 {
                                eprintln!(
                                    "text changed: {} {name}\n  was: {:?}\n  now: {:?}",
                                    file.display(),
                                    around(&x),
                                    around(&y)
                                );
                            }
                        }
                        (Ok(_), Err(e)) => {
                            changed += 1;
                            eprintln!("broken XML: {} {name}: {e}", file.display());
                        }
                        (Err(_), _) => {} // the original wasn't well-formed either
                    }
                }
                ok += 1;
            }
            Err(hoard_core::Error::Drm(_)) => drm += 1,
            Err(e) => {
                failed += 1;
                eprintln!("failed: {}: {e}", file.display());
            }
        }
    }
    println!(
        "{ok} converted ({docs} documents, {spans} spans), {drm} DRM, {failed} failed, {changed} documents with problems, {:.1} s",
        started.elapsed().as_secs_f64()
    );
}
