//! The commands that drive a session without attaching to it (`send`,
//! `send-key`, `capture`, `wait`, `label`, `end`, `new --wait`) and `here`, run
//! as the real binary against real sessions.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Env, PATIENCE};
use kablo_proto::v1::{Capture, State};

const KABLO: &str = env!("CARGO_BIN_EXE_kablo");

/// What a command did.
#[derive(Debug)]
struct Ran {
    code: i32,
    out: String,
    err: String,
}

/// Runs `command` and collects what it printed, failing the test if it has not
/// ended in `patience`: a command that hangs is the failure these tests are for.
fn run_within(mut command: Command, patience: Duration) -> Ran {
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > patience {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{command:?} had not ended after {patience:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (mut out, mut err) = (String::new(), String::new());
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    Ran {
        code: status.code().unwrap_or(-1),
        out,
        err,
    }
}

fn kablo(env: &Env, args: &[&str]) -> Ran {
    let mut command = env.kablo();
    command.args(args);
    run_within(command, PATIENCE)
}

/// Runs `kablo capture <target> --trim` until what it prints says `done`.
fn capture_until(env: &Env, target: &str, done: impl Fn(&str) -> bool) -> String {
    let start = Instant::now();
    loop {
        let ran = kablo(env, &["capture", target, "--trim"]);
        assert_eq!(ran.code, 0, "{ran:?}");
        if done(&ran.out) {
            return ran.out;
        }
        assert!(
            start.elapsed() < PATIENCE,
            "the screen never said it; it says:\n{}",
            ran.out
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|l| l == line)
}

#[test]
fn text_and_keys_typed_from_outside_are_read_back_from_outside() {
    let mut env = Env::new();
    let id = env.start("typing", &["--label", "Build", "--", "/bin/sh"]);

    // A session is reached by its id and by its label, whatever the case.
    for (target, word) in [(id.as_str(), "hi"), ("build", "there")] {
        let ran = kablo(&env, &["send", target, &format!("echo {word}")]);
        assert_eq!(ran.code, 0, "{ran:?}");
        let ran = kablo(&env, &["send-key", target, "enter"]);
        assert_eq!(ran.code, 0, "{ran:?}");
        capture_until(&env, target, |text| has_line(text, word));
    }

    // A key with a modifier: Ctrl-C ends the program that is running.
    assert_eq!(kablo(&env, &["send", &id, "sleep 60"]).code, 0);
    assert_eq!(kablo(&env, &["send-key", &id, "enter"]).code, 0);
    assert_eq!(kablo(&env, &["send-key", &id, "ctrl+c"]).code, 0);
    assert_eq!(kablo(&env, &["send", &id, "echo after"]).code, 0);
    assert_eq!(kablo(&env, &["send-key", &id, "enter"]).code, 0);
    capture_until(&env, &id, |text| has_line(text, "after"));

    // Text from the input, to a session named.
    let mut child = env
        .kablo()
        .args(["send", &id, "-"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"echo piped\n")
        .unwrap();
    assert!(child.wait().unwrap().success());
    capture_until(&env, &id, |text| has_line(text, "piped"));

    // The whole screen has the blank rows under the text; trimmed has not.
    let whole = kablo(&env, &["capture", &id]);
    assert_eq!(whole.code, 0);
    let trimmed = kablo(&env, &["capture", &id, "--trim"]);
    assert!(whole.out.ends_with("\n\n"), "{:?}", whole.out);
    assert!(
        trimmed.out.ends_with('\n') && !trimmed.out.ends_with("\n\n"),
        "{:?}",
        trimmed.out
    );
    assert_eq!(whole.out.lines().count(), 24);

    // The screen as the bytes that draw it.
    let vt = kablo(&env, &["capture", &id, "--vt"]);
    assert_eq!(vt.code, 0);
    assert!(vt.out.contains("piped"), "{:?}", vt.out);

    // None of this attached: nothing counts as a client.
    assert_eq!(env.connect(&id).status().clients, 0);
}

#[test]
fn a_program_that_has_ended_cannot_be_typed_to_and_can_still_be_read() {
    let mut env = Env::new();
    let id = env.start("ended", &["--", "/bin/sh", "-c", "echo last words; exit 0"]);
    // `wait` is how the end is seen; the session stays for the ones who have not.
    let waited = kablo(&env, &["wait", &id]);
    assert_eq!(waited.code, 0, "{waited:?}");

    let said = kablo(&env, &["send", &id, "x"]);
    assert_eq!(said.code, 1, "{said:?}");
    assert!(said.err.contains("ended"), "{said:?}");
    // The output of its last moments is on the screen, and a second wait
    // is answered at once.
    let screen = kablo(&env, &["capture", &id, "--trim"]);
    assert!(has_line(&screen.out, "last words"), "{screen:?}");
    assert_eq!(kablo(&env, &["wait", &id]).code, 0);
}

#[test]
fn wait_ends_with_the_programs_status_and_new_wait_takes_the_session_with_it() {
    let mut env = Env::new();
    let id = env.name("new-wait");
    env.track(&id);
    let ran = kablo(
        &env,
        &[
            "new", "--id", &id, "--wait", "--", "/bin/sh", "-c", "exit 3",
        ],
    );
    assert_eq!(ran.code, 3, "{ran:?}");
    assert_eq!(ran.out.trim(), id);
    // kablo's own failure is 255 when the status is the program's, and 1 when not.
    let taken = env.start("taken", &["--", "/bin/sh"]);
    assert_eq!(
        kablo(&env, &["new", "--id", &taken, "--wait", "--", "/bin/sh"]).code,
        255
    );
    assert_eq!(
        kablo(&env, &["new", "--id", &taken, "--", "/bin/sh"]).code,
        1
    );
    // It was heard, so it is gone: not kept for a day for a client that has
    // heard it.
    env.wait_gone(&id);
    assert!(!kablo(&env, &["list"]).out.contains(&id));

    // A signal is 128 plus its number, as `kablo connect` ends.
    let signalled = env.name("new-signal");
    env.track(&signalled);
    let ran = kablo(
        &env,
        &[
            "new",
            "--id",
            &signalled,
            "--wait",
            "--",
            "/bin/sh",
            "-c",
            "kill -9 $$",
        ],
    );
    assert_eq!(ran.code, 137, "{ran:?}");
}

#[test]
fn wait_gives_up_when_it_is_told_to_and_otherwise_goes_on_to_the_end() {
    let mut env = Env::new();
    let id = env.start(
        "wait",
        &["--", "/bin/sh", "-c", "read go; echo bye; exit 5"],
    );

    let start = Instant::now();
    let ran = kablo(&env, &["wait", &id, "--for", "1s"]);
    assert_eq!(ran.code, 124, "{ran:?}");
    assert!(start.elapsed() >= Duration::from_secs(1));
    assert!(ran.err.contains("had not ended"), "{ran:?}");

    // Seeing the end does not close the session: the output is still there.
    let mut waiting = env.kablo();
    waiting.args(["wait", &id]);
    let waiter = std::thread::spawn(move || run_within(waiting, PATIENCE));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(kablo(&env, &["send", &id, "go"]).code, 0);
    assert_eq!(kablo(&env, &["send-key", &id, "enter"]).code, 0);
    let ran = waiter.join().unwrap();
    assert_eq!(ran.code, 5, "{ran:?}");
    let screen = kablo(&env, &["capture", &id, "--trim"]);
    assert!(has_line(&screen.out, "bye"), "{screen:?}");
    let listed = kablo(&env, &["list"]);
    assert!(listed.out.contains("ended (exit 5)"), "{listed:?}");
}

#[test]
fn a_wait_goes_on_over_an_upgrade_and_gets_the_end() {
    let mut env = Env::new();
    let id = env.start("wait-up", &["--", "/bin/sh", "-c", "read go; exit 7"]);
    let mut waiting = env.kablo();
    waiting.args(["wait", &id]);
    let waiter = std::thread::spawn(move || run_within(waiting, PATIENCE));
    // Long enough for it to be waiting on the relay that is replaced; if it is
    // not, it waits on the new one, and the end comes either way.
    std::thread::sleep(Duration::from_millis(800));
    let newer = env.newer_kablo("wait");
    let upgraded = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&newer)
        .output()
        .unwrap();
    assert!(
        upgraded.status.success(),
        "{}",
        String::from_utf8_lossy(&upgraded.stderr)
    );
    // Typed to the new relay.
    assert_eq!(kablo(&env, &["send", &id, "go"]).code, 0);
    assert_eq!(kablo(&env, &["send-key", &id, "enter"]).code, 0);
    let ran = waiter.join().unwrap();
    assert_eq!(ran.code, 7, "{ran:?}");
}

#[test]
fn a_relay_from_before_these_commands_is_refused_at_once_and_upgraded_it_is_not() {
    let mut env = Env::new();
    // A relay that says it is the release before: what a session started by
    // kablo 0.1.0 says.
    let id = env.name("old");
    env.track(&id);
    let made = env
        .kablo()
        .env("KABLO_TEST_VERSION", "0.1.0")
        .args(["new", "--id", &id, "--", "/bin/sh"])
        .output()
        .unwrap();
    assert!(made.status.success());
    let listed = kablo(&env, &["list", "--json"]);
    assert!(listed.out.contains("\"version\":\"0.1.0\""), "{listed:?}");

    let patience = Duration::from_secs(10);
    for (args, status) in [
        (vec!["send", &id, "x"], 1),
        (vec!["send-key", &id, "enter"], 1),
        (vec!["capture", &id], 1),
        (vec!["label", &id, "nope"], 1),
        (vec!["wait", &id], 255),
    ] {
        let mut command = env.kablo();
        command.args(&args);
        let ran = run_within(command, patience);
        assert_eq!(ran.code, status, "{args:?}: {ran:?}");
        assert!(
            ran.err.contains("kablo upgrade") && ran.err.contains("0.1.0"),
            "{args:?}: {ran:?}"
        );
    }
    // Nothing was written: no label, nothing typed.
    assert!(!env.dir.join(format!("{id}.label")).exists());

    // What the message says to do makes it work.
    let renewed = env.script(
        "renewed",
        &format!("#!/bin/sh\nunset KABLO_TEST_VERSION\nexec '{KABLO}' \"$@\"\n"),
    );
    let upgraded = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&renewed)
        .output()
        .unwrap();
    assert!(
        upgraded.status.success(),
        "{}",
        String::from_utf8_lossy(&upgraded.stderr)
    );
    let ran = kablo(&env, &["send", &id, "echo again"]);
    assert_eq!(ran.code, 0, "{ran:?}");
}

#[test]
fn a_label_is_made_set_listed_and_cleared() {
    let mut env = Env::new();
    let id = env.start("labels", &["--label", "first one", "--", "/bin/sh"]);
    let table = kablo(&env, &["list"]);
    assert!(table.out.lines().next().unwrap().ends_with("LABEL"));
    assert!(table.out.contains("first one"), "{table:?}");
    // The first word of two is the session; set from outside, read by `list`.
    assert_eq!(kablo(&env, &["label", &id, "Second"]).code, 0);
    let json = kablo(&env, &["list", "--json"]);
    assert!(json.out.contains("\"label\":\"Second\""), "{json:?}");
    // An empty text clears it: null in the JSON.
    assert_eq!(kablo(&env, &["label", &id, ""]).code, 0);
    assert!(
        kablo(&env, &["list", "--json"])
            .out
            .contains("\"label\":null")
    );

    // A label that another session is called already is allowed and said: a name
    // that fits both does nothing.
    let other = env.start("labels-other", &["--", "/bin/sh"]);
    let ran = kablo(&env, &["label", &id, &other]);
    assert_eq!(ran.code, 0, "{ran:?}");
    assert!(
        ran.err.contains("note:") && ran.err.contains(&other),
        "{ran:?}"
    );
    let ran = kablo(&env, &["label", &id, "unique"]);
    assert!(ran.err.is_empty(), "{ran:?}");

    // At most 64 bytes, said before anything is done.
    let long = "x".repeat(65);
    assert_eq!(kablo(&env, &["label", &id, &long]).code, 64);
    assert_eq!(kablo(&env, &["new", "--label", &long]).code, 64);
}

#[test]
fn a_name_that_fits_two_sessions_types_to_neither() {
    let mut env = Env::new();
    let second = env.name("second");
    // The first session's label is the second one's id.
    let first = env.start("first", &["--label", &second, "--", "/bin/sh"]);
    let started = env.start("second", &["--", "/bin/sh"]);
    assert_eq!(started, second);

    let ran = kablo(&env, &["send", &second, "zzz"]);
    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(
        ran.err.contains(&first) && ran.err.contains(&second),
        "{ran:?}"
    );
    for id in [&first, &second] {
        let mut shell = env.connect(id);
        let (_, _, text) = shell.read_text(Capture::default());
        assert!(!text.contains("zzz"), "{id}: {text:?}");
    }
    // The name of a session that is not there.
    let ran = kablo(&env, &["capture", "nobody-called-this"]);
    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("no session"), "{ran:?}");
}

#[test]
fn with_no_session_named_a_command_means_the_terminal_it_runs_in() {
    let mut env = Env::new();
    // Outside any session, with nothing named: a mistake in how it was called.
    for args in [
        &["capture"][..],
        &["send", "text"],
        &["send-key", "enter"],
        &["label"],
    ] {
        let ran = kablo(&env, args);
        assert_eq!(ran.code, 64, "{args:?}: {ran:?}");
    }
    // A wait cannot be for oneself.
    assert_eq!(kablo(&env, &["wait"]).code, 64);
    // Nor does a send with nothing to type and nowhere to type it succeed.
    assert_eq!(kablo(&env, &["send"]).code, 64);
    assert_eq!(kablo(&env, &["send", "nobody-called-this", ""]).code, 1);

    // Inside one: the session reads its own screen, labels itself and shows
    // the label (one word is the text, none is to show it).
    let out = env.dir.join("own.txt");
    let said = env.dir.join("label.txt");
    let script = format!(
        "echo marker-line; '{KABLO}' label inside; '{KABLO}' label > '{}'; \
         '{KABLO}' capture --trim > '{}'; sleep 60",
        said.display(),
        out.display()
    );
    let id = env.start("own", &["--", "/bin/sh", "-c", &script]);
    let start = Instant::now();
    let text = loop {
        let text = std::fs::read_to_string(&out).unwrap_or_default();
        if has_line(&text, "marker-line") {
            break text;
        }
        assert!(start.elapsed() < PATIENCE, "it read {text:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(has_line(&text, "marker-line"), "{text:?}");
    assert_eq!(std::fs::read_to_string(&said).unwrap(), "inside\n");
    let json = kablo(&env, &["list", "--json"]);
    assert!(
        json.out.contains(&format!("\"id\":\"{id}\"")) && json.out.contains("\"label\":\"inside\""),
        "{json:?}"
    );
}

// --- ending a session, and saying which one this is ------------------------------

#[test]
fn end_hangs_up_on_the_program_and_leaves_the_session_to_be_read() {
    let mut env = Env::new();
    let id = env.start(
        "end",
        &[
            "--label",
            "Doomed",
            "--",
            "/bin/sh",
            "-c",
            "echo last words; exec sleep 600",
        ],
    );
    capture_until(&env, &id, |text| has_line(text, "last words"));

    // By its label, whatever the case; it prints nothing and ends with 0 once
    // the program has gone.
    let ran = kablo(&env, &["end", "doomed"]);
    assert_eq!((ran.code, ran.out.as_str(), ran.err.as_str()), (0, "", ""));

    // The end was seen through a wait, which is not delivery: nobody was told
    // and the session is kept as it is, to be read and waited for. The program
    // was hung up on (128 and SIGHUP's number).
    assert_eq!(env.connect(&id).status().state, State::ENDED);
    assert_eq!(kablo(&env, &["wait", &id]).code, 129);
    let screen = kablo(&env, &["capture", &id, "--trim"]);
    assert!(has_line(&screen.out, "last words"), "{screen:?}");
}

#[test]
fn a_program_that_ignores_the_hang_up_is_ended_by_force() {
    let mut env = Env::new();
    // The signal's disposition is the program's: `sleep` inherits the ignoring.
    let id = env.start(
        "stubborn",
        &[
            "--",
            "/bin/sh",
            "-c",
            "trap '' HUP; echo ready; exec sleep 600",
        ],
    );
    capture_until(&env, &id, |text| has_line(text, "ready"));

    let ran = kablo(&env, &["end", &id]);
    assert_eq!(ran.code, 1, "{ran:?}");
    assert_eq!(ran.out, "");
    assert!(
        ran.err.contains(&format!("kablo end --force {id}")),
        "{ran:?}"
    );
    assert_eq!(env.connect(&id).status().state, State::RUNNING);

    let ran = kablo(&env, &["end", "--force", &id]);
    assert_eq!((ran.code, ran.err.as_str()), (0, ""), "{ran:?}");
    assert_eq!(env.connect(&id).status().state, State::ENDED);
    // Killed (128 and SIGKILL's number).
    assert_eq!(kablo(&env, &["wait", &id]).code, 137);
}

#[test]
fn ending_a_session_that_has_ended_is_nothing_to_do() {
    let mut env = Env::new();
    let id = env.start("over", &["--", "/bin/sh", "-c", "echo done; exit 3"]);
    assert_eq!(kablo(&env, &["wait", &id]).code, 3);
    for args in [&["end", id.as_str()][..], &["end", "--force", id.as_str()]] {
        let ran = kablo(&env, args);
        assert_eq!((ran.code, ran.out.as_str(), ran.err.as_str()), (0, "", ""));
    }
    // Still there to be read.
    let screen = kablo(&env, &["capture", &id, "--trim"]);
    assert!(has_line(&screen.out, "done"), "{screen:?}");
}

#[test]
fn end_always_names_its_session() {
    let env = Env::new();
    // Not guessed from where it runs, which is the usage mistake.
    for args in [&["end"][..], &["end", "--force"], &["end", "a", "b"]] {
        let ran = kablo(&env, args);
        assert_eq!(ran.code, 64, "{args:?}: {ran:?}");
    }
    let ran = kablo(&env, &["end", "nobody-called-this"]);
    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("no session"), "{ran:?}");
}

#[test]
fn here_says_the_session_it_runs_in_from_the_system_and_none_from_outside() {
    let mut env = Env::new();
    // Outside every session it is no usage mistake: the answer is "none".
    let ran = kablo(&env, &["here"]);
    assert_eq!((ran.code, ran.out.as_str()), (1, ""), "{ran:?}");
    assert!(
        ran.err.contains("not run in a terminal of a session"),
        "{ran:?}"
    );
    assert_eq!(kablo(&env, &["here", "x"]).code, 64);

    // Inside one: its id. Another session beside it, labelled `here`, changes
    // nothing: the answer is the system's, not a lookup of a name.
    let said = env.dir.join("here.txt");
    let script = format!("'{KABLO}' here > '{}'; sleep 60", said.display());
    let other = env.start("other", &["--label", "here", "--", "/bin/sh"]);
    let id = env.start("inside", &["--", "/bin/sh", "-c", &script]);
    let start = Instant::now();
    let text = loop {
        let text = std::fs::read_to_string(&said).unwrap_or_default();
        if text.ends_with('\n') {
            break text;
        }
        assert!(start.elapsed() < PATIENCE, "it said {text:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(text, format!("{id}\n"));
    assert_ne!(id, other);
}
