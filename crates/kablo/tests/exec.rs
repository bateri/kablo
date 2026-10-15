//! `kablo exec`, end to end on this machine: the command that runs a program
//! on a bridge, with a real `kablo attach` as the carrier, and the engine of it
//! in-process against bridges that are not this build's.
//!
//! Like the stream tests these are about order and about what each side is
//! told, not about time: nothing asserts that something took long or short,
//! only that it happened, and the patience is long. The programs are the
//! system's own (`sh`, `cat`, `echo`, `yes`, `sleep`), and every test that
//! starts one that does not end by itself ends it before it is over, or has the
//! guard below do it.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{Env, Exec, PATIENCE, exec, pattern};
use kablo::carrier::{End, Wire};
use kablo::exec as client;
use kablo::streams::Local;
use kablo_proto::WIRE;
use kablo_proto::v1::stream::{Command, Incoming, IncomingDecoder};
use kablo_proto::v1::{ErrorCode, Features, Message, Packet, PacketDecoder};

const KABLO: &str = env!("CARGO_BIN_EXE_kablo");

/// Whether the process `pid` is not there: not even as a program that ended and
/// was not collected, which still answers a signal.
fn gone(pid: u32) -> bool {
    // SAFETY: signal 0 only checks.
    let found = unsafe { libc::kill(libc::pid_t::try_from(pid).unwrap(), 0) };
    found == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn wait_gone(what: &str, pid: u32) {
    let start = Instant::now();
    while !gone(pid) {
        assert!(start.elapsed() < PATIENCE, "{what} is still there");
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

/// What a program wrote to a file for the test once the file is whole (the
/// output of a live `kablo exec` comes only at its end, so a program that must
/// be heard before then says so in a file).
fn first_line(path: &std::path::Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text.trim().to_owned();
        }
        assert!(start.elapsed() < PATIENCE, "{} never came", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_program_runs_on_the_bridge_and_this_ends_with_its_status() {
    let env = Env::new();
    let ran = exec(
        &env,
        &[],
        &["/bin/sh", "-c", "echo out; echo err >&2; exit 3"],
        b"",
    );
    assert_eq!(ran.status.code(), Some(3), "{ran:?}");
    assert_eq!(ran.out(), "out\n");
    assert_eq!(ran.err(), "err\n", "errors are kept apart");

    let ran = exec(&env, &[], &["/bin/echo", "ok"], b"");
    assert_eq!(ran.status.code(), Some(0));
    assert_eq!(ran.out(), "ok\n");
    assert!(ran.err.is_empty(), "{}", ran.err());
}

#[test]
fn the_arguments_are_the_programs_as_they_are_and_may_hold_a_separator() {
    let env = Env::new();
    let ran = exec(
        &env,
        &[],
        &[
            "/bin/sh",
            "-c",
            "for a in \"$@\"; do echo \"[$a]\"; done",
            "name",
            "--",
            "two words",
            "$HOME;",
            "*",
            "-x",
        ],
        b"",
    );
    assert_eq!(ran.status.code(), Some(0), "{ran:?}");
    assert_eq!(ran.out(), "[--]\n[two words]\n[$HOME;]\n[*]\n[-x]\n");
}

#[test]
fn what_is_typed_goes_to_the_program_and_what_it_writes_comes_back_more_than_the_windows() {
    let env = Env::new();
    let input = pattern(1_500_000, 3);
    let ran = exec(&env, &[], &["/bin/cat"], &input);
    assert_eq!(ran.status.code(), Some(0), "{}", ran.err());
    assert_eq!(ran.out.len(), input.len());
    assert!(ran.out == input, "what came back is not what was sent");
}

#[test]
fn the_end_of_the_input_is_the_end_of_the_programs() {
    let env = Env::new();
    let mut running = Exec::start(&env, &[], &["/bin/sh", "-c", "cat; echo end of input"]);
    running.type_in(b"line\n");
    running.end_input();
    let ran = running.finish();
    assert_eq!(ran.out(), "line\nend of input\n");
    assert_eq!(ran.status.code(), Some(0));
}

#[test]
fn the_directory_and_the_environment_are_the_programs() {
    let env = Env::new();
    let dir = std::fs::canonicalize(&env.dir).unwrap();
    let dir = dir.to_str().unwrap();
    let ran = exec(
        &env,
        &["--cwd", dir, "--env", "KABLO_TEST_KEY=a b=c"],
        &["/bin/sh", "-c", "pwd -P; echo \"$KABLO_TEST_KEY\""],
        b"",
    );
    assert_eq!(ran.status.code(), Some(0), "{ran:?}");
    assert_eq!(ran.out(), format!("{dir}\na b=c\n"));

    let ran = exec(
        &env,
        &["--cwd", "/no/such/directory"],
        &["/bin/echo", "never"],
        b"",
    );
    assert_eq!(ran.status.code(), Some(255));
    assert!(ran.out.is_empty());
    assert!(
        ran.err().contains(
            "cannot run /bin/echo: cannot enter /no/such/directory: No such file or directory"
        ),
        "{}",
        ran.err()
    );
}

#[test]
fn a_program_that_cannot_start_is_said_and_is_255() {
    let env = Env::new();
    let ran = exec(&env, &[], &["/no/such/program"], b"");
    assert_eq!(ran.status.code(), Some(255));
    assert!(ran.out.is_empty());
    assert_eq!(
        ran.err(),
        "kablo: cannot run /no/such/program: No such file or directory\n"
    );
}

#[test]
fn a_program_killed_by_a_signal_is_128_and_its_number() {
    let env = Env::new();
    let ran = exec(&env, &[], &["/bin/sh", "-c", "kill -9 $$"], b"");
    assert_eq!(ran.status.code(), Some(137), "{ran:?}");
}

#[test]
fn ctrl_c_ends_the_program_and_what_it_started_and_this_with_130() {
    ended_by(libc::SIGINT);
}

#[test]
fn a_terminate_ends_the_program_and_what_it_started_and_this_with_143() {
    ended_by(libc::SIGTERM);
}

/// Sends `signal` to a `kablo exec` whose program has started something of its
/// own, and checks that this ends with 128 and the number and that nothing of
/// the program is left.
fn ended_by(signal: libc::c_int) {
    let env = Env::new();
    let file = env.dir.join("pids");
    let script = format!(
        "sleep 60 & echo \"$! $$\" > {0}.part; mv {0}.part {0}; wait",
        file.display()
    );
    let mut running = Exec::start(&env, &[], &["/bin/sh", "-c", &script]);
    let said = first_line(&file);
    let pids: Vec<u32> = said.split(' ').map(|n| n.parse().unwrap()).collect();
    assert_eq!(pids.len(), 2);
    let _guard = Guard(pids.clone());
    // SAFETY: the pid of the `kablo exec` this test started.
    unsafe {
        libc::kill(libc::pid_t::try_from(running.child.id()).unwrap(), signal);
    }
    let start = Instant::now();
    let status = loop {
        if let Some(status) = running.child.try_wait().unwrap() {
            break status;
        }
        assert!(start.elapsed() < PATIENCE, "it never ended");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(128 + signal), "{status:?}");
    for pid in pids {
        wait_gone("a program of a command that was ended by a signal", pid);
    }
}

#[test]
fn a_carrier_that_dies_ends_this_with_255_and_the_programs_input_with_it() {
    let env = Env::new();
    let file = env.dir.join("who");
    // Waits for its input to end: the bridge that holds it is gone with the
    // carrier, and the program goes with that.
    let script = format!("echo $$ > {0}.part; mv {0}.part {0}; cat", file.display());
    let running = Exec::start(&env, &[], &["/bin/sh", "-c", &script]);
    let program: u32 = first_line(&file).parse().unwrap();
    let _guard = Guard(vec![program]);
    // The carrier is the one child this has.
    let children = std::process::Command::new("pgrep")
        .args(["-P", &running.child.id().to_string()])
        .output()
        .unwrap();
    let carrier: libc::pid_t = String::from_utf8_lossy(&children.stdout)
        .lines()
        .next()
        .expect("kablo exec has a child")
        .trim()
        .parse()
        .unwrap();
    // SAFETY: the pid of the carrier this test's `kablo exec` started.
    unsafe { libc::kill(carrier, libc::SIGKILL) };
    let ran = running.finish();
    assert_eq!(ran.status.code(), Some(255), "{ran:?}");
    assert!(
        ran.err()
            .contains("was killed by signal 9; the connection to the server is gone"),
        "{}",
        ran.err()
    );
    wait_gone("the program of a bridge that was killed", program);
}

#[test]
fn something_that_is_not_a_bridge_is_said_and_is_255() {
    let env = Env::new();
    // What it prints before it ends is noise before the signature, which is
    // skipped; the end of it is a carrier that is gone before it answered.
    let running = Exec::start_over(&env, &[], &["/bin/echo", "hello"], &["/bin/echo", "x"]);
    let ran = running.finish();
    assert_eq!(ran.status.code(), Some(255), "{ran:?}");
    assert!(ran.out.is_empty());
    assert!(
        ran.err()
            .contains("echo ended (exit status 0); the connection to the server is gone"),
        "{}",
        ran.err()
    );

    let running = Exec::start_over(&env, &[], &["/no/such/carrier"], &["/bin/echo", "x"]);
    let ran = running.finish();
    assert_eq!(ran.status.code(), Some(255));
    assert!(
        ran.err()
            .contains("cannot run /no/such/carrier: No such file or directory"),
        "{}",
        ran.err()
    );

    // A command quoted as one word is named whole, so the quoting shows, and
    // not by its last part, which would read as kablo missing on the server.
    let quoted = "ssh -o BatchMode=yes host .local/bin/kablo attach";
    let running = Exec::start_over(&env, &[], &[quoted], &["/bin/echo", "x"]);
    let ran = running.finish();
    assert_eq!(ran.status.code(), Some(255));
    assert!(
        ran.err()
            .contains(&format!("cannot run {quoted}: No such file or directory")),
        "{}",
        ran.err()
    );
}

#[test]
fn a_reader_that_goes_away_ends_the_program_and_this() {
    let env = Env::new();
    let file = env.dir.join("who");
    let script = format!(
        "echo $$ > {0}.part; mv {0}.part {0}; exec yes",
        file.display()
    );
    let (running, mut stdout) = Exec::start_holding_output(&env, &[], &["/bin/sh", "-c", &script]);
    let program: u32 = first_line(&file).parse().unwrap();
    let _guard = Guard(vec![program]);
    // Takes a little of it and then no more: the pipe it is written to has no
    // reader.
    let mut little = [0u8; 100];
    stdout.read_exact(&mut little).unwrap();
    assert!(little.starts_with(b"y\ny\n"));
    drop(stdout);
    // Ended as a writer to a pipe nobody reads is: quietly, with the status
    // a shell reports for it.
    let ran = running.finish();
    assert_eq!(ran.status.code(), Some(128 + libc::SIGPIPE), "{ran:?}");
    assert!(ran.err.is_empty(), "{}", ran.err());
    wait_gone("the program whose output has nowhere to go", program);
}

#[test]
fn errors_that_cannot_be_written_do_not_end_the_program() {
    let env = Env::new();
    let mut command = env.kablo();
    command
        .arg("exec")
        .args(["--", KABLO, "attach", "--"])
        .args(["/bin/sh", "-c", "echo out; echo err >&2; echo more; exit 2"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Open for reading only: every write to it fails.
        .stderr(Stdio::from(std::fs::File::open("/dev/null").unwrap()));
    let ended = command.spawn().unwrap().wait_with_output().unwrap();
    assert_eq!(ended.status.code(), Some(2), "{ended:?}");
    assert_eq!(String::from_utf8_lossy(&ended.stdout), "out\nmore\n");
}

#[test]
fn a_command_line_that_is_not_one_is_a_usage_error() {
    let env = Env::new();
    for args in [
        &["exec"][..],
        &["exec", "--", KABLO, "attach"],
        &["exec", "--", "--", "ls"],
        &[
            "exec", "--env", "NOEQUALS", "--", KABLO, "attach", "--", "ls",
        ],
        &["exec", "--bogus", "--", KABLO, "attach", "--", "ls"],
    ] {
        let out = env.kablo().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(64), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).starts_with("kablo: "),
            "{args:?}"
        );
    }
}

// --- bridges that are not this build's ------------------------------------------

/// How a bridge that is not this build's answers `features`.
#[derive(Clone)]
enum Kind {
    /// One from before streams and commands: reads with the session reader
    /// alone, which skips what it does not know, and answers a `ping`.
    Old,
    /// One that has streams and no commands.
    Streams,
    /// One that answers a request it does not know with an error.
    Rejects,
}

/// Plays a bridge of `kind` on `sock`: the handshake, then every `ping` is
/// answered, and `features` as the kind does. Returns when the client's end
/// closes.
fn bridge(kind: Kind, sock: UnixStream) {
    let mut reader = sock.try_clone().unwrap();
    let mut writer = sock;
    WIRE.server(&mut reader, &mut writer).unwrap();
    let mut old = PacketDecoder::new();
    let mut new = IncomingDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let mut asked = Vec::new();
        match kind {
            Kind::Old => {
                old.push(&buf[..n]);
                while let Some(packet) = old.next_packet().unwrap() {
                    asked.push(packet);
                }
            }
            _ => {
                new.push(&buf[..n]);
                while let Some(incoming) = new.next_incoming().unwrap() {
                    if let Incoming::Session(packet) = incoming {
                        asked.push(packet);
                    }
                }
            }
        }
        for packet in asked {
            let reply = match (&kind, packet.message) {
                (_, Message::Ping(bytes)) => Some(Message::Pong(bytes)),
                (Kind::Streams, Message::Features) => Some(Message::FeaturesReply(Features {
                    streams: true,
                    ..Features::default()
                })),
                (Kind::Rejects, Message::Features) => Some(Message::Error {
                    code: ErrorCode::BAD_REQUEST,
                    text: "not a request".to_owned(),
                }),
                _ => None,
            };
            if let Some(message) = reply {
                let mut bytes = Vec::new();
                Packet::connection(message).encode(&mut bytes).unwrap();
                if writer.write_all(&bytes).is_err() {
                    return;
                }
            }
        }
    }
}

/// Runs `exec::serve` for `echo` against a bridge of `kind` and returns how it
/// ended.
fn against(kind: Kind) -> Result<kablo_proto::v1::Exit, End> {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let playing = std::thread::spawn(move || bridge(kind, theirs));
    let (done, ended) = mpsc::channel();
    std::thread::spawn(move || {
        let input = OwnedFd::from(ours.try_clone().unwrap());
        let output = OwnedFd::from(ours);
        let null = || OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let local = Local {
            input: null(),
            output: null(),
            errors: null(),
        };
        let command = Command {
            argv: vec![b"/bin/echo".to_vec()],
            ..Command::default()
        };
        let mut err = Vec::new();
        let end = client::serve(Wire { input, output }, command, local, None, &mut err);
        let _ = done.send(end);
    });
    let result = ended
        .recv_timeout(PATIENCE)
        .expect("the client waited for a bridge that has nothing to say");
    // The client end is gone with the end of `serve`, which is what lets the
    // bridge stop.
    playing.join().unwrap();
    result
}

#[test]
fn a_bridge_that_predates_commands_is_said_not_waited_for() {
    for kind in [Kind::Old, Kind::Streams, Kind::Rejects] {
        let end = against(kind);
        let Err(End::Declined(why)) = end else {
            panic!("{end:?}");
        };
        assert!(why.contains("too old to run commands"), "{why}");
    }
}

#[test]
fn a_carrier_that_is_gone_as_soon_as_it_has_answered_ends_the_command_and_does_not_wait() {
    // Both ways at once, right behind the answer to `features`: nothing is left
    // to wake the client, and it must see that for itself.
    let (bridge_in, client_out) = kablo::sys::pipe().unwrap();
    let (client_in, bridge_out) = kablo::sys::pipe().unwrap();
    std::thread::spawn(move || {
        let mut reader = std::fs::File::from(bridge_in);
        let mut writer = std::fs::File::from(bridge_out);
        WIRE.server(&mut reader, &mut writer).unwrap();
        let mut decoder = IncomingDecoder::new();
        let mut buf = [0u8; 4096];
        'asked: loop {
            let n = reader.read(&mut buf).unwrap();
            decoder.push(&buf[..n]);
            while let Some(incoming) = decoder.next_incoming().unwrap() {
                if let Incoming::Session(packet) = incoming
                    && packet.message == Message::Features
                {
                    break 'asked;
                }
            }
        }
        drop(reader);
        let mut bytes = Vec::new();
        let features = Features::with_streams().with_commands();
        Packet::connection(Message::FeaturesReply(features))
            .encode(&mut bytes)
            .unwrap();
        writer.write_all(&bytes).unwrap();
        drop(writer);
    });
    let (done, ended) = mpsc::channel();
    std::thread::spawn(move || {
        let null = || OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let local = Local {
            input: null(),
            output: null(),
            errors: null(),
        };
        let command = Command {
            argv: vec![b"/bin/echo".to_vec()],
            ..Command::default()
        };
        let wire = Wire {
            input: client_in,
            output: client_out,
        };
        let _ = done.send(client::serve(wire, command, local, None, &mut Vec::new()));
    });
    let end = ended
        .recv_timeout(PATIENCE)
        .expect("the client waited for a carrier that is gone");
    assert_eq!(end, Err(End::Carrier));
}

#[test]
fn a_carrier_that_ends_in_the_middle_of_the_handshake_ends_the_command() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    drop(theirs);
    let input = OwnedFd::from(ours.try_clone().unwrap());
    let output = OwnedFd::from(ours);
    let null = || OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    let local = Local {
        input: null(),
        output: null(),
        errors: null(),
    };
    let command = Command {
        argv: vec![b"/bin/echo".to_vec()],
        ..Command::default()
    };
    let end = client::serve(
        Wire { input, output },
        command,
        local,
        None,
        &mut Vec::new(),
    );
    assert_eq!(end, Err(End::Carrier));
}
