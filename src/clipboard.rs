//! Clipboard helpers: `.env` line formatting and the OSC 52 sequence that asks the terminal to copy.

/// Formats one `NAME=value` line for a `.env` file (no trailing newline). Values that
/// dotenv parsers could misread are double-quoted with `\\`, `\"`, LF and CR escaped.
pub fn env_line(name: &str, value: &str) -> String {
    let bare = !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '#' | '"' | '\'' | '\\' | '=' | '$' | '`'));
    if bare {
        return format!("{name}={value}");
    }
    if value.is_empty() {
        return format!("{name}=");
    }
    let mut out = format!("{name}=\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The OSC 52 sequence that asks the terminal to put `text` on the system clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Standard base64 (RFC 4648) with `=` padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_line_leaves_plain_values_bare() {
        assert_eq!(env_line("PORT", "8080"), "PORT=8080");
        assert_eq!(
            env_line("URL", "postgres://u@h:5432/db"),
            "URL=postgres://u@h:5432/db"
        );
    }

    #[test]
    fn env_line_quotes_and_escapes_where_needed() {
        assert_eq!(env_line("E", ""), "E=");
        assert_eq!(env_line("A", "has space"), "A=\"has space\"");
        assert_eq!(env_line("B", "a#b"), "B=\"a#b\"");
        assert_eq!(env_line("C", "x=y"), "C=\"x=y\"");
        assert_eq!(env_line("D", "${{X.Y}}"), "D=\"${{X.Y}}\"");
        assert_eq!(env_line("F", "say \"hi\""), "F=\"say \\\"hi\\\"\"");
        assert_eq!(env_line("G", "a\\b"), "G=\"a\\\\b\"");
        assert_eq!(env_line("H", "l1\nl2\r"), "H=\"l1\\nl2\\r\"");
        assert_eq!(env_line("I", "it's"), "I=\"it's\"");
        assert_eq!(env_line("J", "`x`"), "J=\"`x`\"");
    }

    #[test]
    fn osc52_wraps_base64_of_utf8() {
        assert_eq!(osc52(""), "\x1b]52;c;\x07");
        assert_eq!(osc52("f"), "\x1b]52;c;Zg==\x07");
        assert_eq!(osc52("fo"), "\x1b]52;c;Zm8=\x07");
        assert_eq!(osc52("foo"), "\x1b]52;c;Zm9v\x07");
        assert_eq!(osc52("é\n"), "\x1b]52;c;w6kK\x07");
    }
}
