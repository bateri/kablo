//! `kablo bateri --next-number` and `--hex`, the two words that are not a verb: the counter that
//! gives the terminals their numbers (the bateri app's bootstrap and the agents' command both count with
//! it, and must never give one number twice), and the words the bateri app sends a server, written in hex so
//! that no shell on the way can read them.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;
mod stage;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use stage::Stage;

/// `kablo bateri ARGS` with `home` as the home, and no terminal.
fn bateri(stage: &Stage, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kablo"));
    command
        .arg("bateri")
        .args(args)
        .current_dir(&stage.home)
        .env("HOME", &stage.home)
        .env("KABLO_DIR", &stage.env.dir)
        .stdin(Stdio::null());
    command
}

fn text(output: &Output) -> (i32, String, String) {
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn number(stage: &Stage) -> u64 {
    let said = text(&bateri(stage, &["--next-number"]).output().unwrap());
    assert_eq!(said.0, 0, "{said:?}");
    said.1.trim().parse().unwrap_or_else(|_| panic!("{said:?}"))
}

fn state(stage: &Stage) -> std::path::PathBuf {
    stage.home.join(".local/share/bateri")
}

#[test]
fn the_counter_gives_each_caller_its_own_number_and_leaves_no_lock() {
    let stage = Stage::new();
    let numbers: Vec<u64> = std::thread::scope(|scope| {
        let callers: Vec<_> = (0..6)
            .map(|_| scope.spawn(|| (0..8).map(|_| number(&stage)).collect::<Vec<u64>>()))
            .collect();
        callers
            .into_iter()
            .flat_map(|caller| caller.join().unwrap())
            .collect()
    });
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, (1..=48).collect::<Vec<u64>>(), "{numbers:?}");
    assert!(!state(&stage).join("next.lock").exists());
    assert_eq!(number(&stage), 49);
    // Everything it wrote is for its owner alone.
    let mode = |path: &Path| {
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path).unwrap().permissions())
            & 0o777
    };
    assert_eq!(mode(&state(&stage)), 0o700);
    assert_eq!(mode(&state(&stage).join("next")), 0o600);
}

#[test]
fn a_lock_nobody_holds_is_taken_over_and_the_number_still_comes() {
    let stage = Stage::new();
    for _ in 0..49 {
        number(&stage);
    }
    // A holder that died between making the lock and naming itself: it is waited for a few
    // seconds, then the lock is taken.
    std::fs::create_dir(state(&stage).join("next.lock")).unwrap();
    let start = Instant::now();
    assert_eq!(number(&stage), 50);
    let waited = start.elapsed();
    assert!(waited < Duration::from_secs(30), "{waited:?}");
    assert!(!state(&stage).join("next.lock").exists());

    // One that named itself and is gone loses it at once.
    let mut gone = Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    std::fs::create_dir(state(&stage).join("next.lock")).unwrap();
    std::fs::write(state(&stage).join("next.lock/pid"), format!("{dead}\n")).unwrap();
    let start = Instant::now();
    assert_eq!(number(&stage), 51);
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn a_lock_held_by_a_live_process_is_waited_for() {
    let stage = Stage::new();
    let mut holder = Command::new("sleep").arg("60").spawn().unwrap();
    std::fs::create_dir_all(state(&stage).join("next.lock")).unwrap();
    std::fs::write(
        state(&stage).join("next.lock/pid"),
        format!("{}\n", holder.id()),
    )
    .unwrap();
    let mut asking = bateri(&stage, &["--next-number"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Longer than a holder that is gone would be waited for, and the caller is still waiting.
    std::thread::sleep(Duration::from_secs(7));
    assert!(asking.try_wait().unwrap().is_none(), "it did not wait");
    // The holder lets go, and the number comes.
    std::fs::remove_file(state(&stage).join("next.lock/pid")).unwrap();
    std::fs::remove_dir(state(&stage).join("next.lock")).unwrap();
    let done = asking.wait_with_output().unwrap();
    assert_eq!(text(&done).1, "1\n");
    holder.kill().unwrap();
    holder.wait().unwrap();
}

#[test]
fn a_counter_that_is_no_number_is_not_guessed_at_and_no_home_is_no_count() {
    let stage = Stage::new();
    std::fs::create_dir_all(state(&stage)).unwrap();
    for bad in ["abc\n", "0\n", "007\n", "", "-3\n"] {
        std::fs::write(state(&stage).join("next"), bad).unwrap();
        let said = text(&bateri(&stage, &["--next-number"]).output().unwrap());
        assert_eq!(said.0, 1, "{bad:?}: {said:?}");
        assert_eq!(
            said.1, "",
            "{bad:?}: nothing is printed for a number it did not give"
        );
        assert!(!state(&stage).join("next.lock").exists(), "{bad:?}");
        assert_eq!(
            std::fs::read_to_string(state(&stage).join("next")).unwrap(),
            bad,
            "a counter it cannot read is left as it was"
        );
    }
    // No home, no counter.
    let said = text(
        &bateri(&stage, &["--next-number"])
            .env_remove("HOME")
            .output()
            .unwrap(),
    );
    assert_eq!(said.0, 1, "{said:?}");
    assert_eq!(said.1, "", "{said:?}");
}

#[test]
fn the_counter_is_shared_with_the_terminals_a_split_starts() {
    let mut stage = Stage::new();
    stage.install_command();
    let mut seen = Vec::new();
    for round in 0..3 {
        let said = stage.run(&["split", "--", "/bin/sh", "-c", "cat"]);
        assert_eq!(said.status, 0, "round {round}: {}", said.err);
        seen.push(said.out.trim().to_owned());
        seen.push(format!("p{}", number(&stage)));
    }
    assert_eq!(seen, ["p1", "p2", "p3", "p4", "p5", "p6"]);
}

#[test]
fn the_words_of_a_hex_line_are_read_back_whole_and_anything_else_is_a_mistake() {
    let stage = Stage::new();
    let hex = |word: &str| {
        let mut out = String::from("x");
        for byte in word.as_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    };
    // `help` in hex is `help`.
    let plain = text(&bateri(&stage, &["help"]).output().unwrap());
    assert_eq!(plain.0, 0, "{plain:?}");
    let said = text(&bateri(&stage, &["--hex", &hex("help")]).output().unwrap());
    assert_eq!(said, plain);

    // A word that is not hex, an odd number of digits, a stray digit, a NUL.
    for bad in ["help", "x6", "x6g", "xzz", "x00", "x68656c7"] {
        let said = text(&bateri(&stage, &["--hex", bad]).output().unwrap());
        assert_eq!(said.0, 64, "{bad:?}: {said:?}");
        assert_eq!(said.1, "", "{bad:?}");
        assert!(said.2.starts_with("bateri: "), "{bad:?}: {said:?}");
    }

    // A word the verb reads whole: a terminal named in hex is the one it names, quotes and all.
    let said = text(
        &bateri(&stage, &["--hex", &hex("send"), &hex("p 9"), &hex("x")])
            .output()
            .unwrap(),
    );
    assert_eq!(said.0, 64, "{said:?}");
    assert!(said.2.contains("p 9"), "the word arrived whole: {said:?}");
}
