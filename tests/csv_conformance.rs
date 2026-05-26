//! CSV conformance: confirm our scanner matches RFC 4180 / the `csv` crate on
//! well-formed input, and pin the one deliberate deviation (no newlines inside
//! quoted fields — a row is a line, which is what lets input be chunked and
//! sharded in parallel; csvm has the same constraint).

/// Parse every row with our scanner.
fn ours(input: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    csvm::csv::parse_chunk(input, |r| {
        rows.push(r.iter().map(|f| f.as_str().into_owned()).collect());
    });
    rows
}

/// Parse every row with the reference `csv` crate (no header handling, ragged
/// rows allowed) — RFC 4180.
fn reference(input: &str) -> Vec<Vec<String>> {
    csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(input.as_bytes())
        .records()
        .map(|rec| rec.unwrap().iter().map(|s| s.to_string()).collect())
        .collect()
}

#[test]
fn matches_csv_crate_on_wellformed_input() {
    // Every case here is RFC-4180 well-formed and free of embedded newlines, so
    // our scanner and the reference must agree field-for-field.
    let corpus = [
        "a,b,c\n1,2,3\n",
        "a,b,c\n1,2,3",               // no trailing newline
        "a,,c\n",                     // empty middle field
        "a,b,\n",                     // trailing empty field
        ",,\n",                       // all empty
        "\"x,y\",z\n",                // comma inside quotes
        "\"he said \"\"hi\"\"\",x\n", // escaped quotes
        "\"\",\"\"\n",                // two empty quoted fields
        "plain,\"quoted\",mix\n",
        " leading,trailing \n", // spaces are significant
        "a,b\r\n1,2\r\n",       // CRLF
        "a,b\r\n1,2",           // CRLF, no final newline
        "single\n",
        "x\ny\nz\n", // single column, several rows
        "name,age\n\"Doe, John\",42\n\"O'Brien\",37\n",
        "utf8,café,naïve,日本\n1,2,3,4\n",
    ];
    for input in corpus {
        assert_eq!(ours(input), reference(input), "mismatch on input {input:?}");
    }
}

#[test]
fn rfc_specific_cases() {
    assert_eq!(ours("\"a,b\",c"), vec![vec!["a,b", "c"]]);
    assert_eq!(ours("\"a\"\"b\""), vec![vec!["a\"b"]]); // "" -> "
    assert_eq!(ours("a,,c"), vec![vec!["a", "", "c"]]);
    assert_eq!(ours("a,b,"), vec![vec!["a", "b", ""]]); // trailing comma -> empty
    assert_eq!(ours(" a , b "), vec![vec![" a ", " b "]]); // spaces kept
    assert_eq!(ours(""), Vec::<Vec<String>>::new()); // empty input -> no rows
    assert_eq!(ours("a,b\n"), vec![vec!["a", "b"]]); // trailing newline -> no extra row
}

#[test]
fn empty_input_matches() {
    assert_eq!(ours(""), reference(""));
}

#[test]
fn embedded_newline_in_quotes_is_not_supported() {
    // RFC 4180 / the csv crate read this as ONE record with a newline in field
    // 0. We treat every '\n' as a row break, so the quoted field is split. This
    // is the documented deviation.
    let input = "\"a\nb\",c\n";
    assert_eq!(reference(input), vec![vec!["a\nb", "c"]]); // what RFC says
    assert_ne!(ours(input), reference(input)); // we differ, by design
    // Our concrete behavior: two rows.
    assert_eq!(ours(input).len(), 2);
}

#[test]
fn write_then_read_roundtrips() {
    use csvm::field::Field;
    // Values that need quoting on output must come back identical when re-read.
    let rows: Vec<Vec<Field>> = vec![
        vec![
            Field::Str("plain"),
            Field::Str("a,b"),
            Field::Str("he\"said"),
        ],
        vec![Field::Str(""), Field::Str("  spaced  "), Field::Num(42.0)],
    ];
    let mut buf = String::new();
    for row in &rows {
        csvm::csv::write_row(&mut buf, row);
    }
    // Re-read and compare to the logical values (numbers format to text).
    let expected: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|f| f.as_str().into_owned()).collect())
        .collect();
    assert_eq!(ours(&buf), expected);
    // And the reference parser agrees with our output's field values.
    assert_eq!(reference(&buf), expected);
}
