pub fn strip_leading_whitespace(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len()
        && (bytes[start] == b' '
            || bytes[start] == b'\t'
            || bytes[start] == b'\n'
            || bytes[start] == b'\r')
    {
        start += 1;
    }
    &bytes[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        assert_eq!(strip_leading_whitespace(b""), b"");
    }

    #[test]
    fn test_all_whitespace() {
        assert_eq!(strip_leading_whitespace(b"   \t\n\r"), b"");
    }

    #[test]
    fn test_leading_spaces() {
        assert_eq!(strip_leading_whitespace(b"   hello"), b"hello");
    }

    #[test]
    fn test_leading_tabs() {
        assert_eq!(strip_leading_whitespace(b"\t\thello"), b"hello");
    }

    #[test]
    fn test_leading_newlines() {
        assert_eq!(strip_leading_whitespace(b"\n\nhello"), b"hello");
    }

    #[test]
    fn test_leading_carriage_returns() {
        assert_eq!(strip_leading_whitespace(b"\r\rhello"), b"hello");
    }

    #[test]
    fn test_mixed_whitespace() {
        assert_eq!(
            strip_leading_whitespace(b" \t\n\rhello world"),
            b"hello world"
        );
    }

    #[test]
    fn test_no_leading_whitespace() {
        assert_eq!(strip_leading_whitespace(b"hello"), b"hello");
    }

    #[test]
    fn test_trailing_whitespace_preserved() {
        assert_eq!(strip_leading_whitespace(b"hello   "), b"hello   ");
    }
}
