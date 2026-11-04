//! `kablo bateri` for a pane of a machine other than this one (`-S prod`, `p9@prod`), run for real.
//! The Mac is a test that answers the way the window does: the words of an `ssh` (which is a fake
//! `ssh` that hands the line to a login shell in the home of "prod", a machine of its own with its
//! own terminals), `relay`, or `here`. What the command says to the Mac is read back with the
//! window's own reader (`kablo-bateri`).

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;
mod stage;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kablo_bateri::message::{Arrange, Home, Place, Reach, Relay, Relayed, Reply, Request, Subject};
use kablo_bateri::{Anchor, Side, Terminal};
use stage::{Run, Stage, parse_told};

const PLACED: &str = "bateri 2\nok placed\nlanded as_asked\nnext-to p40\nsqueezed no\nhidden no\n";
const MOVED: &str =
    "bateri 2\nok placed\nlanded as_asked\nsqueezed no\nhidden no\ntab t7\nwindow w1\n";
const OK: &str = "bateri 2\nok\n";
const REFUSED: &str =
    "bateri 2\nerror refused this reaches another machine, which only \"full\" does\n";
const NO_TAB: &str = "bateri 2\nerror unknown there is no tab t9\n";

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

/// The Mac's answer to the question how to reach `prod`: the words of an `ssh` command.
fn ssh_words() -> String {
    Reply::Text(
        Reach::Words(
            ["-T", "-o", "BatchMode=yes", "prod"]
                .map(str::to_owned)
                .to_vec(),
        )
        .text(),
    )
    .encode()
}

/// The Mac's answer to a program on a server that asks how: `relay`.
fn relay_word() -> String {
    Reply::Text(Reach::Relay.text()).encode()
}

/// The Mac's answer when the machine asked about is the asker's own.
fn here_word() -> String {
    Reply::Text(Reach::Here.text()).encode()
}

/// The Mac's answer to a relay: how the command ended and what it said.
fn relayed(status: u8, stderr: &str, stdout: &str) -> String {
    Reply::Relayed(Relayed::of(status, stderr, stdout)).encode()
}

/// What a program asks of the Mac to run `words` on `server`.
fn relay_text(server: &str, words: &[&str]) -> String {
    Request::Relay(Relay {
        server: server.to_owned(),
        words: words.iter().map(|word| (*word).to_owned()).collect(),
    })
    .encode()
    .unwrap()
}

fn ssh_text(server: &str) -> String {
    Request::Ssh(server.to_owned()).encode().unwrap()
}

/// The shells a person's login may be, those this machine has: the line an `ssh` carries is
/// handed to one of them, and is read the same by each.
fn login_shells() -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut real: Vec<PathBuf> = Vec::new();
    for shell in [
        "/bin/sh",
        "/bin/dash",
        "/bin/bash",
        "/bin/zsh",
        "/usr/bin/zsh",
        "/usr/bin/fish",
        "/opt/homebrew/bin/fish",
        "/usr/local/bin/fish",
    ] {
        if let Ok(path) = std::fs::canonicalize(shell) {
            if !real.contains(&path) {
                real.push(path);
                found.push(shell.to_owned());
            }
        }
    }
    found
}

/// A Mac that reaches a server `prod` over a fake `ssh` whose far end is `shell`.
fn mac_and_prod(shell: &str) -> (Stage, Stage) {
    let mut prod = Stage::new();
    prod.install_command();
    let mut mac = Stage::new();
    mac.ssh_to(&prod, shell);
    mac.connect_mac();
    (mac, prod)
}

/// A program that says how it was started, its words NUL after NUL, into the files named by its
/// first word, and holds the terminal open.
fn recorder(stage: &Stage) -> String {
    stage
        .env
        .script(
            "recorder.sh",
            "#!/bin/sh\nout=$1\nshift\n{ for a in \"$@\"; do printf '%s\\0' \"$a\"; done; } > \"$out.args\"\npwd > \"$out.cwd\"\n: > \"$out.done\"\nexec cat\n",
        )
        .display()
        .to_string()
}

/// What the recorder wrote under `out`: the words, and the directory.
fn recorded(out: &Path) -> (Vec<String>, String) {
    let done = PathBuf::from(format!("{}.done", out.display()));
    let start = Instant::now();
    while !done.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the program did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let args = std::fs::read(format!("{}.args", out.display())).unwrap();
    let words = args
        .split(|byte| *byte == 0)
        .map(|word| String::from_utf8_lossy(word).into_owned())
        .collect::<Vec<_>>();
    // Each word ends in a NUL, so one empty piece is the end.
    let words = words[..words.len() - 1].to_vec();
    let cwd = std::fs::read_to_string(format!("{}.cwd", out.display())).unwrap();
    (words, cwd.trim_end().to_owned())
}

/// The terminals of `stage` that run.
fn running(stage: &Stage) -> Vec<String> {
    stage.running()
}

/// What the terminal `id` of `stage` shows.
fn shown(stage: &Stage, id: &str) -> String {
    let said = stage.env.kablo().args(["capture", id]).output().unwrap();
    String::from_utf8_lossy(&said.stdout).into_owned()
}

/// Waits until `shown` has `text`.
fn shows(stage: &Stage, id: &str, text: &str) -> bool {
    let start = Instant::now();
    loop {
        if shown(stage, id).contains(text) {
            return true;
        }
        if start.elapsed() > Duration::from_secs(20) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn address(text: &str) -> Anchor {
    Anchor::address(text).unwrap()
}

#[test]
fn a_split_on_a_server_asks_the_mac_how_to_reach_it_starts_the_terminal_there_and_asks_for_the_pane()
 {
    for shell in login_shells() {
        let (mut mac, prod) = mac_and_prod(&shell);
        let program = recorder(&prod);
        let out = prod.env.dir.join("one");
        let out_name = out.display().to_string();
        let ssh = ssh_words();
        let said = mac.go(&Run {
            here: Some("p40"),
            args: &[
                "split", "--below", "-S", "prod", "--", &program, &out_name, "make", "it's", "ok",
            ],
            input: None,
            turns: &[&ssh, PLACED],
            vars: &[],
        });
        assert_eq!(said.status, 0, "{shell}: {}", said.err);
        // The terminal is the server's `p1`, written by its machine.
        assert_eq!(said.out, "p1@prod\n", "{shell}: {}", said.err);
        assert_eq!(said.err, "", "a pane that landed as asked is silent");

        // The Mac was asked how, then for the pane: a terminal of the server, beside this Mac's.
        assert_eq!(mac.told.len(), 2, "{:?}", mac.told);
        assert_eq!(mac.told[0], ssh_text("prod"));
        let wanted = Place {
            side: Some(Side::Below),
            server: Some("prod".to_owned()),
            ..place(1, 40)
        };
        assert_eq!(mac.told[1], encoded(wanted));

        // ssh got the Mac's words and one command line.
        let calls = mac.ssh_calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0][..4], ["-T", "-o", "BatchMode=yes", "prod"]);
        assert_eq!(calls[0].len(), 5, "{calls:?}");

        // The server started the terminal, in its home, the program's words as they were written;
        // nothing was started or recorded on the Mac.
        let (words, cwd) = recorded(&out);
        assert_eq!(words, ["make", "it's", "ok"], "{shell}");
        assert_eq!(cwd, prod.home.display().to_string());
        assert!(running(&prod).contains(&"p1".to_owned()));
        assert!(!running(&mac).contains(&"p1".to_owned()));
        assert!(mac.record("p1").is_none());
        assert!(prod.record("p1").is_some());
        // The server was not asked to place anything: it ran in no terminal.
        assert!(prod.told.is_empty(), "{:?}", prod.told);
    }
}

#[test]
fn a_server_that_cannot_be_reached_is_said_so_before_or_after_anything_is_started() {
    let shell = login_shells().remove(0);
    let (mut mac, prod) = mac_and_prod(&shell);

    // A name that is not a name is a mistake before the Mac is asked.
    for bad in ["-oProxyCommand=x", "a b", "a;b", "", "p9@"] {
        let said = mac.run_in("p40", &["split", "-S", bad]);
        assert_eq!(said.status, 64, "{bad:?}: {}", said.err);
    }
    let said = mac.run_in("p40", &["split", "-S"]);
    assert_eq!(said.status, 64, "{}", said.err);
    let said = mac.run_in("p40", &["split", "-S", "prod", "-S", "web"]);
    assert_eq!(said.status, 64, "{}", said.err);
    assert!(mac.told.is_empty(), "{:?}", mac.told);

    // The level, the server's own `commands` and the gate of `ssh prod` are the Mac's: its words are
    // the command's, and nothing is started.
    mac.mac_says(REFUSED);
    let said = mac.run_in("p40", &["split", "-S", "prod", "--", "make"]);
    assert_eq!(said.status, 77, "{}", said.err);
    assert!(said.err.contains("only \"full\""), "{}", said.err);
    assert_eq!(said.out, "");
    mac.mac_says("bateri 2\nerror failed prod is not a server bateri keeps terminals on\n");
    let said = mac.run_in("p40", &["split", "-S", "prod", "--", "make"]);
    assert_eq!(said.status, 1, "{}", said.err);
    assert!(said.err.contains("keeps terminals on"), "{}", said.err);
    assert_eq!(mac.ssh_calls().len(), 0);
    assert!(running(&prod).is_empty());

    // A Mac that says something that is no answer.
    mac.mac_says("bateri 2\nok text\nnot this\n");
    let said = mac.run_in("p40", &["split", "-S", "prod", "--", "make"]);
    assert_eq!(said.status, 1, "{}", said.err);
    assert_eq!(said.out, "");

    // No Mac.
    mac.no_mac();
    let said = mac.run_in("p40", &["split", "-S", "prod"]);
    assert_eq!(said.status, 69, "{}", said.err);
    assert_eq!(mac.ssh_calls().len(), 0);

    // A server bateri never opened has no command: it is told to be, with nothing installed.
    std::fs::remove_file(prod.state().join("shell/bin/bateri")).unwrap();
    mac.mac_says(&ssh_words());
    let said = mac.run_in("p40", &["split", "-S", "prod", "--", "make"]);
    assert_eq!(said.status, 1, "{}", said.err);
    assert!(
        said.err.contains("not set up on prod") && said.err.contains("first"),
        "{}",
        said.err
    );
    assert!(running(&prod).is_empty());
}

#[test]
fn a_pane_the_mac_refuses_ends_the_terminal_the_server_started() {
    let shell = login_shells().remove(0);
    let (mut mac, prod) = mac_and_prod(&shell);
    let program = recorder(&prod);
    let out = prod.env.dir.join("one");
    let out_name = out.display().to_string();
    let ssh = ssh_words();
    // The pane is refused, and the Mac is asked no more: the terminal is ended where it is.
    let said = mac.go(&Run {
        here: Some("p40"),
        args: &[
            "split", "--tab", "t9", "-S", "prod", "--", &program, &out_name,
        ],
        input: None,
        turns: &[&ssh, NO_TAB],
        vars: &[],
    });
    assert_eq!(
        (said.status, said.err.as_str()),
        (1, "bateri: there is no tab t9\n"),
        "{}",
        said.err
    );
    assert_eq!(said.out, "");
    // The terminal was made (the server's counter went past `p1`) and is gone, and the program need
    // not have got as far as running.
    assert_eq!(
        std::fs::read_to_string(prod.state().join("next")).unwrap(),
        "2\n"
    );
    let start = Instant::now();
    while running(&prod).contains(&"p1".to_owned()) {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the terminal was not ended"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(prod.record("p1").is_none());
    // It was the server's own command that ended it, over the same ssh: two calls.
    assert_eq!(mac.ssh_calls().len(), 2, "{:?}", mac.ssh_calls());
}

#[test]
fn send_capture_and_wait_for_a_pane_of_a_server_run_there_and_keep_their_status() {
    for shell in login_shells() {
        let (mut mac, mut prod) = mac_and_prod(&shell);
        prod.terminal("p9");
        prod.terminal_running("p8", &["/bin/sh", "-c", "exit 7"]);
        mac.mac_says(&ssh_words());

        let said = mac.run_in("p40", &["send", "p9@prod", "echo 'hi' $HOME; ls"]);
        assert_eq!(said.status, 0, "{shell}: {}", said.err);
        // The text arrives as it was written: nothing of it is read by any shell on the way.
        assert!(
            shows(&prod, "p9", "echo 'hi' $HOME; ls"),
            "{shell}: {}",
            shown(&prod, "p9")
        );
        let said = mac.run_in("p40", &["send-key", "p9@prod", "Enter"]);
        assert_eq!(said.status, 0, "{}", said.err);

        // A key first is the caller's own terminal's: no machine is named, nobody is asked.
        let before = mac.told.len();
        let said = mac.run_in("p40", &["send-key", "Enter"]);
        assert_eq!(said.status, 0, "{}", said.err);
        assert_eq!(mac.told.len(), before);

        let said = mac.run_in("p40", &["capture", "--history", "20", "p9@prod", "--trim"]);
        assert_eq!(said.status, 0, "{}", said.err);
        assert!(said.out.contains("echo 'hi' $HOME; ls"), "{}", said.out);

        // The program's own status comes back through ssh.
        let said = mac.run_in("p40", &["wait", "p8@prod", "--for", "20s"]);
        assert_eq!(said.status, 7, "{shell}: {}", said.err);

        // Each went to the Mac for the connection first.
        assert!(
            mac.told
                .iter()
                .all(|text| text.starts_with("bateri 2\nssh\n")),
            "{:?}",
            mac.told
        );

        // This Mac's own pane written out is this Mac's, when the Mac says the machine is this one:
        // no ssh.
        let before = mac.ssh_calls().len();
        let here = here_word();
        let said = mac.go(&Run {
            here: Some("p40"),
            args: &["capture", "p40@local"],
            input: None,
            turns: &[&here],
            vars: &[],
        });
        assert_eq!(said.status, 0, "{}", said.err);
        assert_eq!(mac.ssh_calls().len(), before);
        assert_eq!(mac.told.last().unwrap(), &ssh_text("@local"));

        // A malformed address is no pane, and an unreachable server is the Mac's word.
        let said = mac.run_in("p40", &["capture", "p9@pr od"]);
        assert_eq!(said.status, 64, "{}", said.err);
        mac.mac_says(REFUSED);
        let said = mac.run_in("p40", &["wait", "p9@prod"]);
        assert_eq!(said.status, 77, "{}", said.err);
        assert!(said.err.contains("only \"full\""), "{}", said.err);
    }
}

#[test]
fn focus_close_and_the_layout_verbs_hand_the_address_of_a_pane_of_another_machine_to_the_mac() {
    let mut stage = Stage::new();
    stage.mac_says(OK);
    for args in [
        &["focus", "pane", "p9@prod"][..],
        &["zoom", "p9@prod"],
        &["swap", "p9@prod", "p3"],
    ] {
        let said = stage.run_in("p40", args);
        assert_eq!(said.status, 0, "{args:?}: {}", said.err);
    }
    stage.mac_says(MOVED);
    let said = stage.run_in(
        "p40",
        &["move", "pane", "p9@prod", "--beside", "p3@local", "--right"],
    );
    assert_eq!(said.status, 0, "{}", said.err);
    stage.mac_says(OK);
    let said = stage.run_in("p40", &["resize", "p9@prod", "--left"]);
    assert_eq!(said.status, 0, "{}", said.err);
    let said = stage.run_in("p40", &["close", "pane", "p9@prod"]);
    assert_eq!(said.status, 0, "{}", said.err);
    let told: Vec<Request> = stage.told.iter().map(|text| parse_told(text)).collect();
    assert_eq!(
        told,
        [
            Request::Arrange(Arrange::Focus(Subject::Pane(Some(address("p9@prod"))))),
            Request::Arrange(Arrange::Zoom(Some(address("p9@prod")))),
            Request::Arrange(Arrange::Swap {
                pane: address("p9@prod"),
                other: address("p3"),
            }),
            Request::Arrange(Arrange::Move {
                pane: address("p9@prod"),
                beside: address("p3@local"),
                side: Side::Right,
            }),
            Request::Arrange(Arrange::Resize {
                pane: Some(address("p9@prod")),
                side: Side::Left,
                steps: 1,
            }),
            Request::Close(address("p9@prod")),
        ]
    );

    // A pane of another machine is closed by the Mac or by nobody: with none, this machine's own
    // `kablo end` would end another terminal of the same number.
    stage.terminal("p9");
    stage.no_mac();
    let said = stage.run_in("p40", &["close", "pane", "p9@prod"]);
    assert_eq!(said.status, 69, "{}", said.err);
    assert!(stage.running().contains(&"p9".to_owned()));
    // A label is kablo's own where the pane is; and an address is no anchor.
    assert_eq!(
        stage
            .run_in("p40", &["rename", "pane", "p9@prod", "x"])
            .status,
        64
    );
    assert_eq!(stage.run_in("p40", &["split", "p9@prod"]).status, 64);
    assert_eq!(stage.run_in("p40", &["focus", "pane", "p9@"]).status, 64);
}

#[test]
fn the_line_a_mac_sends_a_server_is_read_the_same_by_whatever_shell_and_its_words_arrive_whole() {
    let dir_of = |stage: &Stage| stage.env.dir.display().to_string();
    for shell in login_shells() {
        let (mut mac, prod) = mac_and_prod(&shell);
        let program = recorder(&prod);
        let out = prod.env.dir.join("odd");
        let out_name = out.display().to_string();
        let never = format!("{}/never", dir_of(&prod));
        let injected = format!("$(touch {never})");
        let backticks = format!("`touch {never}`");
        let odd = [
            "echo 'hi' $HOME; `x` \"y\" \\z",
            "C:\\",
            "\\';echo INJECTED;#",
            "two\nlines\n",
            injected.as_str(),
            backticks.as_str(),
            "",
            "-",
            "--",
            "ünï ✓",
            "  spaced  ",
        ];
        let mut args = vec![
            "split",
            "-S",
            "prod",
            "--",
            program.as_str(),
            out_name.as_str(),
        ];
        args.extend(odd);
        let ssh = ssh_words();
        let said = mac.go(&Run {
            here: Some("p40"),
            args: &args,
            input: None,
            turns: &[&ssh, PLACED],
            vars: &[],
        });
        assert_eq!(said.status, 0, "{shell}: {}", said.err);
        assert_eq!(said.out, "p1@prod\n", "{shell}");
        let (words, _) = recorded(&out);
        assert_eq!(words, odd, "{shell}");
        assert!(!Path::new(&never).exists(), "{shell}: a word was run");
    }
}

#[test]
fn the_remote_line_says_what_is_wrong_when_the_server_has_no_command() {
    let shell = login_shells().remove(0);
    let (mut mac, prod) = mac_and_prod(&shell);
    std::fs::remove_file(prod.state().join("shell/bin/bateri")).unwrap();
    mac.mac_says(&ssh_words());
    // Carried verbs say the same, and the status is the line's own (127), not 255.
    let said = mac.run_in("p40", &["send", "p9@prod", "x"]);
    assert_eq!(said.status, 127, "{}", said.err);
    assert!(said.err.contains("not set up on prod"), "{}", said.err);
}

// ─── from a server that has `full`: the Mac relays ───────────────────────────────────────────────

#[test]
fn a_server_given_full_hands_the_commands_for_another_machine_to_the_mac_and_prints_what_comes_back()
 {
    let mut a = Stage::new();
    a.connect_mac();
    let relay = relay_word();
    let run = |a: &mut Stage, args: &[&str], turns: &[&str], input: Option<&[u8]>| {
        a.go(&Run {
            here: Some("p40"),
            args,
            input,
            turns,
            vars: &[],
        })
    };

    // capture: the Mac is asked how (`relay`), then to run it; stdout comes back as it was.
    let first = relayed(0, "", "first\n\ntrail");
    let said = run(
        &mut a,
        &["capture", "--history", "20", "p9@rasp", "--trim"],
        &[&relay, &first],
        None,
    );
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "first\n\ntrail");
    assert_eq!(said.err, "");
    assert_eq!(a.told.len(), 2, "{:?}", a.told);
    assert_eq!(a.told[0], ssh_text("rasp"));
    assert_eq!(
        a.told[1],
        relay_text("rasp", &["capture", "--history", "20", "p9", "--trim"])
    );
    assert!(a.ssh_calls().is_empty());

    // send: the text with a line break and a quote arrives as one word.
    a.forget_told();
    let ok = relayed(0, "", "");
    let said = run(
        &mut a,
        &["send", "p9@rasp", "it's\nme"],
        &[&relay, &ok],
        None,
    );
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(a.told[1], relay_text("rasp", &["send", "p9", "it's\nme"]));

    // `send ID -` reads its text here, since standard input does not cross.
    a.forget_told();
    let said = run(
        &mut a,
        &["send", "p9@rasp", "-"],
        &[&relay, &ok],
        Some(b"from\nstdin\n"),
    );
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(
        a.told[1],
        relay_text("rasp", &["send", "p9", "from\nstdin\n"])
    );

    // send-key, and the status and stderr of `wait` come back.
    a.forget_told();
    let said = run(
        &mut a,
        &["send-key", "p9@rasp", "Enter"],
        &[&relay, &ok],
        None,
    );
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(a.told[1], relay_text("rasp", &["send-key", "p9", "Enter"]));
    a.forget_told();
    let ended = relayed(7, "kablo: it ended\nbadly\n", "");
    let said = run(
        &mut a,
        &["wait", "p9@rasp", "--for", "5s"],
        &[&relay, &ended],
        None,
    );
    assert_eq!(said.status, 7, "{}", said.err);
    assert_eq!(said.err, "kablo: it ended\nbadly\n");
    assert_eq!(said.out, "");
    assert_eq!(
        a.told[1],
        relay_text("rasp", &["wait", "p9", "--for", "5s"])
    );

    // This Mac is `@local` to a server, and asked for under that word.
    a.forget_told();
    let said = run(&mut a, &["send", "p3@local", "hi"], &[&relay, &ok], None);
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(a.told[0], ssh_text("@local"));
    assert_eq!(a.told[1], relay_text("@local", &["send", "p3", "hi"]));

    // A key first, or a pane with no machine, is this machine's own: nobody is asked.
    a.forget_told();
    a.terminal("p3");
    let said = run(&mut a, &["send", "p3", "mine"], &[], None);
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(a.told.is_empty(), "{:?}", a.told);
    assert!(shows(&a, "p3", "mine"));
    // A malformed address is no pane.
    let said = run(&mut a, &["capture", "p9@pr od"], &[], None);
    assert_eq!(said.status, 64, "{}", said.err);
    assert!(a.told.is_empty());
}

#[test]
fn a_server_that_is_not_given_full_is_told_77_and_a_mac_that_is_gone_or_odd_is_said_so() {
    let mut a = Stage::new();
    a.connect_mac();
    let relay = relay_word();
    let run = |a: &mut Stage, args: &[&str], turns: &[&str]| {
        a.go(&Run {
            here: Some("p40"),
            args,
            input: None,
            turns,
            vars: &[],
        })
    };

    // The first question is the Mac's to refuse: nothing more is asked, nothing is run here.
    let said = run(&mut a, &["send", "p9@rasp", "x"], &[REFUSED]);
    assert_eq!(said.status, 77, "{}", said.err);
    assert!(said.err.contains("only \"full\""), "{}", said.err);
    assert_eq!(a.told.len(), 1);

    // So is the relay itself.
    let said = run(&mut a, &["capture", "p9@rasp"], &[&relay, REFUSED]);
    assert_eq!(said.status, 77, "{}", said.err);
    assert_eq!(said.out, "");

    // A server whose `commands` are off, or that bateri does not keep terminals on.
    let said = run(
        &mut a,
        &["wait", "p9@rasp"],
        &["bateri 2\nerror failed rasp is not a server bateri keeps terminals on\n"],
    );
    assert_eq!(said.status, 1, "{}", said.err);
    assert!(said.err.contains("keeps terminals on"), "{}", said.err);

    // A command the Mac could not run in time, an answer this does not read, no Mac.
    let said = run(
        &mut a,
        &["wait", "p9@rasp"],
        &[
            &relay,
            "bateri 2\nerror failed the command ran longer than 3600 seconds and was ended\n",
        ],
    );
    assert_eq!(said.status, 1, "{}", said.err);
    assert!(said.err.contains("longer than"), "{}", said.err);
    let said = run(
        &mut a,
        &["wait", "p9@rasp"],
        &[&relay, "bateri 2\nok text\nnot this\n"],
    );
    assert_eq!(said.status, 1, "{}", said.err);
    assert_eq!(said.out, "");
    a.no_mac();
    let said = run(&mut a, &["send", "p9@rasp", "x"], &[]);
    assert_eq!(said.status, 69, "{}", said.err);
}

#[test]
fn a_server_given_full_starts_a_terminal_on_another_machine_through_the_mac_and_asks_for_its_pane()
{
    let mut a = Stage::new();
    a.connect_mac();
    let relay = relay_word();
    let ok = relayed(0, "", "");

    // The terminal is started there by the command the Mac runs; the pane is asked for beside this
    // machine's own, with the server's name.
    let started = relayed(0, "", "p7\n");
    let said = a.go(&Run {
        here: Some("p40"),
        args: &[
            "split",
            "--below",
            "-S",
            "rasp",
            "--label",
            "work",
            "--keep-open",
            "--",
            "make",
            "it's ok",
        ],
        input: None,
        turns: &[&relay, &started, PLACED],
        vars: &[],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "p7@rasp\n");
    assert_eq!(said.err, "");
    assert_eq!(a.told.len(), 3, "{:?}", a.told);
    assert_eq!(a.told[0], ssh_text("rasp"));
    assert_eq!(
        a.told[1],
        relay_text(
            "rasp",
            &[
                "split",
                "--label",
                "work",
                "--keep-open",
                "--",
                "make",
                "it's ok"
            ]
        )
    );
    let wanted = Place {
        side: Some(Side::Below),
        keep_open: true,
        server: Some("rasp".to_owned()),
        ..place(7, 40)
    };
    assert_eq!(a.told[2], encoded(wanted));
    // Nothing was started or recorded here, and no ssh was run.
    assert!(a.record("p7").is_none());
    assert!(a.ssh_calls().is_empty());
    assert!(!a.running().contains(&"p7".to_owned()));

    // This Mac is `@local`.
    a.forget_told();
    let started = relayed(0, "", "p2\n");
    let said = a.go(&Run {
        here: Some("p40"),
        args: &["split", "-S", "@local", "--", "make"],
        input: None,
        turns: &[&relay, &started, PLACED],
        vars: &[],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "p2@local\n");
    assert_eq!(a.told[0], ssh_text("@local"));
    assert_eq!(a.told[1], relay_text("@local", &["split", "--", "make"]));
    let wanted = Place {
        server: Some("@local".to_owned()),
        ..place(2, 40)
    };
    assert_eq!(a.told[2], encoded(wanted));

    // `--wait` waits there, through the Mac, and ends with the program's status.
    a.forget_told();
    let started = relayed(0, "", "p8\n");
    let waited = relayed(5, "", "");
    let said = a.go(&Run {
        here: Some("p40"),
        args: &["split", "-S", "rasp", "--wait", "--", "make"],
        input: None,
        turns: &[&relay, &started, PLACED, &waited],
        vars: &[],
    });
    assert_eq!(said.status, 5, "{}", said.err);
    assert_eq!(a.told[3], relay_text("rasp", &["wait", "p8"]));

    // A pane the Mac refuses ends the terminal that was started there, through the Mac.
    a.forget_told();
    let started = relayed(0, "", "p9\n");
    let said = a.go(&Run {
        here: Some("p40"),
        args: &["split", "--tab", "t9", "-S", "rasp", "--", "make"],
        input: None,
        turns: &[&relay, &started, NO_TAB, &ok],
        vars: &[],
    });
    assert_eq!(
        (said.status, said.err.as_str()),
        (1, "bateri: there is no tab t9\n")
    );
    assert_eq!(
        a.told[3],
        relay_text("rasp", &["close", "pane", "p9", "--force"])
    );

    // A command that cannot start the terminal is its failure: the status, what it said, and no
    // pane is asked for.
    a.forget_told();
    let failed = relayed(
        127,
        "bateri: bateri is not set up on rasp: open a terminal on it in bateri first\n",
        "",
    );
    let said = a.go(&Run {
        here: Some("p40"),
        args: &["split", "-S", "rasp", "--", "make"],
        input: None,
        turns: &[&relay, &failed],
        vars: &[],
    });
    assert_eq!(said.status, 1, "{}", said.err);
    assert!(said.err.contains("not set up on rasp"), "{}", said.err);
    assert_eq!(a.told.len(), 2);
}

#[test]
fn on_this_mac_at_local_is_the_plain_terminal_and_pane_and_nobody_is_asked_for_a_relay() {
    let mut mac = Stage::new();
    mac.install_command();
    mac.connect_mac();
    let program = recorder(&mac);
    let out = mac.env.dir.join("local");
    let out_name = out.display().to_string();
    let here = here_word();
    // The Mac says the machine is this one: the terminal is this Mac's own, started here, written
    // without a machine, and the Mac is asked for a plain pane.
    let said = mac.go(&Run {
        here: Some("p40"),
        args: &["split", "-S", "@local", "--", &program, &out_name, "make"],
        input: None,
        turns: &[&here, PLACED],
        vars: &[],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    assert_eq!(said.out, "p1\n", "{}", said.err);
    assert_eq!(mac.told.len(), 2, "{:?}", mac.told);
    assert_eq!(mac.told[0], ssh_text("@local"));
    assert_eq!(mac.told[1], encoded(place(1, 40)));
    let (words, _) = recorded(&out);
    assert_eq!(words, ["make"]);
    assert!(mac.running().contains(&"p1".to_owned()));
    // Two machines are a mistake before the Mac is asked.
    let before = mac.told.len();
    let said = mac.run_in("p40", &["split", "-S", "@local", "-S", "prod"]);
    assert_eq!(said.status, 64, "{}", said.err);
    assert_eq!(mac.told.len(), before);
    // A pane of this Mac written out is this Mac's own.
    mac.terminal("p3");
    let said = mac.go(&Run {
        here: Some("p40"),
        args: &["send", "p3@local", "hi"],
        input: None,
        turns: &[&here],
        vars: &[],
    });
    assert_eq!(said.status, 0, "{}", said.err);
    assert!(shows(&mac, "p3", "hi"));
    // With no Mac there is nobody to say it is this one.
    mac.no_mac();
    let said = mac.run_in("p40", &["send", "p3@local", "again"]);
    assert_eq!(said.status, 69, "{}", said.err);
}
