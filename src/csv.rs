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
use memchr::{memchr, memchr_iter, memchr2, memchr2_iter};

/// Parse every row in `chunk`, calling `on_row` for each. The row buffer is
/// owned here and reused across the chunk's rows (one allocation per chunk);
/// the fields it holds borrow from `chunk`, so it cannot outlive the chunk.
pub fn parse_chunk<'a>(chunk: &'a str, mut on_row: impl FnMut(&mut Vec<Field<'a>>)) {
    let mut row: Vec<Field<'a>> = Vec::new();
    let bytes = chunk.as_bytes();
    let mut start = 0;
    // The fields of the line before, counted before `on_row` may change the
    // row (see `parse_line`).
    let mut fields_before = 1;
    for nl in memchr_iter(b'\n', bytes) {
        parse_line(strip_cr(&chunk[start..nl]), &mut row, fields_before);
        fields_before = row.len();
        on_row(&mut row);
        start = nl + 1;
    }
    // Trailing content not terminated by a newline is still a row; a trailing
    // newline (start == len) leaves nothing and emits no spurious empty row.
    if start < chunk.len() {
        parse_line(strip_cr(&chunk[start..]), &mut row, fields_before);
        on_row(&mut row);
    }
}

/// [`parse_chunk`], calling `on_row` with each row's line too, without its
/// line break: for [`write_unchanged`].
pub fn parse_chunk_lines<'a>(chunk: &'a str, mut on_row: impl FnMut(&mut Vec<Field<'a>>, &'a str)) {
    let mut row: Vec<Field<'a>> = Vec::new();
    let bytes = chunk.as_bytes();
    let mut start = 0;
    let mut fields_before = 1;
    // Each line's end: every newline, and the chunk's end after content with
    // none.
    let tail = (!chunk.ends_with('\n') && !chunk.is_empty()).then_some(chunk.len());
    for end in memchr_iter(b'\n', bytes).chain(tail) {
        let line = strip_cr(&chunk[start..end]);
        parse_line(line, &mut row, fields_before);
        fields_before = row.len();
        on_row(&mut row, line);
        start = end + 1;
    }
}

/// Parse a CSV header line into owned column names.
pub fn parse_header(line: &str) -> Vec<String> {
    let mut row: Vec<Field> = Vec::new();
    parse_line(strip_cr(line), &mut row, 1);
    row.iter().map(|f| f.as_str().into_owned()).collect()
}

#[inline]
fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// Split one line into fields. `line` must not contain `\n`.
///
/// One scan finds the commas of a line with no quote, nearly every line, and
/// stops at the first quote, whose field and those after it are read field by
/// field. Which scan depends on how long the fields run: over 16 bytes each
/// when the line is longer than that for each of the `fields_before`, the
/// fields of the line before, as lines of a file are alike.
fn parse_line<'a>(line: &'a str, row: &mut Vec<Field<'a>>, fields_before: usize) {
    row.clear();
    if line.len() > 16 * fields_before {
        split_long(line, row);
    } else {
        split_short(line, row);
    }
}

/// [`parse_line`]'s scan for long fields: memchr's iterator, whose wide scan
/// pays for its start on a long field.
fn split_long<'a>(line: &'a str, row: &mut Vec<Field<'a>>) {
    let bytes = line.as_bytes();
    let mut start = 0;
    for at in memchr2_iter(b',', b'"', bytes) {
        if bytes[at] == b'"' {
            return parse_fields(line, start, row);
        }
        row.push(Field::Str(&line[start..at]));
        start = at + 1;
    }
    row.push(Field::Str(&line[start..]));
}

/// [`parse_line`]'s scan for short fields: eight bytes at a time, with memchr's
/// wider scan for a field that runs on.
fn split_short<'a>(line: &'a str, row: &mut Vec<Field<'a>>) {
    let bytes = line.as_bytes();
    let mut start = 0;
    let mut at = 0;
    while let Some(word) = bytes.get(at..at + 8) {
        let word = u64::from_le_bytes(word.try_into().expect("8 bytes"));
        let mut hits = bytes_eq(word, b',') | bytes_eq(word, b'"');
        if hits == 0 {
            // A field that runs on: memchr's wider scan finds its end.
            at += 8;
            at = memchr2(b',', b'"', &bytes[at..]).map_or(bytes.len(), |r| at + r);
            continue;
        }
        while hits != 0 {
            let hit = at + (hits.trailing_zeros() / 8) as usize;
            if bytes[hit] == b'"' {
                return parse_fields(line, start, row);
            }
            row.push(Field::Str(&line[start..hit]));
            start = hit + 1;
            hits &= hits - 1;
        }
        at += 8;
    }
    for hit in at..bytes.len() {
        match bytes[hit] {
            b',' => {
                row.push(Field::Str(&line[start..hit]));
                start = hit + 1;
            }
            b'"' => return parse_fields(line, start, row),
            _ => {}
        }
    }
    row.push(Field::Str(&line[start..]));
}

/// The high bit of each byte of `word` that is `byte`, and no other bit.
#[inline]
fn bytes_eq(word: u64, byte: u8) -> u64 {
    const SEVEN: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    // A byte of `x` is zero where `word`'s matched. Adding seven bits to its
    // low seven never carries into the next byte.
    let x = word ^ (0x0101_0101_0101_0101 * u64::from(byte));
    !(((x & SEVEN) + SEVEN) | x | SEVEN)
}

/// Push the fields of `line` from the one starting at `i` onto `row`.
fn parse_fields<'a>(line: &'a str, mut i: usize, row: &mut Vec<Field<'a>>) {
    let bytes = line.as_bytes();
    let len = bytes.len();
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
    write_cells(buf, row);
}

/// [`write_row`] over any sequence of cells.
pub fn write_cells<'f, 'a: 'f>(buf: &mut String, cells: impl IntoIterator<Item = &'f Field<'a>>) {
    for (i, f) in cells.into_iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        match f {
            Field::Num(n) => crate::field::format_num_into(*n, buf),
            Field::Str(s) => write_text(buf, s),
            Field::Owned(s) => write_text(buf, s),
        }
    }
    buf.push('\n');
}

/// Whether writing the fields of `line` gives `line` itself: when it holds no
/// quote and no carriage return.
#[inline]
pub fn is_verbatim(line: &str) -> bool {
    memchr2(b'"', b'\r', line.as_bytes()).is_none()
}

/// Write `row`, the fields of `line` as parsed and not changed since, onto
/// `buf`: `line` itself when that is what writing them gives
/// ([`is_verbatim`]).
#[inline]
pub fn write_unchanged(buf: &mut String, line: &str, row: &[Field]) {
    if is_verbatim(line) {
        buf.push_str(line);
        buf.push('\n');
    } else {
        write_row(buf, row);
    }
}

/// CSV-encode one field's text onto `buf`, quoting it only if it holds a
/// comma, a quote or a line break.
#[inline]
pub fn write_text(buf: &mut String, s: &str) {
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
    fn both_scans_split_as_field_by_field_reading_does() {
        let long = "x".repeat(40);
        let lines = [
            String::new(),
            ",".into(),
            "a,,b,".into(),
            "1,22,333,4444,55555,666666,7777777,88888888,999999999".into(),
            format!("{long},{long},b"),
            format!("{long}\"x,y"),
            r#"a,"b,c",d"#.into(),
            r#"0123456789,"q ""x"" q",z"#.into(),
            format!(r#"a,{long},"{long},",z"#),
            "abcdefg\",h".into(),
            "日本,語,🙂x".into(),
        ];
        for line in &lines {
            let mut expect = Vec::new();
            parse_fields(line, 0, &mut expect);
            for split in [split_short, split_long] {
                let mut row = Vec::new();
                split(line, &mut row);
                assert_eq!(format!("{row:?}"), format!("{expect:?}"), "{line:?}");
            }
        }
    }

    #[test]
    fn bytes_eq_marks_exactly_the_matching_bytes() {
        // A match in one byte, and every value in each byte above it, where a
        // carry or borrow from the match could show.
        for byte in [b',', b'"', 0, 0x7f, 0x80, 0xff] {
            for other in 0..=255u8 {
                for (at, above) in (0..8).flat_map(|a| (a + 1..8).map(move |b| (a, b))) {
                    let mut word = [byte ^ 0x5a; 8];
                    word[at] = byte;
                    word[above] = other;
                    let hits = bytes_eq(u64::from_le_bytes(word), byte);
                    let expect = word
                        .iter()
                        .enumerate()
                        .filter(|(_, b)| **b == byte)
                        .fold(0u64, |m, (i, _)| m | 0x80 << (8 * i));
                    assert_eq!(hits, expect, "{word:x?} for {byte:#x}");
                }
            }
        }
    }

    #[test]
    fn write_unchanged_gives_what_writing_the_fields_gives() {
        for line in ["a,b,,c", r#""x",1"#, r#"a,"b,c""#, "y\rz,1", "", ","] {
            let mut row = Vec::new();
            parse_fields(line, 0, &mut row);
            let (mut got, mut expect) = (String::new(), String::new());
            write_unchanged(&mut got, line, &row);
            write_row(&mut expect, &row);
            assert_eq!(got, expect, "{line:?}");
        }
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
