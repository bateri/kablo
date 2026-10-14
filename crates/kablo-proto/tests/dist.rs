//! What `make dist` builds is what clients are told to ask for: the targets
//! and the file names in the Makefile are the ones `kablo_proto::install`
//! answers with. They are written twice (a Makefile cannot call Rust), so this
//! is the one place that notices when they stop agreeing.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use kablo_proto::install::{self, Target};

const MAKEFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Makefile");

/// The words a Makefile variable is set to, from its single `NAME = words` line.
fn variable<'a>(makefile: &'a str, name: &str) -> &'a str {
    let mut lines = makefile
        .lines()
        .filter_map(|line| line.strip_prefix(name))
        .filter_map(|rest| rest.trim_start().strip_prefix('='));
    let value = lines.next().unwrap_or_else(|| panic!("{name} is not set"));
    assert!(lines.next().is_none(), "{name} is set twice");
    value.trim()
}

#[test]
fn dist_builds_the_targets_clients_ask_for_in_the_order_they_are_listed() {
    let makefile = std::fs::read_to_string(MAKEFILE).unwrap();
    let words = |name| -> Vec<String> {
        variable(&makefile, name)
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    let mut built = words("DIST_LINUX_TARGETS");
    built.extend(words("DIST_MACOS_TARGETS"));
    let known: Vec<&str> = Target::ALL.iter().map(|t| t.triple()).collect();
    assert_eq!(built, known);
}

#[test]
fn dist_names_a_binary_as_the_release_does() {
    let makefile = std::fs::read_to_string(MAKEFILE).unwrap();
    let pattern = variable(&makefile, "DIST_NAME");
    assert_eq!(pattern.matches('%').count(), 1, "{pattern}");
    let version = "0.1.0";
    let pattern = pattern.replace("$(DIST_VERSION)", version);
    for target in Target::ALL {
        assert_eq!(
            pattern.replace('%', target.triple()),
            install::artifact_name(version, target).unwrap()
        );
    }
    assert_eq!(variable(&makefile, "DIST_SUMS"), install::CHECKSUMS);
}
