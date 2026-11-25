//! Display sanitizing. Everything shown comes from the daemon and may contain imported,
//! untrusted text; control characters and bidirectional overrides would let such text
//! move the cursor, change colours or disguise itself, so they are made visible.

/// Replaces control and bidi-override characters with visible placeholders.
pub fn clean(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => ' ',
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' => {
                '\u{FFFD}'
            }
            c if c.is_control() => '\u{FFFD}',
            c => c,
        })
        .collect()
}

/// Splits text into sanitized display lines.
pub fn lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|l| clean(l.strip_suffix('\r').unwrap_or(l)))
        .collect()
}

/// A single sanitized line, truncated to `max` characters with an ellipsis.
pub fn short(text: &str, max: usize) -> String {
    let flat = clean(&text.replace(['\n', '\r'], " "));
    if flat.chars().count() <= max {
        flat
    } else {
        let mut s: String = flat.chars().take(max.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

/// Pretty JSON as sanitized lines.
pub fn json_lines(value: &serde_json::Value) -> Vec<String> {
    lines(&serde_json::to_string_pretty(value).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_terminal_control_sequences() {
        let hostile = "ok\u{1b}[2J\u{1b}]0;title\u{7}\u{202E}gnp.exe";
        let shown = clean(hostile);
        assert!(!shown.chars().any(|c| c.is_control()));
        assert!(!shown.contains('\u{202E}'));
        assert!(shown.starts_with("ok\u{FFFD}[2J"));
    }

    #[test]
    fn short_truncates_on_char_boundaries() {
        assert_eq!(short("héllo wörld", 6), "héllo…");
        assert_eq!(short("a\nb", 10), "a b");
    }
}
