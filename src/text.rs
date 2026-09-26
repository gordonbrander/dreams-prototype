//! Plain text helpers.

/// Collapse whitespace, then cut at a word boundary to `max` characters or
/// less, `…` included. A first word longer than `max` is cut in the word.
pub fn truncate(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let room = max.saturating_sub(1);
    let mut cut = String::new();
    let mut len = 0;
    for word in flat.split(' ') {
        let add = word.chars().count() + usize::from(len > 0);
        if len + add > room {
            break;
        }
        if len > 0 {
            cut.push(' ');
        }
        cut.push_str(word);
        len += add;
    }
    if cut.is_empty() {
        cut = flat.chars().take(room).collect();
    }
    cut.push('…');
    cut
}

/// Lossy UTF-8, cut at a character boundary. The flag says whether it was cut.
pub fn truncate_bytes(bytes: &[u8], max: usize) -> (String, bool) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

/// Escape `&`, `<` and `>` for XML text.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_collapses_whitespace() {
        assert_eq!(truncate("  a \n\n b\tc ", 150), "a b c");
    }

    #[test]
    fn truncate_cuts_at_a_word_boundary() {
        assert_eq!(truncate("one two three", 10), "one two…");
        assert_eq!(truncate("one two three", 13), "one two three", "exactly max is not cut");
    }

    #[test]
    fn truncate_cuts_a_long_first_word() {
        let cut = truncate(&"é".repeat(200), 150);
        assert_eq!(cut.chars().count(), 150);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn truncate_bytes_keeps_char_boundaries() {
        let s = "héllo wörld".repeat(10);
        let (cut, was_cut) = truncate_bytes(s.as_bytes(), 8);
        assert!(was_cut);
        assert!(cut.len() <= 8);
        assert!(s.starts_with(&cut));
        let (whole, was_cut) = truncate_bytes(b"abc", 8);
        assert_eq!((whole.as_str(), was_cut), ("abc", false));
    }

    #[test]
    fn xml_escape_escapes_markup() {
        assert_eq!(xml_escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
