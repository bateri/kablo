//! The bridge, `kablo list` and `kablo connect`, with real sessions.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{Bridge, Env, PATIENCE};
use kablo_proto::WIRE;
use kablo_proto::v1::{ErrorCode, Message, NewSession, OFFSET_END, Size, State};

const SH: [&str; 2] = ["--", "/bin/sh"];

fn kill(pid: u32) {
    // SAFETY: a signal to a process of this test's own session.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

fn numbers(text: &str) -> Vec<u32> {
    // A number sits at the end of a line, after the prompt on the first one.
    text.split("\r\n")
        .filter_map(|l| l.rsplit_once("line"))
        .filter_map(|(_, n)| n.parse().ok())
        .collect()
}

#[test]
fn one_connection_carries_many_sessions_and_each_stays_itself() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let mut ids = Vec::new();
    for label in ["a", "b", "c"] {
        let id = env.name(label);
        env.track(&id);
        let status = bridge
            .try_new(Some(&id), &["/bin/sh"])
            .expect("a new session");
        assert_eq!(status.id.as_str(), id);
        assert!(status.pid > 0 && status.state == State::RUNNING);
        ids.push(id);
    }
    for id in &ids {
        assert_eq!(bridge.attach(id, 0), (0, false));
    }
    for (n, id) in ids.iter().enumerate() {
        bridge.type_in(id, &format!("echo marker-{}\n", n * 11));
    }
    for (n, id) in ids.iter().enumerate() {
        bridge.wait_for(id, &format!("marker-{}\r\n", n * 11));
    }
    for (n, id) in ids.iter().enumerate() {
        for other in 0..3 {
            let text = String::from_utf8_lossy(&bridge.seen(id).data).into_owned();
            // The echo of what was typed also names its marker, so the
            // output line is what tells sessions apart.
            let line = format!("marker-{}\r\n", other * 11);
            assert_eq!(
                text.contains(&line),
                other == n,
                "{id} has {other}: {text:?}"
            );
        }
    }

    let listed = bridge.list();
    let mut names: Vec<_> = listed.iter().map(|s| s.id.as_str().to_owned()).collect();
    names.sort();
    assert_eq!(names, ids);
    assert!(
        listed
            .iter()
            .all(|s| s.state == State::RUNNING && s.pid > 0)
    );

    bridge.send(None, Message::Ping(b"are you there".to_vec()));
    bridge.until("a pong", |b| !b.pongs.is_empty());
    assert_eq!(bridge.pongs[0], b"are you there");
    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);
}

#[test]
fn a_session_the_bridge_starts_takes_the_program_the_directory_and_the_size() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let id = env.name("shaped");
    env.track(&id);
    let request = NewSession {
        argv: vec![
            b"/bin/sh".to_vec(),
            b"-c".to_vec(),
            b"pwd; echo $GREETING; sleep 60".to_vec(),
        ],
        env: vec![(b"GREETING".to_vec(), b"hello from the client".to_vec())],
        cwd: Some(b"/usr".to_vec()),
        size: Some(Size {
            cols: 100,
            rows: 30,
            xpixel: 0,
            ypixel: 0,
        }),
        journal_size: Some(64 * 1024),
    };
    bridge.send(Some(&id), Message::New(request));
    bridge.until("created", |b| !b.created.is_empty());
    let status = bridge.created.pop().unwrap();
    assert_eq!((status.size.cols, status.size.rows), (100, 30));
    bridge.attach(&id, 0);
    bridge.wait_for(&id, "hello from the client\r\n");
    assert!(String::from_utf8_lossy(&bridge.seen(&id).data).contains("/usr\r\n"));
}

#[test]
fn the_bridge_names_a_session_when_asked_to_and_refuses_a_name_that_is_taken() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let status = bridge
        .try_new(None, &["/bin/sh"])
        .expect("a session with a made-up name");
    let made = status.id.as_str().to_owned();
    env.track(&made);
    assert!(!made.is_empty() && bridge.list().iter().any(|s| s.id.as_str() == made));

    let id = env.name("taken");
    env.track(&id);
    bridge
        .try_new(Some(&id), &["/bin/sh"])
        .expect("the first one");
    let (code, text) = bridge
        .try_new(Some(&id), &["/bin/sh"])
        .expect_err("the same name twice");
    assert_eq!(code, ErrorCode::SESSION_EXISTS, "{text}");
    assert!(text.contains(&id), "{text}");
    assert_eq!(bridge.list().len(), 2, "the refusal left the first alone");

    let (code, _) = bridge
        .try_new(Some(&env.name("nope")), &["/nonexistent/program"])
        .expect_err("a program that is not there");
    assert_eq!(code, ErrorCode::UNAVAILABLE);
    assert!(
        !env.socket(&env.name("nope")).exists(),
        "nothing was left behind"
    );
}

#[test]
fn what_the_bridge_cannot_do_is_answered_not_dropped() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    for message in [
        Message::Status,
        Message::Attach { offset: 0 },
        Message::Terminate { force: false },
    ] {
        bridge.send(Some("no-such-session"), message);
        let (session, code, _) = bridge.next_error();
        assert_eq!(session.as_deref(), Some("no-such-session"));
        assert_eq!(code, ErrorCode::UNKNOWN_SESSION);
    }
    // Typing at a session nobody attached to: the same answer the relay gives.
    bridge.send(Some("no-such-session"), Message::Input(b"x".to_vec()));
    assert_eq!(bridge.next_error().1, ErrorCode::NOT_ATTACHED);
    // A request that names no session.
    bridge.send(None, Message::Status);
    assert_eq!(bridge.next_error().1, ErrorCode::BAD_REQUEST);
    // A detach with nothing attached is nothing.
    bridge.send(Some("no-such-session"), Message::Detach);
    bridge.send(None, Message::Ping(b"still here".to_vec()));
    bridge.until("a pong", |b| !b.pongs.is_empty());
    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);
    assert!(bridge.list().is_empty());
}

#[test]
fn noise_before_the_hello_is_skipped_and_the_reply_is_clean() {
    let env = Env::new();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut command = env.kablo();
    command
        .arg("attach")
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(
            theirs.try_clone().unwrap(),
        )))
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(theirs)));
    let mut child = command.spawn().unwrap();
    let mut sock = ours;
    sock.set_read_timeout(Some(PATIENCE)).unwrap();
    let mut bytes = b"Welcome to a very chatty server\nLast login: never\n".to_vec();
    WIRE.write_hello(&mut bytes).unwrap();
    sock.write_all(&bytes).unwrap();
    let reply = WIRE.read_reply(&mut sock).unwrap();
    assert_eq!(reply.selected, 1);
    drop(sock);
    assert!(
        child.wait().unwrap().success(),
        "a closed input ends the bridge quietly"
    );
}

#[test]
fn a_client_that_goes_away_ends_the_bridge_and_nothing_else() {
    let mut env = Env::new();
    let id = env.start("stays", &SH);
    let mut bridge = Bridge::start(&env);
    bridge.attach(&id, 0);
    bridge.type_in(&id, "echo before\n");
    bridge.wait_for(&id, "before\r\n");
    let seen = bridge.seen(&id).next;
    bridge.hang_up();
    assert!(bridge.wait_exit().success());

    // The session did not notice, and the next client picks up from its offset.
    assert!(env.find("warden", &id).is_some());
    let mut direct = env.connect(&id);
    assert_eq!(
        direct.status().clients,
        0,
        "the bridge's connection is gone"
    );
    direct.attach(seen);
    direct.type_in("echo after\n");
    direct.wait_for("after\r\n", 0);
}

#[test]
fn a_relay_that_dies_under_the_bridge_is_not_noticed() {
    let mut env = Env::new();
    let id = env.start("under", &SH);
    let mut bridge = Bridge::start(&env);
    bridge.attach(&id, 0);
    let before = bridge.status(&id);
    let relay = env.find("relay", &id).expect("a relay");

    // Numbered lines, so that a missing or doubled byte shows as a missing or
    // doubled number.
    bridge.type_in(
        &id,
        "i=0; while [ $i -lt 150 ]; do echo line$i; i=$((i+1)); sleep 0.02; done &\n",
    );
    bridge.wait_for(&id, "line20\r\n");
    kill(relay);
    bridge.wait_for(&id, "line149\r\n");

    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);
    assert_eq!(
        bridge.seen(&id).attached.len(),
        1,
        "the client was attached once"
    );
    let new_relay = env.find("relay", &id).expect("a new relay");
    assert_ne!(new_relay, relay);
    assert_eq!(
        bridge.status(&id).pid,
        before.pid,
        "the shell was never touched"
    );
    let text = String::from_utf8_lossy(&bridge.seen(&id).data).into_owned();
    assert_eq!(numbers(&text), (0..150).collect::<Vec<u32>>());

    // And it keeps working: typing reaches the shell through the new relay.
    bridge.type_in(&id, "echo still-$((20+2))\n");
    bridge.wait_for(&id, "still-22\r\n");
}

#[test]
fn a_bridge_that_is_slow_to_be_read_loses_nothing() {
    let mut env = Env::new();
    let id = env.start("slow", &SH);
    let mut bridge = Bridge::start(&env);
    bridge.attach(&id, 0);
    // Three megabytes of numbered lines that the client does not read for a
    // while: the bridge and the relay hold their ends and drop nothing.
    bridge.type_in(&id, "i=0; while [ $i -lt 40000 ]; do echo line$i-padding-padding-padding-padding-padding; i=$((i+1)); done\n");
    std::thread::sleep(Duration::from_secs(2));
    bridge.wait_for(&id, "line39999-padding-padding-padding-padding-padding\r\n");
    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);
    assert_eq!(bridge.seen(&id).attached.len(), 1);
    let text = String::from_utf8_lossy(&bridge.seen(&id).data).into_owned();
    let found: Vec<u32> = text
        .split("\r\n")
        .filter_map(|l| {
            l.strip_prefix("line")
                .or_else(|| l.rsplit_once("line").map(|x| x.1))
        })
        .filter_map(|l| l.split('-').next()?.parse().ok())
        .collect();
    assert_eq!(found, (0..40000).collect::<Vec<u32>>());
}

#[test]
fn a_session_whose_relay_is_gone_for_good_is_listed_as_broken_and_a_dead_one_is_not_listed() {
    let mut env = Env::new();
    let broken = env.start("broken", &["--", "/bin/sh", "-c", "sleep 120"]);
    let healthy = env.start("healthy", &SH);
    let dead = env.start("dead", &["--", "/bin/sh", "-c", "sleep 120"]);

    // A killed keeper leaves its socket file behind.
    let (warden, relay) = (
        env.find("warden", &dead).unwrap(),
        env.find("relay", &dead).unwrap(),
    );
    let shell = env.connect(&dead).status().pid;
    kill(warden);
    kill(shell);
    kill(relay);
    let start = Instant::now();
    while env.find("warden", &dead).is_some() {
        assert!(start.elapsed() < PATIENCE);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(env.socket(&dead).exists(), "the file stays");

    // Five young relay deaths in a row: the keeper stops starting them.
    let mut killed = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let relay = loop {
            match env.find("relay", &broken) {
                Some(relay) if !killed.contains(&relay) => break relay,
                _ => {
                    assert!(start.elapsed() < PATIENCE, "no new relay after {killed:?}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        kill(relay);
        killed.push(relay);
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.find("relay", &broken), None);
    assert!(env.find("warden", &broken).is_some(), "the keeper stays");

    let mut bridge = Bridge::start(&env);
    let listed = bridge.list();
    let mut names: Vec<_> = listed.iter().map(|s| (s.id.as_str(), s.state.0)).collect();
    names.sort();
    let mut expected = vec![
        (broken.as_str(), State::BROKEN.0),
        (healthy.as_str(), State::RUNNING.0),
    ];
    expected.sort();
    assert_eq!(names, expected, "the dead one is not there");

    // The same through the command line, for a person and for a program.
    let table = env.kablo().arg("list").output().unwrap();
    let table = String::from_utf8_lossy(&table.stdout).into_owned();
    assert!(table.starts_with("ID "), "{table}");
    let row = table
        .lines()
        .find(|l| l.starts_with(&broken))
        .expect("a row for it");
    assert!(row.contains("broken"), "{row}");
    assert!(!table.contains(&dead), "{table}");
    let json = env.kablo().args(["list", "--json"]).output().unwrap();
    let json = String::from_utf8_lossy(&json.stdout).into_owned();
    assert!(
        json.contains(&format!("\"id\":\"{broken}\",\"state\":\"broken\"")),
        "{json}"
    );
    assert!(
        json.contains(&format!("\"id\":\"{healthy}\",\"state\":\"running\"")),
        "{json}"
    );

    // Attaching to it is told why not, after a while rather than never.
    let start = Instant::now();
    bridge.send(Some(&broken), Message::Attach { offset: 0 });
    let (session, code, _) = bridge.next_error();
    assert_eq!(
        (session.as_deref(), code),
        (Some(broken.as_str()), ErrorCode::UNAVAILABLE)
    );
    assert!(
        start.elapsed() >= Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
    assert!(
        bridge.list().iter().any(|s| s.id.as_str() == healthy),
        "the bridge goes on"
    );
}

#[test]
fn list_prints_a_table_for_people_and_json_for_programs() {
    let mut env = Env::new();
    let first = env.start("first", &["--", "/bin/sh", "-c", "sleep 120"]);
    let second = env.start(
        "second",
        &[
            "--cols",
            "100",
            "--rows",
            "30",
            "--",
            "/bin/sh",
            "-c",
            "sleep 121",
        ],
    );

    let said = env.kablo().arg("list").output().unwrap();
    assert!(said.status.success() && said.stderr.is_empty());
    let table = String::from_utf8(said.stdout).unwrap();
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), 3, "{table}");
    assert!(
        lines[0].starts_with("ID ") && lines[0].contains("STATE") && lines[0].contains("CLIENTS")
    );
    let mut rows = [lines[1], lines[2]];
    rows.sort();
    assert!(
        rows[0].starts_with(&first) && rows[0].contains("running"),
        "{table}"
    );
    assert!(
        rows[1].starts_with(&second) && rows[1].contains("100x30"),
        "{table}"
    );

    let said = env.kablo().args(["list", "--json"]).output().unwrap();
    let json = String::from_utf8(said.stdout).unwrap();
    assert!(
        json.starts_with("[\n{\"id\":") && json.ends_with("}\n]\n"),
        "{json}"
    );
    assert!(json.contains("\"cols\":100,\"rows\":30"), "{json}");
    assert_eq!(json.matches("{\"id\"").count(), 2);

    // No sessions: nothing for a person, an empty array for a program.
    let empty = Env::new();
    let said = empty.kablo().arg("list").output().unwrap();
    assert!(said.status.success() && said.stdout.is_empty() && said.stderr.is_empty());
    let said = empty.kablo().args(["list", "--json"]).output().unwrap();
    assert_eq!(said.stdout, b"[]\n");
}

#[test]
fn list_and_connect_take_only_what_they_know() {
    let env = Env::new();
    for args in [
        &["list", "--bogus"][..],
        &["connect"],
        &["connect", "a b"],
        &["attach", "x"],
    ] {
        let said = env.kablo().args(args).output().unwrap();
        assert_eq!(said.status.code(), Some(64), "{args:?}");
        assert!(
            String::from_utf8_lossy(&said.stderr).starts_with("kablo: "),
            "{args:?}"
        );
    }
}

// --- kablo connect, on a terminal of its own ------------------------------

/// A terminal for a program under test: a pty with the program as its session
/// leader, so that the program has it for a controlling terminal. It is read
/// whenever the test waits, as a terminal emulator reads: a terminal that is
/// not read is one some systems stop answering on, and what a program wrote
/// just before it exited is lost with the last close.
struct Tty {
    master: std::os::fd::OwnedFd,
    child: std::process::Child,
    /// Everything the terminal has shown.
    seen: Vec<u8>,
}

impl Tty {
    fn run(env: &Env, args: &[&str]) -> Tty {
        let pty = kablo::pty::open(Size {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        })
        .unwrap();
        let mut argv = vec![env!("CARGO_BIN_EXE_kablo").as_bytes().to_vec()];
        argv.extend(args.iter().map(|a| a.as_bytes().to_vec()));
        let request = NewSession {
            argv,
            env: vec![(
                b"KABLO_DIR".to_vec(),
                env.dir.as_os_str().as_encoded_bytes().to_vec(),
            )],
            ..NewSession::default()
        };
        let child = kablo::pty::spawn(&pty.slave, &request).unwrap();
        Tty {
            master: pty.master,
            child,
            seen: Vec::new(),
        }
    }

    fn type_in(&self, text: &[u8]) {
        let mut set = [kablo::sys::want(self.master.as_raw_fd(), libc::POLLOUT)];
        kablo::sys::poll(&mut set, Some(PATIENCE)).unwrap();
        kablo::sys::write_fd(self.master.as_fd(), text).unwrap();
    }

    /// Reads whatever the terminal shows in the next `wait`.
    fn drain(&mut self, wait: Duration) {
        let mut buf = [0u8; 4096];
        let mut set = [kablo::sys::want(self.master.as_raw_fd(), libc::POLLIN)];
        kablo::sys::poll(&mut set, Some(wait)).unwrap();
        if set[0].revents != 0
            && let Ok(n) = kablo::sys::read_fd(self.master.as_fd(), &mut buf)
        {
            self.seen.extend_from_slice(&buf[..n]);
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.seen).into_owned()
    }

    /// Reads the terminal until `needle` has been shown.
    fn show(&mut self, needle: &str) {
        let start = Instant::now();
        while !self.text().contains(needle) {
            assert!(
                start.elapsed() < PATIENCE,
                "waiting for {needle:?}, the terminal showed {:?}",
                self.text()
            );
            self.drain(Duration::from_millis(50));
        }
    }

    /// Waits for a shell's prompt (`$ `, or `# ` for root).
    fn prompt(&mut self) {
        let start = Instant::now();
        while !(self.text().contains("$ ") || self.text().contains("# ")) {
            assert!(
                start.elapsed() < PATIENCE,
                "no prompt; the terminal showed {:?}",
                self.text()
            );
            self.drain(Duration::from_millis(50));
        }
    }

    fn wait(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // What it wrote on its way out.
                self.drain(Duration::from_millis(50));
                return status;
            }
            assert!(
                start.elapsed() < PATIENCE,
                "the program never exited; the terminal showed {:?}",
                self.text()
            );
            self.drain(Duration::from_millis(20));
        }
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn connect_puts_a_session_on_a_terminal_and_a_detach_leaves_it_running() {
    let mut env = Env::new();
    let id = env.start("connected", &SH);
    let mut tty = Tty::run(&env, &["connect", &id]);

    // What is typed reaches the session, what it writes reaches the screen.
    let mut control = env.connect(&id);
    let start = Instant::now();
    while control.status().clients == 0 {
        assert!(start.elapsed() < PATIENCE, "never attached");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Typing ahead of a shell that has not started reading loses the bytes
    // (it flushes the terminal as it sets it up): wait for its prompt.
    tty.prompt();
    tty.type_in(b"echo $((6*7))\n");
    tty.show("42\r\n");

    // Resizing the window resizes the session's terminal.
    kablo::sys::set_window_size(
        std::os::fd::AsFd::as_fd(&tty.master),
        Size {
            cols: 101,
            rows: 33,
            xpixel: 0,
            ypixel: 0,
        },
    )
    .unwrap();
    let start = Instant::now();
    while control.status().size.cols != 101 {
        assert!(start.elapsed() < PATIENCE, "the size never followed");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Ctrl-\ leaves the session running.
    tty.type_in(&[0x1c]);
    let status = tty.wait();
    assert_eq!(status.code(), Some(0));
    tty.show("detached");
    assert_eq!(control.status().clients, 0);
    assert!(env.find("warden", &id).is_some());

    // And coming back shows what happened meanwhile: the journal is replayed.
    control.attach(OFFSET_END);
    control.type_in("echo while-away\n");
    control.wait_for("while-away\r\n", 0);
    let mut again = Tty::run(&env, &["connect", &id]);
    again.show("while-away\r\n");
    again.show("42\r\n");
    again.type_in(&[0x1c]);
    assert_eq!(again.wait().code(), Some(0));
}

#[test]
fn connect_ends_with_the_programs_own_status_when_it_ends() {
    let mut env = Env::new();
    let id = env.start("finishes", &SH);
    let mut tty = Tty::run(&env, &["connect", &id]);
    tty.prompt();
    tty.type_in(b"echo up\n");
    tty.show("up\r\n");
    tty.type_in(b"exit 7\n");
    let status = tty.wait();
    assert_eq!(status.code(), Some(7));
    tty.show("the session ended (exit status 7)");
}

#[test]
fn connect_to_a_session_that_is_not_there_says_so() {
    let env = Env::new();
    let mut tty = Tty::run(&env, &["connect", "nothing-here"]);
    tty.show("no such session");
    assert_eq!(tty.wait().code(), Some(1));
}

#[test]
fn connect_survives_its_relay_being_replaced() {
    let mut env = Env::new();
    let id = env.start("replaced", &SH);
    let mut tty = Tty::run(&env, &["connect", &id]);
    tty.prompt();
    tty.type_in(b"echo one\n");
    tty.show("one\r\n");
    kill(env.find("relay", &id).unwrap());
    // Typing does not reach a relay that is gone for a moment; try until it
    // goes through, which is the person's own experience.
    let start = Instant::now();
    while !tty.text().contains("two-2\r\n") {
        assert!(start.elapsed() < PATIENCE);
        tty.type_in(b"echo two-$((1+1))\n");
        let probe = Instant::now();
        while probe.elapsed() < Duration::from_millis(300) && !tty.text().contains("two-2\r\n") {
            tty.drain(Duration::from_millis(50));
        }
    }
    tty.type_in(&[0x1c]);
    assert_eq!(tty.wait().code(), Some(0));
}

#[test]
fn attach_will_not_speak_binary_to_a_terminal() {
    let env = Env::new();
    let mut tty = Tty::run(&env, &["attach"]);
    tty.show("kablo connect");
    assert_eq!(tty.wait().code(), Some(64));
}

// --- surviving the end of a login -------------------------------------------

/// A `systemd-run` that records how it was called and then does what the real
/// one does in scope mode: runs the command.
fn fake_systemd_run(env: &Env, log: &std::path::Path, works: bool) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = env
        .dir
        .join(if works { "fake-run-ok" } else { "fake-run-bad" });
    let body = if works {
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
            log.display()
        )
    } else {
        format!("#!/bin/sh\necho \"$@\" >> {}\nexit 1\n", log.display())
    };
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// The directory systemd keeps its marks of lingering users in, with this
/// user's mark in it when `lingering`.
fn linger_dir(env: &Env, lingering: bool) -> std::path::PathBuf {
    let dir = env
        .dir
        .join(if lingering { "linger-on" } else { "linger-off" });
    std::fs::create_dir_all(&dir).unwrap();
    if lingering {
        let user = std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout;
        std::fs::write(dir.join(String::from_utf8(user).unwrap().trim()), "").unwrap();
    }
    dir
}

#[test]
fn the_keeper_is_started_under_systemd_run_where_there_is_one_and_without_where_it_fails() {
    let mut env = Env::new();
    let log = env.dir.join("runner.log");
    let linger = linger_dir(&env, true);

    // A `systemd-run` that works: the keeper runs under it.
    let works = fake_systemd_run(&env, &log, true);
    let id = env.name("scoped");
    env.track(&id);
    let said = env
        .kablo()
        .env("KABLO_SYSTEMD_RUN", &works)
        .env("KABLO_LINGER_DIR", &linger)
        .args(["new", "--id", &id, "--", "/bin/sh"])
        .output()
        .unwrap();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(
        logged.starts_with("--user --scope --quiet --collect "),
        "{logged}"
    );
    assert!(
        logged.contains(&format!("warden --id {id} --dir ")),
        "{logged}"
    );
    let mut conn = env.connect(&id);
    conn.attach(0);
    conn.type_in("echo scoped-ok\n");
    conn.wait_for("scoped-ok\r\n", 0);

    // One that starts nothing: the keeper is started without it.
    std::fs::remove_file(&log).unwrap();
    let bad = fake_systemd_run(&env, &log, false);
    let id = env.name("unscoped");
    env.track(&id);
    let said = env
        .kablo()
        .env("KABLO_SYSTEMD_RUN", &bad)
        .env("KABLO_LINGER_DIR", &linger)
        .args(["new", "--id", &id, "--", "/bin/sh"])
        .output()
        .unwrap();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
    assert!(
        std::fs::read_to_string(&log).unwrap().contains("--scope"),
        "it was tried"
    );
    let mut conn = env.connect(&id);
    conn.attach(0);
    conn.type_in("echo unscoped-ok\n");
    conn.wait_for("unscoped-ok\r\n", 0);

    // One that is not there at all, and none wanted.
    for runner in ["/nonexistent/systemd-run", ""] {
        let id = env.name(&format!("none{}", runner.len()));
        env.track(&id);
        let said = env
            .kablo()
            .env("KABLO_SYSTEMD_RUN", runner)
            .env("KABLO_LINGER_DIR", &linger)
            .args(["new", "--id", &id, "--", "/bin/sh"])
            .output()
            .unwrap();
        assert!(
            said.status.success(),
            "{runner:?}: {}",
            String::from_utf8_lossy(&said.stderr)
        );
    }
}

#[test]
fn the_scope_is_for_a_user_who_lingers_and_nobody_else() {
    // Without lingering the user manager stops after the last login and takes
    // the scope, and the session with it: a plain `setsid` does better, so the
    // runner is not used at all, however good it is.
    let mut env = Env::new();
    let log = env.dir.join("runner.log");
    let works = fake_systemd_run(&env, &log, true);
    for (label, linger) in [
        ("nobody", linger_dir(&env, false)),
        ("nothere", env.dir.join("no-such-linger-dir")),
    ] {
        let id = env.name(label);
        env.track(&id);
        let said = env
            .kablo()
            .env("KABLO_SYSTEMD_RUN", &works)
            .env("KABLO_LINGER_DIR", &linger)
            .args(["new", "--id", &id, "--", "/bin/sh"])
            .output()
            .unwrap();
        assert!(
            said.status.success(),
            "{}",
            String::from_utf8_lossy(&said.stderr)
        );
        assert!(
            !log.exists(),
            "the runner was used by a user who does not linger"
        );
        let mut conn = env.connect(&id);
        conn.attach(0);
        conn.type_in("echo plain-ok\n");
        conn.wait_for("plain-ok\r\n", 0);
    }
}

#[test]
fn a_keeper_that_is_refused_is_not_tried_again_without_the_runner() {
    // A refusal in words (the name is taken) is the keeper's answer, not a
    // runner that failed: it is passed on, not retried.
    let mut env = Env::new();
    let log = env.dir.join("runner.log");
    let linger = linger_dir(&env, true);
    let works = fake_systemd_run(&env, &log, true);
    let id = env.start("twice", &SH);
    let said = env
        .kablo()
        .env("KABLO_SYSTEMD_RUN", &works)
        .env("KABLO_LINGER_DIR", &linger)
        .args(["new", "--id", &id])
        .output()
        .unwrap();
    assert_eq!(said.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&said.stderr).contains("exists already"));
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
}
