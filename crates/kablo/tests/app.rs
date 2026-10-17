//! `kablo app`, the program that hands bytes to the app: the real binary against
//! real bridges, with a test in the place of the app (a connection to
//! `kablo attach`, as the app is a program with one).

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Bridge, Env, PATIENCE};
use kablo::app::{self, Patience};
use kablo::streams::Local;
use kablo_proto::v1::Message;
use kablo_proto::v1::stream::FailCode;

const SH: [&str; 2] = ["--", "/bin/sh"];

/// `kablo app` with its input and output piped, in `env`'s directory, outside
/// every session.
fn spawn(env: &Env) -> Child {
    env.kablo()
        .arg("app")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// What `kablo app` did, run to its end with `input` as its input.
fn run_app(env: &Env, input: &[u8]) -> Output {
    let mut child = spawn(env);
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn said(output: &[u8]) -> String {
    String::from_utf8_lossy(output).into_owned()
}

/// An app that takes tickets.
fn app_that_takes_mail(env: &Env) -> Bridge {
    let mut app = Bridge::start(env);
    app.take_tickets();
    app
}

/// Answers the `n`th ticket (from 0) the app has been given, with `window`, and
/// waits until the app's stream is open. Returns the stream.
fn take(app: &mut Bridge, n: usize, window: u32) -> u32 {
    app.until("a ticket", |b| b.tickets.len() > n);
    let (_, ticket) = app.tickets[n].clone();
    let a = app.answer_ticket(ticket, window);
    app.until("the app's stream to open", |b| {
        b.flows
            .get(&a)
            .is_some_and(|f| f.opened.is_some() || f.failed.is_some())
    });
    a
}

fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: a signal to a process this test started.
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

#[test]
fn what_the_program_writes_reaches_the_app_and_what_the_app_says_comes_out_as_it_is() {
    let env = Env::new();
    let mut app = app_that_takes_mail(&env);

    let mut child = spawn(&env);
    let mut stdin = child.stdin.take().unwrap();
    let asked: &[u8] = b"ask: all of it\n\x1b[1mraw\x00bytes";
    stdin.write_all(asked).unwrap();
    drop(stdin);
    let a = take(&mut app, 0, 4096);
    assert_eq!(app.tickets[0].0, None, "from no terminal");
    app.until("the bytes and the end of them", |b| {
        b.flows[&a].eof && b.flows[&a].data.len() == asked.len()
    });
    assert_eq!(app.flows[&a].data, asked);

    app.stream_send(a, b"answer \x1b[0m\r\n\x00end", 5);
    app.stream_close(a);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", said(&output.stderr));
    assert_eq!(output.stdout, b"answer \x1b[0m\r\n\x00end");
    assert_eq!(said(&output.stderr), "");
}

#[test]
fn the_program_ending_its_input_and_the_app_ending_its_output_end_the_conversation_without_a_close()
{
    let env = Env::new();
    let mut app = app_that_takes_mail(&env);

    let mut child = spawn(&env);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"question").unwrap();
    let a = take(&mut app, 0, 4096);
    app.until("the question", |b| b.flows[&a].data == b"question");
    // The program has more to say until it ends its input.
    assert!(child.try_wait().unwrap().is_none());
    app.stream_send(a, b"first", 5);
    drop(stdin);
    app.until("the eof", |b| b.flows[&a].eof);
    app.stream_send(a, b" and last", 9);
    app.stream_eof(a);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", said(&output.stderr));
    assert_eq!(said(&output.stdout), "first and last");
    assert!(!app.flows[&a].closed, "nobody closed it");
}

#[test]
fn what_the_app_says_just_before_it_closes_is_not_lost() {
    let env = Env::new();
    let mut app = app_that_takes_mail(&env);

    let mut child = spawn(&env);
    let stdin = child.stdin.take().unwrap();
    let a = take(&mut app, 0, 1 << 20);
    // All the window allows, and the close right behind it, to a program whose
    // output is a pipe that nobody has begun to read: most of it is still
    // waiting to be written when the close comes.
    let window = app.flow(a).opened.unwrap() as usize;
    let down = common::pattern(window, 11);
    app.stream_send(a, &down, 16 * 1024);
    app.stream_close(a);
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    assert_eq!(output.status.code(), Some(0), "{}", said(&output.stderr));
    assert!(
        output.stdout == down,
        "{} of {} bytes",
        output.stdout.len(),
        down.len()
    );
}

#[test]
fn megabytes_go_both_ways_whole() {
    let env = Env::new();
    let mut app = app_that_takes_mail(&env);

    let up = common::pattern(3 * 1024 * 1024 + 5, 3);
    let down = common::pattern(2 * 1024 * 1024 + 9, 8);
    let mut child = spawn(&env);
    let mut stdin = child.stdin.take().unwrap();
    let input = up.clone();
    let writer = std::thread::spawn(move || {
        stdin.write_all(&input).unwrap();
    });
    let reader = std::thread::spawn(move || child.wait_with_output().unwrap());

    let a = take(&mut app, 0, 64 * 1024);
    let mut sent = 0;
    let start = Instant::now();
    while app.flows[&a].total < up.len() as u64 || sent < down.len() {
        assert!(start.elapsed() < PATIENCE, "timed out");
        let flow = app.flow(a);
        let room = (u64::from(flow.opened.unwrap()) + flow.credit).saturating_sub(flow.sent);
        let room = usize::try_from(room)
            .unwrap()
            .min(down.len() - sent)
            .min(96 * 1024);
        if room > 0 {
            app.stream_send(a, &down[sent..sent + room], 8 * 1024);
            sent += room;
        }
        app.pump(Duration::from_millis(5));
    }
    writer.join().unwrap();
    app.until("the eof", |b| b.flows[&a].eof);
    assert!(app.flows[&a].data == up, "what the program wrote");
    app.stream_close(a);
    let output = reader.join().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", said(&output.stderr));
    assert!(output.stdout == down, "what the app said");
}

// --- where it comes from -----------------------------------------------------------

/// The shell of session `id`, attached, and what it printed so far.
fn shell_of(env: &Env, id: &str) -> common::Conn {
    let mut shell = env.connect(id);
    shell.attach(0);
    shell
}

#[test]
fn from_a_session_the_ticket_names_it_and_the_status_is_the_apps_to_give() {
    let mut env = Env::new();
    let id = env.start("app-in", &SH);
    let mut app = app_that_takes_mail(&env);
    let mut shell = shell_of(&env, &id);
    let exe = env!("CARGO_BIN_EXE_kablo");

    let command = format!(
        "printf 'from the shell' | KABLO_DIR={} {exe} app; echo \"[status=$?]\"\n",
        env.dir.display()
    );
    let before = shell.data.len();
    shell.type_in(&command);
    let a = take(&mut app, 0, 4096);
    assert_eq!(app.tickets[0].0.as_deref(), Some(id.as_str()));
    app.until("what was written", |b| {
        b.flows[&a].data == b"from the shell" && b.flows[&a].eof
    });
    app.stream_send(a, b"seen\r\n", 6);
    app.stream_close(a);
    shell.until("the status", |c| {
        String::from_utf8_lossy(c.data.get(before..).unwrap_or_default()).contains("[status=0]")
    });
    let text = String::from_utf8_lossy(shell.data.get(before..).unwrap()).into_owned();
    assert!(text.contains("seen"), "{text}");
}

// --- who it goes to ------------------------------------------------------------------

#[test]
fn with_two_apps_the_one_at_the_terminal_wins_and_away_from_a_terminal_the_one_used_last() {
    let mut env = Env::new();
    let first = env.start("app-two-a", &SH);
    let second = env.start("app-two-b", &SH);
    let mut at_first = app_that_takes_mail(&env);
    let mut at_second = app_that_takes_mail(&env);
    at_first.attach(&first, 0);
    std::thread::sleep(Duration::from_millis(400));
    at_second.attach(&second, 0);
    let exe = env!("CARGO_BIN_EXE_kablo");

    // From the first session: the app that is at it, though the other was used
    // later.
    let mut shell = shell_of(&env, &first);
    let command = format!(
        "echo x | KABLO_DIR={} {exe} app; echo \"[done=$?]\"\n",
        env.dir.display()
    );
    shell.type_in(&command);
    let a = take(&mut at_first, 0, 4096);
    assert_eq!(at_first.tickets[0].0.as_deref(), Some(first.as_str()));
    assert!(at_second.tickets.is_empty());
    at_first.stream_close(a);

    // From no terminal: the app used last.
    let mut child = spawn(&env);
    drop(child.stdin.take());
    let b = take(&mut at_second, 0, 4096);
    assert_eq!(at_second.tickets[0].0, None);
    assert_eq!(at_first.tickets.len(), 1);
    at_second.stream_close(b);
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

#[test]
fn an_app_that_comes_back_through_a_new_bridge_gets_it_before_the_half_dead_one_it_left() {
    let mut env = Env::new();
    let id = env.start("app-back", &SH);
    let mut old = app_that_takes_mail(&env);
    old.attach(&id, 0);
    std::thread::sleep(Duration::from_millis(500));
    // The old connection is still there and still says it is attached.
    let mut new = app_that_takes_mail(&env);
    new.attach(&id, 0);

    let mut shell = shell_of(&env, &id);
    let exe = env!("CARGO_BIN_EXE_kablo");
    let command = format!(
        "echo x | KABLO_DIR={} {exe} app; echo \"[done=$?]\"\n",
        env.dir.display()
    );
    shell.type_in(&command);
    let a = take(&mut new, 0, 4096);
    assert!(old.tickets.is_empty(), "the half dead one was not asked");
    new.stream_close(a);
}

#[test]
fn an_app_that_does_not_take_mail_is_passed_over() {
    let env = Env::new();
    let mut quiet = Bridge::start(&env);
    let mut app = app_that_takes_mail(&env);

    let mut child = spawn(&env);
    drop(child.stdin.take());
    let a = take(&mut app, 0, 4096);
    app.stream_close(a);
    assert_eq!(child.wait().unwrap().code(), Some(0));
    quiet.pump(Duration::from_millis(50));
    assert!(quiet.tickets.is_empty());
}

// --- when it does not get there ---------------------------------------------------------

#[test]
fn with_no_bridge_it_is_69_and_the_words_name_the_version_that_has_no_mailbox() {
    let env = Env::new();
    let output = run_app(&env, b"anyone?");
    assert_eq!(output.status.code(), Some(69));
    let words = said(&output.stderr);
    assert!(words.contains("no app is connected"), "{words}");
    assert!(
        words.contains("before mail") && words.contains("kablo upgrade"),
        "{words}"
    );
    assert_eq!(output.stdout, b"");

    // Nor is a directory that is not there anything but no bridge.
    let output = Command::new(env!("CARGO_BIN_EXE_kablo"))
        .arg("app")
        .env("KABLO_DIR", env.dir.join("not-there"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(69), "{}", said(&output.stderr));
}

#[test]
fn with_no_app_that_takes_mail_it_is_69() {
    let env = Env::new();
    let mut quiet = Bridge::start(&env);
    let output = run_app(&env, b"anyone?");
    assert_eq!(output.status.code(), Some(69));
    assert!(
        said(&output.stderr).contains("no app that takes mail is connected"),
        "{}",
        said(&output.stderr)
    );
    quiet.pump(Duration::from_millis(50));
    assert!(quiet.tickets.is_empty(), "nothing was given to it");
}

#[test]
fn a_bridge_that_is_gone_but_for_its_name_is_no_bridge_and_a_silent_one_is_not_waited_for_long() {
    let env = Env::new();
    let mut dead = Bridge::start(&env);
    let silent = Bridge::start(&env);
    let mut app = app_that_takes_mail(&env);
    // One is stopped (it accepts nothing), one is killed (its name is stale).
    signal(silent.pid(), libc::SIGSTOP);
    signal(dead.pid(), libc::SIGKILL);
    dead.wait_exit();

    let dir = env.dir.clone();
    let talking = std::thread::spawn(move || {
        let mut words = Vec::new();
        let patience = Patience {
            ask: Duration::from_millis(700),
            answer: Duration::from_secs(20),
        };
        let status = app::talk(&dir, null_local(), None, patience, &mut words);
        (status, words)
    });
    let started = Instant::now();
    let a = take(&mut app, 0, 4096);
    app.stream_close(a);
    let (status, words) = talking.join().unwrap();
    signal(silent.pid(), libc::SIGCONT);
    assert_eq!(status, 0, "{}", said(&words));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it waited for a bridge that does not answer"
    );
}

#[test]
fn an_app_that_declines_is_1_with_its_own_words() {
    let env = Env::new();
    let mut app = app_that_takes_mail(&env);

    let mut child = spawn(&env);
    let stdin = child.stdin.take().unwrap();
    app.until("a ticket", |b| !b.tickets.is_empty());
    let (_, ticket) = app.tickets[0].clone();
    app.send(
        None,
        Message::Decline {
            ticket,
            text: "not now\x1b[2J, I am busy".to_owned(),
        },
    );
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    assert_eq!(output.status.code(), Some(1));
    let words = said(&output.stderr);
    assert!(
        words.contains("the app declined: not now[2J, I am busy"),
        "{words:?}"
    );
    assert!(!words.contains('\x1b'), "control characters are not shown");
}

/// A `Local` that reads nothing and writes nowhere.
fn null_local() -> Local {
    let read = OwnedFd::from(File::open("/dev/null").unwrap());
    let write = || OwnedFd::from(OpenOptions::new().write(true).open("/dev/null").unwrap());
    Local {
        input: read,
        output: write(),
        errors: write(),
    }
}

#[test]
fn an_app_that_does_not_answer_the_ticket_is_given_up_on_with_1_and_the_late_answer_is_refused() {
    let env = Env::new();
    let mut app = app_that_takes_mail(&env);

    let mut words = Vec::new();
    let patience = Patience {
        ask: Duration::from_secs(5),
        answer: Duration::from_millis(400),
    };
    let started = Instant::now();
    let status = app::talk(&env.dir, null_local(), None, patience, &mut words);
    assert_eq!(status, 1, "{}", said(&words));
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(
        said(&words).contains("did not take the conversation"),
        "{}",
        said(&words)
    );

    // The ticket was given and is spent: answering it now is refused.
    app.until("the ticket", |b| !b.tickets.is_empty());
    let (_, ticket) = app.tickets[0].clone();
    let a = app.answer_ticket(ticket, 1000);
    app.until("the answer", |b| b.flows[&a].failed.is_some());
    assert_eq!(
        app.flows[&a].failed.as_ref().unwrap().0,
        FailCode::BAD_REQUEST
    );
}

#[test]
fn an_app_that_goes_away_in_the_middle_is_1_and_so_is_one_whose_connection_ends() {
    let env = Env::new();
    let mut app = app_that_takes_mail(&env);

    let mut child = spawn(&env);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"half").unwrap();
    let a = take(&mut app, 0, 4096);
    app.until("the bytes", |b| b.flows[&a].data == b"half");
    app.hang_up();
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        said(&output.stderr).contains("the app went away"),
        "{}",
        said(&output.stderr)
    );
}

#[test]
fn ctrl_c_closes_the_conversation_and_ends_with_130() {
    let env = Env::new();
    let mut app = app_that_takes_mail(&env);

    let mut child = spawn(&env);
    let stdin = child.stdin.take().unwrap();
    let a = take(&mut app, 0, 4096);
    signal(child.id(), libc::SIGINT);
    app.until("the close", |b| b.flows[&a].closed);
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    assert_eq!(output.status.code(), Some(130));
}

#[test]
fn a_reader_of_its_output_that_goes_away_ends_it_quietly_with_141_and_closes_the_conversation() {
    let env = Env::new();
    let mut app = app_that_takes_mail(&env);

    let mut child = spawn(&env);
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let a = take(&mut app, 0, 4096);
    drop(stdout);
    app.stream_send(a, b"to nobody", 9);
    app.until("the close", |b| b.flows[&a].closed);
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    assert_eq!(output.status.code(), Some(141));
}

#[test]
fn it_takes_no_arguments() {
    let env = Env::new();
    let output = env.kablo().args(["app", "now"]).output().unwrap();
    assert_eq!(output.status.code(), Some(64));
    assert!(said(&output.stderr).contains("takes no arguments"));
}
