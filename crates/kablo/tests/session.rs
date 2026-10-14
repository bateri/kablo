//! Whole sessions, with real shells, over the real socket.

mod common;

use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use common::{Conn, Env, PATIENCE};
use kablo_proto::v1::{ErrorCode, Exit, Message, OFFSET_END, Size, State};

const SH: [&str; 2] = ["--", "/bin/sh"];

#[test]
fn a_returning_client_gets_exactly_the_bytes_it_missed() {
    let mut env = Env::new();
    let id = env.start("resume", &SH);

    // A witness sees everything from the first byte.
    let mut witness = env.connect(&id);
    assert_eq!(witness.attach(0), (0, false));
    let mut a = env.connect(&id);
    a.attach(0);
    a.type_in("echo $((6*7))\n");
    a.wait_for("42\r\n", 0);
    witness.wait_for("42\r\n", 0);

    // The client goes away; the session does not notice.
    let seen = a.next;
    let before = a.data.clone();
    drop(a);

    // Output while nobody who matters is attached.
    witness.type_in("echo later-$((1+2))\n");
    witness.wait_for("later-3\r\n", 0);
    let mut control = env.connect(&id);
    loop {
        let status = control.status();
        witness.pump(Duration::from_millis(20));
        if status.offset == witness.next {
            break;
        }
    }

    let mut back = env.connect(&id);
    assert_eq!(back.attach(seen), (seen, false), "nothing was cut");
    let target = witness.next;
    back.until("everything it missed", |c| c.next == target);
    assert!(back.next > seen, "there was something to catch up on");
    let mut whole = before;
    whole.extend_from_slice(&back.data);
    assert_eq!(whole, witness.data, "not one byte more or less");
}

#[test]
fn attaching_at_the_end_replays_nothing_and_a_detached_client_gets_nothing() {
    let mut env = Env::new();
    let id = env.start("end", &SH);
    let mut a = env.connect(&id);
    a.attach(0);
    a.type_in("echo first\n");
    a.wait_for("first\r\n", 0);

    let mut b = env.connect(&id);
    let status = b.status();
    assert_eq!(status.clients, 1);
    let (start, truncated) = b.attach(OFFSET_END);
    assert!(!truncated);
    assert!(start >= a.next.min(status.offset));
    b.type_in("echo second\n");
    b.wait_for("second\r\n", 0);
    assert!(!String::from_utf8_lossy(&b.data).contains("first\r\n"));

    b.send(Message::Detach);
    b.pump(Duration::from_millis(100));
    assert_eq!(b.status().clients, 1, "only a is attached now");
    b.type_in("echo third\n");
    b.until("an error", |c| !c.errors.is_empty());
    assert_eq!(b.errors[0].0, ErrorCode::NOT_ATTACHED);
}

#[test]
fn terminate_ends_the_shell_and_everything_is_cleaned_up() {
    let mut env = Env::new();
    let id = env.start("term", &SH);
    let mut a = env.connect(&id);
    a.attach(0);
    let status = a.status();
    assert!(status.pid > 1);
    // `/bin/sh` is bash on one system and dash on another.
    assert!(
        matches!(status.program.as_deref(), Some(b"sh" | b"bash" | b"dash")),
        "{:?}",
        status.program
    );
    a.send(Message::Terminate { force: false });
    a.until("the end", |c| c.ended.is_some());
    env.wait_gone(&id);
    // The shell itself is gone, not just its keepers.
    let start = Instant::now();
    // SAFETY: signal 0 only asks whether the pid exists.
    while unsafe { libc::kill(status.pid as libc::pid_t, 0) } == 0 {
        assert!(start.elapsed() < PATIENCE, "the shell is still alive");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_dead_relay_is_replaced_and_no_byte_is_lost() {
    let mut env = Env::new();
    let id = env.start("relay", &SH);
    let mut witness = env.connect(&id);
    witness.attach(0);
    let before = witness.status();
    let relay = env.find("relay", &id).expect("a relay");

    // A steady stream of numbered lines, so that a missing or doubled byte
    // shows as a missing or doubled number.
    witness.type_in("i=0; while [ $i -lt 150 ]; do echo line$i; i=$((i+1)); sleep 0.02; done &\n");
    witness.wait_for("line20\r\n", 0);

    // SAFETY: a signal to the relay of this test's own session.
    unsafe { libc::kill(relay as libc::pid_t, libc::SIGKILL) };
    witness.until_closed();

    let mut back = env.connect(&id);
    let (start, truncated) = back.attach(witness.next);
    assert_eq!((start, truncated), (witness.next, false));
    back.wait_for("line149\r\n", 0);

    let after = back.status();
    assert_eq!(after.pid, before.pid, "the shell was never touched");
    let new_relay = env.find("relay", &id).expect("a new relay");
    assert_ne!(new_relay, relay);

    let mut whole = witness.data.clone();
    whole.extend_from_slice(&back.data);
    let text = String::from_utf8_lossy(&whole);
    // A number sits at the end of a line, after the prompt on the first one.
    let numbers: Vec<u32> = text
        .split("\r\n")
        .filter_map(|l| l.rsplit_once("line"))
        .filter_map(|(_, n)| n.parse().ok())
        .collect();
    assert_eq!(numbers, (0..150).collect::<Vec<u32>>());
}

#[test]
fn a_client_that_stops_reading_is_dropped_and_the_program_never_stalls() {
    let mut env = Env::new();
    let id = env.start("slow", &["--journal-size", "1M", "--", "/bin/sh"]);
    let mut stuck = env.connect(&id);
    stuck.attach(0);
    // `stuck` is never read from again: its socket fills and stays full.
    let mut control = env.connect(&id);

    // Somebody types the command and goes; the program writes 8 MB, which
    // is eight times what the journal holds.
    let mut typist = env.connect(&id);
    typist.attach(OFFSET_END);
    typist.type_in("head -c 8000000 /dev/zero | tr '\\0' x; echo done-$((1+1))\n");
    typist.pump(Duration::from_millis(50));
    drop(typist);

    // The terminal keeps being read: the journal reaches the end of the
    // program's output although nobody takes the bytes.
    let start = Instant::now();
    loop {
        let status = control.status();
        if status.offset >= 8_000_000 {
            assert_eq!(status.retained_from, status.offset - (1 << 20));
            break;
        }
        assert!(
            start.elapsed() < PATIENCE,
            "the program stalled at {}",
            status.offset
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // The program ran to its end and the shell is at its prompt again.
    let status = loop {
        let before = control.status().offset;
        std::thread::sleep(Duration::from_millis(300));
        let status = control.status();
        if status.offset == before {
            break status;
        }
    };
    let mut tail = env.connect(&id);
    let (from, truncated) = tail.attach(status.offset - 100);
    assert!(!truncated);
    tail.until("the tail", |c| c.next == status.offset);
    assert!(
        String::from_utf8_lossy(&tail.data).contains("done-2\r\n"),
        "{:?}",
        String::from_utf8_lossy(&tail.data)
    );
    assert_eq!(from, status.offset - 100);

    // The one that did not read was let go, with what it had been sent.
    assert_eq!(
        status.clients, 0,
        "nobody is attached, the stuck one was let go"
    );
    stuck.until_closed();
    let last = stuck.next;
    assert!(last < status.retained_from);

    // It comes back with its offset and is told what it lost.
    let mut back = env.connect(&id);
    let (resume, truncated) = back.attach(last);
    assert!(truncated);
    assert!(resume >= status.retained_from);
}

#[test]
fn a_shell_that_exits_is_reported_to_the_client_that_comes_for_it() {
    let mut env = Env::new();
    let id = env.start("exit", &["--", "/bin/sh", "-c", "echo bye; exit 3"]);
    // Nobody is attached when it ends; the session waits.
    std::thread::sleep(Duration::from_millis(500));
    assert!(env.socket(&id).exists());
    assert!(env.find("warden", &id).is_some());

    let mut a = env.connect(&id);
    let status = a.status();
    assert_eq!(status.exit, Some(Exit::Code(3)));
    a.attach(0);
    a.until("the end", |c| c.ended.is_some());
    assert_eq!(a.ended, Some(Exit::Code(3)));
    assert!(String::from_utf8_lossy(&a.data).contains("bye\r\n"));
    env.wait_gone(&id);
}

#[test]
fn a_shell_that_exits_while_attached_is_reported_at_once() {
    let mut env = Env::new();
    let id = env.start(
        "exit2",
        &["--", "/bin/sh", "-c", "sleep 1; echo bye; exit 5"],
    );
    let mut a = env.connect(&id);
    a.attach(0);
    a.until("the end", |c| c.ended.is_some());
    assert_eq!(a.ended, Some(Exit::Code(5)));
    assert!(String::from_utf8_lossy(&a.data).ends_with("bye\r\n"));
    env.wait_gone(&id);
}

#[test]
fn a_signal_death_is_a_signal_not_a_code() {
    let mut env = Env::new();
    let id = env.start("sig", &["--", "/bin/sh", "-c", "sleep 1; kill -9 $$"]);
    let mut a = env.connect(&id);
    a.attach(0);
    a.until("the end", |c| c.ended.is_some());
    assert_eq!(a.ended, Some(Exit::Signal(libc::SIGKILL)));
}

#[test]
fn a_directory_that_is_not_private_is_refused() {
    let mut env = Env::new();
    std::fs::set_permissions(
        &env.dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let why = env.try_start("open", &SH).unwrap_err();
    assert!(why.contains("0700"), "{why}");
    assert!(env.find("warden", &env.name("open")).is_none());
    // And it was not repaired.
    let mode = std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&env.dir).unwrap());
    assert_eq!(mode & 0o777, 0o755);
}

#[test]
fn a_session_name_is_taken_once() {
    let mut env = Env::new();
    let id = env.start("twice", &SH);
    let why = env.try_start("twice", &SH).unwrap_err();
    assert!(why.contains("exists already"), "{why}");
    // The first one is untouched.
    let mut a = env.connect(&id);
    assert!(a.status().pid > 1);
}

#[test]
fn a_program_that_is_not_there_is_an_error() {
    let mut env = Env::new();
    let why = env
        .try_start("nope", &["--", "/nonexistent/program"])
        .unwrap_err();
    assert!(why.contains("cannot start the program"), "{why}");
    assert!(env.find("warden", &env.name("nope")).is_none());
    assert!(!env.socket(&env.name("nope")).exists());
}

#[test]
fn status_names_the_foreground_program_its_directory_and_the_size() {
    let mut env = Env::new();
    let cwd = std::fs::canonicalize(&env.dir).unwrap();
    let id = env.start(
        "status",
        &[
            "--cols",
            "100",
            "--rows",
            "30",
            "--cwd",
            cwd.to_str().unwrap(),
            "--",
            "/bin/sleep",
            "600",
        ],
    );
    let mut a = env.connect(&id);
    let status = a.status();
    assert_eq!(status.program.as_deref(), Some(&b"sleep"[..]));
    assert_eq!(
        status.cwd.as_deref(),
        Some(cwd.to_str().unwrap().as_bytes())
    );
    assert_eq!((status.size.cols, status.size.rows), (100, 30));
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));

    a.attach(0);
    a.send(Message::Resize(Size {
        cols: 90,
        rows: 20,
        xpixel: 0,
        ypixel: 0,
    }));
    let start = Instant::now();
    while (a.status().size.cols, a.status().size.rows) != (90, 20) {
        assert!(start.elapsed() < PATIENCE);
    }
}

#[test]
fn the_socket_speaks_the_protocol_and_answers_what_it_cannot_do() {
    let mut env = Env::new();
    let id = env.start("proto", &SH);
    let mut a = env.connect(&id);
    a.send(Message::Ping(b"hello".to_vec()));
    a.until("a pong", |c| {
        c.replies
            .iter()
            .any(|p| matches!(&p.message, Message::Pong(b) if b == b"hello"))
    });
    a.send(Message::List);
    a.until("a list", |c| {
        c.replies
            .iter()
            .any(|p| matches!(&p.message, Message::ListReply(all) if all.len() == 1))
    });
    a.send(Message::attach(1 << 40));
    a.until("an error", |c| !c.errors.is_empty());
    assert_eq!(a.errors[0].0, ErrorCode::BAD_REQUEST);
    a.send(Message::Upgrade {
        path: b"/bin/true".to_vec(),
    });
    a.until("a refusal", |c| c.errors.len() > 1);
    assert_eq!(a.errors[1].0, ErrorCode::UPGRADE_FAILED);

    // A banner before the signature is skipped.
    use std::io::{Read, Write};
    let mut raw = std::os::unix::net::UnixStream::connect(env.socket(&id)).unwrap();
    raw.write_all(b"Welcome to somewhere\r\n").unwrap();
    kablo_proto::WIRE.write_hello(&mut raw).unwrap();
    let reply = kablo_proto::WIRE.read_reply(&mut raw).unwrap();
    assert_eq!(reply.selected, 1);
    // Another protocol's hello is refused without a version.
    let mut other = std::os::unix::net::UnixStream::connect(env.socket(&id)).unwrap();
    other.write_all(b"KABLO\0\x07\x09").unwrap();
    let mut said = Vec::new();
    other.read_to_end(&mut said).unwrap();
    assert_eq!(said, b"KABLO\0\0\x01\x01");
}

#[test]
fn many_clients_see_the_same_stream_and_any_of_them_may_type() {
    let mut env = Env::new();
    let id = env.start("many", &SH);
    let mut clients: Vec<Conn> = (0..3).map(|_| env.connect(&id)).collect();
    for client in &mut clients {
        client.attach(0);
    }
    clients[0].type_in("echo one\n");
    clients[1].type_in("echo two\n");
    for client in &mut clients {
        client.wait_for("one\r\n", 0);
        client.wait_for("two\r\n", 0);
    }
    let target = clients[0].next;
    for client in &mut clients {
        client.until("the same end", |c| c.next >= target);
    }
    let first = clients[0].data[..usize::try_from(target).unwrap()].to_vec();
    for client in &clients {
        assert_eq!(client.data[..first.len()], first[..]);
    }
}

#[test]
fn a_session_whose_relay_keeps_dying_still_has_its_terminal_read() {
    let mut env = Env::new();
    // The program waits for the relays to be killed, then writes far more
    // than a terminal buffers: it can only finish if somebody reads.
    let id = env.start(
        "strikes",
        &[
            "--",
            "/bin/sh",
            "-c",
            "sleep 2; head -c 2000000 /dev/zero | tr '\\0' x",
        ],
    );
    let mut a = env.connect(&id);
    let pid = a.status().pid;
    drop(a);

    let mut killed = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let relay = loop {
            match env.find("relay", &id) {
                Some(relay) if !killed.contains(&relay) => break relay,
                _ => {
                    assert!(start.elapsed() < PATIENCE, "no new relay after {killed:?}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        // SAFETY: a signal to the relay of this test's own session.
        unsafe { libc::kill(relay as libc::pid_t, libc::SIGKILL) };
        killed.push(relay);
    }
    // Five young deaths in a row: the warden stops starting relays.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.find("relay", &id), None);
    assert!(env.find("warden", &id).is_some(), "the warden stays");

    // The shell finishes: the warden reads its terminal in the relay's place.
    let start = Instant::now();
    // SAFETY: signal 0 only asks whether the pid exists.
    while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        assert!(
            start.elapsed() < PATIENCE,
            "the program is stuck on a full terminal"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_shell_starts_with_the_default_signals_whoever_started_the_keeper() {
    // `nohup kablo new`, or a script that runs it in the background, hands the
    // keeper signals that are ignored; a shell that kept them would shrug off
    // the hang-up `terminate` sends.
    let mut env = Env::new();
    let id = env.name("ignored");
    env.track(&id);
    let mut command = env.kablo();
    command.args([
        "new",
        "--id",
        &id,
        "--",
        "/bin/sh",
        "-c",
        "kill -HUP $$; echo survived; sleep 3",
    ]);
    // SAFETY: the closure only calls `signal`, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
                libc::signal(signal, libc::SIG_IGN);
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");

    let mut a = env.connect(&id);
    a.attach(0);
    a.until("the end", |c| c.ended.is_some());
    assert_eq!(a.ended, Some(Exit::Signal(libc::SIGHUP)));
    assert!(!String::from_utf8_lossy(&a.data).contains("survived"));
}

#[test]
fn a_relay_whose_keeper_is_gone_and_whose_program_has_ended_goes_too() {
    let mut env = Env::new();
    let id = env.start("orphan", &["--", "/bin/sh", "-c", "sleep 1"]);
    let warden = env.find("warden", &id).expect("a keeper");
    // SAFETY: a signal to the keeper of this test's own session.
    unsafe { libc::kill(warden as libc::pid_t, libc::SIGKILL) };

    // Nothing can tell the relay to quit, or keep the session for a client
    // that comes later: when the program is over it has nothing left to do.
    let start = Instant::now();
    while env.find("relay", &id).is_some() {
        assert!(
            start.elapsed() < PATIENCE,
            "the relay of a session nobody keeps is still there"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Seconds of processor time the process has used, as `ps` tells it (`M:SS.cc`
/// on macOS, `HH:MM:SS` on Linux).
fn cpu_seconds(pid: u32) -> Option<f64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .split(':')
        .try_fold(0.0, |total, part| {
            Some(total * 60.0 + part.parse::<f64>().ok()?)
        })
}

#[test]
fn a_relay_out_of_descriptors_waits_for_a_client_to_leave_instead_of_spinning() {
    let mut env = Env::new();
    let id = env.name("fds");
    env.track(&id);
    let mut command = env.kablo();
    command.args(["new", "--id", &id, "--", "/bin/sh"]);
    // SAFETY: the closure only calls `setrlimit`, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 24,
                rlim_max: 24,
            };
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");

    // More connections than the relay has descriptors for. Where a refused
    // accept leaves the connection queued (Linux) the listening socket stays
    // readable and a relay that waits on it spins; macOS drops the
    // connection instead, so there the check passes either way.
    let crowd: Vec<_> = (0..40)
        .filter_map(|_| std::os::unix::net::UnixStream::connect(env.socket(&id)).ok())
        .collect();
    assert!(crowd.len() >= 30, "{} connections", crowd.len());
    std::thread::sleep(Duration::from_millis(500));
    let relay = env.find("relay", &id).expect("a relay");
    let before = cpu_seconds(relay).expect("the processor time of the relay");
    std::thread::sleep(Duration::from_secs(2));
    let spent = cpu_seconds(relay).expect("the processor time of the relay") - before;
    assert!(spent < 1.0, "the relay used {spent} s of processor in two");

    // The ones that were there leave and the ones that waited are served.
    drop(crowd);
    let mut a = env.connect(&id);
    assert_eq!(a.status().id.as_str(), id);
}

/// Types more than a session holds for its program at a connection, and
/// returns once the session has refused some of it.
fn type_past_what_the_session_holds(typist: &mut Conn) {
    let chunk = "x".repeat(256 * 1024);
    for _ in 0..12 {
        typist.type_in(&chunk);
    }
    typist.until("the input to be refused", |c| {
        c.errors
            .iter()
            .any(|(code, _)| *code == ErrorCode::INPUT_FULL)
    });
}

#[test]
fn a_program_that_ignores_its_input_does_not_keep_anyone_from_asking_or_ending_it() {
    let mut env = Env::new();
    // `sleep` never reads the terminal. The terminal is raw so that its own
    // line discipline does not throw the excess away: it stops taking bytes
    // instead, as it does for a program that reads in its own time.
    let id = env.start(
        "deaf",
        &[
            "--",
            "/bin/sh",
            "-c",
            "stty raw -echo; echo ready; exec sleep 300",
        ],
    );
    let mut typist = env.connect(&id);
    typist.attach(0);
    typist.wait_for("ready", 0);
    type_past_what_the_session_holds(&mut typist);

    // The one who typed is told and still served.
    assert_eq!(typist.status().state, State::RUNNING);
    typist.send(Message::Resize(Size {
        cols: 90,
        rows: 30,
        xpixel: 0,
        ypixel: 0,
    }));
    let start = Instant::now();
    while typist.status().size.cols != 90 {
        assert!(start.elapsed() < PATIENCE, "the size never followed");
    }
    // So is everyone else, and a new connection.
    let mut other = env.connect(&id);
    assert_eq!(other.status().state, State::RUNNING);
    let mut late = env.connect(&id);
    assert!(late.status().pid > 1);

    // And the session can be ended.
    other.send(Message::Terminate { force: false });
    typist.until("the end", |c| c.ended.is_some());
    env.wait_gone(&id);
}
