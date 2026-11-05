//! What `kablo bateri` and its messages call the application: the bateri app (the role) and this
//! computer (the machine), never the name of one system. Linux and other systems run the same
//! application, and a text that says otherwise is wrong there.

// Test support: a failure here is a failed test.
#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};

/// The `.rs` files under `dir`.
fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a directory").flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn no_text_or_name_of_the_command_or_its_messages_calls_the_app_after_one_system() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&here.join("src/command"), &mut files);
    sources(&here.join("../kablo-bateri/src"), &mut files);
    assert!(files.len() > 5, "the sources were not found: {files:?}");
    // The word, built here so that this file does not hold it.
    let word = ["M", "ac"].concat();
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("a source");
        for (number, line) in text.lines().enumerate() {
            let mut from = 0;
            while let Some(at) = line[from..].find(&word) {
                let (start, end) = (from + at, from + at + word.len());
                let before = line[..start].chars().next_back();
                let after = line[end..].chars().next();
                let edge = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                if edge(before) && edge(after) {
                    found.push(format!("{}:{}: {line}", file.display(), number + 1));
                }
                from = end;
            }
        }
    }
    assert!(
        found.is_empty(),
        "name the role `the bateri app` and the machine `this computer`:\n{}",
        found.join("\n")
    );
}
