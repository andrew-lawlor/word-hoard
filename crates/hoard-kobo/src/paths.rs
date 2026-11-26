//! Title and author from a sideloaded book's path, for books no longer in
//! the device's database (ported from Kollate, verified on a Clara 2E).
//!
//! calibre names files on a Kobo `…/Toole, John Kennedy/Confederacy of
//! Dunces, A - John Kennedy Toole.kepub.epub`, writing characters a FAT
//! filesystem can't hold (`:` and others) as `_`.

pub(crate) fn book_from_path(id: &str) -> (String, Option<String>) {
    let mut parts = id.rsplit('/');
    let file = parts.next().unwrap_or(id);
    let folder = parts.next().unwrap_or("");
    let stem = file.trim_end_matches(".epub").trim_end_matches(".kepub");
    let unmangle = |s: &str| s.replace("_ ", ": ");
    let (title, author) = match stem.rsplit_once(" - ") {
        Some((title, author)) => (title, Some(initials(author))),
        None => (stem, folder_author(folder)),
    };
    let mut title = unmangle(title).trim().to_owned();
    for article in ["The", "A", "An"] {
        if let Some(rest) = title.strip_suffix(&format!(", {article}")) {
            title = format!("{article} {rest}");
            break;
        }
    }
    (title, author.filter(|a| !a.is_empty()))
}

/// "Toole, John Kennedy" → "John Kennedy Toole"; several joined by " & ".
fn folder_author(folder: &str) -> Option<String> {
    let generic = ["books", "onboard"].iter().any(|g| folder.eq_ignore_ascii_case(g));
    if folder.is_empty() || generic || folder.starts_with("file:") {
        return None;
    }
    let names: Vec<String> = folder
        .split(" & ")
        .map(|name| match name.split_once(", ") {
            Some((last, first)) if !first.contains(',') => format!("{first} {last}"),
            _ => name.to_owned(),
        })
        .map(|n| initials(&n))
        .collect();
    Some(names.join(" & "))
}

/// calibre's "David W_" is "David W.".
fn initials(name: &str) -> String {
    name.split(' ')
        .map(|w| match w.strip_suffix('_') {
            Some(i) if i.chars().count() == 1 => format!("{i}."),
            _ => w.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_calibre_paths() {
        let b = |p: &str| book_from_path(&format!("file:///mnt/onboard/books/{p}"));
        assert_eq!(
            b("Toole, John Kennedy/Confederacy of Dunces, A.kepub.epub"),
            ("A Confederacy of Dunces".into(), Some("John Kennedy Toole".into()))
        );
        assert_eq!(
            b("Crawford, Jackson/Poetic Edda_ Stories of the Norse Gods and Heroes, The - Jackson Crawford.kepub.epub"),
            (
                "The Poetic Edda: Stories of the Norse Gods and Heroes".into(),
                Some("Jackson Crawford".into())
            )
        );
        assert_eq!(
            b("Anthony, David W_/Horse, the Wheel, and Language, The.kepub.epub"),
            ("The Horse, the Wheel, and Language".into(), Some("David W. Anthony".into()))
        );
        assert_eq!(
            b("Conrique, Sarah & Haynes, Graham I_/Some Book.epub").1.as_deref(),
            Some("Sarah Conrique & Graham I. Haynes")
        );
        assert_eq!(book_from_path("file:///mnt/onboard/Loose File.epub"), ("Loose File".into(), None));
        assert_eq!(book_from_path("file:///mnt/onboard/Books/Gone Book.epub"), ("Gone Book".into(), None));
    }
}
