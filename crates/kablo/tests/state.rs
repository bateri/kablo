//! What a session tells the clients that are attached about its terminal:
//! the program in the foreground, whether a password is being asked for, and
//! where an ssh session goes. Real shells, over the real socket.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Conn, Env, PATIENCE};
use kablo_proto::v1::{State, TerminalState};

const SH: [&str; 2] = ["--", "/bin/sh"];

fn last(conn: &Conn) -> TerminalState {
    conn.states.last().cloned().unwrap_or_default()
}

fn program(conn: &Conn) -> Option<String> {
    last(conn)
        .program
        .map(|name| String::from_utf8_lossy(&name).into_owned())
}

/// The programs a client was told about, a run of one written once.
fn programs(conn: &Conn) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for state in &conn.states {
        let name = state.program.as_ref().map_or_else(
            || "(none)".to_owned(),
            |n| String::from_utf8_lossy(n).into_owned(),
        );
        if seen.last() != Some(&name) {
            seen.push(name);
        }
    }
    seen
}

/// Pumps until the shell has said its prompt, which it does once started.
fn wait_for_the_prompt(client: &mut Conn) {
    client.until("a prompt", |c| !c.data.is_empty());
    client.pump(Duration::from_millis(300));
}

fn text(bytes: &Option<Vec<u8>>) -> Option<String> {
    bytes
        .as_ref()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

#[test]
fn the_state_comes_right_after_attached_and_before_the_picture_and_the_output() {
    let mut env = Env::new();
    let id = env.start(
        "order",
        &["--", "/bin/sh", "-c", "echo hello; exec sleep 600"],
    );

    let mut a = env.connect(&id);
    a.attach(0);
    a.wait_for("hello", 0);
    assert_eq!(a.trace, ["attached", "state", "output"]);

    let mut b = env.connect(&id);
    b.attach_with_picture(0, 80, 24);
    assert_eq!(
        b.trace.get(..3),
        Some(&["attached", "state", "picture"][..])
    );

    // The program that said nothing after `hello` is seen without anyone
    // typing: the look that follows output is the one that finds it.
    a.until("sleep", |c| program(c).as_deref() == Some("sleep"));
    b.until("sleep", |c| program(c).as_deref() == Some("sleep"));
}

#[test]
fn the_state_follows_what_runs_and_a_silent_program_is_seen() {
    let mut env = Env::new();
    let id = env.start("follow", &SH);
    let mut a = env.connect(&id);
    a.attach(0);
    let mut b = env.connect(&id);
    b.attach(0);
    for client in [&mut a, &mut b] {
        client.until("a state", |c| !c.states.is_empty());
    }
    // What the shell is called depends on the system (`/bin/sh` is bash on
    // macOS). Neither its prompt nor another client coming told anyone
    // anything new.
    let shell = program(&a).expect("the shell's name");
    wait_for_the_prompt(&mut a);
    b.pump(Duration::from_millis(300));
    assert_eq!((a.states.len(), b.states.len()), (1, 1));

    // `sleep` says nothing: only the input that started it points at it.
    a.type_in("sleep 30\n");
    for client in [&mut a, &mut b] {
        client.until("sleep", |c| program(c).as_deref() == Some("sleep"));
    }
    assert_eq!(a.status().program.as_deref(), Some(&b"sleep"[..]));

    // Interrupted, the shell is what is in the foreground again.
    a.type_in("\x03");
    for client in [&mut a, &mut b] {
        client.until("the shell again", |c| {
            program(c).as_deref() == Some(shell.as_str()) && c.states.len() > 1
        });
        assert_eq!(programs(client), [shell.as_str(), "sleep", shell.as_str()]);
    }
}

#[test]
fn when_the_program_ends_the_clients_are_told_there_is_nothing_left() {
    let mut env = Env::new();
    // It asks for a password and ends without ever showing what is typed again.
    let id = env.start(
        "ends-asking",
        &[
            "--",
            "/bin/sh",
            "-c",
            "stty -echo; echo armed; read a; exit 3",
        ],
    );
    let mut a = env.connect(&id);
    a.attach(0);
    a.wait_for("armed", 0);
    a.until("a secret", |c| last(c).secret);
    a.type_in("\n");
    a.until("the end", |c| c.ended.is_some());
    assert_eq!(last(&a), TerminalState::default(), "{:?}", a.states);
}

#[test]
fn a_terminal_that_stops_showing_what_is_typed_is_a_secret_until_it_shows_it_again() {
    let mut env = Env::new();
    // What `sudo`, `read -s` and ssh's password prompt do to the terminal,
    // then single keys as an editor takes them, then the terminal as it was.
    // (An interactive shell is no use here: bash sets the terminal for
    // itself at every prompt.)
    let id = env.start(
        "secret",
        &[
            "--",
            "/bin/sh",
            "-c",
            "stty -echo; echo armed; read a; stty -icanon; echo single; read b; \
             stty icanon echo; echo done; exec sleep 600",
        ],
    );
    let mut a = env.connect(&id);
    a.attach(0);
    a.wait_for("armed", 0);
    a.until("a secret", |c| last(c).secret);
    assert!(a.status().secret);
    let mut late = env.connect(&id);
    late.attach(0);
    assert!(last(&late).secret, "told at once to one who comes now");

    // Single keys, nothing shown: a program that wants keys, not a password.
    a.type_in("go\n");
    a.wait_for("single", 0);
    a.until("no secret", |c| !last(c).secret);
    late.until("no secret", |c| !last(c).secret);
    assert!(!a.status().secret);

    a.type_in("go\n");
    a.wait_for("done", 0);
    a.until("sleep", |c| program(c).as_deref() == Some("sleep"));
    assert!(a.states.iter().filter(|state| state.secret).count() >= 1);
    assert!(!last(&a).secret);
    assert!(!a.status().secret);
}

/// Whether an ssh client can be run here; the tests that use one say so when
/// they are skipped.
fn ssh_here() -> bool {
    Command::new("ssh")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// A listener that takes connections and says nothing: an ssh to it stays in
/// the foreground, waiting for a server that does not speak.
fn silent_server() -> u16 {
    common::target(|connection| {
        std::thread::sleep(Duration::from_secs(600));
        drop(connection);
    })
}

#[test]
fn an_interactive_ssh_is_told_with_its_target_and_port_and_nothing_else_of_its_line() {
    if !ssh_here() {
        eprintln!("skipped: no ssh client here (the parser is tested without one)");
        return;
    }
    let mut env = Env::new();
    let id = env.start("ssh", &SH);
    let port = silent_server();
    let mut a = env.connect(&id);
    a.attach(0);
    a.until("a state", |c| !c.states.is_empty());
    let shell = program(&a).expect("the shell's name");
    assert_eq!(last(&a).remote, None);

    // A key whose name looks like a secret, and a proxy command with one.
    a.type_in(&format!(
        "ssh -F /dev/null -o BatchMode=yes -i /nonexistent/hunter2-key \
         -o ProxyCommand=none deploy@127.0.0.1 -p {port}\n"
    ));
    a.until("the ssh session", |c| last(c).remote.is_some());
    let state = last(&a);
    assert_eq!(program(&a).as_deref(), Some("ssh"));
    assert_eq!(text(&state.remote).as_deref(), Some("deploy@127.0.0.1"));
    assert_eq!(state.remote_port, Some(port));
    let status = a.status();
    assert_eq!(text(&status.remote).as_deref(), Some("deploy@127.0.0.1"));
    assert_eq!(status.remote_port, Some(port));
    for told in [format!("{state:?}"), format!("{status:?}")] {
        assert!(!told.contains("hunter2"), "{told}");
        assert!(!told.contains("ProxyCommand"), "{told}");
    }

    // Interrupted, the shell is back and the session is not remote.
    a.type_in("\x03");
    a.until("the shell", |c| {
        program(c).as_deref() == Some(shell.as_str()) && last(c).remote.is_none()
    });
    assert_eq!(last(&a).remote_port, None);

    // A command on the other side without `-t` is not an interactive session.
    a.type_in(&format!(
        "ssh -F /dev/null -o BatchMode=yes deploy@127.0.0.1 -p {port} uptime\n"
    ));
    a.until("ssh", |c| program(c).as_deref() == Some("ssh"));
    assert_eq!(last(&a).remote, None);
    assert_eq!(a.status().remote, None);
}

#[test]
fn what_a_wrapper_was_given_on_its_command_line_is_never_looked_at() {
    let mut env = Env::new();
    // A program that stands in for `sshpass -p SECRET ssh host`, and stays in
    // the foreground itself (the `sleep` it waits on is only a member of the
    // group).
    let wrapper = env.script("sshpass", "#!/bin/sh\nsleep 600\nexit 0\n");
    let id = env.start(
        "wrapper",
        &[
            "--",
            wrapper.to_str().unwrap(),
            "-p",
            "SECRETPASSWORD",
            "ssh",
            "deploy@host",
        ],
    );
    let mut a = env.connect(&id);
    a.attach(0);
    a.until("a program", |c| program(c).is_some());
    a.type_in("x");
    std::thread::sleep(Duration::from_millis(500));
    a.pump(Duration::from_millis(50));
    let status = a.status();
    assert_eq!(status.remote, None);
    for told in [format!("{:?}", a.states), format!("{status:?}")] {
        assert!(!told.contains("SECRETPASSWORD"), "{told}");
        assert!(!told.contains("deploy@host"), "{told}");
    }
}

/// A release as a coding agent keeps it: a program that is a file named after
/// its version, in a directory of versions. The kernel calls a process by its
/// file (`2.1.296`), the person knows it by the name they start it with. The
/// program is a copy of `kablo` that waits for a session to end, because it is
/// quiet while it waits and, unlike a copy of a system binary, runs on macOS
/// wherever it is put.
fn a_release(env: &Env) -> std::path::PathBuf {
    let versions = env.dir.join("versions");
    std::fs::create_dir(&versions).unwrap();
    let release = versions.join("2.1.296");
    std::fs::copy(env!("CARGO_BIN_EXE_kablo"), &release).unwrap();
    release
}

#[test]
fn a_release_kept_under_its_version_is_told_by_the_name_it_was_started_by() {
    let mut env = Env::new();
    let release = a_release(&env);
    // What the releases wait for.
    let awaited = env.start("awaited", &["--", "/bin/sleep", "600"]);

    // Started under another name than the file's own: the one scene in which
    // the kernel's name is the version on both systems. The shell says
    // something before it becomes the release, as the program that says
    // nothing afterwards is found by the look that follows that output.
    let command = format!(
        "echo ready; exec -a claude {} wait {awaited}",
        release.display()
    );
    let id = env.start("named", &["--", "/bin/bash", "-c", &command]);
    let mut a = env.connect(&id);
    a.attach(0);
    a.until("claude", |c| program(c).as_deref() == Some("claude"));
    assert_eq!(a.status().program.as_deref(), Some(&b"claude"[..]));
    // The name is all of the command line that is told.
    for told in [format!("{:?}", a.states), format!("{:?}", a.status())] {
        assert!(!told.contains("wait"), "{told}");
        assert!(!told.contains("versions"), "{told}");
    }

    // Started through a link of the name the person knows: the kernel's name
    // for the process is the file's on macOS and the link's on Linux, and the
    // answer is the link's on both.
    let link = env.dir.join("claude");
    std::os::unix::fs::symlink(&release, &link).unwrap();
    let id = env.start("linked", &["--", link.to_str().unwrap(), "wait", &awaited]);
    let mut b = env.connect(&id);
    b.attach(0);
    b.until("claude", |c| program(c).as_deref() == Some("claude"));

    // Started by the file itself, version and all: nothing else to call it.
    let id = env.start("bare", &["--", release.to_str().unwrap(), "wait", &awaited]);
    let mut c = env.connect(&id);
    c.attach(0);
    c.until("the version", |c| program(c).as_deref() == Some("2.1.296"));
}

#[test]
fn a_state_is_told_when_it_changes_and_a_quiet_session_is_left_alone() {
    let mut env = Env::new();
    let id = env.start("quiet", &["--", "/bin/sleep", "600"]);
    let mut a = env.connect(&id);
    a.attach(0);
    a.until("a state", |c| !c.states.is_empty());
    assert_eq!(program(&a).as_deref(), Some("sleep"));
    // Input that changes nothing is looked at once and tells nothing; nothing
    // is looked at on a timer.
    a.type_in("x");
    for _ in 0..4 {
        a.pump(Duration::from_millis(200));
    }
    assert_eq!(a.states.len(), 1);
}

#[test]
fn a_client_that_comes_for_an_ended_session_is_told_it_has_nothing_in_its_terminal() {
    let mut env = Env::new();
    let id = env.start("ended", &["--", "/bin/sh", "-c", "echo bye"]);
    // Asking is not coming for it: the session waits to be told to somebody.
    let mut asker = env.connect(&id);
    let start = Instant::now();
    while asker.status().state != State::ENDED {
        assert!(start.elapsed() < PATIENCE, "the session never ended");
    }
    let mut a = env.connect(&id);
    a.attach(0);
    a.until("the end", |c| c.ended.is_some());
    assert_eq!(a.states, [TerminalState::default()]);
}

/// How many times the process has given up the processor to wait, as Linux
/// counts them.
#[cfg(target_os = "linux")]
fn waits(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
        .map(|count| count.trim().parse().unwrap())
        .expect("a count of context switches")
}

#[cfg(target_os = "linux")]
#[test]
fn a_relay_with_a_client_attached_does_not_wake_when_nothing_happens() {
    let mut env = Env::new();
    let id = env.start("idle", &["--", "/bin/sleep", "600"]);
    let mut a = env.connect(&id);
    a.attach(0);
    a.until("a state", |c| !c.states.is_empty());
    // The look that follows a client's input has come and gone.
    a.type_in("x");
    std::thread::sleep(Duration::from_millis(700));
    let relay = env.find("relay", &id).expect("a relay");
    let before = waits(relay);
    std::thread::sleep(Duration::from_millis(2000));
    let woken = waits(relay) - before;
    assert!(
        woken <= 2,
        "the relay woke {woken} times in two seconds of nothing"
    );
}
