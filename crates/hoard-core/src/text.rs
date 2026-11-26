//! Text cleanup for passages read off devices (ported from Kollate).

use unicode_normalization::UnicodeNormalization;

/// Cleans text for display: NFC, whitespace collapsed within each line,
/// blank leading and trailing lines removed. Paragraph breaks are kept.
pub fn clean_text(s: &str) -> String {
    let nfc: String = s.nfc().collect();
    let lines: Vec<String> = nfc
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let start = lines.iter().position(|l| !l.is_empty()).unwrap_or(lines.len());
    let end = lines.iter().rposition(|l| !l.is_empty()).map_or(start, |i| i + 1);
    lines[start..end].join("\n")
}

/// [`clean_text`], with `None` for missing or blank input.
pub fn clean_opt(s: Option<&str>) -> Option<String> {
    s.map(clean_text).filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_whitespace_but_keeps_paragraphs() {
        assert_eq!(clean_text("  a  b \n\n"), "a b");
        assert_eq!(clean_text("\n one \n two  three\n"), "one\ntwo three");
        assert_eq!(clean_opt(Some("  ")), None);
        // NFC: "e" + combining acute becomes "é".
        assert_eq!(clean_text("e\u{301}"), "\u{e9}");
    }
}
