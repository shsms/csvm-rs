//! A zero-copy, quote-aware CSV scanner.
//!
//! Rows are newline-delimited (no embedded newlines in fields) so the input can
//! be split into chunks at line boundaries and parsed in parallel. Within a
//! line, fields are comma-separated and may be `"`-quoted; a quoted field may
//! contain commas, and `""` is an escaped quote. Fields are sliced directly out
//! of the chunk buffer — an allocation happens only to unescape a `""`.
//!
//! See [`crate::field`] for how values are represented and written back.

use crate::field::Field;
use memchr::{memchr, memchr_iter};

/// Parse every row in `chunk`, calling `on_row` for each. The row buffer is
/// owned here and reused across the chunk's rows (one allocation per chunk);
/// the fields it holds borrow from `chunk`, so it cannot outlive the chunk.
pub fn parse_chunk<'a>(chunk: &'a str, mut on_row: impl FnMut(&mut Vec<Field<'a>>)) {
    let mut row: Vec<Field<'a>> = Vec::new();
    let bytes = chunk.as_bytes();
    let mut start = 0;
    for nl in memchr_iter(b'\n', bytes) {
        parse_line(strip_cr(&chunk[start..nl]), &mut row);
        on_row(&mut row);
        start = nl + 1;
    }
    // Trailing content not terminated by a newline is still a row; a trailing
    // newline (start == len) leaves nothing and emits no spurious empty row.
    if start < chunk.len() {
        parse_line(strip_cr(&chunk[start..]), &mut row);
        on_row(&mut row);
    }
}

/// Parse a CSV header line into owned column names.
pub fn parse_header(line: &str) -> Vec<String> {
    let mut row: Vec<Field> = Vec::new();
    parse_line(strip_cr(line), &mut row);
    row.iter().map(|f| f.as_str().into_owned()).collect()
}

#[inline]
fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// Split one line into fields. `line` must not contain `\n`.
fn parse_line<'a>(line: &'a str, row: &mut Vec<Field<'a>>) {
    row.clear();
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    loop {
        let (field, next) = if bytes.get(i) == Some(&b'"') {
            parse_quoted(line, i)
        } else {
            parse_plain(line, i)
        };
        row.push(field);
        i = next;
        if i >= len {
            break;
        }
        // `next` lands on a comma; step past it. A comma in the last position
        // means a trailing empty field.
        i += 1;
        if i == len {
            row.push(Field::Str(""));
            break;
        }
    }
}

/// Plain field from `i` up to the next comma (or end of line).
#[inline]
fn parse_plain(line: &str, i: usize) -> (Field<'_>, usize) {
    let bytes = line.as_bytes();
    match memchr(b',', &bytes[i..]) {
        Some(rel) => (Field::Str(&line[i..i + rel]), i + rel),
        None => (Field::Str(&line[i..]), line.len()),
    }
}

/// Quoted field starting at the opening `"` at `i`. Returns the field and the
/// index of the following comma (or end of line); any stray bytes between the
/// closing quote and that comma are dropped.
fn parse_quoted(line: &str, i: usize) -> (Field<'_>, usize) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let start = i + 1; // past opening quote
    let mut j = start;
    let mut escaped = false;
    loop {
        match memchr(b'"', &bytes[j..]) {
            Some(rel) => {
                let q = j + rel;
                if bytes.get(q + 1) == Some(&b'"') {
                    escaped = true;
                    j = q + 2; // skip the escaped quote pair
                } else {
                    let field = make_quoted(&line[start..q], escaped);
                    let next = memchr(b',', &bytes[q + 1..])
                        .map(|r| q + 1 + r)
                        .unwrap_or(len);
                    return (field, next);
                }
            }
            // Unterminated quote: take the rest of the line.
            None => return (make_quoted(&line[start..], escaped), len),
        }
    }
}

#[inline]
fn make_quoted(inner: &str, escaped: bool) -> Field<'_> {
    if escaped {
        Field::Owned(inner.replace("\"\"", "\""))
    } else {
        Field::Str(inner)
    }
}

/// Append a CSV-encoded row (with trailing newline) to `buf`. A field is quoted
/// only when it contains a delimiter, quote, or newline; `"` is escaped as `""`.
/// Numbers are formatted and never need quoting.
pub fn write_row(buf: &mut String, row: &[Field]) {
    for (i, f) in row.iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        match f {
            Field::Num(n) => buf.push_str(&crate::field::format_num(*n)),
            Field::Str(s) => write_text(buf, s),
            Field::Owned(s) => write_text(buf, s),
        }
    }
    buf.push('\n');
}

/// CSV-encode one field's text (quoting only if needed), returned as a String.
/// Used when colouring plain CSV output, where each cell is encoded then wrapped
/// in ANSI separately.
pub fn encode_field(s: &str) -> String {
    let mut buf = String::new();
    write_text(&mut buf, s);
    buf
}

#[inline]
fn write_text(buf: &mut String, s: &str) {
    if s.bytes().any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r')) {
        buf.push('"');
        // Wrap in quotes and double every interior `"`: join the `"`-split
        // parts with `""`.
        let mut first = true;
        for part in s.split('"') {
            if !first {
                buf.push_str("\"\"");
            }
            first = false;
            buf.push_str(part);
        }
        buf.push('"');
    } else {
        buf.push_str(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(chunk: &str) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        parse_chunk(chunk, |r| {
            out.push(r.iter().map(|f| f.as_str().into_owned()).collect());
        });
        out
    }

    #[test]
    fn plain_rows() {
        assert_eq!(
            rows("a,b,c\n1,2,3\n"),
            vec![vec!["a", "b", "c"], vec!["1", "2", "3"],]
        );
    }

    #[test]
    fn no_trailing_newline() {
        assert_eq!(rows("1,2,3"), vec![vec!["1", "2", "3"]]);
    }

    #[test]
    fn trailing_and_empty_fields() {
        assert_eq!(rows("a,,c\n"), vec![vec!["a", "", "c"]]);
        assert_eq!(rows("a,b,\n"), vec![vec!["a", "b", ""]]);
    }

    #[test]
    fn crlf_line_endings() {
        assert_eq!(rows("a,b\r\n1,2\r\n"), vec![vec!["a", "b"], vec!["1", "2"]]);
    }

    #[test]
    fn quoted_with_comma() {
        assert_eq!(rows(r#"a,"b,c",d"#), vec![vec!["a", "b,c", "d"]]);
    }

    #[test]
    fn quoted_with_escaped_quote() {
        assert_eq!(
            rows(r#""he said ""hi""",x"#),
            vec![vec![r#"he said "hi""#, "x"]]
        );
    }

    #[test]
    fn header_parsing() {
        assert_eq!(parse_header("A,B,C"), vec!["A", "B", "C"]);
        assert_eq!(
            parse_header(r#""first,name",age"#),
            vec!["first,name", "age"]
        );
    }

    #[test]
    fn roundtrip_requoting() {
        // A field with a comma is re-quoted; a plain field is not.
        let mut buf = String::new();
        write_row(
            &mut buf,
            &[Field::Str("a,b"), Field::Str("plain"), Field::Num(25.0)],
        );
        assert_eq!(buf, "\"a,b\",plain,25\n");
    }

    #[test]
    fn roundtrip_escaped_quote() {
        let mut buf = String::new();
        write_row(&mut buf, &[Field::Owned(r#"a"b"#.into())]);
        assert_eq!(buf, "\"a\"\"b\"\n");
    }
}
