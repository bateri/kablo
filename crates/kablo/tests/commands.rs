//! Commands through a real bridge: programs it runs for a client, on channel 1,
//! next to sessions.
//!
//! The tests are about order and about what each side is told, not about time:
//! nothing here asserts that something took long or short, only that it happened
//! (and the patience is long). The programs are the system's own (`sh`, `cat`,
//! `echo`, `yes`, `sleep`), and every test that starts one that does not end by
//! itself ends it before it is over, or has the guard below do it.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::{Duration, Instant};

use common::{Bridge, Env, PATIENCE, pattern};
use kablo::streams::WINDOW;
use kablo::tune;
use kablo_proto::tlv;
use kablo_proto::v1::Exit;
use kablo_proto::v1::stream::{self, FailCode, MAX_WINDOW};
use kablo_proto::v1::{Message, Offers};

fn command(argv: &[&str]) -> stream::Command {
    stream::Command {
        argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
        ..stream::Command::default()
    }
}

fn sh(script: &str) -> stream::Command {
    command(&["/bin/sh", "-c", script])
}

/// Whether the process `pid` is not there: not even as a program that ended and
/// was not collected, which still answers a signal.
fn gone(pid: u32) -> bool {
    // SAFETY: signal 0 only checks.
    let found = unsafe { libc::kill(libc::pid_t::try_from(pid).unwrap(), 0) };
    found == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn wait_gone(what: &str, pids: &[u32]) {
    let start = Instant::now();
    while !pids.iter().all(|&pid| gone(pid)) {
        assert!(start.elapsed() < PATIENCE, "{what} are still there");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Kills the groups of programs when the test is over, whatever happened in it:
/// a program that outlives a failed test would sit there for a minute.
struct Guard(Vec<u32>);

impl Drop for Guard {
    fn drop(&mut self) {
        for &pid in &self.0 {
            let pid = libc::pid_t::try_from(pid).unwrap();
            // SAFETY: signals to a group and a process; a number that is gone
            // is an error, not a fault.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

/// The numbers a command printed, one to a line, once it has printed `count`.
fn numbers(bridge: &mut Bridge, id: u32, count: usize) -> Vec<u32> {
    bridge.until("the command to say who it is", |b| {
        b.flows[&id].data.iter().filter(|&&c| c == b'\n').count() >= count
    });
    String::from_utf8(bridge.flows[&id].data.clone())
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect()
}

#[test]
fn a_bridge_says_it_runs_commands() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    bridge.send(None, Message::Features);
    bridge.until("features", |b| b.features.len() == 1);
    assert!(bridge.features[0].commands);
    assert!(bridge.features[0].streams);
}

#[test]
fn a_bridge_answers_a_features_that_says_what_its_client_takes_as_it_answers_any() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    bridge.send(None, Message::FeaturesWith(Offers::with_tickets()));
    bridge.send(None, Message::Features);
    bridge.until("both answers", |b| b.features.len() == 2);
    assert_eq!(bridge.features[0], bridge.features[1]);
    assert!(bridge.features[0].streams);
    // What the client takes does not change what the bridge says it has.
    assert!(bridge.features[0].mailbox);
}

#[test]
fn output_errors_and_the_end_come_apart_and_in_order() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);

    let (said, window) = bridge
        .run_command(command(&["/bin/echo", "hello", "$HOME;", "*"]), MAX_WINDOW)
        .unwrap();
    // A bridge that has not measured the link yet starts a stream at the start.
    assert_eq!(window, tune::START);
    bridge.until("the end", |b| b.flows[&said].closed);
    let flow = &bridge.flows[&said];
    // The arguments were nobody's to read as a shell would.
    assert_eq!(flow.data, b"hello $HOME; *\n");
    assert!(flow.stderr.is_empty());
    assert_eq!(flow.exited, Some(Exit::Code(0)));
    assert_eq!(flow.ends, ["eof", "exited", "close"]);

    let (apart, _) = bridge
        .run_command(sh("echo out; echo err >&2; exit 3"), MAX_WINDOW)
        .unwrap();
    bridge.until("the end", |b| b.flows[&apart].closed);
    let flow = &bridge.flows[&apart];
    assert_eq!(flow.data, b"out\n");
    assert_eq!(flow.stderr, b"err\n");
    assert_eq!(flow.exited, Some(Exit::Code(3)));
    assert_eq!(flow.ends, ["eof", "exited", "close"]);

    let (killed, _) = bridge.run_command(sh("kill -9 $$"), MAX_WINDOW).unwrap();
    bridge.until("the end", |b| b.flows[&killed].closed);
    assert_eq!(
        bridge.flows[&killed].exited,
        Some(Exit::Signal(libc::SIGKILL))
    );
    assert!(bridge.errors.is_empty());
}

#[test]
fn the_environment_and_the_directory_are_the_clients() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let dir = std::env::temp_dir().canonicalize().unwrap();
    let mut asked = sh("echo $KABLO_COMMAND_VAR; pwd -P");
    asked.env = vec![b"KABLO_COMMAND_VAR=a=b".to_vec()];
    asked.cwd = Some(dir.to_str().unwrap().as_bytes().to_vec());
    let (id, _) = bridge.run_command(asked, MAX_WINDOW).unwrap();
    bridge.until("the end", |b| b.flows[&id].closed);
    assert_eq!(
        String::from_utf8(bridge.flows[&id].data.clone()).unwrap(),
        format!("a=b\n{}\n", dir.display())
    );
}

#[test]
fn input_larger_than_the_window_goes_in_as_credit_comes_and_comes_back_whole() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let input = pattern(4 * WINDOW as usize, 9);
    let (id, window) = bridge
        .run_command(command(&["/bin/cat"]), MAX_WINDOW)
        .unwrap();
    assert!(input.len() as u64 > u64::from(window));

    // Only what the bridge has said it takes: its window, and the credit it
    // gives as `cat` takes what was sent.
    let mut sent = 0usize;
    let start = Instant::now();
    while sent < input.len() {
        let room = (u64::from(window) + bridge.flows[&id].credit).saturating_sub(sent as u64);
        let piece = usize::try_from(room).unwrap().min(input.len() - sent);
        if piece > 0 {
            bridge.stream_send(id, &input[sent..sent + piece], 7_000);
            sent += piece;
        }
        bridge.pump(Duration::from_millis(5));
        assert!(start.elapsed() < PATIENCE, "the input never went in");
        assert!(!bridge.flows[&id].closed, "the bridge closed the stream");
    }
    bridge.stream_eof(id);
    bridge.until("the end", |b| b.flows[&id].closed);
    let flow = &bridge.flows[&id];
    assert_eq!(flow.data, input);
    assert_eq!(flow.exited, Some(Exit::Code(0)));
    assert_eq!(flow.ends, ["eof", "exited", "close"]);
}

#[test]
fn a_command_that_cannot_start_is_failed_in_the_systems_words() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let (_, code, text) = bridge
        .run_command(command(&["/no/such/program"]), MAX_WINDOW)
        .unwrap_err();
    assert_eq!(
        (code, text.as_str()),
        (FailCode::START_FAILED, "No such file or directory")
    );
    let mut elsewhere = command(&["/bin/true"]);
    elsewhere.cwd = Some(b"/no/such/directory".to_vec());
    let (_, code, text) = bridge.run_command(elsewhere, MAX_WINDOW).unwrap_err();
    // The directory is named: the system's answer to the `exec` that follows
    // would have named the program.
    assert_eq!(
        (code, text.as_str()),
        (
            FailCode::START_FAILED,
            "cannot enter /no/such/directory: No such file or directory"
        )
    );
    // Nothing else is affected: the next command runs.
    let (id, _) = bridge
        .run_command(command(&["/bin/echo", "fine"]), MAX_WINDOW)
        .unwrap();
    bridge.until("the end", |b| b.flows[&id].closed);
    assert_eq!(bridge.flows[&id].data, b"fine\n");

    // What a conforming client cannot write: a program with a NUL in its name,
    // in an `open` of a number above the others.
    let mut block = tlv::Writer::new();
    block.put(3, b"/bin/true\0");
    let mut rest = 1000u32.to_be_bytes().to_vec();
    rest.extend_from_slice(&block.finish());
    bridge.stream_raw(stream::kind::OPEN, 1000, &rest);
    bridge.until("the answer to a program with a NUL", |b| {
        b.flows.get(&1000).is_some_and(|f| f.failed.is_some())
    });
    assert_eq!(
        bridge.flows[&1000].failed.as_ref().unwrap().0,
        FailCode::BAD_REQUEST
    );
}

#[test]
fn a_close_ends_a_long_command_and_what_it_started_and_collects_them() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let (id, _) = bridge
        .run_command(sh("sleep 60 & echo $!; echo $$; wait"), MAX_WINDOW)
        .unwrap();
    let pids = numbers(&mut bridge, id, 2);
    let _guard = Guard(pids.clone());
    assert!(pids.iter().all(|&pid| !gone(pid)));

    bridge.stream_close(id);
    // Not even as a program that ended and was not collected.
    wait_gone("the command and what it started", &pids);
    // And the bridge goes on: the number is free and a command runs.
    let (next, _) = bridge
        .run_command(command(&["/bin/echo", "after"]), MAX_WINDOW)
        .unwrap();
    bridge.until("the end", |b| b.flows[&next].closed);
    assert_eq!(bridge.flows[&next].data, b"after\n");
}

#[test]
fn a_client_that_goes_away_ends_its_commands_and_the_bridge_leaves_once_they_have() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let (polite, _) = bridge
        .run_command(sh("sleep 60 & echo $!; echo $$; wait"), MAX_WINDOW)
        .unwrap();
    let (stubborn, _) = bridge
        .run_command(
            sh("trap '' TERM; echo $$; while :; do sleep 1; done"),
            MAX_WINDOW,
        )
        .unwrap();
    let mut pids = numbers(&mut bridge, polite, 2);
    pids.extend(numbers(&mut bridge, stubborn, 1));
    let _guard = Guard(pids.clone());

    bridge.hang_up();
    assert!(bridge.wait_exit().success());
    // The bridge did not leave before it had seen to them: the one that does as
    // it is told is gone, and so is the one that ignores the signal, killed
    // when its time was up.
    wait_gone("the commands of a connection that ended", &pids);
}

#[test]
fn a_command_that_floods_does_not_keep_a_session_from_answering() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let id = env.name("echo");
    env.track(&id);
    bridge.try_new(Some(&id), &["/bin/cat"]).unwrap();
    bridge.attach(&id, 0);

    // The client takes nothing of the output (no credit): the bridge holds at
    // most a window of it, and the session is typed into meanwhile.
    bridge.next_flow = (true, false);
    let (flood, _) = bridge
        .run_command(command(&["/usr/bin/yes"]), MAX_WINDOW)
        .unwrap();
    bridge.until("the first of the flood", |b| b.flows[&flood].total > 0);
    bridge.type_in(&id, "typed during the flood\n");
    bridge.wait_for(&id, "typed during the flood");
    assert!(!bridge.flows[&flood].closed);

    // Taking it lets more come; closing it ends `yes`.
    bridge.flow(flood).hold = false;
    bridge.stream_release(flood);
    let before = bridge.flows[&flood].total;
    bridge.until("more of the flood", |b| b.flows[&flood].total > before);
    bridge.stream_close(flood);
    bridge.type_in(&id, "typed after\n");
    bridge.wait_for(&id, "typed after");
}
