//! `kablo bateri split`, `tab` and `window` on this machine, run for real: the real `kablo` starts
//! real terminals and asks a test in the place of the bateri app where their panes go (`stage`). What it
//! asks is read back with the window's own reader (`kablo-bateri`).

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;
mod stage;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kablo_bateri::message::{Home, Place, Request, WindowHome, hex_word};
use kablo_bateri::{Anchor, Id, IdKind, Side, Terminal};
use stage::{Pane, Run, Stage, parse_told};

const PLACED: &str = "bateri 2\nok placed\nlanded as_asked\nnext-to p40\nsqueezed no\nhidden no\n";
const VERSION: &str = "0.19.0";

fn place(terminal: u64, anchor: u64) -> Place {
    Place {
        terminal: Terminal(terminal),
        anchor: Some(Anchor::of(Terminal(anchor))),
        side: None,
        ratio: None,
        keep_open: false,
        focus: false,
        home: Home::default(),
        fresh: None,
        shell: None,
        server: None,
    }
}

fn encoded(place: Place) -> String {
    Request::Place(place).encode().unwrap()
}

/// What a terminal that was started with the recorder was started with.
struct Recorded {
    args: Vec<String>,
    cwd: String,
    umask: String,
    env: HashMap<String, String>,
}

/// A program that says how it was started — its words, directory, umask and environment — into
/// the files named by its first word, and holds the terminal open. Returns the program.
fn recorder(stage: &Stage) -> PathBuf {
    stage.env.script(
        "recorder.sh",
        "#!/bin/sh\nout=$1\nshift\n{ for a in \"$@\"; do printf '%s\\n' \"$a\"; done; } > \"$out.args\"\npwd > \"$out.cwd\"\numask > \"$out.umask\"\nenv > \"$out.env\"\n: > \"$out.done\"\nexec cat\n",
    )
}

/// Waits for what the recorder wrote under `out`.
fn recorded(out: &Path) -> Recorded {
    let done = PathBuf::from(format!("{}.done", out.display()));
    let start = Instant::now();
    while !done.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the program did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let read =
        |suffix: &str| std::fs::read_to_string(format!("{}.{suffix}", out.display())).unwrap();
    Recorded {
        args: read("args").lines().map(str::to_owned).collect(),
        cwd: read("cwd").trim_end().to_owned(),
        umask: read("umask").trim_end().to_owned(),
        env: read("env")
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
    }
}

/// A stage with the command installed, as the bootstrap writes it, in a pane of the app on this
/// machine: the app makes the pane of a terminal with no program. A test of a pane of another
/// machine's app, or of none, says so with [`Stage::in_pane`].
fn stage() -> Stage {
    let mut stage = Stage::new();
    stage.install_command();
    stage.in_pane(Pane::Local);
    stage
}

#[test]
fn a_split_starts_the_terminal_and_tells_the_app_where_the_pane_goes() {
    let mut stage = stage();
    stage.app_says(PLACED);
    let program = recorder(&stage);
    let out = stage.env.dir.join("one");
    let program = program.display().to_string();
    let out_name = out.display().to_string();
    let said = stage.run_in(
        "p40",
        &[
            "split", "--below", "--ratio", "30", "--focus", "--", &program, &out_name, "npm",
            "run", "dev",
        ],
    );
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "p1\n");
    assert_eq!(said.err, "", "a pane that landed as asked is silent");
    // The message is the one the bateri app reads, and the one it writes.
    let wanted = Place {
        side: Some(Side::Below),
        ratio: Some(30),
        focus: true,
        ..place(1, 40)
    };
    assert_eq!(stage.told, [encoded(wanted.clone())]);
    assert_eq!(parse_told(&stage.told[0]), Request::Place(wanted));
    // The terminal runs the program with its words as they were written, in the caller's directory.
    let seen = recorded(&out);
    assert_eq!(seen.args, ["npm", "run", "dev"]);
    assert_eq!(seen.cwd, stage.home.display().to_string());
    assert!(stage.running().contains(&"p1".to_owned()));
    // Who opened it, for `close`.
    assert_eq!(
        stage.record("p1").as_deref(),
        Some("opener p40\nkeep-open no\n")
    );
    // Nothing waits: the bateri app took it.
    assert_eq!(stage.note("p1"), None);
    // The record is for its owner alone.
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(stage.state().join("terminals/p1"))
            .unwrap()
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_caller_with_the_command_on_its_path_hands_that_path_on_as_it_is() {
    let mut stage = stage();
    stage.app_says(PLACED);
    let program = recorder(&stage).display().to_string();
    let bin = stage.state().join("shell/bin").display().to_string();
    // The command is on `PATH` already: it is not put there twice.
    let path = format!("/usr/bin:{bin}:/bin");
    let out = stage.env.dir.join("on");
    let said = stage.go(&Run {
        here: Some("p40"),
        args: &["split", "--", &program, &out.display().to_string()],
        input: None,
        turns: &[],
        vars: &[("PATH", &path)],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(recorded(&out).env["PATH"], path);
    // Not on it: the terminal gets it, first, since a program there calls `bateri` all the same.
    let out = stage.env.dir.join("off");
    let said = stage.go(&Run {
        here: Some("p40"),
        args: &["split", "--", &program, &out.display().to_string()],
        input: None,
        turns: &[],
        vars: &[("PATH", "/usr/bin:/bin")],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(recorded(&out).env["PATH"], format!("{bin}:/usr/bin:/bin"));
}

#[test]
fn a_split_with_no_program_asks_for_a_pane_of_the_apps_own_and_starts_nothing() {
    let mut stage = stage();
    stage.app_says(PLACED);
    let said = stage.run_in("p40", &["split", "--right", "--cwd", "~"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "p1\n");
    // No terminal was started: the bateri app opens the pane under the number.
    assert!(stage.running().iter().all(|id| id == "p40"));
    let wanted = Place {
        side: Some(Side::Right),
        fresh: Some(stage.home.display().to_string()),
        ..place(1, 40)
    };
    assert_eq!(stage.told, [encoded(wanted)]);
    // The record of who opened it is written all the same.
    assert!(stage.state().join("terminals/p1").is_file());

    // `--wait` has nothing to wait for; no terminal to go beside is a mistake of its own; both
    // before anything is numbered or asked.
    let said = stage.run_in("p40", &["split", "--wait"]);
    assert_eq!(said.status, 64, "{}", said.err);
    let said = stage.run(&["split"]);
    assert_eq!(said.status, 1, "{}", said.err);
    assert!(said.err.contains("beside a pane"), "{}", said.err);
    assert_eq!(stage.told.len(), 1);

    // The bateri app refuses: the number is spent, and nothing is left behind.
    stage.app_says("bateri 2\nerror failed no room\n");
    let said = stage.run_in("p40", &["tab"]);
    assert_eq!(said.status, 1, "{}", said.err);
    assert!(!stage.state().join("terminals/p2").exists());
}

#[test]
fn a_program_kept_open_is_followed_by_the_login_shell_in_its_terminal() {
    let mut stage = stage();
    stage.app_says(PLACED);
    // The login shell that follows the program says it was started, with the words of one.
    let shell = stage.env.script(
        "login.sh",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}/login.args'\nexec cat\n",
            stage.home.display()
        ),
    );
    let said = stage.go(&Run {
        here: Some("p40"),
        args: &["split", "--keep-open", "--", "true"],
        input: None,
        turns: &[],
        vars: &[("SHELL", &shell.display().to_string())],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    let file = stage.home.join("login.args");
    let start = Instant::now();
    while !file.exists() {
        assert!(start.elapsed() < Duration::from_secs(20), "no login shell");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "-l\n");
    assert_eq!(
        parse_told(&stage.told[0]),
        Request::Place(Place {
            keep_open: true,
            ..place(1, 40)
        })
    );
    assert_eq!(
        stage.record("p1").as_deref(),
        Some("opener p40\nkeep-open yes\n")
    );
}

#[test]
fn without_an_app_the_message_waits_on_the_machine_and_the_command_still_answers() {
    // A server: its bootstrap starts what has no program, and the message waits for the bateri app.
    let mut stage = stage();
    stage.in_pane(Pane::Carried);
    stage.install_bootstrap();
    stage.set_var("LC_TERMINAL_VERSION", VERSION);
    let program = recorder(&stage).display().to_string();
    for (args, wanted) in [
        (
            vec!["split".to_owned(), "--keep-open".to_owned()],
            Place {
                keep_open: true,
                ..place(1, 40)
            },
        ),
        (
            vec![
                "tab".to_owned(),
                "p7".to_owned(),
                "--focus".to_owned(),
                "--".to_owned(),
                program.clone(),
                stage.env.dir.join("tabout").display().to_string(),
            ],
            Place {
                side: Some(Side::Tab),
                focus: true,
                anchor: Some(Anchor::of(Terminal(7))),
                ..place(2, 40)
            },
        ),
    ] {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        let said = stage.run_in("p40", &words);
        assert_eq!(said.status, 0, "{}", said.err);
        let id = format!("p{}", wanted.terminal.0);
        assert_eq!(said.out, format!("{id}\n"));
        assert_eq!(
            said.err,
            format!("bateri: {id} will take its place when the bateri app connects\n")
        );
        // The note is the message, whole — the anchor included, since nobody names its sender when
        // it is read — and is read as one.
        let note = stage.note(&id).expect("a note");
        assert_eq!(note, encoded(wanted.clone()));
        assert_eq!(parse_told(&note), Request::Place(wanted));
    }
    // The answer as an object.
    let said = stage.run_in("p40", &["split", "--json", "--left"]);
    assert_eq!(said.status, 0);
    assert_eq!(
        said.out,
        "{\"pane\":\"p3\",\"tab\":null,\"group\":null,\"window\":null,\"landed\":\"waiting\",\"next_to\":null,\"squeezed\":false,\"hidden\":false}\n"
    );
    // The bootstrap was run from its file, as the one-liner of a server's pane runs it: no parent
    // block, no nonce, the terminal's own version.
    let (args, _) = stage.bootstrap_saw().expect("the bootstrap ran");
    assert_eq!(args, format!("||{VERSION}|-|"));
}

#[test]
fn an_app_that_answers_with_something_else_leaves_the_pane_to_be_placed_later() {
    let mut stage = stage();
    stage.in_pane(Pane::Carried);
    stage.install_bootstrap();
    // Version 1 of the text, which nothing here reads.
    stage.app_says("bateri 1\nok placed\nlanded as_asked\nnext-to p40\nsqueezed no\nhidden no\n");
    let said = stage.run_in("p40", &["split", "--json"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(
        said.err
            .contains("answered with something this does not read"),
        "{}",
        said.err
    );
    assert_eq!(
        said.out,
        "{\"pane\":\"p1\",\"tab\":null,\"group\":null,\"window\":null,\"landed\":\"waiting\",\"next_to\":null,\"squeezed\":false,\"hidden\":false}\n"
    );
    assert!(stage.note("p1").is_some());
    // A reply that is read whole or not at all: one with a name of another kind in it is not read,
    // and nothing of it is taken.
    stage.app_says(
        "bateri 2\nok placed\nlanded as_asked\nnext-to p40\nsqueezed no\nhidden no\ntab g1\n",
    );
    let said = stage.run_in("p40", &["split", "--json"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(said.err.contains("does not read"), "{}", said.err);
    assert!(said.out.contains("\"landed\":\"waiting\""), "{}", said.out);
}

/// Waits for the bootstrap of a terminal to have run.
fn bootstrap_ran(stage: &Stage, which: &str) {
    let start = Instant::now();
    while stage.bootstrap_saw().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{which}: the bootstrap did not run"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_app_that_does_not_say_leaves_the_start_to_the_pane_that_asked_with_a_bootstrap_there_or_not()
{
    // The pane that asked says whose app it is. The app on this machine makes the pane of a
    // terminal with no program or nobody does, and a pane that belongs to no app has none to ask:
    // no app, or a reply that is not read, leaves nothing behind (no terminal, no record, no note
    // for an app that would find no terminal, no bootstrap run) and the command ends with 69. The
    // server of another machine's app starts it with its bootstrap and leaves the message for
    // that app. A bootstrap in the home, or none, changes nothing: the command does not look.
    for bootstrap in [false, true] {
        for pane in [Pane::Local, Pane::Carried, Pane::Bare] {
            for reply in [
                None,
                Some("bateri 1\nok placed\nlanded as_asked\nnext-to p40\nsqueezed no\nhidden no\n"),
            ] {
                let mut stage = stage();
                stage.in_pane(pane);
                if bootstrap {
                    stage.install_bootstrap();
                }
                if let Some(reply) = reply {
                    stage.app_says(reply);
                }
                let said = stage.run_in("p40", &["split"]);
                let which = format!("{pane:?}, bootstrap {bootstrap}, reply {reply:?}");
                if matches!(pane, Pane::Carried) {
                    assert_eq!(said.status, 0, "{which}: {}", said.err);
                    assert_eq!(said.out, "p1\n", "{which}");
                    assert_eq!(
                        said.err.contains("does not read"),
                        reply.is_some(),
                        "{which}: {}",
                        said.err
                    );
                    assert!(stage.note("p1").is_some(), "{which}");
                    assert!(stage.state().join("terminals/p1").exists(), "{which}");
                    if bootstrap {
                        bootstrap_ran(&stage, &which);
                    }
                    continue;
                }
                assert_eq!(said.status, 69, "{which}: {}", said.err);
                let wanted = match (pane, reply) {
                    (Pane::Local, None) => "the bateri app is not connected",
                    (Pane::Local, Some(_)) => {
                        "it is the bateri app that makes a pane with no program"
                    }
                    _ => "nothing says which bateri app this terminal belongs to",
                };
                assert!(said.err.contains(wanted), "{which}: {}", said.err);
                assert_eq!(said.out, "", "{which}");
                assert!(!stage.running().contains(&"p1".to_owned()), "{which}");
                assert!(!stage.state().join("terminals/p1").exists(), "{which}");
                assert_eq!(stage.note("p1"), None, "{which}");
                assert!(stage.bootstrap_saw().is_none(), "{which}");
            }
        }
    }
    // Outside every terminal there is no pane to go beside, with or without a bootstrap. (The
    // pane of another machine's app does start one there, as the test of `with_no_app` shows.)
    for pane in [Pane::Local, Pane::Bare] {
        let mut stage = stage();
        stage.in_pane(pane);
        stage.install_bootstrap();
        let said = stage.run(&["split"]);
        assert_eq!(said.status, 1, "{pane:?}: {}", said.err);
        assert!(
            said.err
                .contains("a pane with no program goes beside a pane"),
            "{pane:?}: {}",
            said.err
        );
        assert!(stage.bootstrap_saw().is_none(), "{pane:?}");
    }
}

#[test]
fn a_start_the_app_says_is_obeyed_in_any_pane() {
    for pane in [Pane::Local, Pane::Carried, Pane::Bare] {
        let mut stage = stage();
        stage.in_pane(pane);
        stage.install_bootstrap();
        stage.set_var("LC_TERMINAL_VERSION", VERSION);
        stage.connect_app();
        let bin = shells(&stage);
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        stage.set_var("PATH", &path);
        let said = stage.go(&Run {
            here: Some("p40"),
            args: &["split"],
            input: None,
            turns: &[START, PLACED],
            vars: &[],
        });
        assert_eq!(said.status, 0, "{pane:?}: {}", said.err);
        assert_eq!(said.out, "p1\n", "{pane:?}");
        assert_eq!(stage.told.len(), 2, "{pane:?}: {:?}", stage.told);
        bootstrap_ran(&stage, &format!("{pane:?}"));
    }
}

#[test]
fn a_pane_that_landed_says_its_tab_group_and_window() {
    let mut stage = stage();
    let program = recorder(&stage).display().to_string();
    let run = |stage: &mut Stage, n: u64| {
        let out = stage.env.dir.join(format!("st{n}")).display().to_string();
        stage.run_in("p40", &["split", "--json", "--", &program, &out])
    };
    // The bateri app says where the pane stands, and the object has the four names always.
    stage.app_says(
        "bateri 2\nok placed\nlanded as_asked\nnext-to p40\nsqueezed no\nhidden no\ntab t3\ngroup g1\nwindow w1\n",
    );
    let said = run(&mut stage, 1);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(
        said.out,
        "{\"pane\":\"p1\",\"tab\":\"t3\",\"group\":\"g1\",\"window\":\"w1\",\"landed\":\"as_asked\",\"next_to\":\"p40\",\"squeezed\":false,\"hidden\":false}\n"
    );
    // A tab in no group: `null` for the group, the other two as given.
    stage.app_says(
        "bateri 2\nok placed\nlanded as_asked\nnext-to p40\nsqueezed no\nhidden no\ntab t8\nwindow w2\n",
    );
    let said = run(&mut stage, 2);
    assert_eq!(
        said.out,
        "{\"pane\":\"p2\",\"tab\":\"t8\",\"group\":null,\"window\":\"w2\",\"landed\":\"as_asked\",\"next_to\":\"p40\",\"squeezed\":false,\"hidden\":false}\n"
    );
    // Without --json the number is all there is.
    let out = stage.env.dir.join("st3").display().to_string();
    let said = stage.run_in("p40", &["split", "--", &program, &out]);
    assert_eq!(said.out, "p3\n");
}

#[test]
fn a_pane_that_did_not_land_as_asked_is_told_on_one_line() {
    let mut stage = stage();
    let program = recorder(&stage).display().to_string();
    let mut n = 0;
    let mut run = |stage: &mut Stage, args: &[&str]| {
        n += 1;
        let out = stage.env.dir.join(format!("ln{n}")).display().to_string();
        let mut words = args.to_vec();
        words.extend(["--", &program, &out]);
        stage.run_in("p40", &words)
    };
    stage.app_says("bateri 2\nok placed\nlanded new_tab\nnext-to p40\nsqueezed yes\nhidden no\n");
    let said = run(&mut stage, &["split", "--json", "--below"]);
    assert_eq!(said.status, 0);
    assert_eq!(
        said.out,
        "{\"pane\":\"p1\",\"tab\":null,\"group\":null,\"window\":null,\"landed\":\"new_tab\",\"next_to\":\"p40\",\"squeezed\":true,\"hidden\":false}\n"
    );
    assert_eq!(
        said.err,
        "bateri: p1 is in a tab of its own: there was no room beside p40\n"
    );
    // The bateri app has it, waiting for the anchor to be on screen: that is not "when the bateri app connects".
    stage.app_says("bateri 2\nok placed\nlanded waiting\nnext-to p40\nsqueezed no\nhidden no\n");
    let said = run(&mut stage, &["split", "--json"]);
    assert_eq!(said.status, 0);
    assert_eq!(
        said.out,
        "{\"pane\":\"p2\",\"tab\":null,\"group\":null,\"window\":null,\"landed\":\"waiting\",\"next_to\":\"p40\",\"squeezed\":false,\"hidden\":false}\n"
    );
    assert_eq!(
        said.err,
        "bateri: p2 waits in the bateri app's list of terminals running in the background, until p40 is on screen\n"
    );
    assert_eq!(
        stage.note("p2"),
        None,
        "the bateri app has it; nothing waits on the machine"
    );
    // The bateri app could not make the pane just now and kept nothing to place it by: it does not say
    // the pane will come when the anchor is on screen.
    stage.app_says("bateri 2\nok placed\nlanded waiting\nsqueezed no\nhidden no\n");
    let said = run(&mut stage, &["split"]);
    assert_eq!(said.out, "p3\n");
    assert_eq!(
        said.err,
        "bateri: p3 waits in the bateri app's list of terminals running in the background: the bateri app could not make its pane now\n"
    );
    stage.app_says("bateri 2\nok placed\nlanded as_asked\nnext-to p40\nsqueezed no\nhidden yes\n");
    let said = run(&mut stage, &["split"]);
    assert_eq!(said.out, "p4\n");
    assert_eq!(said.err, "bateri: p4 is behind a pane that fills its tab\n");
    // A pane that did not take the keyboard says so on one line.
    stage.app_says(
        "bateri 2\nok placed\nlanded as_asked\nnext-to p40\nsqueezed no\nhidden no\nkeyboard stayed\n",
    );
    let said = run(&mut stage, &["split"]);
    assert_eq!(said.out, "p5\n");
    assert!(
        said.err.contains("p5 did not take the keyboard"),
        "{}",
        said.err
    );
}

/// A terminal the bateri app opened before the numbers existed, and one a person made with kablo.
const MADE: &str = "b-0123456789abcdef0123456789abcdef";

#[test]
fn the_callers_own_terminal_is_the_anchor_whatever_it_is_called() {
    let mut stage = stage();
    let program = recorder(&stage).display().to_string();
    let mut n = 0;
    for here in [MADE, "mine", "build.2", "t9"] {
        n += 1;
        stage.forget_told();
        stage.app_says(&format!(
            "bateri 2\nok placed\nlanded as_asked\nnext-to {here}\nsqueezed no\nhidden no\n"
        ));
        let out = stage.env.dir.join(format!("an{n}")).display().to_string();
        let said = stage.run_in(here, &["split", "--below", "--json", "--", &program, &out]);
        assert_eq!(said.status, 0, "{here}: {}", said.err);
        let id = said.out.split('"').nth(3).unwrap().to_owned();
        // The bateri app is told the anchor by the name kablo gave the terminal, in a message it reads,
        // and says the same name back.
        let wanted = Place {
            terminal: Terminal::parse(&id).expect("a number"),
            anchor: Anchor::parse(here),
            side: Some(Side::Below),
            ..place(1, 1)
        };
        assert_eq!(stage.told, [encoded(wanted.clone())], "{here}");
        assert_eq!(parse_told(&stage.told[0]), Request::Place(wanted));
        assert!(
            said.out.contains(&format!("\"next_to\":\"{here}\"")),
            "{here}: {}",
            said.out
        );
        assert_eq!(said.err, "", "{here}");
        // Who opened it, so that it can close what it opened.
        assert_eq!(
            stage.record(&id).unwrap(),
            format!("opener {here}\nkeep-open no\n")
        );
    }
    // A `tab` from such a terminal has the same anchor; one named is still only a number.
    stage.app_says(PLACED);
    let out = stage.env.dir.join("anx").display().to_string();
    let said = stage.run_in(MADE, &["tab", "--", &program, &out]);
    assert_eq!(said.status, 0, "{}", said.err);
    let said = stage.run_in(MADE, &["split", MADE]);
    assert_eq!(said.status, 64, "an anchor the agent writes is a number");
    assert!(said.err.contains("is not a pane"), "{}", said.err);
}

#[test]
fn a_pane_that_waits_for_the_terminal_that_asked_says_so_without_its_long_name() {
    let mut stage = stage();
    let program = recorder(&stage).display().to_string();
    // The bateri app has it, waiting for the anchor to be on screen.
    stage.app_says(&format!(
        "bateri 2\nok placed\nlanded waiting\nnext-to {MADE}\nsqueezed no\nhidden no\n"
    ));
    let out = stage.env.dir.join("w1").display().to_string();
    let said = stage.run_in(MADE, &["split", "--", &program, &out]);
    assert_eq!(
        said.err,
        "bateri: p1 waits in the bateri app's list of terminals running in the background, until this terminal is on screen\n"
    );
    // No bateri app: the note is the message, anchor and all, and is read as one.
    stage.no_app();
    let out = stage.env.dir.join("w2").display().to_string();
    let said = stage.run_in(
        MADE,
        &["split", "--right", "--keep-open", "--", &program, &out],
    );
    assert_eq!(
        said.err,
        "bateri: p2 will take its place when the bateri app connects\n"
    );
    let wanted = Place {
        terminal: Terminal(2),
        anchor: Anchor::parse(MADE),
        side: Some(Side::Right),
        keep_open: true,
        ..place(2, 1)
    };
    let note = stage.note("p2").expect("a note");
    assert_eq!(note, encoded(wanted.clone()));
    assert_eq!(parse_told(&note), Request::Place(wanted));
    // A terminal that has a number is called by it.
    stage.app_says("bateri 2\nok placed\nlanded new_tab\nnext-to p40\nsqueezed no\nhidden no\n");
    let out = stage.env.dir.join("w3").display().to_string();
    let said = stage.run_in("p40", &["tab", "--", &program, &out]);
    assert_eq!(
        said.err,
        "bateri: p3 is in a tab of its own: there was no room beside p40\n"
    );
    stage.app_says(&format!(
        "bateri 2\nok placed\nlanded new_tab\nnext-to {MADE}\nsqueezed no\nhidden no\n"
    ));
    let out = stage.env.dir.join("w4").display().to_string();
    let said = stage.run_in(MADE, &["tab", "--", &program, &out]);
    assert_eq!(
        said.err,
        "bateri: p4 is in a tab of its own: there was no room beside this terminal\n"
    );
}

#[test]
fn run_in_no_terminal_a_pane_with_no_anchor_leaves_the_terminal_in_the_list() {
    let mut stage = stage();
    stage.app_says(PLACED);
    let program = recorder(&stage).display().to_string();
    for (n, mode) in [(1, "split"), (2, "tab")] {
        let out = stage.env.dir.join(format!("cr{n}")).display().to_string();
        let said = stage.run(&[mode, "--keep-open", "--", &program, &out]);
        assert_eq!(said.status, 0, "{mode}: {}", said.err);
        assert_eq!(said.out, format!("p{n}\n"));
        assert_eq!(
            said.err,
            format!(
                "bateri: p{n} waits in the bateri app's list of terminals running in the background: this ran in no terminal, so it has no pane to go beside\n"
            )
        );
        // The terminal runs, nobody is asked where to put a pane and no note waits.
        assert!(
            recorded(&stage.env.dir.join(format!("cr{n}")))
                .args
                .is_empty()
        );
        assert_eq!(stage.note(&format!("p{n}")), None);
        assert_eq!(
            stage.record(&format!("p{n}")).unwrap(),
            "opener -\nkeep-open yes\n"
        );
    }
    assert!(stage.told.is_empty());
    // The answer as an object says it waits.
    let out = stage.env.dir.join("cr3").display().to_string();
    let said = stage.run(&["split", "--json", "--below", "--", &program, &out]);
    assert_eq!(said.status, 0);
    assert_eq!(
        said.out,
        "{\"pane\":\"p3\",\"tab\":null,\"group\":null,\"window\":null,\"landed\":\"waiting\",\"next_to\":null,\"squeezed\":false,\"hidden\":false}\n"
    );
    // A name the agent wrote is an anchor even from outside every terminal.
    let out = stage.env.dir.join("cr4").display().to_string();
    let said = stage.run(&["split", "p40", "--", &program, &out]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(
        stage.told,
        [encoded(Place {
            terminal: Terminal(4),
            ..place(4, 40)
        })]
    );
}

#[test]
fn a_request_the_app_refuses_leaves_no_terminal_running() {
    let mut stage = stage();
    let program = recorder(&stage).display().to_string();
    stage.app_says("bateri 2\nerror refused the user said no\n");
    let out = stage.env.dir.join("rf1").display().to_string();
    let said = stage.run_in("p40", &["split", "--", &program, &out]);
    assert_eq!(said.status, 77);
    assert_eq!(said.out, "");
    assert_eq!(said.err, "bateri: the user said no\n");
    assert!(!stage.running().contains(&"p1".to_owned()));
    assert!(!stage.state().join("terminals/p1").exists());
    assert_eq!(stage.note("p1"), None);
    for (n, (kind, status)) in [("usage", 64), ("unknown", 1), ("failed", 1), ("version", 1)]
        .into_iter()
        .enumerate()
    {
        stage.app_says(&format!("bateri 2\nerror {kind} it went wrong\n"));
        let out = stage
            .env
            .dir
            .join(format!("rf{}", n + 2))
            .display()
            .to_string();
        let said = stage.run_in("p40", &["tab", "--", &program, &out]);
        assert_eq!(said.status, status, "{kind}");
    }
}

#[test]
fn the_terminal_it_starts_has_the_callers_umask_and_none_of_the_callers_tab() {
    let mut stage = stage();
    stage.app_says(PLACED);
    stage.set_umask("027");
    let program = recorder(&stage).display().to_string();
    let out = stage.env.dir.join("um");
    let said = stage.go(&Run {
        here: Some("p40"),
        args: &["split", "--", &program, &out.display().to_string()],
        input: None,
        turns: &[],
        vars: &[
            ("LC_BATERI_TAB_URL", "tab://1"),
            ("BATERI_TAB_URL", "tab://1"),
            ("TERM_SESSION_ID", "w0t0p0"),
            ("BATERI_DOCK", "off"),
        ],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    let seen = recorded(&out);
    // The umask of the caller's terminal is the one the new terminal has, not the owner-only one
    // the state is written with.
    assert_eq!(seen.umask.trim_start_matches('0'), "27");
    // And not the tab the caller stands in: that tab's pane is not this terminal's.
    for name in ["LC_BATERI_TAB_URL", "BATERI_TAB_URL", "TERM_SESSION_ID"] {
        assert!(!seen.env.contains_key(name), "{name}");
    }
    // The dock's tier travels, where the shell it runs in has it.
    assert_eq!(seen.env["BATERI_DOCK"], "off");
    let out = stage.env.dir.join("um2");
    let said = stage.run_in(
        "p40",
        &["split", "--", &program, &out.display().to_string()],
    );
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(!recorded(&out).env.contains_key("BATERI_DOCK"));
}

#[test]
fn words_another_machines_app_relayed_say_nothing_of_a_pane_they_have_no_anchor_for() {
    let mut stage = stage();
    stage.app_says(PLACED);
    let program = recorder(&stage).display().to_string();
    let out = stage.env.dir.join("hx1").display().to_string();
    let hexed = |words: &[&str]| -> Vec<String> { words.iter().map(|w| hex_word(w)).collect() };
    let words = hexed(&["split", "--keep-open", "--", &program, &out]);
    let mut args = vec!["--hex"];
    args.extend(words.iter().map(String::as_str));
    let said = stage.run(&args);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "p1\n");
    // The relaying app is the one that tells where the pane goes: no note from here.
    assert_eq!(said.err, "");
    assert!(stage.told.is_empty());
    // The same words, as the caller's own command, say it.
    let out = stage.env.dir.join("hx2").display().to_string();
    let said = stage.run(&["split", "--keep-open", "--", &program, &out]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(
        said.err.contains("waits in the bateri app's list"),
        "{}",
        said.err
    );
}

#[test]
fn words_another_machines_app_relayed_start_a_terminal_with_no_program_where_the_environment_would_refuse()
 {
    // A pane of the app on this machine, whose app is not there, would be refused (69); words
    // that came from another machine's app are that app's, which this machine is the server of.
    let mut stage = stage();
    stage.install_bootstrap();
    stage.set_var("LC_TERMINAL_VERSION", VERSION);
    let refused = stage.run_in("p40", &["split", "--keep-open"]);
    assert_eq!(refused.status, 69, "{}", refused.err);
    let words = ["split", "--keep-open"].map(hex_word);
    let mut args = vec!["--hex"];
    args.extend(words.iter().map(String::as_str));
    let said = stage.run_in("p40", &args);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "p2\n", "the refused one had its number");
    let (seen, _) = stage.bootstrap_saw().expect("the bootstrap ran");
    assert_eq!(seen, format!("||{VERSION}|-|"));
}

#[test]
fn a_mistake_in_the_command_is_64_before_anything_is_started() {
    let mut stage = stage();
    stage.app_says(PLACED);
    let program = recorder(&stage).display().to_string();
    let tries: [&[&str]; 14] = [
        &["split", "--ratio", "0"],
        &["split", "--ratio", "100"],
        &["split", "--ratio", "x"],
        &["split", "--ratio"],
        &["split", "--left", "--below"],
        &["tab", "--left"],
        &["tab", "--ratio", "30"],
        &["new", "--keep-open"],
        &["new", "p40"],
        &["split", "p40", "p5"],
        &["split", "--bogus"],
        &["notify"],
        &["frobnicate"],
        &[],
    ];
    for args in tries {
        let said = stage.run_in("p40", args);
        assert_eq!(said.status, 64, "{args:?}: {}", said.err);
    }
    assert_eq!(stage.running(), ["p40"], "nothing was started");
    // `new` is no command: the mistake is the usage, which does not offer it.
    let said = stage.run(&["new", "--", "true"]);
    assert_eq!(said.status, 64, "{}", said.err);
    assert!(said.err.contains("usage: bateri split"), "{}", said.err);
    assert!(!said.err.contains("bateri new"), "{}", said.err);
    // From outside every terminal a `split` needs no anchor and the number is still given.
    let out = stage.env.dir.join("mk").display().to_string();
    let said = stage.run(&["split", "--", &program, &out]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "p1\n");
    assert_eq!(stage.record("p1").unwrap(), "opener -\nkeep-open no\n");
    assert!(
        stage.told.is_empty(),
        "no pane to place, so the bateri app is told nothing"
    );
}

#[test]
fn a_number_is_never_given_twice_and_one_a_hand_made_terminal_holds_is_skipped() {
    let mut stage = stage();
    let program = recorder(&stage).display().to_string();
    let mut n = 0;
    let mut split = |stage: &mut Stage| {
        n += 1;
        let out = stage.env.dir.join(format!("nu{n}")).display().to_string();
        stage.run(&["split", "--", &program, &out])
    };
    assert_eq!(split(&mut stage).out, "p1\n");
    assert_eq!(split(&mut stage).out, "p2\n");
    // p1 is closed and gone; the next is not p1.
    let _ = std::fs::remove_file(stage.state().join("terminals/p1"));
    assert_eq!(split(&mut stage).out, "p3\n");
    // A session somebody made with `kablo new --id p4`: kablo refuses the id, the next is tried.
    stage.terminal("p4");
    stage.terminal("p5");
    let said = split(&mut stage);
    assert_eq!(said.out, "p6\n", "{}", said.err);
    assert_eq!(
        std::fs::read_to_string(stage.state().join("next")).unwrap(),
        "7\n"
    );
}

#[test]
fn a_pane_or_a_tab_opens_where_it_is_told_and_what_contradicts_is_64() {
    let id = |kind: IdKind, number: u64| Id::new(kind, number);
    let mut stage = stage();
    stage.app_says(PLACED);
    let program = recorder(&stage).display().to_string();
    let tries: Vec<(Vec<&str>, Option<Side>, Home)> = vec![
        (
            vec!["split", "--tab", "t3"],
            None,
            Home {
                tab: id(IdKind::Tab, 3),
                ..Home::default()
            },
        ),
        (
            vec!["split", "--tab=t3", "--right"],
            Some(Side::Right),
            Home {
                tab: id(IdKind::Tab, 3),
                ..Home::default()
            },
        ),
        (
            vec!["tab", "--group", "g1"],
            Some(Side::Tab),
            Home {
                group: id(IdKind::Group, 1),
                ..Home::default()
            },
        ),
        (
            vec!["tab", "--window", "w2"],
            Some(Side::Tab),
            Home {
                window: Some(WindowHome::Existing(id(IdKind::Window, 2).unwrap())),
                ..Home::default()
            },
        ),
        (
            vec![
                "tab",
                "--new-group",
                "api",
                "--window",
                "w2",
                "--label",
                "web",
            ],
            Some(Side::Tab),
            Home {
                new_group: Some("api".to_owned()),
                window: Some(WindowHome::Existing(id(IdKind::Window, 2).unwrap())),
                label: Some("web".to_owned()),
                ..Home::default()
            },
        ),
        (
            vec!["tab", "--new-group", "api v2"],
            Some(Side::Tab),
            Home {
                new_group: Some("api v2".to_owned()),
                ..Home::default()
            },
        ),
        (
            vec!["window"],
            Some(Side::Tab),
            Home {
                window: Some(WindowHome::New),
                ..Home::default()
            },
        ),
        (
            vec!["window", "--focus"],
            Some(Side::Tab),
            Home {
                window: Some(WindowHome::New),
                ..Home::default()
            },
        ),
    ];
    for (n, (args, side, home)) in tries.into_iter().enumerate() {
        stage.forget_told();
        let out = stage.env.dir.join(format!("ho{n}")).display().to_string();
        let mut words = args.clone();
        words.extend(["--", &program, &out]);
        let said = stage.run_in("p40", &words);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert_eq!(said.out, format!("p{}\n", n + 1), "{args:?}");
        assert_eq!(stage.told.len(), 1, "{args:?}");
        let Request::Place(placed) = parse_told(&stage.told[0]) else {
            panic!("{args:?}: {:?}", stage.told[0]);
        };
        assert_eq!(placed.side, side, "{args:?}");
        assert_eq!(placed.home, home, "{args:?}");
        assert_eq!(placed.anchor, Some(Anchor::of(Terminal(40))), "{args:?}");
        // The words are frozen.
        if n == 4 {
            assert_eq!(
                stage.told[0],
                "bateri 2\nplace\nterminal p5\nanchor p40\nside tab\nnew-group api\nwindow w2\nlabel web\n"
            );
        }
        if n == 6 {
            assert_eq!(
                stage.told[0],
                "bateri 2\nplace\nterminal p7\nanchor p40\nside tab\nwindow new\n"
            );
        }
    }
    // What contradicts, and what is not for the command, is 64 before a terminal starts.
    let started = stage.running().len();
    for (args, hint) in [
        (&["split", "p9", "--tab", "t3"][..], "a pane or into a tab"),
        (&["split", "--tab", "g1"], "g1 is a group, not a tab"),
        (&["split", "--tab"], "--tab takes a tab"),
        (&["split", "--tab", "t3", "--tab", "t4"], "name one tab"),
        (&["split", "--group", "g1"], "split takes no --group"),
        (&["split", "--window", "w1"], "split takes no --window"),
        (
            &["split", "--new-group", "api"],
            "split takes no --new-group",
        ),
        (&["tab", "--tab", "t3"], "tab takes no --tab"),
        (
            &["tab", "--group", "g1", "--window", "w1"],
            "it goes with no --window",
        ),
        (
            &["tab", "--group", "g1", "--new-group", "api"],
            "it goes with no --window",
        ),
        (&["tab", "--group", "t3"], "t3 is a tab, not a group"),
        (&["tab", "--window", "g1"], "g1 is a group, not a window"),
        (&["tab", "--new-group", ""], "--new-group is 1 to 64 bytes"),
        (&["tab", "--new-group", "   "], "only spaces"),
        (
            &["tab", "--label", "a\nwindow w1"],
            "one line of plain text",
        ),
        (&["tab", "--label", "x", "--label", "y"], "name one label"),
        (&["window", "--tab", "t3"], "window takes no --tab"),
        (&["window", "--group", "g1"], "window takes no --group"),
        (&["window", "--window", "w1"], "window takes no --window"),
        (&["window", "--left"], "window takes no side"),
        (&["window", "--ratio", "30"], "window takes no ratio"),
    ] {
        let said = stage.run_in("p40", args);
        assert_eq!(said.status, 64, "{args:?}: {}", said.err);
        assert!(said.err.contains(hint), "{args:?}: {}", said.err);
    }
    assert_eq!(stage.running().len(), started);
}

#[test]
fn a_label_is_kablos_for_a_pane_and_the_apps_for_a_tab() {
    let mut stage = stage();
    stage.app_says(PLACED);
    let program = recorder(&stage).display().to_string();
    // A pane's label rides on the terminal and is not in the message.
    let out = stage.env.dir.join("lb1").display().to_string();
    let said = stage.run_in(
        "p40",
        &["split", "--label", "dev server", "--", &program, &out],
    );
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(stage.told, ["bateri 2\nplace\nterminal p1\nanchor p40\n"]);
    let listed = stage.env.kablo().args(["list", "--json"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("\"label\":\"dev server\""),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );
    // A tab's is in the message and not on the terminal.
    stage.forget_told();
    let out = stage.env.dir.join("lb2").display().to_string();
    let said = stage.run_in("p40", &["tab", "--label=web", "--", &program, &out]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(
        stage.told,
        ["bateri 2\nplace\nterminal p2\nanchor p40\nside tab\nlabel web\n"]
    );
    let listed = stage.env.kablo().args(["list", "--json"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("\"label\":\"web\""));
}

#[test]
fn a_window_waits_for_an_app_as_a_tab_does_and_a_place_the_app_refuses_ends_the_terminal() {
    let mut stage = stage();
    let program = recorder(&stage).display().to_string();
    // No bateri app: the number is given, the message waits with its fields, and the command says so on
    // one line.
    let out = stage.env.dir.join("wi1").display().to_string();
    let said = stage.run_in("p40", &["window", "--json", "--", &program, &out]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(said.out.starts_with("{\"pane\":\"p1\""), "{}", said.out);
    assert!(said.out.contains("\"window\":null"), "{}", said.out);
    assert_eq!(
        said.err,
        "bateri: p1 will take its place when the bateri app connects\n"
    );
    let note = stage.note("p1").expect("a note");
    let Request::Place(waiting) = parse_told(&note) else {
        panic!("{note:?}");
    };
    assert_eq!(waiting.side, Some(Side::Tab));
    assert_eq!(
        waiting.home,
        Home {
            window: Some(WindowHome::New),
            ..Home::default()
        }
    );
    // The bateri app that does not have the place the pane asked for refuses it: the terminal the command
    // started ends and no note is left, with the code the bateri app's kind says.
    stage.app_says("bateri 2\nerror unknown there is no tab t9\n");
    let out = stage.env.dir.join("wi2").display().to_string();
    let said = stage.run_in("p40", &["split", "--tab", "t9", "--", &program, &out]);
    assert_eq!(
        (said.status, said.err.as_str()),
        (1, "bateri: there is no tab t9\n")
    );
    assert_eq!(stage.note("p2"), None);
    stage.app_says("bateri 2\nerror refused this holds a pane of another machine\n");
    let out = stage.env.dir.join("wi3").display().to_string();
    let said = stage.run_in("p40", &["tab", "--group", "g1", "--", &program, &out]);
    assert_eq!(said.status, 77, "{}", said.err);
    let running = stage.running();
    assert!(
        !running.contains(&"p2".to_owned()) && !running.contains(&"p3".to_owned()),
        "{running:?}"
    );
}

#[test]
fn a_split_with_no_pane_to_go_beside_prints_the_number_and_waits_with_the_programs_status() {
    let mut stage = stage();
    // `--wait` ends with the program's own status, here 3, and the number is printed before it.
    let said = stage.run(&["split", "--wait", "--", "/bin/sh", "-c", "exit 3"]);
    assert_eq!(said.status, 3, "{}", said.err);
    assert_eq!(said.out, "p1\n");
}

// ─── who starts it: the bateri app says ─────────────────────────────────────────

const START: &str = "bateri 2\nok start\n";

/// Every terminal of the stage, the ones that ended too.
fn listed(stage: &Stage) -> Vec<String> {
    let out = stage.env.kablo().arg("list").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .collect()
}

/// A directory of stand-ins for the three shells bateri has an integration for.
fn shells(stage: &Stage) -> PathBuf {
    let bin = stage.home.join("shells");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["zsh", "bash", "fish"] {
        let file = bin.join(name);
        std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
    }
    bin
}

#[test]
fn a_shell_bateri_has_an_integration_for_named_alone_is_a_shell_and_any_other_a_program() {
    let mut stage = stage();
    let bin = shells(&stage);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    stage.set_var("PATH", &path);
    stage.app_says(PLACED);
    // The terminal the runs are made in exists before anything is counted.
    assert_eq!(stage.run_in("p40", &["help"]).status, 0);
    // The bateri app makes the pane, with that shell: nothing is started here.
    for (args, program) in [
        (&["split", "--", "zsh"][..], "zsh"),
        (&["tab", "--", "fish"], "fish"),
        (&["window", "--", "bash", "-l"], "bash"),
    ] {
        stage.forget_told();
        let before = listed(&stage);
        let said = stage.run_in("p40", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert_eq!(listed(&stage), before, "{args:?}: the bateri app makes it");
        let wanted = format!("shell {}\n", bin.join(program).display());
        assert!(
            stage.told[0].contains("fresh "),
            "{args:?}: {}",
            stage.told[0]
        );
        assert!(
            stage.told[0].contains(&wanted),
            "{args:?}: {}",
            stage.told[0]
        );
        assert!(Request::parse(&stage.told[0]).is_ok(), "{}", stage.told[0]);
    }
    // By its path too; and what is not such a shell, or runs something, is a program.
    stage.forget_told();
    let fish = bin.join("fish").display().to_string();
    let before = listed(&stage);
    assert_eq!(stage.run_in("p40", &["split", "--", &fish]).status, 0);
    assert_eq!(listed(&stage), before);
    assert!(stage.told[0].contains(&format!("shell {fish}\n")));
    for args in [
        &["split", "--", "sh"][..],
        &["split", "--", "zsh", "-c", "make"],
        &["split", "--", "bash", "build.sh"],
        &["split", "--wait", "--", "zsh"],
    ] {
        stage.forget_told();
        let before = listed(&stage).len();
        let said = stage.run_in("p40", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert_eq!(listed(&stage).len(), before + 1, "{args:?}: a program");
        assert!(
            !stage.told[0].contains("shell "),
            "{args:?}: {}",
            stage.told[0]
        );
        assert!(
            !stage.told[0].contains("fresh "),
            "{args:?}: {}",
            stage.told[0]
        );
    }
    // In no terminal there is no pane for the bateri app to make it beside: a program.
    let before = listed(&stage).len();
    assert_eq!(stage.run(&["split", "--", "zsh"]).status, 0);
    assert_eq!(listed(&stage).len(), before + 1);

    // A bootstrap left in this home (someone ran `ssh` to it once) does not make it a server: the
    // bateri app says what it is, and it makes the pane.
    stage.install_bootstrap();
    for (args, shell) in [(&["split", "--", "zsh"][..], true), (&["split"], false)] {
        stage.forget_told();
        let before = listed(&stage);
        let said = stage.run_in("p40", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert_eq!(listed(&stage), before, "{args:?}: the bateri app makes it");
        assert!(
            stage.told[0].contains("fresh "),
            "{args:?}: {}",
            stage.told[0]
        );
        assert_eq!(stage.told[0].contains("shell "), shell, "{}", stage.told[0]);
    }
}

#[test]
fn a_server_is_told_to_start_it_and_does_so_with_its_bootstrap_and_asks_again_for_the_pane() {
    let mut stage = stage();
    stage.in_pane(Pane::Carried);
    stage.install_bootstrap();
    stage.set_var("LC_TERMINAL_VERSION", VERSION);
    stage.connect_app();
    let bin = shells(&stage);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    stage.set_var("PATH", &path);
    // The asker is a server: `ok start`, and the pane is asked for once more, the terminal made.
    let said = stage.go(&Run {
        here: Some("p40"),
        args: &["split", "--below"],
        input: None,
        turns: &[START, PLACED],
        vars: &[],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "p1\n");
    assert_eq!(said.err, "");
    assert_eq!(stage.told.len(), 2, "{:?}", stage.told);
    let asked = Place {
        side: Some(Side::Below),
        fresh: Some(stage.home.display().to_string()),
        ..place(1, 40)
    };
    assert_eq!(stage.told[0], encoded(asked));
    let again = Place {
        side: Some(Side::Below),
        ..place(1, 40)
    };
    assert_eq!(stage.told[1], encoded(again));
    // The terminal is the bootstrap's, with the shell the user has.
    let start = Instant::now();
    let seen = loop {
        if let Some(seen) = stage.bootstrap_saw() {
            break seen;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the bootstrap did not run"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(seen.0, format!("||{VERSION}|-|"));
    assert_eq!(
        stage.record("p1").as_deref(),
        Some("opener p40\nkeep-open no\n")
    );
    assert_eq!(stage.note("p1"), None);

    // A shell named alone is the shell the terminal is for: the terminal gets it as its own.
    std::fs::remove_file(stage.home.join("boot.shell")).unwrap();
    let said = stage.go(&Run {
        here: Some("p40"),
        args: &["split", "--", "fish"],
        input: None,
        turns: &[START, PLACED],
        vars: &[],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(
        stage.told[2].contains(&format!("shell {}/fish\n", bin.display())),
        "{}",
        stage.told[2]
    );
    // The second message is for a terminal that exists: no `fresh`, and no `shell` (the terminal
    // has it already).
    assert!(
        !stage.told[3].contains("fresh ") && !stage.told[3].contains("shell "),
        "{}",
        stage.told[3]
    );
    let start = Instant::now();
    let shell = loop {
        if let Ok(shell) = std::fs::read_to_string(stage.home.join("boot.shell")) {
            if !shell.is_empty() {
                break shell;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the bootstrap did not run"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(shell, format!("{}/fish", bin.display()));

    // The pane it asks for the second time is refused: the terminal it started ends, with the
    // bateri app's words.
    let said = stage.go(&Run {
        here: Some("p40"),
        args: &["split"],
        input: None,
        turns: &[START, "bateri 2\nerror unknown there is no tab t9\n"],
        vars: &[],
    });
    assert_eq!(
        (said.status, said.err.as_str()),
        (1, "bateri: there is no tab t9\n")
    );
    assert!(!stage.running().contains(&"p3".to_owned()));
    assert!(!stage.state().join("terminals/p3").exists());
}

#[test]
fn with_no_app_the_pane_says_whose_it_is_a_servers_starts_and_the_rest_end_with_69() {
    // The pane of another machine's app: this machine is its server, whose bootstrap starts it,
    // and the message waits for that app without `fresh`.
    let mut stage = stage();
    stage.in_pane(Pane::Carried);
    stage.install_bootstrap();
    let bin = shells(&stage);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    stage.set_var("PATH", &path);
    let said = stage.run_in("p40", &["split", "--", "zsh"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(
        said.err,
        "bateri: p1 will take its place when the bateri app connects\n"
    );
    let note = stage.note("p1").unwrap();
    assert!(
        !note.contains("fresh ") && !note.contains("shell "),
        "{note}"
    );
    // In no terminal, with no app to ask, the pane's environment is all there is to go by.
    let said = stage.run(&["split", "--", "zsh"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(said.err.contains("ran in no terminal"), "{}", said.err);
    let said = stage.run(&["split"]);
    assert_eq!(said.status, 0, "{}", said.err);
    let start = Instant::now();
    while stage.bootstrap_saw().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the bootstrap did not run"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // A pane of the app on this machine with no app connected (it is not running), or a pane of
    // none: nobody makes a pane with no program, and the number is spent for nothing that was left
    // behind.
    for (pane, wanted) in [
        (Pane::Local, "the bateri app is not connected"),
        (
            Pane::Bare,
            "nothing says which bateri app this terminal belongs to",
        ),
    ] {
        let mut stage = self::stage();
        stage.in_pane(pane);
        let said = stage.run_in("p40", &["split"]);
        assert_eq!(said.status, 69, "{pane:?}: {}", said.err);
        assert!(said.err.contains(wanted), "{pane:?}: {}", said.err);
        assert_eq!(said.out, "", "{pane:?}");
        assert!(!stage.state().join("terminals/p1").exists(), "{pane:?}");
        assert_eq!(stage.note("p1"), None, "{pane:?}");
        assert!(stage.told.is_empty(), "{pane:?}");
        // A program is a terminal any machine starts, and its pane waits.
        let program = recorder(&stage).display().to_string();
        let out = stage.env.dir.join("nm").display().to_string();
        let said = stage.run_in("p40", &["split", "--", &program, &out]);
        assert_eq!(said.status, 0, "{pane:?}: {}", said.err);
        assert_eq!(said.out, "p2\n", "{pane:?}");
    }
}
