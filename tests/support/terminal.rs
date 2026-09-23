//! Running csvm on a terminal, for the integration tests of what it does
//! only there.

use std::process::Command;

/// Whether util-linux `script` is here to give csvm a terminal. Tests that
/// need one return early without it, except on CI, which must have it so
/// they cannot all skip there unnoticed.
pub fn have_script() -> bool {
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

/// `script` set up to run `shell` on a terminal (`TERM=xterm`), for the
/// caller to add its own environment to and run.
pub fn script(shell: &str) -> Command {
    let mut command = Command::new("script");
    // `script` runs the line with $SHELL, which may not read sh syntax.
    command
        .args(["-qec", shell, "/dev/null"])
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        // Colour variables in the caller's environment would colour the output.
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("COLORTERM")
        .env_remove("COLORFGBG");
    command
}
