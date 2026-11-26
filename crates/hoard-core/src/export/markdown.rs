//! One Markdown file per book, Obsidian-friendly: YAML front matter, a
//! heading per chapter, each highlight as a quote with a block id (`^kobo-…`)
//! so notes elsewhere can link to it.

use std::fmt::Write;

use crate::model::{Annotation, AnnotationKind, BookAnnotations, BookInfo};

pub struct MarkdownOptions {
    /// Date written to the `exported` field, as `YYYY-MM-DD`.
    pub exported_on: String,
}

pub fn render_book(entry: &BookAnnotations, opts: &MarkdownOptions) -> String {
    let book = &entry.book;
    let title = book.display_title();
    let mut out = String::new();

    out.push_str("---\n");
    yaml_field(&mut out, "title", &title);
    if !book.authors.is_empty() {
        out.push_str("authors:\n");
        for author in &book.authors {
            let _ = writeln!(out, "  - {}", yaml_str(&format!("[[{author}]]")));
        }
    }
    if let Some(first) = entry.annotations.first() {
        yaml_field(&mut out, "source", &first.source);
    }
    yaml_field(&mut out, "kobo_content_id", &book.device_id);
    let _ = writeln!(out, "origin: {}", book.origin.as_str());
    for (key, value) in [
        ("isbn", &book.isbn),
        ("publisher", &book.publisher),
        ("series", &book.series),
        ("series_index", &book.series_index),
    ] {
        if let Some(value) = value {
            yaml_field(&mut out, key, value);
        }
    }
    if let Some(percent) = book.percent_read {
        let _ = writeln!(out, "progress: {percent}");
    }
    if let Some(status) = book.read_status {
        let _ = writeln!(out, "status: {}", status.as_str());
    }
    if let Some(last) = &book.last_read {
        yaml_field(&mut out, "last_read", date_part(last));
    }
    if !book.on_device {
        out.push_str("on_device: false\n");
    }
    let _ = writeln!(out, "highlights: {}", entry.count(AnnotationKind::Highlight));
    let _ = writeln!(out, "notes: {}", entry.count(AnnotationKind::Note));
    let _ = writeln!(out, "bookmarks: {}", entry.count(AnnotationKind::Bookmark));
    yaml_field(&mut out, "exported", &opts.exported_on);
    out.push_str("tags:\n  - kobo-highlights\n---\n\n");

    let _ = writeln!(out, "# {title}\n");
    if !book.authors.is_empty() {
        let _ = writeln!(out, "*{}*\n", book.authors.join(", "));
    }
    if !book.on_device {
        out.push_str("*This book is no longer on the device; its annotations were still in the device database.*\n\n");
    }

    let mut current_chapter: Option<Option<&str>> = None;
    for ann in entry.annotations.iter().filter(|a| a.kind != AnnotationKind::Bookmark) {
        let chapter = ann.chapter.as_deref();
        if current_chapter != Some(chapter) {
            let _ = writeln!(out, "## {}\n", chapter.unwrap_or("Unknown chapter"));
            current_chapter = Some(chapter);
        }
        render_annotation(&mut out, ann);
    }

    let bookmarks: Vec<&Annotation> = entry
        .annotations
        .iter()
        .filter(|a| a.kind == AnnotationKind::Bookmark)
        .collect();
    if !bookmarks.is_empty() {
        out.push_str("## Bookmarks\n\n");
        for mark in bookmarks {
            let mut line = format!("- {}", mark.chapter.as_deref().unwrap_or("Unknown chapter"));
            if let Some(progress) = mark.position.chapter_progress.filter(|p| *p > 0.0) {
                let _ = write!(line, " ({:.0}%)", progress * 100.0);
            }
            if let Some(created) = &mark.created {
                let _ = write!(line, " · {}", date_part(created));
            }
            if mark.hidden {
                line.push_str(" · hidden on device");
            }
            let _ = writeln!(out, "{line}");
        }
        out.push('\n');
    }

    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

fn render_annotation(out: &mut String, ann: &Annotation) {
    let id = block_id(&ann.source_id);
    match (ann.text.as_deref(), ann.note.as_deref()) {
        (Some(text), note) => {
            for line in text.trim().lines() {
                let line = line.trim_end();
                if line.is_empty() {
                    out.push_str(">\n");
                } else {
                    let _ = writeln!(out, "> {line}");
                }
            }
            // Obsidian needs the id of a quote on its own line after the block.
            let _ = writeln!(out, "\n^{id}\n");
            if let Some(note) = note {
                let _ = writeln!(out, "**Note:** {}\n", note.trim());
            }
        }
        (None, Some(note)) => {
            let _ = writeln!(out, "**Note:** {} ^{id}\n", note.trim());
        }
        (None, None) => {
            let _ = writeln!(out, "*(no text)* ^{id}\n");
        }
    }

    let mut meta: Vec<String> = Vec::new();
    if ann.kind == AnnotationKind::Markup {
        meta.push("handwritten markup".into());
    }
    if let Some(created) = &ann.created {
        meta.push(date_part(created).to_string());
    }
    // Yellow is the default highlight colour, so it is not worth repeating.
    if let Some(color) = ann.color.as_deref().filter(|c| *c != "yellow") {
        meta.push(color.to_string());
    }
    if ann.hidden {
        meta.push("hidden on device".into());
    }
    if !meta.is_empty() {
        let _ = writeln!(out, "*{}*\n", meta.join(" · "));
    }
}

/// A filesystem-safe file name: `Author - Title.md`.
pub fn file_name(book: &BookInfo) -> String {
    let title = book.display_title();
    let base = match book.authors.first() {
        Some(author) => format!("{author} - {title}"),
        None => title,
    };
    let cleaned: String = base
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed: String = cleaned.trim().trim_matches('.').trim().chars().take(120).collect();
    if trimmed.is_empty() {
        "Untitled.md".to_string()
    } else {
        format!("{}.md", trimmed.trim_end())
    }
}

/// Obsidian block ids allow letters, digits and dashes.
pub fn block_id(source_id: &str) -> String {
    let id: String = source_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_ascii_lowercase();
    format!("kobo-{id}")
}

fn date_part(timestamp: &str) -> &str {
    match timestamp.get(..10) {
        Some(date) => date,
        None => timestamp,
    }
}

fn yaml_field(out: &mut String, key: &str, value: &str) {
    let _ = writeln!(out, "{key}: {}", yaml_str(value));
}

fn yaml_str(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BookOrigin, Position, ReadStatus};

    fn book() -> BookInfo {
        BookInfo {
            device_id: "file:///mnt/onboard/a.kepub.epub".into(),
            title: Some("A \"Quoted\" Title: Part 1/2".into()),
            authors: vec!["Ursula K. Le Guin".into()],
            isbn: None,
            publisher: None,
            series: None,
            series_index: None,
            percent_read: Some(40),
            read_status: Some(ReadStatus::Reading),
            last_read: Some("2026-11-20T21:14:03Z".into()),
            origin: BookOrigin::Sideloaded,
            on_device: true,
        }
    }

    fn ann(id: &str, kind: AnnotationKind, chapter: &str, text: Option<&str>, note: Option<&str>) -> Annotation {
        Annotation {
            source: "kobo:N1".into(),
            source_id: id.into(),
            kind,
            text: text.map(Into::into),
            note: note.map(Into::into),
            color: Some("yellow".into()),
            chapter: Some(chapter.into()),
            position: Position::default(),
            created: Some("2026-11-02T10:00:00.000".into()),
            modified: None,
            hidden: false,
        }
    }

    #[test]
    fn renders_front_matter_chapters_and_block_ids() {
        let entry = BookAnnotations {
            book: book(),
            annotations: vec![
                ann("AB-1", AnnotationKind::Highlight, "One", Some("line one\n\nline two"), None),
                ann("ab-2", AnnotationKind::Note, "One", Some("quoted"), Some("my thought")),
                ann("ab-3", AnnotationKind::Highlight, "Two", Some("later"), None),
                ann("ab-4", AnnotationKind::Bookmark, "Two", None, None),
            ],
        };
        let md = render_book(&entry, &MarkdownOptions { exported_on: "2026-11-26".into() });

        assert!(md.starts_with("---\ntitle: \"A \\\"Quoted\\\" Title: Part 1/2\"\n"));
        assert!(md.contains("authors:\n  - \"[[Ursula K. Le Guin]]\"\n"));
        assert!(md.contains("highlights: 2\nnotes: 1\nbookmarks: 1\n"));
        assert!(md.contains("last_read: \"2026-11-20\""));
        assert!(md.contains("> line one\n>\n> line two\n\n^kobo-ab-1\n"));
        assert!(md.contains("**Note:** my thought"));
        assert_eq!(md.matches("## One").count(), 1);
        assert!(md.contains("## Two\n\n> later"));
        assert!(md.contains("## Bookmarks\n\n- Two · 2026-11-02"));
        assert!(!md.contains("yellow"));
        assert!(md.ends_with('\n') && !md.ends_with("\n\n"));
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name(&book()), "Ursula K. Le Guin - A _Quoted_ Title_ Part 1_2.md");
        let mut untitled = book();
        untitled.title = None;
        untitled.authors.clear();
        assert_eq!(file_name(&untitled), "Untitled.md");
    }
}
