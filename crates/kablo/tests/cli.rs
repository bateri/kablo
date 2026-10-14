//! The real binary, run as a user would.

use std::process::Command;

fn kablo() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kablo"))
}

#[test]
fn version_prints_one_line_and_succeeds() {
    let out = kablo().arg("version").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    assert!(
        stdout.starts_with(&format!("version={} ", env!("CARGO_PKG_VERSION"))),
        "{stdout:?}"
    );
    assert!(stdout.contains(" wire_min=1 wire_max=1 "), "{stdout:?}");
    assert!(out.stderr.is_empty());
}

#[test]
fn an_unknown_command_prints_the_usage_on_stderr_and_exits_64() {
    for args in [&["frobnicate"][..], &[], &["version", "now"]] {
        let out = kablo().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(64), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8(out.stderr)
                .unwrap()
                .starts_with("usage: kablo"),
            "{args:?}"
        );
    }
}
