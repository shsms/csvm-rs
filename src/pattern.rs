//! File patterns in `join` and `from` sources: `*` and `?` in the file
//! name, and `{a,b}` / `{x..y}` brace groups anywhere. A pattern expands
//! to files, each with the part its varying pieces matched.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// The most files one pattern may name.
pub const MAX_FILES: usize = 10_000;

/// One file a source names, and the part its pattern matched: the texts
/// its brace groups and wildcards matched, in order, joined with `_`
/// (`None` when the source has no varying piece).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expanded {
    pub path: String,
    pub part: Option<String>,
}

/// How [`expand`] looks at files.
pub trait Fs {
    /// The names in directory `dir`, or `None` when it cannot be listed.
    fn list(&mut self, dir: &Path) -> Option<Vec<String>>;
    /// Whether `path` is a file.
    fn is_file(&mut self, path: &Path) -> bool;
}

/// The real filesystem.
pub struct RealFs;

impl Fs for RealFs {
    fn list(&mut self, dir: &Path) -> Option<Vec<String>> {
        let entries = std::fs::read_dir(dir).ok()?;
        Some(
            entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect(),
        )
    }

    fn is_file(&mut self, path: &Path) -> bool {
        path.is_file()
    }
}

/// Whether `text` may hold pattern syntax.
pub fn is_pattern(text: &str) -> bool {
    text.contains(['*', '?', '{'])
}

/// The files `written` names, with their parts. A path with no pattern
/// syntax is returned as it is, unchecked; `base` is the directory a
/// relative path is read from (`None`: the process's own).
pub fn expand(written: &str, base: Option<&Path>, fs: &mut dyn Fs) -> Result<Vec<Expanded>, Error> {
    if !is_pattern(written) {
        return Ok(vec![Expanded {
            path: written.to_string(),
            part: None,
        }]);
    }
    let mut out = Vec::new();
    // Each directory is listed once, however many brace texts read it.
    let mut listings: std::collections::HashMap<PathBuf, Option<Vec<String>>> =
        std::collections::HashMap::new();
    for (text, brace_parts) in expand_braces(written)? {
        let (dir, name) = match text.rfind('/') {
            Some(i) => (&text[..=i], &text[i + 1..]),
            None => ("", text.as_str()),
        };
        if dir.contains(['*', '?']) {
            return Err(Error::Other(format!(
                "'{written}': a pattern can match file names only, not directories"
            )));
        }
        if !name.contains(['*', '?']) {
            if !fs.is_file(&full(base, &text)) {
                return Err(Error::Other(format!(
                    "no such file: {text} (from '{written}')"
                )));
            }
            out.push(expanded(text.clone(), brace_parts));
            continue;
        }
        let at = full(base, if dir.is_empty() { "." } else { dir });
        let listed = listings
            .entry(at)
            .or_insert_with_key(|at| fs.list(at))
            .clone();
        let mut matched: Vec<(String, Vec<String>)> = listed
            .unwrap_or_default()
            .into_iter()
            .filter(|n| !n.starts_with('.') || name.starts_with('.'))
            .filter_map(|n| glob(name, &n).map(|caps| (n, caps)))
            .collect();
        if matched.is_empty() {
            return Err(Error::Other(format!("no file matches '{written}'")));
        }
        matched.sort_by(|a, b| natural_cmp(&a.0, &b.0));
        for (n, caps) in matched {
            let mut parts = brace_parts.clone();
            parts.extend(caps);
            out.push(expanded(format!("{dir}{n}"), parts));
        }
        if out.len() > MAX_FILES {
            return Err(too_many(written));
        }
    }
    let mut paths = std::collections::HashSet::new();
    for e in &out {
        if !paths.insert(e.path.as_str()) {
            return Err(Error::Other(format!("'{written}' names {} twice", e.path)));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for e in &out {
        if let Some(p) = &e.part
            && !seen.insert(p.clone())
        {
            return Err(Error::Other(format!("'{written}' gives part '{p}' twice")));
        }
    }
    Ok(out)
}

fn expanded(path: String, parts: Vec<String>) -> Expanded {
    // A piece that matched nothing (`pv*.csv` over `pv.csv`) names no part.
    let mut parts = parts;
    parts.retain(|p| !p.is_empty());
    let part = (!parts.is_empty()).then(|| parts.join("_"));
    Expanded { path, part }
}

fn full(base: Option<&Path>, path: &str) -> PathBuf {
    match base {
        Some(b) => b.join(path),
        None => PathBuf::from(path),
    }
}

fn too_many(written: &str) -> Error {
    Error::Other(format!("'{written}' names more than {MAX_FILES} files"))
}

/// Each text `written`'s brace groups expand to, with the alternative each
/// group took. A group with no top-level `,` and no `..` is literal.
fn expand_braces(written: &str) -> Result<Vec<(String, Vec<String>)>, Error> {
    let mut out: Vec<(String, Vec<String>)> = vec![(String::new(), Vec::new())];
    let mut rest = written;
    while let Some(open) = rest.find('{') {
        let Some(len) = rest[open..].find('}') else {
            break;
        };
        let inner = &rest[open + 1..open + len];
        let alternatives = alternatives(inner, written)?;
        let lead = &rest[..open];
        match alternatives {
            None => {
                for (text, _) in &mut out {
                    text.push_str(&rest[..=open + len]);
                }
            }
            Some(alts) => {
                if out.len().saturating_mul(alts.len()) > MAX_FILES {
                    return Err(too_many(written));
                }
                out = out
                    .into_iter()
                    .flat_map(|(text, parts)| {
                        alts.iter().map(move |a| {
                            let mut parts = parts.clone();
                            parts.push(a.clone());
                            (format!("{text}{lead}{a}"), parts)
                        })
                    })
                    .collect();
            }
        }
        rest = &rest[open + len + 1..];
    }
    for (text, _) in &mut out {
        text.push_str(rest);
    }
    Ok(out)
}

/// A brace group's alternatives (`a,b`, `x..y`, or a mix), or `None` when
/// it is literal text.
fn alternatives(inner: &str, written: &str) -> Result<Option<Vec<String>>, Error> {
    if !inner.contains(',') && !inner.contains("..") {
        return Ok(None);
    }
    let mut alts = Vec::new();
    for item in inner.split(',') {
        match item.split_once("..") {
            None => alts.push(item.to_string()),
            Some((x, y)) => {
                let bad = || Error::Other(format!("bad range in '{written}'"));
                let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
                if !digits(x) || !digits(y) {
                    return Err(bad());
                }
                let (a, b): (u64, u64) =
                    (x.parse().map_err(|_| bad())?, y.parse().map_err(|_| bad())?);
                if a.abs_diff(b) >= MAX_FILES as u64 {
                    return Err(too_many(written));
                }
                let padded = |s: &str| s.len() > 1 && s.starts_with('0');
                let width = if padded(x) || padded(y) {
                    x.len().max(y.len())
                } else {
                    0
                };
                let nums: Vec<u64> = if a <= b {
                    (a..=b).collect()
                } else {
                    (b..=a).rev().collect()
                };
                alts.extend(nums.into_iter().map(|n| format!("{n:0width$}")));
            }
        }
    }
    Ok(Some(alts))
}

/// What each run of `*` and `?` in `pattern` matched in `name`, or `None`
/// when it does not match.
fn glob(pattern: &str, name: &str) -> Option<Vec<String>> {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let mut caps = Vec::new();
    glob_at(&p, &n, &mut caps).then_some(caps)
}

fn glob_at(p: &[char], n: &[char], caps: &mut Vec<String>) -> bool {
    match p.first() {
        None => n.is_empty(),
        Some('*' | '?') => {
            // A run of wildcards is one part: as many characters as its
            // `?`s, or any more when it holds a `*`.
            let run = p.iter().take_while(|c| matches!(c, '*' | '?')).count();
            let least = p[..run].iter().filter(|&&c| c == '?').count();
            let most = if p[..run].contains(&'*') {
                n.len()
            } else {
                least
            };
            for take in least..=most.min(n.len()) {
                caps.push(n[..take].iter().collect());
                if glob_at(&p[run..], &n[take..], caps) {
                    return true;
                }
                caps.pop();
            }
            false
        }
        Some(c) => n.first() == Some(c) && glob_at(&p[1..], &n[1..], caps),
    }
}

/// Order names with each run of digits compared as a number, so `9`
/// comes before `10`; equal numbers put the shorter run first (`9` < `09`).
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    while !a.is_empty() && !b.is_empty() {
        let da = a.bytes().take_while(u8::is_ascii_digit).count();
        let db = b.bytes().take_while(u8::is_ascii_digit).count();
        let (ord, step_a, step_b) = if da > 0 && db > 0 {
            let na = a[..da].trim_start_matches('0');
            let nb = b[..db].trim_start_matches('0');
            let ord = na.len().cmp(&nb.len()).then(na.cmp(nb)).then(da.cmp(&db));
            (ord, da, db)
        } else {
            let (ca, cb) = (a.chars().next().unwrap(), b.chars().next().unwrap());
            (ca.cmp(&cb), ca.len_utf8(), cb.len_utf8())
        };
        if ord != Ordering::Equal {
            return ord;
        }
        (a, b) = (&a[step_a..], &b[step_b..]);
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// An `Fs` over a fixed set of files: `dir/name` paths.
    struct FakeFs(HashSet<String>);

    impl FakeFs {
        fn new(files: &[&str]) -> FakeFs {
            FakeFs(files.iter().map(|f| f.to_string()).collect())
        }
    }

    impl Fs for FakeFs {
        fn list(&mut self, dir: &Path) -> Option<Vec<String>> {
            let dir = dir.to_str()?;
            Some(
                self.0
                    .iter()
                    .filter_map(|f| {
                        let (d, n) = f.rsplit_once('/').unwrap_or(("", f));
                        (d == dir.trim_end_matches('/') || (d.is_empty() && dir == "."))
                            .then(|| n.to_string())
                    })
                    .collect(),
            )
        }
        fn is_file(&mut self, path: &Path) -> bool {
            self.0.contains(path.to_str().unwrap())
        }
    }

    fn files(written: &str, fs: &[&str]) -> Result<Vec<(String, Option<String>)>, String> {
        expand(written, None, &mut FakeFs::new(fs))
            .map(|v| v.into_iter().map(|e| (e.path, e.part)).collect())
            .map_err(|e| e.to_string())
    }

    fn part(p: &str, s: &str) -> (String, Option<String>) {
        (p.to_string(), Some(s.to_string()))
    }

    #[test]
    fn a_plain_path_is_itself_unchecked() {
        assert_eq!(files("a.csv", &[]).unwrap(), vec![("a.csv".into(), None)]);
    }

    #[test]
    fn braces_list_in_the_order_written() {
        let fs = ["pv.2.csv", "pv.1.csv"];
        assert_eq!(
            files("pv.{2,1}.csv", &fs).unwrap(),
            vec![part("pv.2.csv", "2"), part("pv.1.csv", "1")]
        );
    }

    #[test]
    fn ranges_count_up_down_and_pad() {
        let fs = ["a.08", "a.09", "a.10", "a.11", "b.3", "b.2", "b.1"];
        let got = files("a.{08..11}", &fs).unwrap();
        assert_eq!(
            got.iter().map(|g| g.1.clone().unwrap()).collect::<Vec<_>>(),
            ["08", "09", "10", "11"]
        );
        let got = files("b.{3..1}", &fs).unwrap();
        assert_eq!(
            got.iter().map(|g| g.1.clone().unwrap()).collect::<Vec<_>>(),
            ["3", "2", "1"]
        );
    }

    #[test]
    fn ranges_mix_with_alternatives() {
        let fs = ["x.1", "x.2", "x.7"];
        let got = files("x.{1..2,7}", &fs).unwrap();
        assert_eq!(
            got,
            vec![part("x.1", "1"), part("x.2", "2"), part("x.7", "7")]
        );
    }

    #[test]
    fn several_groups_give_their_product_leftmost_slowest() {
        let fs = ["a1", "a2", "b1", "b2"];
        let got = files("{a,b}{1,2}", &fs).unwrap();
        assert_eq!(
            got,
            vec![
                part("a1", "a_1"),
                part("a2", "a_2"),
                part("b1", "b_1"),
                part("b2", "b_2")
            ]
        );
    }

    #[test]
    fn a_brace_group_without_comma_or_range_is_literal() {
        assert_eq!(
            files("{a}.csv", &["{a}.csv"]).unwrap(),
            vec![("{a}.csv".into(), None)]
        );
        assert_eq!(
            files("x{}.csv", &["x{}.csv"]).unwrap(),
            vec![("x{}.csv".into(), None)]
        );
    }

    #[test]
    fn a_brace_file_must_exist() {
        let e = files("pv.{1,2}.csv", &["pv.1.csv"]).unwrap_err();
        assert_eq!(e, "no such file: pv.2.csv (from 'pv.{1,2}.csv')");
    }

    #[test]
    fn a_bad_range_is_an_error() {
        let e = files("a.{x..3}", &[]).unwrap_err();
        assert_eq!(e, "bad range in 'a.{x..3}'");
    }

    #[test]
    fn a_huge_range_is_refused_before_it_is_built() {
        let e = files("a.{1..1000000}", &[]).unwrap_err();
        assert_eq!(e, "'a.{1..1000000}' names more than 10000 files");
    }

    #[test]
    fn star_and_question_match_in_natural_order() {
        let fs = ["d/pv.10.csv", "d/pv.9.csv", "d/pv.x.txt", "d/.pv.1.csv"];
        assert_eq!(
            files("d/pv.*.csv", &fs).unwrap(),
            vec![part("d/pv.9.csv", "9"), part("d/pv.10.csv", "10")]
        );
        assert_eq!(
            files("d/pv.?.csv", &fs).unwrap(),
            vec![part("d/pv.9.csv", "9")]
        );
    }

    #[test]
    fn braces_and_stars_join_their_parts() {
        let fs = ["s1.a.csv", "s1.b.csv", "s2.a.csv"];
        assert_eq!(
            files("s{1,2}.*.csv", &fs).unwrap(),
            vec![
                part("s1.a.csv", "1_a"),
                part("s1.b.csv", "1_b"),
                part("s2.a.csv", "2_a")
            ]
        );
    }

    #[test]
    fn adjacent_wildcards_are_one_part() {
        let fs = ["p.123.csv", "q.ab1.csv"];
        assert_eq!(
            files("p.???.csv", &fs).unwrap(),
            vec![part("p.123.csv", "123")]
        );
        assert_eq!(
            files("q.*?.csv", &fs).unwrap(),
            vec![part("q.ab1.csv", "ab1")]
        );
        assert_eq!(
            files("q.?b?.csv", &fs).unwrap(),
            vec![part("q.ab1.csv", "a_1")]
        );
    }

    #[test]
    fn an_empty_match_is_no_part() {
        let fs = ["pv.csv", "pv1.csv"];
        assert_eq!(
            files("pv*.csv", &fs).unwrap(),
            vec![("pv.csv".into(), None), part("pv1.csv", "1")]
        );
    }

    #[test]
    fn a_file_named_twice_is_an_error() {
        assert_eq!(
            files("pv{,}.csv", &["pv.csv"]).unwrap_err(),
            "'pv{,}.csv' names pv.csv twice"
        );
    }

    #[test]
    fn a_star_skips_hidden_files() {
        let fs = ["d/a.csv", "d/.b.csv"];
        assert_eq!(files("d/*.csv", &fs).unwrap(), vec![part("d/a.csv", "a")]);
    }

    #[test]
    fn a_star_that_matches_nothing_is_an_error() {
        assert_eq!(
            files("q.*.csv", &["a.csv"]).unwrap_err(),
            "no file matches 'q.*.csv'"
        );
    }

    #[test]
    fn a_wildcard_in_a_directory_is_an_error() {
        assert_eq!(
            files("*/a.csv", &["x/a.csv"]).unwrap_err(),
            "'*/a.csv': a pattern can match file names only, not directories"
        );
    }

    #[test]
    fn the_same_part_twice_is_an_error() {
        // Two files whose varying pieces differ but join to the same part.
        assert_eq!(
            files("x*y*", &["xay", "xya"]).unwrap_err(),
            "'x*y*' gives part 'a' twice"
        );
    }

    #[test]
    fn natural_order_compares_digit_runs_as_numbers() {
        let mut v = vec!["a10", "a9", "a09b", "b1", "a1"];
        v.sort_by(|x, y| natural_cmp(x, y));
        assert_eq!(v, ["a1", "a9", "a09b", "a10", "b1"]);
    }
}
