//! The pager end to end: csvm is run on a terminal (a pty from util-linux
//! `script`) with a stand-in `less` that records what it was given. Skipped
//! where `script` is not the util-linux one.

mod common;
use common::temp_csv;

use std::path::{Path, PathBuf};
use std::process::Command;

/// A directory holding a fake `less`: it answers `--version` like less 668 and
/// otherwise saves its options, `$LESS` and its input next to itself.
struct FakeLess(PathBuf);

impl FakeLess {
    fn new(tag: &str) -> FakeLess {
        let dir = std::env::temp_dir().join(format!("csvm_pager_{}_{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let less = dir.join("less");
        std::fs::write(
            &less,
            format!(
                "#!/bin/sh\n\
                 [ \"$1\" = --version ] && {{ echo 'less 668 (fake)'; exit 0; }}\n\
                 echo \"$*\" > '{d}/args'\n\
                 echo \"$LESS\" > '{d}/env'\n\
                 cat > '{d}/input'\n",
                d = dir.display()
            ),
        )
        .unwrap();
        Command::new("chmod").arg("+x").arg(&less).status().unwrap();
        FakeLess(dir)
    }

    fn less(&self) -> PathBuf {
        self.0.join("less")
    }

    /// What the fake saved as `name`, or `None` if it never ran.
    fn saved(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.0.join(name)).ok()
    }
}

impl Drop for FakeLess {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether util-linux `script` is here to give csvm a terminal. CI must have
/// it, so the tests that need it cannot all skip there unnoticed.
fn have_script() -> bool {
    let here = Command::new("script")
        .arg("--version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("util-linux"));
    assert!(
        here || std::env::var_os("CI").is_none(),
        "the terminal tests need util-linux `script` on CI"
    );
    here
}

/// Run csvm with `args` on a terminal `rows` lines tall, with `CSVM_PAGER`
/// set to `pager`; returns what reached the terminal.
fn on_terminal(pager: &Path, rows: usize, args: &[&str]) -> String {
    on_terminal_as("xterm", pager, rows, args)
}

/// [`on_terminal`] with `TERM` set to `term`.
fn on_terminal_as(term: &str, pager: &Path, rows: usize, args: &[&str]) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let line: Vec<String> = std::iter::once(env!("CARGO_BIN_EXE_csvm"))
        .chain(args.iter().copied())
        .map(quote)
        .collect();
    let script = format!("stty rows {rows} cols 80; {}", line.join(" "));
    // `script` runs the line with $SHELL, which may not read sh syntax.
    let out = Command::new("script")
        .args(["-qec", &script, "/dev/null"])
        .env("SHELL", "/bin/sh")
        .env("CSVM_PAGER", pager)
        .env("TERM", term)
        .env_remove("LESS")
        .env_remove("LINES")
        .env_remove("COLUMNS")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("COLORTERM")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).replace('\r', "")
}

#[test]
fn a_tall_table_goes_through_less_with_its_header_pinned() {
    if !have_script() {
        return;
    }
    let data = temp_csv("name,n\nalpha,1\nbeta,22\n");
    let path = data.to_str().unwrap();
    let fake = FakeLess::new("table");
    // Three lines on a three-line terminal: less needs its prompt line too.
    on_terminal(&fake.less(), 3, &["--color", "never", "fmt", path]);
    assert_eq!(fake.saved("args").unwrap().trim(), "-S --header=1");
    assert_eq!(fake.saved("env").unwrap().trim(), "FRX");
    assert_eq!(
        fake.saved("input").unwrap(),
        "name    n\nalpha   1\nbeta   22\n"
    );
}

#[test]
fn a_short_table_gets_no_header_so_less_can_just_print_it() {
    if !have_script() {
        return;
    }
    let data = temp_csv("name,n\nalpha,1\n");
    let path = data.to_str().unwrap();
    let fake = FakeLess::new("short");
    on_terminal(&fake.less(), 24, &["--color", "never", "fmt", path]);
    assert_eq!(fake.saved("args").unwrap().trim(), "-S");
}

#[test]
fn a_table_one_line_short_of_the_window_gets_no_header() {
    if !have_script() {
        return;
    }
    // Two lines fit on a three-line terminal, prompt line and all.
    let data = temp_csv("name,n\nalpha,1\n");
    let path = data.to_str().unwrap();
    let fake = FakeLess::new("fits");
    on_terminal(&fake.less(), 3, &["--color", "never", "fmt", path]);
    assert_eq!(fake.saved("args").unwrap().trim(), "-S");
}

#[test]
fn a_dumb_terminal_is_neither_paged_nor_coloured() {
    if !have_script() {
        return;
    }
    let data = temp_csv("name,n\nalpha,1\n");
    let path = data.to_str().unwrap();
    let fake = FakeLess::new("dumb");
    let shown = on_terminal_as("dumb", &fake.less(), 24, &["fmt", path]);
    assert!(shown.contains("alpha"), "{shown:?}");
    assert!(!shown.contains('\x1b'), "{shown:?}");
    assert_eq!(fake.saved("args"), None);
}

#[test]
fn explain_is_paged_even_with_an_output_file() {
    if !have_script() {
        return;
    }
    // --explain prints the plan to stdout whatever -o says.
    let data = temp_csv("name,n\nalpha,1\n");
    let path = data.to_str().unwrap();
    let fake = FakeLess::new("explain");
    let out = fake.0.join("out.csv");
    on_terminal(
        &fake.less(),
        24,
        &["--explain", "-o", out.to_str().unwrap(), "cols name", path],
    );
    assert!(fake.saved("input").unwrap().contains("stage 1"));
}

#[test]
fn a_paged_table_keeps_its_wide_cells_whole() {
    if !have_script() {
        return;
    }
    // Wider than the 80-column terminal: less scrolls it, so nothing is cut.
    let wide = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    let data = temp_csv(&format!("name,note\nalpha,{wide}\n"));
    let path = data.to_str().unwrap();
    let fake = FakeLess::new("wide");
    on_terminal(&fake.less(), 24, &["--color", "never", "fmt", path]);
    assert!(fake.saved("input").unwrap().contains(wide));
}

#[test]
fn help_goes_through_less_without_the_table_options() {
    if !have_script() {
        return;
    }
    let fake = FakeLess::new("help");
    on_terminal(&fake.less(), 24, &["help", "fmt"]);
    assert_eq!(fake.saved("args").unwrap().trim(), "");
    assert!(fake.saved("input").unwrap().contains("fmt"));
}

#[test]
fn the_pager_stays_out_of_what_it_should_not_page() {
    if !have_script() {
        return;
    }
    let data = temp_csv("name,n\nalpha,1\n");
    let path = data.to_str().unwrap();
    let fake = FakeLess::new("skip");
    // --no-pager, plain CSV, and output to a file never start it.
    let shown = on_terminal(&fake.less(), 24, &["--no-pager", "fmt", path]);
    assert!(shown.contains("alpha"), "{shown}");
    let shown = on_terminal(&fake.less(), 24, &["cols name", path]);
    assert!(shown.contains("alpha"), "{shown}");
    let out = fake.0.join("out.txt");
    on_terminal(
        &fake.less(),
        24,
        &["-o", out.to_str().unwrap(), "fmt", path],
    );
    assert!(std::fs::read_to_string(&out).unwrap().contains("alpha"));
    assert_eq!(fake.saved("args"), None);
}
