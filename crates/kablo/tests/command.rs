//! `kablo bateri`, the verbs that open no terminal, run for real: the real `kablo` against a test in
//! the place of the bateri app (`stage`). What each verb tells the bateri app is read back with the window's own
//! reader (`kablo-bateri`), so the command and the window cannot drift apart without a test saying so.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;
mod stage;

use kablo_bateri::message::{Arrange, Home, Request, Subject, TabHome, WindowHome};
use kablo_bateri::{Anchor, Id, IdKind, Side, Terminal};
use stage::{Stage, parse_told};

const OK: &str = "bateri 2\nok\n";
const MOVED: &str =
    "bateri 2\nok placed\nlanded as_asked\nsqueezed no\nhidden no\ntab t7\nwindow w1\n";
const AT: &str =
    "bateri 2\nok placed\nlanded as_asked\nsqueezed no\nhidden no\ntab t7\ngroup g2\nwindow w1\n";

fn pane(number: u64) -> Anchor {
    Anchor::of(Terminal(number))
}

fn id(kind: IdKind, number: u64) -> Option<Id> {
    Id::new(kind, number)
}

/// Whether the terminal `id` of this stage runs yet (`kablo list` says so).
fn runs(stage: &Stage, id: &str) -> bool {
    let listed = stage.env.kablo().arg("list").output().unwrap();
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .any(|line| line.split_whitespace().next() == Some(id) && !line.contains("ended"))
}

/// What a request of the bateri app tells, one of them: the single message of a run.
fn told_one(stage: &Stage) -> &str {
    assert_eq!(stage.told.len(), 1, "{:?}", stage.told);
    &stage.told[0]
}

#[test]
fn only_a_plain_terminal_number_is_a_target() {
    let mut stage = Stage::with_app(OK);
    stage.terminal("p1");
    // Labels, bare numbers, a leading zero, a made-up id, and the `t7` the numbers were called
    // before panes: none reaches kablo, all are 64.
    for target in ["build", "7", "p07", "P7", "p", "b-0123", "p7x", "t7", "t07"] {
        for args in [
            vec!["close", "pane", target],
            vec!["wait", target],
            vec!["capture", target],
            vec!["send", target, "text"],
            vec!["send-key", target, "enter"],
        ] {
            // `close pane t7` names a tab: it says which command that is.
            if args[0] == "close" && target == "t7" {
                continue;
            }
            let said = stage.run_in("p4", &args);
            assert_eq!(said.status, 64, "{args:?}: {}", said.err);
            assert!(said.err.contains("is not a pane"), "{args:?}: {}", said.err);
            assert!(said.err.contains("p1"), "the list is shown: {}", said.err);
        }
    }
    let said = stage.run_in("p4", &["close", "pane", "t7"]);
    assert_eq!(said.status, 64);
    assert!(
        said.err
            .contains("t7 is a tab, not a pane. Did you mean: bateri close tab t7"),
        "{}",
        said.err
    );
    assert!(stage.told.is_empty(), "{:?}", stage.told);
    assert!(runs(&stage, "p1"));
    // The words that are not targets pass: the text of a one-word `send`, a key.
    assert_eq!(stage.run_in("p4", &["send", "build"]).status, 0);
    assert_eq!(stage.run_in("p4", &["send-key", "enter"]).status, 0);
    assert_eq!(
        stage.run_in("p4", &["send-key", "Ctrl+C", "enter"]).status,
        0
    );
}

#[test]
fn the_commands_that_are_kablos_own_pass_on_with_their_words_and_their_status() {
    let mut stage = Stage::new();
    stage.terminal("p3");
    let said = stage.run_in("p4", &["send", "p3", "cargo test"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(stage.run_in("p4", &["send-key", "p3", "enter"]).status, 0);
    let said = stage.run_in("p4", &["capture", "p3", "--history", "200", "--trim"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(said.out.contains("cargo test"), "{:?}", said.out);
    // With no pane named, the caller's own.
    let said = stage.run_in("p4", &["capture", "--trim"]);
    assert_eq!(said.status, 0, "{}", said.err);
    // `-` reads the text from standard input.
    let said = stage.go(&stage::Run {
        here: Some("p4"),
        args: &["send", "p3", "-"],
        input: Some(b"from the input"),
        turns: &[],
        vars: &[],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    let said = stage.run_in("p4", &["capture", "p3", "--trim"]);
    assert!(said.out.contains("from the input"), "{:?}", said.out);
    // `wait` ends with the program's own status, and with 124 when the time ran out.
    stage.terminal_running("p5", &["/bin/sh", "-c", "exit 3"]);
    assert_eq!(stage.run_in("p4", &["wait", "p5"]).status, 3);
    assert_eq!(
        stage.run_in("p4", &["wait", "--for", "1s", "p3"]).status,
        124,
        "the clock's, not the program's"
    );
    // A `wait` with no terminal is a mistake, not a wait for itself.
    assert_eq!(stage.run_in("p4", &["wait"]).status, 64);
    // Alone a pane is no text to type.
    let said = stage.run_in("p4", &["send", "p3"]);
    assert_eq!(said.status, 64);
    assert!(
        said.err.contains("send needs the text: send p3 TEXT"),
        "{}",
        said.err
    );
    // `--help` is no option of what is typed: it is kablo's to read, and not the usage of this
    // command that answers.
    let said = stage.run_in("p4", &["send", "p3", "--help"]);
    assert_eq!(said.status, 64);
    assert!(said.err.contains("unknown option --help"), "{}", said.err);
    assert!(said.out.is_empty(), "{}", said.out);
    // After `--` it is text.
    let said = stage.run_in("p4", &["send", "p3", "--", "--help"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(said.out.is_empty(), "{}", said.out);
}

#[test]
fn close_goes_to_the_app_and_without_one_ends_any_terminal_of_the_machine() {
    let mut stage = Stage::new();
    for n in 1..=4 {
        stage.terminal(&format!("p{n}"));
    }
    // The records of terminals the command opened go with them.
    for (name, dir) in [("p1", "terminals"), ("p2", "terminals"), ("p2", "placing")] {
        let path = stage.state().join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join(name), "x\n").unwrap();
    }

    // No bateri app: kablo ends it, nobody is asked, the records go.
    let said = stage.run_in("p9", &["close", "pane", "p1"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(!runs(&stage, "p1"));
    assert!(!stage.state().join("terminals/p1").exists());
    assert_eq!(said.err, "");
    // With the words kablo takes: `--force` kills.
    let said = stage.run_in("p9", &["close", "pane", "p2", "--force"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(!runs(&stage, "p2"));
    assert!(
        !stage.state().join("placing/p2").exists(),
        "a note of a closed terminal goes with it"
    );

    // With the bateri app the question is its, whoever opened it: it closes the pane and the terminal, and
    // the command ends nothing itself unless it was told to kill.
    stage.app_says(OK);
    let said = stage.run_in("p9", &["close", "pane", "p3"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(stage.told, ["bateri 2\nclose\nterminal p3\n"]);
    assert_eq!(
        parse_told(&stage.told[0]),
        Request::Close(Anchor::of(Terminal(3)))
    );
    assert!(runs(&stage, "p3"), "the bateri app closes it, not this");
    let said = stage.run_in("p9", &["close", "pane", "p3", "--force"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(!runs(&stage, "p3"), "a kill is kablo's to do");

    // The bateri app refuses by the level it has for this machine, and says which and how to change it; it
    // does not know the pane; it could not tell.
    stage.forget_told();
    stage.app_says("bateri 2\nerror refused this holds a pane of another machine, which \"own\" does not change on rasp (level \"own\"); set `commands` in the [remote] hosts entry for rasp to \"screen\" in settings.toml to allow it\n");
    let said = stage.run_in("p9", &["close", "pane", "p4"]);
    assert_eq!(said.status, 77);
    assert!(said.err.contains("\"screen\""), "{}", said.err);
    assert!(runs(&stage, "p4"));
    stage.app_says("bateri 2\nerror unknown p4 is no pane on this server\n");
    let said = stage.run_in("p9", &["close", "pane", "p4"]);
    assert_eq!(
        (said.status, said.err.as_str()),
        (1, "bateri: p4 is no pane on this server\n")
    );
    assert!(runs(&stage, "p4"));
    // The bateri app could not tell: the terminal is this machine's own all the same.
    stage.app_says("bateri 2\nerror failed the server did not answer\n");
    let said = stage.run_in("p9", &["close", "pane", "p4"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(!runs(&stage, "p4"));

    // A pane of another machine is the bateri app's to close, or nobody's.
    stage.no_app();
    let said = stage.run_in("p9", &["close", "pane", "p7@prod"]);
    assert_eq!(said.status, 69, "{}", said.err);
    assert!(
        said.err
            .contains("a pane of another machine is closed through it")
    );
    // Outside every terminal a named pane is closed too; named nothing there is none to close.
    stage.terminal("p6");
    assert_eq!(stage.run(&["close", "pane", "p6"]).status, 0);
    assert!(!runs(&stage, "p6"));
    assert_eq!(stage.run(&["close", "pane"]).status, 64);
}

#[test]
fn a_terminal_named_nothing_is_the_callers_own_whatever_it_is_called() {
    let made = format!("b-{}", "0123456789abcdef".repeat(2));
    let mut stage = Stage::new();
    // With the bateri app the bateri app is told which terminal, so that it closes the pane that shows it.
    stage.app_says(OK);
    let said = stage.run_in(&made, &["close", "pane"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(
        parse_told(&stage.told[0]),
        Request::Close(Anchor::parse(&made).expect("a name"))
    );
    assert_eq!(stage.told[0], format!("bateri 2\nclose\nterminal {made}\n"));
    // The rule that a target is a number is for what is written: this one is not a number.
    stage.forget_told();
    let said = stage.run_in("p4", &["close", "pane", &made]);
    assert_eq!(said.status, 64);
    assert!(stage.told.is_empty());
    // No bateri app: kablo ends it by the name it has.
    let mut stage = Stage::new();
    stage.run_ending_in(&made, &["close", "pane"]);
    assert!(!runs(&stage, &made));
}

#[test]
fn ls_prints_what_the_app_says_and_without_one_what_the_server_has() {
    let mut stage =
        Stage::with_app("bateri 2\nok text\nWindow 1\n  Tab 1.1\n\u{1b}[31mred\u{7}\n\n");
    // The bateri app's text, byte for byte but for the bytes a terminal would act on.
    let said = stage.run_in("p4", &["ls"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "Window 1\n  Tab 1.1\n[31mred\n\n");
    assert_eq!(stage.told, ["bateri 2\nls\n"]);
    stage.forget_told();
    stage.app_says("bateri 2\nok text\n[]\n");
    let said = stage.run_in("p4", &["ls", "--json"]);
    assert_eq!(said.out, "[]\n");
    assert_eq!(stage.told, ["bateri 2\nls\nformat json\n"]);

    // No bateri app: kablo's own list and what waits to be placed.
    stage.no_app();
    stage.terminal("p1");
    let note = stage.state().join("placing");
    std::fs::create_dir_all(&note).unwrap();
    std::fs::write(
        note.join("p1"),
        "bateri 2\nplace\nterminal p1\nanchor p4\nside below\nratio 30\n",
    )
    .unwrap();
    let said = stage.run_in("p4", &["ls"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(said.out.starts_with("ID"), "{}", said.out);
    assert!(
        said.out
            .ends_with("\nWaiting for the bateri app to place:\n  p1  below p4 (30%)\n"),
        "{}",
        said.out
    );
    assert!(
        said.err
            .starts_with("bateri: the bateri app is not connected"),
        "{}",
        said.err
    );
    let said = stage.run_in("p4", &["ls", "--json"]);
    assert!(said.out.starts_with('['), "{}", said.out);
    // The bateri app's mistakes are the status, as for any request...
    stage.app_says("bateri 2\nerror usage no such format\n");
    assert_eq!(stage.run_in("p4", &["ls"]).status, 64);
    // ...but one that is only that it could not tell now leaves the server's own list.
    stage.app_says("bateri 2\nerror failed the server did not give its terminals in time\n");
    let said = stage.run_in("p4", &["ls"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(said.out.starts_with("ID"), "{}", said.out);
    assert!(
        said.err
            .starts_with("bateri: the bateri app could not tell what is on screen:"),
        "{}",
        said.err
    );
    assert_eq!(stage.run_in("p4", &["ls", "--bogus"]).status, 64);
}

#[test]
fn notify_needs_an_app_and_sends_one_line() {
    let mut stage = Stage::new();
    let said = stage.run_in("p4", &["notify", "the build is done"]);
    assert_eq!(said.status, 69);
    assert_eq!(
        said.err,
        "bateri: the bateri app is not connected to show it\n"
    );
    stage.app_says(OK);
    let said = stage.run_in("p4", &["notify", "3 tests\nfailed:", "\u{1b}[31mx\ty"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(
        parse_told(told_one(&stage)),
        Request::Notify("3 tests failed: [31mx y".to_owned())
    );
    let long = "x".repeat(1001);
    assert_eq!(stage.run_in("p4", &["notify", &long]).status, 64);
    assert_eq!(stage.run_in("p4", &["notify"]).status, 64);
    assert_eq!(stage.told.len(), 1, "nothing else reached the bateri app");
}

#[test]
fn the_layout_verbs_are_the_apps_alone_and_say_what_they_asked() {
    let mut stage = Stage::new();
    // With no bateri app they are 69, and the words say what is missing.
    for args in [
        &["focus", "tab", "t3"][..],
        &["zoom", "p9"],
        &["swap", "p9", "p3"],
        &["move", "pane", "p9", "--beside", "p3", "--right"],
        &["resize", "p9", "--left"],
    ] {
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 69, "{args:?}: {}", said.err);
        assert_eq!(
            said.err,
            "bateri: the bateri app is not connected to do it\n"
        );
    }
    stage.app_says(OK);
    let tries: Vec<(Vec<&str>, Arrange)> = vec![
        (
            vec!["focus", "tab", "t3"],
            Arrange::Focus(Subject::Tab(id(IdKind::Tab, 3))),
        ),
        (vec!["focus", "tab"], Arrange::Focus(Subject::Tab(None))),
        (
            vec!["focus", "window", "w2"],
            Arrange::Focus(Subject::Window(id(IdKind::Window, 2))),
        ),
        (vec!["focus", "group"], Arrange::Focus(Subject::Group(None))),
        (
            vec!["focus", "pane", "p9"],
            Arrange::Focus(Subject::Pane(Some(pane(9)))),
        ),
        (
            vec!["focus", "pane"],
            Arrange::Focus(Subject::Pane(Some(pane(4)))),
        ),
        (vec!["zoom", "p9"], Arrange::Zoom(Some(pane(9)))),
        (vec!["zoom"], Arrange::Zoom(None)),
        (
            vec!["swap", "p9", "p3"],
            Arrange::Swap {
                pane: pane(9),
                other: pane(3),
            },
        ),
        (
            vec!["move", "pane", "p9", "--beside", "p3", "--right"],
            Arrange::Move {
                pane: pane(9),
                beside: pane(3),
                side: Side::Right,
            },
        ),
        (
            vec!["resize", "p9", "--left", "--by", "3"],
            Arrange::Resize {
                pane: Some(pane(9)),
                side: Side::Left,
                steps: 3,
            },
        ),
        (
            vec!["resize", "--below"],
            Arrange::Resize {
                pane: None,
                side: Side::Below,
                steps: 1,
            },
        ),
    ];
    for (args, arrange) in &tries {
        stage.forget_told();
        stage.app_says(if args[0] == "move" { MOVED } else { OK });
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert_eq!(said.out, "");
        assert_eq!(
            parse_told(told_one(&stage)),
            Request::Arrange(arrange.clone()),
            "{args:?}"
        );
    }
    // Run in no terminal the caller's own is nothing; a named one is still fine.
    stage.app_says(OK);
    let said = stage.run(&["focus", "tab"]);
    assert_eq!(said.status, 64, "{}", said.err);
    assert!(said.err.contains("run in no terminal"), "{}", said.err);
    let said = stage.run(&["focus", "tab", "t3"]);
    assert_eq!(said.status, 0, "{}", said.err);
    // The bateri app refuses what would take the keyboard: 77, and its words.
    stage.app_says("bateri 2\nerror refused the keyboard is in another pane\n");
    let said = stage.run_in("p4", &["focus", "pane", "p9"]);
    assert_eq!(said.status, 77, "{}", said.err);
    assert_eq!(said.err, "bateri: the keyboard is in another pane\n");
    stage.app_says("bateri 2\nerror unknown t9 is no tab\n");
    let said = stage.run_in("p4", &["focus", "tab", "t9"]);
    assert_eq!(said.status, 1, "{}", said.err);
    // The bateri app that answers something that is no reply is said so.
    stage.app_says("what is this\n");
    let said = stage.run_in("p4", &["focus", "tab", "t9"]);
    assert_eq!(said.status, 1);
    assert_eq!(
        said.err,
        "bateri: the bateri app answered with something this does not read\n"
    );
}

#[test]
fn a_word_and_an_id_of_another_kind_is_64_and_says_what_to_write() {
    let mut stage = Stage::with_app(OK);
    let said = stage.run_in("p4", &["focus", "group", "t3"]);
    assert_eq!(said.status, 64, "{}", said.err);
    assert_eq!(
        said.err,
        "bateri: t3 is a tab, not a group. Did you mean: bateri focus tab t3\n"
    );
    let said = stage.run_in("p4", &["focus", "tab", "w1"]);
    assert!(
        said.err.contains("w1 is a window, not a tab"),
        "{}",
        said.err
    );
    let said = stage.run_in("p4", &["zoom", "t3"]);
    assert_eq!(said.status, 64, "{}", said.err);
    assert_eq!(
        said.err,
        "bateri: t3 is a tab, not a pane: zoom takes a pane\n"
    );
    for args in [
        &["focus"][..],
        &["focus", "frame", "t3"],
        &["focus", "tab", "t3", "t4"],
        &["focus", "tab", "t03"],
        &["focus", "tab", "web"],
        &["focus", "tab", "--json"],
        &["zoom", "p4", "p5"],
        &["swap", "p4"],
        &["swap", "p4", "p4"],
        &["swap", "p4", "t3"],
        &["move"],
        &["move", "tab", "t3", "--beside", "p3", "--right"],
        &["move", "pane", "p9", "--right"],
        &["move", "pane", "p9", "--beside", "p3"],
        &["move", "pane", "p9", "--beside", "p9", "--right"],
        &["move", "pane", "p9", "--beside", "p3", "--right", "--left"],
        &["move", "pane", "p9", "p8", "--beside", "p3", "--right"],
        &["resize", "p9"],
        &["resize", "p9", "--left", "--right"],
        &["resize", "p9", "--left", "--by", "0"],
        &["resize", "p9", "--left", "--by", "51"],
        &["resize", "p9", "--left", "--by", "x"],
        &["resize", "p9", "p8", "--left"],
        &["resize", "--bogus", "--left"],
    ] {
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 64, "{args:?}: {}", said.err);
    }
    assert!(
        stage.told.is_empty(),
        "nothing reached the bateri app: {:?}",
        stage.told
    );
}

#[test]
fn close_names_its_kind_and_a_tab_a_group_or_a_window_is_the_apps_alone() {
    let mut stage = Stage::new();
    // No kind, or an id where the kind goes: 64, and what to write. Nothing is asked of kablo or of
    // the bateri app.
    for (args, hint) in [
        (&["close"][..], "Did you mean: bateri close pane"),
        (&["close", "p9"], "Did you mean: bateri close pane p9"),
        (&["close", "t3"], "Did you mean: bateri close tab t3"),
        (&["close", "g1"], "Did you mean: bateri close group g1"),
        (&["close", "w2"], "Did you mean: bateri close window w2"),
        (
            &["close", "--force"],
            "close takes pane, tab, group or window",
        ),
        (
            &["close", "frame", "t3"],
            "close takes pane, tab, group or window",
        ),
        (&["close", "tab", "g1"], "g1 is a group, not a tab"),
        (&["close", "tab", "t3", "t4"], "close tab names one id"),
        (&["close", "tab", "t03"], "is not a tab"),
        (&["close", "tab", "--force"], "close tab takes no option"),
        (&["close", "window", "web"], "is not a window"),
        (
            &["close", "pane", "t3"],
            "Did you mean: bateri close tab t3",
        ),
    ] {
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 64, "{args:?}: {}", said.err);
        assert!(said.err.contains(hint), "{args:?}: {}", said.err);
    }
    // No bateri app: a tab, a group and a window belong to its windows, so they are 69.
    for args in [
        &["close", "tab", "t3"][..],
        &["close", "group", "g1"],
        &["close", "window", "w1"],
        &["close", "tab"],
    ] {
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 69, "{args:?}: {}", said.err);
    }
    // With one the bateri app is told which layer, or none: the caller's own.
    stage.app_says(OK);
    let tries: Vec<(Vec<&str>, Arrange)> = vec![
        (
            vec!["close", "tab", "t3"],
            Arrange::Close(Subject::Tab(id(IdKind::Tab, 3))),
        ),
        (vec!["close", "tab"], Arrange::Close(Subject::Tab(None))),
        (
            vec!["close", "group", "g1"],
            Arrange::Close(Subject::Group(id(IdKind::Group, 1))),
        ),
        (
            vec!["close", "window"],
            Arrange::Close(Subject::Window(None)),
        ),
    ];
    for (args, arrange) in &tries {
        stage.forget_told();
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert_eq!(said.out, "");
        assert_eq!(
            parse_told(told_one(&stage)),
            Request::Arrange(arrange.clone()),
            "{args:?}"
        );
        if args == &["close", "tab", "t3"] {
            assert_eq!(
                stage.told[0], "bateri 2\nclose\nsubject tab\nid t3\n",
                "the words are frozen"
            );
        }
    }
    // Run in no terminal the caller's own is nothing.
    assert_eq!(stage.run(&["close", "tab"]).status, 64);
    // The level for this machine refuses (77); a tab that is not there is 1.
    stage.app_says("bateri 2\nerror refused this holds a pane of another machine\n");
    assert_eq!(stage.run_in("p4", &["close", "tab", "t3"]).status, 77);
    stage.app_says("bateri 2\nerror unknown there is no tab t9\n");
    let said = stage.run_in("p4", &["close", "tab", "t9"]);
    assert_eq!(
        (said.status, said.err.as_str()),
        (1, "bateri: there is no tab t9\n")
    );
}

#[test]
fn rename_gives_a_tab_or_a_group_a_label_through_the_app_and_a_pane_one_through_kablo() {
    let mut stage = Stage::new();
    stage.terminal("p9");
    // A pane's label is kablo's: the bateri app is not asked, an empty one clears.
    let labelled = |stage: &Stage| {
        let listed = stage.env.kablo().args(["list", "--json"]).output().unwrap();
        String::from_utf8_lossy(&listed.stdout).into_owned()
    };
    let said = stage.run_in("p4", &["rename", "pane", "p9", "server"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(
        labelled(&stage).contains("\"label\":\"server\""),
        "{}",
        labelled(&stage)
    );
    let said = stage.run_in("p4", &["rename", "pane", "p9", ""]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(!labelled(&stage).contains("server"), "{}", labelled(&stage));
    // Words after the id are the label, as `notify`'s are.
    stage.run_in("p4", &["rename", "pane", "p9", "unit", "tests"]);
    assert!(
        labelled(&stage).contains("\"label\":\"unit tests\""),
        "{}",
        labelled(&stage)
    );
    assert!(stage.told.is_empty());

    // What is wrong is 64 before anything is asked.
    let long = "x".repeat(65);
    for (args, hint) in [
        (&["rename"][..], "rename names what gets a label"),
        (&["rename", "window", "w1", "x"], "a window has no label"),
        (
            &["rename", "frame", "t3", "x"],
            "rename takes pane, tab or group",
        ),
        (&["rename", "tab"], "rename names the tab to label"),
        (&["rename", "tab", "web"], "is not a tab"),
        (&["rename", "tab", "t3"], "rename needs a label"),
        (
            &["rename", "tab", "g1", "web"],
            "Did you mean: bateri rename group g1",
        ),
        (&["rename", "group", "g1", ""], "a group always has a label"),
        (&["rename", "tab", "t3", &long], "1 to 64 bytes"),
        (
            &["rename", "tab", "t3", "a\nwindow w1"],
            "one line of plain text",
        ),
        (&["rename", "tab", "t3", "   "], "only spaces"),
        (
            &["rename", "pane", "t3", "x"],
            "Did you mean: bateri rename tab t3",
        ),
        (&["rename", "pane", "p9@prod", "x"], "rename the pane there"),
        (&["rename", "tab", "t3", "t4"], "label 't4' reads as an id"),
    ] {
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 64, "{args:?}: {}", said.err);
        assert!(said.err.contains(hint), "{args:?}: {}", said.err);
    }
    assert!(stage.told.is_empty());

    // A tab's and a group's are the bateri app's: 69 with none.
    let said = stage.run_in("p4", &["rename", "tab", "t3", "web"]);
    assert_eq!(said.status, 69, "{}", said.err);
    stage.app_says(OK);
    let tries: Vec<(Vec<&str>, Arrange)> = vec![
        (
            vec!["rename", "tab", "t3", "web"],
            Arrange::Rename {
                subject: Subject::Tab(id(IdKind::Tab, 3)),
                label: Some("web".to_owned()),
            },
        ),
        (
            vec!["rename", "tab", "t3", ""],
            Arrange::Rename {
                subject: Subject::Tab(id(IdKind::Tab, 3)),
                label: None,
            },
        ),
        (
            vec!["rename", "group", "g1", "api", "v2"],
            Arrange::Rename {
                subject: Subject::Group(id(IdKind::Group, 1)),
                label: Some("api v2".to_owned()),
            },
        ),
    ];
    for (args, arrange) in &tries {
        stage.forget_told();
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert_eq!(
            parse_told(told_one(&stage)),
            Request::Arrange(arrange.clone()),
            "{args:?}"
        );
        if args == &["rename", "tab", "t3", "web"] {
            assert_eq!(
                stage.told[0],
                "bateri 2\nrename\nsubject tab\nid t3\nlabel web\n"
            );
        }
    }
}

#[test]
fn move_takes_a_pane_or_a_tab_where_it_says_and_prints_what_it_made() {
    let group = id(IdKind::Group, 1);
    let window = id(IdKind::Window, 2);
    let tab = id(IdKind::Tab, 3).expect("an id");
    let named = |text: &str| Some(text.to_owned());
    let at = "{\"pane\":\"p9\",\"tab\":\"t7\",\"group\":\"g2\",\"window\":\"w1\"}\n";
    let mut stage = Stage::new();
    for (args, hint) in [
        (
            &["move", "pane", "p9", "--new-tab", "--right"][..],
            "a new tab has no side",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "--beside", "p3"],
            "one of them",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "--tab", "t3"],
            "one of them",
        ),
        (&["move", "pane", "p9"], "move needs a place"),
        (
            &["move", "pane", "p9", "--group", "g1"],
            "go with --new-tab",
        ),
        (
            &["move", "pane", "p9", "--tab", "g1"],
            "g1 is a group, not a tab",
        ),
        (&["move", "pane", "p9", "--tab"], "--tab takes a tab"),
        // The word after --new-tab is a name: an id there is the pane in the wrong place, and it
        // must not move the caller's pane into a tab called p12.
        (
            &["move", "pane", "--new-tab", "p12"],
            "the pane goes before --new-tab",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "t3"],
            "the pane goes before --new-tab",
        ),
        (
            &["move", "pane", "p9", "--new-tab=g1"],
            "the pane goes before --new-tab",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "web", "api"],
            "a new tab's name goes after --new-tab",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "--label", "web"],
            "move takes no --label",
        ),
        (
            &["move", "pane", "p9", "--new-tab="],
            "--new-tab is 1 to 64 bytes",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "a", "--new-tab", "b"],
            "name one new tab",
        ),
        (
            &["move", "pane", "t3", "--new-tab"],
            "t3 is a tab, not a pane",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "--group", "t3"],
            "t3 is a tab, not a group",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "--window", "new"],
            "'new' is not a window",
        ),
        (
            &[
                "move",
                "pane",
                "p9",
                "--new-tab",
                "--group",
                "g1",
                "--window",
                "w2",
            ],
            "a group or into a window, not both",
        ),
        (
            &[
                "move",
                "pane",
                "p9",
                "--new-tab",
                "--group",
                "g1",
                "--new-group",
                "api",
            ],
            "a group or into a new group, not both",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "--new-group", "  "],
            "--new-group is not only spaces",
        ),
        (
            &["move", "pane", "p9", "--new-tab", "--new-group", "g1"],
            "--new-group 'g1' reads as an id",
        ),
        (
            &["move", "tab", "t3", "--new-group", "api", "--group", "g1"],
            "a group or into a new group, not both",
        ),
        (
            &["move", "tab", "t3", "--new-group"],
            "--new-group takes the name of the group",
        ),
        (
            &["move", "tab", "t3", "--group", "g1", "--label", "web"],
            "move tab takes no --label",
        ),
    ] {
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 64, "{args:?}: {}", said.err);
        assert!(said.err.contains(hint), "{args:?}: {}", said.err);
    }
    assert!(stage.told.is_empty());
    // In no terminal there is no pane or tab of its own to take.
    for (args, hint) in [
        (&["move", "pane", "--new-tab"][..], "name the pane to move"),
        (&["move", "tab", "--group", "g1"], "name the tab to move"),
    ] {
        let said = stage.run(args);
        assert_eq!(said.status, 64, "{args:?}: {}", said.err);
        assert!(said.err.contains(hint), "{args:?}: {}", said.err);
    }
    // With no bateri app nothing was made, and the words say what is missing.
    let said = stage.run_in("p4", &["move", "pane", "p9", "--new-tab"]);
    assert_eq!(said.status, 69, "{}", said.err);
    assert_eq!(
        said.err,
        "bateri: the bateri app is not connected to do it\n"
    );

    stage.app_says(AT);
    let tries: Vec<(Vec<&str>, Arrange, &str, &str)> = vec![
        (
            vec!["move", "pane", "p9", "--new-tab"],
            Arrange::PaneToNewTab {
                pane: pane(9),
                home: Home::default(),
            },
            "bateri 2\nmove\nterminal p9\nside tab\n",
            "t7\n",
        ),
        (
            vec![
                "move",
                "pane",
                "p9",
                "--new-tab",
                "foo",
                "--new-group",
                "hello",
            ],
            Arrange::PaneToNewTab {
                pane: pane(9),
                home: Home {
                    new_group: named("hello"),
                    label: named("foo"),
                    ..Home::default()
                },
            },
            "bateri 2\nmove\nterminal p9\nside tab\nnew-group hello\nlabel foo\n",
            "t7\n",
        ),
        (
            vec!["move", "pane", "p9", "--new-tab=-odd"],
            Arrange::PaneToNewTab {
                pane: pane(9),
                home: Home {
                    label: named("-odd"),
                    ..Home::default()
                },
            },
            "bateri 2\nmove\nterminal p9\nside tab\nlabel -odd\n",
            "t7\n",
        ),
        // With none named, the one this runs in.
        (
            vec!["move", "pane", "--new-tab", "web", "--group", "g1"],
            Arrange::PaneToNewTab {
                pane: pane(4),
                home: Home {
                    group,
                    label: named("web"),
                    ..Home::default()
                },
            },
            "bateri 2\nmove\nterminal p4\nside tab\ngroup g1\nlabel web\n",
            "t7\n",
        ),
        (
            vec![
                "move",
                "pane",
                "p9",
                "--new-tab",
                "--new-group=api",
                "--window=w2",
            ],
            Arrange::PaneToNewTab {
                pane: pane(9),
                home: Home {
                    new_group: named("api"),
                    window: window.map(WindowHome::Existing),
                    ..Home::default()
                },
            },
            "bateri 2\nmove\nterminal p9\nside tab\nnew-group api\nwindow w2\n",
            "t7\n",
        ),
        (
            vec!["move", "pane", "p9", "--new-tab", "--json"],
            Arrange::PaneToNewTab {
                pane: pane(9),
                home: Home::default(),
            },
            "bateri 2\nmove\nterminal p9\nside tab\n",
            at,
        ),
        // What makes nothing prints nothing, and says where it stands if asked.
        (
            vec!["move", "pane", "p9", "--tab", "t3"],
            Arrange::PaneIntoTab {
                pane: pane(9),
                tab,
                side: None,
            },
            "bateri 2\nmove\nterminal p9\ntab t3\n",
            "",
        ),
        (
            vec!["move", "pane", "p9", "--tab=t3", "--left", "--json"],
            Arrange::PaneIntoTab {
                pane: pane(9),
                tab,
                side: Some(Side::Left),
            },
            "bateri 2\nmove\nterminal p9\ntab t3\nside left\n",
            at,
        ),
        (
            vec!["move", "pane", "p9", "--beside", "p3", "--right", "--json"],
            Arrange::Move {
                pane: pane(9),
                beside: pane(3),
                side: Side::Right,
            },
            "bateri 2\nmove\nterminal p9\nbeside p3\nside right\n",
            at,
        ),
        (
            vec!["move", "tab", "t3", "--new-group", "api"],
            Arrange::MoveTab {
                tab: Some(tab),
                to: TabHome::NewGroup {
                    name: "api".to_owned(),
                    window: None,
                },
            },
            "bateri 2\nmove\nsubject tab\nid t3\nnew-group api\n",
            "g2\n",
        ),
        (
            vec!["move", "tab", "--new-group", "api"],
            Arrange::MoveTab {
                tab: None,
                to: TabHome::NewGroup {
                    name: "api".to_owned(),
                    window: None,
                },
            },
            "bateri 2\nmove\nsubject tab\nnew-group api\n",
            "g2\n",
        ),
        (
            vec![
                "move",
                "tab",
                "t3",
                "--window",
                "w2",
                "--new-group",
                "api",
                "--json",
            ],
            Arrange::MoveTab {
                tab: Some(tab),
                to: TabHome::NewGroup {
                    name: "api".to_owned(),
                    window,
                },
            },
            "bateri 2\nmove\nsubject tab\nid t3\nnew-group api\nwindow w2\n",
            "{\"tab\":\"t7\",\"group\":\"g2\",\"window\":\"w1\"}\n",
        ),
        (
            vec!["move", "tab", "t3", "--group", "g1", "--json"],
            Arrange::MoveTab {
                tab: Some(tab),
                to: TabHome::Group(group.expect("an id")),
            },
            "bateri 2\nmove\nsubject tab\nid t3\ngroup g1\n",
            "{\"tab\":\"t7\",\"group\":\"g2\",\"window\":\"w1\"}\n",
        ),
    ];
    for (args, arrange, text, out) in &tries {
        stage.forget_told();
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert_eq!(said.err, "", "{args:?}");
        assert_eq!(said.out, *out, "{args:?}");
        assert_eq!(told_one(&stage), *text, "{args:?}");
        assert_eq!(
            parse_told(&stage.told[0]),
            Request::Arrange(arrange.clone()),
            "{args:?}"
        );
    }
    // A tab in no group: `null` where the bateri app said nothing.
    stage.app_says(MOVED);
    let said = stage.run_in("p4", &["move", "pane", "p9", "--new-tab", "--json"]);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(
        said.out,
        "{\"pane\":\"p9\",\"tab\":\"t7\",\"group\":null,\"window\":\"w1\"}\n"
    );
    // What the bateri app refuses is said, and exits as a refusal does.
    stage.app_says("bateri 2\nerror refused the person is typing there\n");
    let said = stage.run_in("p4", &["move", "tab", "t3", "--new-group", "api"]);
    assert_eq!(said.status, 77, "{}", said.err);
    assert_eq!(said.out, "");
}

#[test]
fn move_tab_goes_into_a_group_or_a_window_and_nothing_else() {
    let mut stage = Stage::new();
    for (args, hint) in [
        (&["move"][..], "move names what moves"),
        (&["move", "group", "g1"], "move takes a pane or a tab"),
        (&["move", "tab"], "move tab needs a place"),
        (&["move", "tab", "t3"], "move tab needs a place"),
        (
            &["move", "tab", "t3", "--group", "g1", "--window", "w1"],
            "a group or into a window, not both",
        ),
        (
            &["move", "tab", "t3", "--group", "t4"],
            "t4 is a tab, not a group",
        ),
        (
            &["move", "tab", "t3", "--window", "g1"],
            "g1 is a group, not a window",
        ),
        (
            &["move", "tab", "p9", "--group", "g1"],
            "Did you mean: bateri move pane p9",
        ),
        (
            &["move", "tab", "t3", "t4", "--group", "g1"],
            "move names one tab",
        ),
        (&["move", "tab", "t3", "--group"], "--group takes a group"),
        (
            &["move", "tab", "t3", "--right"],
            "move tab takes no option",
        ),
        (
            &["move", "tab", "t3", "--group", "g1", "--group", "g2"],
            "name one group",
        ),
    ] {
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 64, "{args:?}: {}", said.err);
        assert!(said.err.contains(hint), "{args:?}: {}", said.err);
    }
    assert!(stage.told.is_empty());
    let said = stage.run_in("p4", &["move", "tab", "t3", "--group", "g1"]);
    assert_eq!(said.status, 69, "{}", said.err);

    stage.app_says(MOVED);
    let tab = id(IdKind::Tab, 3).expect("an id");
    let tries: Vec<(Vec<&str>, Arrange)> = vec![
        (
            vec!["move", "tab", "t3", "--group", "g1"],
            Arrange::MoveTab {
                tab: Some(tab),
                to: TabHome::Group(id(IdKind::Group, 1).expect("an id")),
            },
        ),
        (
            vec!["move", "tab", "t3", "--window=w2"],
            Arrange::MoveTab {
                tab: Some(tab),
                to: TabHome::Window(id(IdKind::Window, 2).expect("an id")),
            },
        ),
    ];
    for (args, arrange) in &tries {
        stage.forget_told();
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert_eq!(
            parse_told(told_one(&stage)),
            Request::Arrange(arrange.clone()),
            "{args:?}"
        );
        if args.contains(&"--group") {
            assert_eq!(
                stage.told[0],
                "bateri 2\nmove\nsubject tab\nid t3\ngroup g1\n"
            );
        }
    }
}

#[test]
fn the_words_of_a_command_are_held_to_the_same_rules_in_every_verb() {
    let mut stage = Stage::new();
    for (args, hint) in [
        // A verb that says the kind first tells what to write, whichever it is.
        (&["focus", "p9"][..], "Did you mean: bateri focus pane p9"),
        (&["close", "t3"], "Did you mean: bateri close tab t3"),
        (
            &["rename", "g1", "x"],
            "Did you mean: bateri rename group g1",
        ),
        (
            &["move", "p9", "--new-tab"],
            "Did you mean: bateri move pane p9",
        ),
        (&["move", "g1"], "move takes a pane or a tab, not 'g1'"),
        // Alone a pane is no text to type.
        (&["send", "p9"], "send needs the text: send p9 TEXT"),
    ] {
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 64, "{args:?}: {}", said.err);
        assert!(said.err.contains(hint), "{args:?}: {}", said.err);
    }
    assert!(stage.told.is_empty());
    // `--help` is where a command's options are, and only there.
    for args in [
        &["move", "pane", "p9", "--help"][..],
        &["close", "-h"],
        &["ls", "--help"],
        &["help"],
    ] {
        let said = stage.run_in("p4", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
        assert!(said.out.starts_with("usage: bateri split"), "{args:?}");
    }
    // A word that is no command is named; no word at all is the usage.
    let said = stage.run_in("p4", &["new"]);
    assert_eq!(said.status, 64, "{}", said.err);
    assert!(
        said.err.starts_with("bateri: no command 'new'\n"),
        "{}",
        said.err
    );
    assert!(!said.err.contains("bateri new"), "{}", said.err);
    let said = stage.run_in("p4", &[]);
    assert_eq!(said.status, 64);
    assert!(said.err.starts_with("usage: bateri split"), "{}", said.err);
    assert!(stage.told.is_empty());
}
