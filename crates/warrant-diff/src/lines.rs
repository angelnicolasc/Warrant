//! Byte-exact line handling.
//!
//! Every line carries its own terminator, so joining a split is the identity
//! function on *any* byte string — CRLF, mixed endings, a missing final
//! newline, invalid UTF-8. That property is not a nicety: hunk application
//! reconstructs files from these pieces, and a reconstruction that silently
//! normalises line endings would make the necessity map lie about what the
//! agent wrote.

/// Split into lines, each retaining its trailing `\n` if it had one.
///
/// The last line has no terminator when the input did not end with a newline,
/// which is how the distinction survives a round trip.
pub fn split_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            lines.push(bytes[start..=i].to_vec());
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push(bytes[start..].to_vec());
    }
    lines
}

/// Concatenate lines back into the original byte string.
pub fn join_lines(lines: &[Vec<u8>]) -> Vec<u8> {
    let capacity = lines.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(capacity);
    for line in lines {
        out.extend_from_slice(line);
    }
    out
}

/// Strip a trailing `\n` or `\r\n`, for display only.
pub fn trim_terminator(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Whether content should be treated as binary.
///
/// A NUL byte in the first 8 KiB, which is the heuristic git uses. Binary
/// files are still tracked and still diffed — they simply become a single
/// whole-file hunk rather than a set of line hunks.
pub fn is_binary(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(8192)];
    window.contains(&0)
}

/// Render a line for the terminal, replacing invalid UTF-8 rather than failing.
pub fn to_display(line: &[u8]) -> String {
    String::from_utf8_lossy(trim_terminator(line)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_on_the_cases_that_break_naive_splitters() {
        let cases: &[&[u8]] = &[
            b"",
            b"\n",
            b"a",
            b"a\n",
            b"a\nb",
            b"a\nb\n",
            b"\n\n\n",
            b"crlf\r\nmixed\nendings\r\n",
            b"no trailing newline",
            &[0xFF, 0xFE, b'\n', 0x00, b'x'],
        ];
        for case in cases {
            assert_eq!(
                join_lines(&split_lines(case)),
                case.to_vec(),
                "round trip failed for {case:?}"
            );
        }
    }

    #[test]
    fn a_missing_final_newline_is_preserved_as_a_distinction() {
        assert_eq!(split_lines(b"a\nb"), vec![b"a\n".to_vec(), b"b".to_vec()]);
        assert_eq!(split_lines(b"a\nb\n"), vec![b"a\n".to_vec(), b"b\n".to_vec()]);
        assert_ne!(split_lines(b"a\nb"), split_lines(b"a\nb\n"));
    }

    #[test]
    fn crlf_stays_attached_to_its_line() {
        assert_eq!(split_lines(b"a\r\nb\r\n"), vec![b"a\r\n".to_vec(), b"b\r\n".to_vec()]);
    }

    #[test]
    fn binary_detection_keys_on_nul() {
        assert!(!is_binary(b"plain text\n"));
        assert!(is_binary(b"has\0nul"));
        // A NUL past the sniff window is not detected, matching git.
        let mut late = vec![b'a'; 9000];
        late.push(0);
        assert!(!is_binary(&late));
    }

    #[test]
    fn display_drops_only_the_terminator() {
        assert_eq!(to_display(b"hello\r\n"), "hello");
        assert_eq!(to_display(b"hello\n"), "hello");
        assert_eq!(to_display(b"hello"), "hello");
        assert_eq!(to_display(b"  indented  \n"), "  indented  ");
    }

    proptest::proptest! {
        /// The property the whole hunk engine rests on.
        #[test]
        fn split_then_join_is_the_identity(bytes: Vec<u8>) {
            proptest::prop_assert_eq!(join_lines(&split_lines(&bytes)), bytes);
        }

        #[test]
        fn every_line_but_the_last_ends_in_a_newline(bytes: Vec<u8>) {
            let lines = split_lines(&bytes);
            if lines.len() > 1 {
                for line in &lines[..lines.len() - 1] {
                    proptest::prop_assert!(line.ends_with(b"\n"));
                }
            }
        }
    }
}
