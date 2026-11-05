//! End to end, over a real sshd. Run by `make e2e`, which starts the server in
//! Docker and names it in the environment; without it these do not run, and
//! `cargo test` leaves them out (`--ignored` brings them in).
//!
//! The client is the one a terminal app would be: `ssh -T host kablo attach`
//! with a socket for its standard input and output, speaking the protocol to
//! the bridge on the other side of the login.

// Test support: a failure here is a failed test.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Bridge, PATIENCE, rebuilt, same, signal, whole};
use kablo_proto::v1::stream::{self, FailCode, MAX_WINDOW};
use kablo_proto::v1::{ErrorCode, Exit, Message, NewSession, Size, State};

/// The port of the TCP echo server inside the container (tests/e2e/Dockerfile),
/// which only the server's own `127.0.0.1` reaches.
const ECHO_PORT: u16 = 7001;

/// The sessions of the 15-at-once scenario: more than sshd's default
/// `MaxSessions` of 10.
const MANY: usize = 15;

struct Server {
    port: String,
    key: String,
    container: String,
}

fn server() -> Server {
    let get = |name: &str| {
        std::env::var(name)
            .unwrap_or_else(|_| panic!("{name} is not set; run this with `make e2e`"))
    };
    Server {
        port: get("KABLO_E2E_PORT"),
        key: get("KABLO_E2E_KEY"),
        container: get("KABLO_E2E_CONTAINER"),
    }
}

impl Server {
    /// `ssh -T tester@server <remote command>`.
    fn ssh(&self, remote: &str) -> Command {
        let mut command = self.ssh_options();
        command.arg("tester@127.0.0.1").arg(remote);
        command
    }

    /// `ssh` with the options of every connection to this server, and no host.
    fn ssh_options(&self) -> Command {
        let mut command = Command::new("ssh");
        command
            .args(["-T", "-p", &self.port, "-i", &self.key])
            .args(["-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes"])
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
            ])
            .args(["-o", "LogLevel=ERROR", "-o", "ConnectTimeout=10"])
            // One ssh connection per client: nothing is borrowed from a
            // master that the machine's own configuration might have.
            .args(["-o", "ControlMaster=no", "-o", "ControlPath=none"]);
        command
    }

    /// A client of the bridge, over a new ssh connection. Returns what came
    /// before the signature too.
    fn bridge(&self) -> (Bridge, Vec<u8>) {
        Bridge::over(self.ssh("kablo attach"))
    }

    /// Runs a command in the container as root and returns what it printed.
    fn exec(&self, program: &str, args: &[&str]) -> String {
        let out = Command::new("docker")
            .args(["exec", &self.container, program])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The process of the container whose command line has `needle`, if one.
    fn pid_of(&self, needle: &str) -> Option<u32> {
        self.exec("pgrep", &["-f", needle])
            .lines()
            .next()?
            .trim()
            .parse()
            .ok()
    }

    /// The parent of a process of the container.
    fn parent_of(&self, pid: u32) -> Option<u32> {
        let stat = self.exec("cat", &[&format!("/proc/{pid}/stat")]);
        // After the name, which is in parentheses and may hold anything: the
        // state, then the parent.
        let after = stat.rsplit_once(')')?.1;
        after.split_whitespace().nth(1)?.parse().ok()
    }

    /// How many processes of the container have a command line with `needle`.
    fn count(&self, needle: &str) -> usize {
        self.exec("pgrep", &["-f", needle]).lines().count()
    }

    /// Ends every session whose name starts with `prefix`. A session whose
    /// program has ended waits to be told so, so each is attached to first.
    fn clean_up(&self, prefix: &str) {
        let (mut bridge, _) = self.bridge();
        let ours: Vec<String> = bridge
            .list()
            .into_iter()
            .map(|s| s.id.as_str().to_owned())
            .filter(|id| id.starts_with(prefix))
            .collect();
        for id in &ours {
            bridge.attach(id, kablo_proto::v1::OFFSET_END);
            bridge.send(Some(id), Message::Terminate { force: true });
        }
        for id in &ours {
            let id = id.clone();
            bridge.until("the end of a session", move |b| {
                b.sessions.get(&id).is_some_and(|s| s.ended.is_some())
            });
        }
        let start = Instant::now();
        while self.count(&format!("kablo warden --id {prefix}")) > 0 && start.elapsed() < PATIENCE {
            bridge.pump(Duration::from_millis(100));
        }
    }
}

/// A prefix for the sessions of one scenario.
fn prefix(scenario: &str) -> String {
    format!("e2e-{scenario}-{}", std::process::id())
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn the_server_is_the_distributions_own_with_its_default_session_limit() {
    let server = server();
    let config = server.exec("sshd", &["-T"]);
    assert!(
        config.lines().any(|l| l == "maxsessions 10"),
        "the premise of the many-sessions scenario is MaxSessions 10: {config}"
    );
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn fifteen_sessions_travel_over_one_ssh_connection() {
    let server = server();
    let prefix = prefix("many");
    let (mut bridge, _) = server.bridge();

    let ids: Vec<String> = (0..MANY).map(|n| format!("{prefix}-{n}")).collect();
    for id in &ids {
        let status = bridge
            .try_new(Some(id), &["/bin/sh"])
            .unwrap_or_else(|e| panic!("{id}: {e:?}"));
        assert_eq!(status.id.as_str(), id);
        bridge.attach(id, 0);
    }
    // Everything is written first, then read: the sessions are open at once.
    for (n, id) in ids.iter().enumerate() {
        bridge.type_in(id, &format!("echo answer-{n}-$(({n}*3))\n"));
    }
    for (n, id) in ids.iter().enumerate() {
        bridge.wait_for(id, &format!("answer-{n}-{}\r\n", n * 3));
    }
    let listed = bridge.list();
    let ours: Vec<_> = listed
        .iter()
        .filter(|s| s.id.as_str().starts_with(&prefix))
        .collect();
    assert_eq!(ours.len(), MANY);
    assert!(
        ours.iter()
            .all(|s| s.state == State::RUNNING && s.clients == 1)
    );
    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);
    drop(bridge);
    server.clean_up(&prefix);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn a_dead_ssh_client_costs_nothing_and_the_next_connection_resumes_at_its_offsets() {
    let server = server();
    let prefix = prefix("resume");
    let ids: Vec<String> = (0..3).map(|n| format!("{prefix}-{n}")).collect();
    let (mut first, _) = server.bridge();
    for id in &ids {
        first.try_new(Some(id), &["/bin/sh"]).expect("a session");
        first.attach(id, 0);
        // A steady stream of numbered lines, which goes on whether anybody
        // reads it or not.
        first.type_in(
            id,
            "i=0; while [ $i -lt 300 ]; do echo line$i; i=$((i+1)); sleep 0.02; done &\n",
        );
    }
    for id in &ids {
        first.wait_for(id, "line10\r\n");
    }
    let resume: Vec<u64> = ids.iter().map(|id| first.seen(id).next).collect();
    let before: Vec<Vec<u8>> = ids.iter().map(|id| first.seen(id).data.clone()).collect();

    // The ssh client dies. Nothing is told to the sessions.
    drop(first);
    let start = Instant::now();
    while server.count("kablo attach") > 0 {
        assert!(start.elapsed() < PATIENCE, "the bridge outlived its login");
        std::thread::sleep(Duration::from_millis(100));
    }
    for id in &ids {
        assert_eq!(
            server.count(&format!("kablo warden --id {id}")),
            1,
            "{id} lives with no login at all"
        );
    }
    std::thread::sleep(Duration::from_secs(1));

    // A new connection asks for the byte after the last it had.
    let (mut second, _) = server.bridge();
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(second.attach(id, resume[n]), (resume[n], false), "{id}");
    }
    for id in &ids {
        second.wait_for(id, "line299\r\n");
    }
    for (n, id) in ids.iter().enumerate() {
        let mut whole = before[n].clone();
        whole.extend_from_slice(&second.seen(id).data);
        let text = String::from_utf8_lossy(&whole).into_owned();
        let numbers: Vec<u32> = text
            .split("\r\n")
            .filter_map(|l| l.rsplit_once("line"))
            .filter_map(|(_, n)| n.parse().ok())
            .collect();
        assert_eq!(
            numbers,
            (0..300).collect::<Vec<u32>>(),
            "{id}: not one byte more or less"
        );
    }
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    drop(second);
    server.clean_up(&prefix);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn what_a_startup_file_prints_before_the_program_starts_does_not_matter() {
    let server = server();
    let prefix = prefix("noise");
    let (mut bridge, noise) = server.bridge();
    let noise = String::from_utf8_lossy(&noise).into_owned();
    assert!(
        noise.contains("kablo-e2e: noise from .bashrc"),
        "the startup file did not talk, so this proves nothing: {noise:?}"
    );
    let id = format!("{prefix}-0");
    bridge.try_new(Some(&id), &["/bin/sh"]).expect("a session");
    bridge.attach(&id, 0);
    bridge.type_in(&id, "echo after-the-noise\n");
    bridge.wait_for(&id, "after-the-noise\r\n");
    drop(bridge);
    server.clean_up(&prefix);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn sessions_live_when_every_ssh_session_of_the_user_is_closed() {
    let server = server();
    let prefix = prefix("closed");
    let (mut bridge, _) = server.bridge();
    let id = format!("{prefix}-0");
    let started = bridge.try_new(Some(&id), &["/bin/sh"]).expect("a session");
    bridge.attach(&id, 0);
    bridge.type_in(&id, "sleep 300 &\n");
    bridge.type_in(&id, "echo up\n");
    bridge.wait_for(&id, "up\r\n");
    drop(bridge);

    // No process of the user is left from any login: only the sessions.
    let start = Instant::now();
    while server.count("sshd: tester") > 0 {
        assert!(start.elapsed() < PATIENCE, "a login is still open");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(server.count("kablo attach"), 0);
    assert_eq!(server.count(&format!("kablo warden --id {id}")), 1);
    assert_eq!(server.count(&format!("kablo relay --id {id}")), 1);

    // A new login finds it as it was.
    let listing = server.ssh("kablo list --json").output().unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    assert!(
        listing.contains(&format!(
            "\"id\":\"{id}\",\"state\":\"running\",\"pid\":{}",
            started.pid
        )),
        "{listing}"
    );
    let (mut again, _) = server.bridge();
    again.attach(&id, 0);
    again.wait_for(&id, "up\r\n");
    assert!(again.errors.is_empty());
    drop(again);
    server.clean_up(&prefix);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn a_session_an_app_asked_to_have_kept_is_read_after_the_app_was_told_and_then_goes() {
    let server = server();
    let prefix = prefix("kept");
    let id = format!("{prefix}-0");
    // The wait after the end is made short for the sessions of this login: the
    // bridge starts them with its own environment.
    let (mut app, _) = Bridge::over(server.ssh("KABLO_TEST_END_KEEP_MS=6000 kablo attach"));
    app.try_new(
        Some(&id),
        &["/bin/sh", "-c", "sleep 1; echo last words; exit 4"],
    )
    .expect("a session");
    // The view travels through ssh and the bridge as the app sent it.
    app.attach_keeping(&id, 0);
    app.until("the end", |b| {
        b.sessions.get(&id).is_some_and(|s| s.ended.is_some())
    });
    let told = Instant::now();
    drop(app);

    // A program on the server comes after the app was told, from a login of
    // its own: it waits for the end and reads the screen.
    let waited = server.ssh(&format!("kablo wait {id}")).output().unwrap();
    assert_eq!(waited.status.code(), Some(4), "{}", said(&waited.stderr));
    let screen = server
        .ssh(&format!("kablo capture {id} --trim"))
        .output()
        .unwrap();
    assert_eq!(screen.status.code(), Some(0), "{}", said(&screen.stderr));
    assert!(said(&screen.stdout).contains("last words"));
    assert!(
        told.elapsed() < Duration::from_secs(5),
        "too slow to say anything"
    );

    // Then the wait ends and nobody is left.
    let start = Instant::now();
    while server.count(&format!("kablo warden --id {id}")) > 0 {
        assert!(start.elapsed() < PATIENCE, "the session is still there");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(told.elapsed() >= Duration::from_secs(5));
    server.clean_up(&prefix);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn an_app_over_ssh_that_takes_a_terminal_lets_the_other_app_go_and_that_one_keeps_its_others() {
    let server = server();
    let prefix = prefix("take");
    let (one, two) = (format!("{prefix}-0"), format!("{prefix}-1"));
    // The app of one computer holds two terminals over one ssh connection.
    let (mut imac, _) = server.bridge();
    for id in [&one, &two] {
        imac.try_new(Some(id), &["/bin/sh"]).expect("a session");
        imac.attach_named(id, 0, "iMac");
    }
    // The app of another computer, over an ssh connection of its own, takes the
    // first.
    let (mut macbook, _) = server.bridge();
    let size = Size {
        cols: 100,
        rows: 30,
        xpixel: 0,
        ypixel: 0,
    };
    macbook.attach_taking(&one, 0, size, Some("MacBook"));

    // The first app is told which computer took it, through ssh and its bridge.
    let (session, code, text) = imac.next_error();
    assert_eq!(
        (session.as_deref(), code, text.as_str()),
        (Some(one.as_str()), ErrorCode::TAKEN, "MacBook")
    );
    // Its other terminal is as it was, on the same connection.
    imac.type_in(&two, "echo here-$((3+4))\n");
    imac.wait_for(&two, "here-7\r\n");
    // The one it lost is the other app's now.
    macbook.type_in(&one, "echo mine-$((6*7))\n");
    macbook.wait_for(&one, "mine-42\r\n");
    imac.type_in(&one, "echo not-mine\n");
    let (session, code, _) = imac.next_error();
    assert_eq!(
        (session.as_deref(), code),
        (Some(one.as_str()), ErrorCode::NOT_ATTACHED)
    );
    let status = macbook.status(&one);
    assert_eq!(status.clients, 1);
    assert_eq!(
        status.clients_named[0].map(|name| name.as_str().to_owned()),
        Some("MacBook".to_owned())
    );
    assert_eq!((status.size.cols, status.size.rows), (100, 30));
    assert_eq!(imac.status(&two).clients, 1);
    drop((imac, macbook));
    server.clean_up(&prefix);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn a_session_the_server_does_not_have_is_an_answer_not_a_hang() {
    let server = server();
    let (mut bridge, _) = server.bridge();
    bridge.send(Some("nobody-home"), Message::attach(0));
    let (session, code, _) = bridge.next_error();
    assert_eq!(session.as_deref(), Some("nobody-home"));
    assert_eq!(code, ErrorCode::UNKNOWN_SESSION);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn the_server_side_is_upgraded_while_the_ssh_connection_carries_on() {
    let server = server();
    let prefix = prefix("upgrade");
    let (mut bridge, _) = server.bridge();
    let ids: Vec<String> = (0..3).map(|n| format!("{prefix}-{n}")).collect();
    let mut shells = Vec::new();
    for id in &ids {
        shells.push(
            bridge
                .try_new(Some(id), &["/bin/sh"])
                .expect("a session")
                .pid,
        );
        bridge.attach(id, 0);
        // Numbered lines that go on all through the change.
        bridge.type_in(
            id,
            "i=0; while [ $i -lt 300 ]; do echo line$i; i=$((i+1)); sleep 0.02; done &\n",
        );
    }
    for id in &ids {
        bridge.wait_for(id, "line10\r\n");
    }
    let wardens: Vec<u32> = ids
        .iter()
        .map(|id| {
            server
                .pid_of(&format!("kablo warden --id {id}"))
                .expect("a warden")
        })
        .collect();

    // A newer kablo is put on the server (a copy of the installed one, which
    // is what matters: another file, run by another path) and the sessions are
    // moved to it over a second ssh connection, as an update would.
    let home = "/home/tester/kablo-new";
    let moves: Vec<String> = ids
        .iter()
        .map(|id| format!("kablo upgrade {id} {home}/kablo"))
        .collect();
    let remote = format!(
        "mkdir -p {home} && cp /usr/local/bin/kablo {home}/kablo && chmod 755 {home}/kablo && {}",
        moves.join(" && ")
    );
    let said = server.ssh(&remote).output().unwrap();
    let stdout = String::from_utf8_lossy(&said.stdout).into_owned();
    assert!(
        said.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&said.stderr)
    );
    for id in &ids {
        assert!(stdout.contains(&format!("{id}: replaced (now")), "{stdout}");
    }

    // Each warden renews itself while the lines are still coming, as the same
    // process: its pid stays, the shell is still its child, and the program it
    // runs is the new binary.
    for (n, id) in ids.iter().enumerate() {
        let wanted = format!("{home}/kablo warden --id {id}");
        let start = Instant::now();
        while server.count(&wanted) != 1 {
            assert!(
                start.elapsed() < PATIENCE,
                "{id}: its warden never ran the new binary"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(server.pid_of(&wanted), wardens.get(n).copied(), "{id}");
        assert_eq!(server.parent_of(shells[n]), wardens.get(n).copied(), "{id}");
    }
    bridge.pump(Duration::from_millis(50));
    for id in &ids {
        let text = String::from_utf8_lossy(&bridge.seen(id).data).into_owned();
        assert!(
            !text.contains("line299"),
            "{id}: the lines were over before the warden renewed"
        );
    }

    // The first connection never noticed.
    for id in &ids {
        bridge.wait_for(id, "line299\r\n");
    }
    assert!(!bridge.closed, "the ssh connection is still up");
    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(bridge.seen(id).attached.len(), 1, "{id} was attached once");
        let text = String::from_utf8_lossy(&bridge.seen(id).data).into_owned();
        let numbers: Vec<u32> = text
            .split("\r\n")
            .filter_map(|l| l.rsplit_once("line"))
            .filter_map(|(_, n)| n.parse().ok())
            .collect();
        assert_eq!(
            numbers,
            (0..300).collect::<Vec<u32>>(),
            "{id}: not one byte more or less"
        );
        assert_eq!(
            bridge.status(id).pid,
            shells[n],
            "{id}: the shell is the same"
        );
        assert_eq!(
            server.count(&format!("{home}/kablo relay --id {id}")),
            1,
            "{id}: its relay runs from the new binary"
        );
        assert_eq!(server.count(&format!("kablo warden --id {id}")), 1);
    }
    drop(bridge);
    server.clean_up(&prefix);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn a_long_gap_over_ssh_comes_back_as_the_right_screen() {
    let server = server();
    let prefix = prefix("gap");
    let id = format!("{prefix}-0");
    let (mut first, _) = server.bridge();
    // A ring small enough for a few hundred lines to wrap.
    let mut new = NewSession::default();
    new.argv = vec![b"/bin/sh".to_vec()];
    new.journal_size = Some(16 * 1024);
    first.try_new_with(Some(&id), new).expect("a session");
    first.attach(&id, 0);
    let filler = "x".repeat(200);
    first.type_in(
        &id,
        &format!("i=0; while [ $i -lt 300 ]; do echo L$i-{filler}; i=$((i+1)); sleep 0.01; done\n"),
    );
    first.wait_for(&id, "L299-");
    let settle = |bridge: &mut Bridge| loop {
        let status = bridge.status(&id);
        bridge.pump(Duration::from_millis(20));
        if status.offset == bridge.seen(&id).next {
            break;
        }
    };
    settle(&mut first);
    assert!(first.status(&id).retained_from > 0, "the ring wrapped");

    // A device that was not there: it asks for everything and is told what is
    // gone, then given the screen.
    let (mut second, _) = server.bridge();
    let (start, truncated) = second.attach(&id, 0);
    assert!(truncated);
    second.until("the picture", |b| {
        b.sessions
            .get(&id)
            .is_some_and(|s| s.pictures.last().is_some_and(|p| p.complete))
    });
    first.type_in(&id, "echo after-$((1+2))\n");
    first.wait_for(&id, "after-3\r\n");
    second.wait_for(&id, "after-3\r\n");
    settle(&mut first);
    second.until("the same end", |b| {
        b.sessions
            .get(&id)
            .is_some_and(|s| s.next == first_next(&first, &id))
    });
    let picture = second.seen(&id).pictures.last().unwrap().clone();
    assert_eq!(picture.offset, start);
    let at = usize::try_from(start).unwrap();
    assert_eq!(second.seen(&id).data, first.seen(&id).data[at..]);
    same(
        rebuilt(&picture, &second.seen(&id).data),
        whole(&first.seen(&id).data, 80, 24),
    );
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    drop((first, second));
    server.clean_up(&prefix);
}

/// Where the stream `id` has reached, as `bridge` has it.
fn first_next(bridge: &Bridge, id: &str) -> u64 {
    bridge.sessions.get(id).map_or(0, |seen| seen.next)
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn the_server_refuses_forwarding_and_has_an_echo_server_for_the_scenarios_that_use_it() {
    let server = server();
    let config = server.exec("sshd", &["-T"]);
    assert!(
        config.lines().any(|l| l == "disableforwarding yes"),
        "the premise of the forwarding scenarios is a sshd that refuses it: {config}"
    );
    // The echo server answers on the server's own loopback.
    let said = server.exec(
        "bash",
        &[
            "-c",
            &format!("exec 3<>/dev/tcp/127.0.0.1/{ECHO_PORT}; echo ping >&3; read -r line <&3; echo \"$line\""),
        ],
    );
    assert_eq!(said.trim(), "ping");
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn the_bridge_carries_what_sshs_own_forwarding_is_refused() {
    let server = server();

    // The control: `ssh -L` to the same echo server. The tunnel comes up (it
    // is the local half) and sshd refuses the connection made through it.
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut tunnel = server
        .ssh_options()
        // `-v` for the line that says why sshd refused: it is not an error
        // for ssh, so the options' LogLevel would hide it.
        .args(["-o", "ExitOnForwardFailure=yes"])
        .args(["-v", "-N", "-L", &format!("{free}:127.0.0.1:{ECHO_PORT}")])
        .arg("tester@127.0.0.1")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let mut through = loop {
        match TcpStream::connect(("127.0.0.1", free)) {
            Ok(socket) => break socket,
            Err(_) => {
                assert!(start.elapsed() < PATIENCE, "ssh never listened on {free}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    through.set_read_timeout(Some(PATIENCE)).unwrap();
    through.write_all(b"through ssh -L").unwrap();
    let mut heard = Vec::new();
    let _ = through.read_to_end(&mut heard);
    assert!(
        heard.is_empty(),
        "sshd forwarded although it was told not to: {heard:?}"
    );
    let _ = tunnel.kill();
    let mut said = String::new();
    let _ = tunnel.stderr.take().unwrap().read_to_string(&mut said);
    let _ = tunnel.wait();
    assert!(
        said.contains("administratively prohibited"),
        "ssh did not say why: {said:?}"
    );

    // The same server, the same echo, over streams of the bridge.
    let (mut bridge, _) = server.bridge();
    let (first, _) = bridge
        .connect_stream("127.0.0.1", ECHO_PORT, MAX_WINDOW)
        .unwrap();
    let (second, _) = bridge
        .connect_stream("127.0.0.1", ECHO_PORT, MAX_WINDOW)
        .unwrap();
    bridge.stream_send(first, b"request from the first", 100);
    bridge.stream_send(second, b"request from the second", 100);
    bridge.until("both echoes", |b| {
        b.flows[&first].data == b"request from the first"
            && b.flows[&second].data == b"request from the second"
    });
    // One of them says it is done and still hears the end of the other half.
    bridge.stream_eof(first);
    bridge.until("the end of the first", |b| b.flows[&first].eof);
    assert!(bridge.flows[&first].data == b"request from the first");
    bridge.stream_send(second, b" and more", 9);
    bridge.until("the rest of the second", |b| {
        b.flows[&second].data == b"request from the second and more"
    });
}

// --- commands --------------------------------------------------------------------

fn command(words: &[&str]) -> stream::Command {
    stream::Command::new(words.iter().map(|w| w.as_bytes().to_vec()).collect())
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn a_command_runs_where_sshs_own_forwarding_is_off_and_its_output_pipes_on() {
    let server = server();
    let (mut bridge, _) = server.bridge();

    // The server's directory, as a stream of an archive made there and listed
    // here: the work `ssh host tar -cf - …` does, over the one connection.
    let (id, _) = bridge
        .run_command(
            command(&["tar", "-cf", "-", "-C", "/etc/ssh", "sshd_config.d"]),
            MAX_WINDOW,
        )
        .unwrap();
    bridge.until("the end of the archive", |b| b.flows[&id].closed);
    let flow = &bridge.flows[&id];
    assert_eq!(flow.exited, Some(Exit::Code(0)));
    assert!(
        flow.stderr.is_empty(),
        "tar's own words are the only ones that could be here: {:?}",
        String::from_utf8_lossy(&flow.stderr)
    );
    let mut listing = Command::new("tar")
        .args(["-tf", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    listing.stdin.take().unwrap().write_all(&flow.data).unwrap();
    let listing = listing.wait_with_output().unwrap();
    let names = String::from_utf8_lossy(&listing.stdout);
    assert!(listing.status.success(), "{listing:?}");
    assert!(
        names.lines().any(|l| l == "sshd_config.d/kablo-e2e.conf"),
        "{names}"
    );
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn a_command_over_ssh_keeps_the_streams_apart_and_ends_with_the_programs_status() {
    let server = server();
    let (mut bridge, _) = server.bridge();

    let mut word = command(&[
        "/bin/sh",
        "-c",
        "echo \"out: $E2E_WORD\"; echo err >&2; cat; exit 7",
    ]);
    word.env = vec![b"E2E_WORD=over ssh".to_vec()];
    let (id, _) = bridge.run_command(word, MAX_WINDOW).unwrap();
    bridge.stream_send(id, b"typed\n", 6);
    bridge.stream_eof(id);
    bridge.until("the end", |b| b.flows[&id].closed);
    let flow = &bridge.flows[&id];
    assert_eq!(flow.exited, Some(Exit::Code(7)));
    // What the startup file prints is the ssh login's and not the program's: it
    // is noise before the signature.
    assert_eq!(flow.data, b"out: over ssh\ntyped\n");
    assert_eq!(flow.stderr, b"err\n");

    // Not a program the server has.
    let (_, code, text) = bridge
        .run_command(command(&["/no/such/program"]), MAX_WINDOW)
        .unwrap_err();
    assert_eq!(
        (code, text.as_str()),
        (FailCode::START_FAILED, "No such file or directory")
    );
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn the_servers_program_goes_with_the_connection_that_ran_it() {
    let server = server();
    // A sleep for a number of seconds of its own, so that it is found by its
    // command line and no other program of the server is taken for it.
    let tag = (1_000_000 + std::process::id()).to_string();
    let (mut bridge, _) = server.bridge();
    bridge
        .run_command(command(&["/bin/sleep", &tag]), MAX_WINDOW)
        .unwrap();
    let start = Instant::now();
    while server.count(&tag) == 0 {
        assert!(start.elapsed() < PATIENCE, "the program never started");
        std::thread::sleep(Duration::from_millis(100));
    }

    signal(bridge.pid(), libc::SIGKILL);
    // The bridge ends with its connection and takes the program with it.
    let start = Instant::now();
    while server.count(&tag) > 0 {
        assert!(
            start.elapsed() < PATIENCE,
            "the program outlived the connection"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// --- the app ---------------------------------------------------------------------

/// An app as one connects over ssh, saying it takes tickets.
fn app_over_ssh(server: &Server) -> Bridge {
    let (mut app, _) = server.bridge();
    app.take_tickets();
    app
}

/// What `kablo bateri ls` asks of the app: the one conversation these
/// scenarios have with it.
const ASKED: &[u8] = b"bateri 2\nls\n";

/// `ssh host kablo bateri ls`, with its output and errors piped. sshd gives it
/// no terminal (`-T`), so it is in none of the server's sessions.
fn asker(server: &Server) -> Child {
    server
        .ssh("kablo bateri ls")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// The app's reply to the question of `kablo bateri ls`: `text`, which the
/// command prints, and the end of the conversation.
fn reply(app: &mut Bridge, stream: u32, text: &str) {
    let reply = format!("bateri 2\nok text\n{text}");
    app.stream_send(stream, reply.as_bytes(), reply.len());
    app.stream_close(stream);
}

/// Waits for `child` to end, and returns how it did and what it wrote.
fn ended(mut child: Child) -> Output {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > PATIENCE {
            let _ = child.kill();
            panic!("kablo bateri never ended");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn said(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// What the server's startup file prints (tests/e2e/bashrc) before any command
/// that ssh runs there, on its standard output: ssh's, not kablo's.
const STARTUP_NOISE: &[u8] = b"kablo-e2e: noise from .bashrc before the program starts\n";

/// What `kablo bateri` itself wrote, of the standard output of a `ssh host
/// kablo bateri`: what is left after the startup file's words.
fn written_by_the_program(stdout: &[u8]) -> &[u8] {
    stdout
        .strip_prefix(STARTUP_NOISE)
        .unwrap_or_else(|| panic!("the startup file said nothing first: {stdout:?}"))
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn a_program_in_a_session_over_ssh_is_from_that_session_with_its_terminal_and_without_it() {
    let server = server();
    let prefix = prefix("mail-in");
    let id = format!("{prefix}-0");
    let mut app = app_over_ssh(&server);
    app.try_new(Some(&id), &["/bin/sh"]).expect("a session");
    app.attach(&id, 0);

    // From the session's own terminal: the kernel says which it is.
    app.type_in(&id, "kablo bateri ls; echo \"[terminal=$?]\"\n");
    let a = app.take_ticket(0, 4096);
    assert_eq!(app.tickets[0].0.as_deref(), Some(id.as_str()));
    app.until("what the program asked", |b| b.flows[&a].data == ASKED);
    reply(&mut app, a, "answer-terminal\n");
    app.wait_for(&id, "answer-terminal\r\n[terminal=0]\r\n");

    // Without its terminal (`setsid` drops it): the parents say which.
    app.type_in(&id, "setsid -w kablo bateri ls; echo \"[parents=$?]\"\n");
    let b = app.take_ticket(1, 4096);
    assert_eq!(app.tickets[1].0.as_deref(), Some(id.as_str()));
    app.until("what the program asked", |b2| b2.flows[&b].data == ASKED);
    reply(&mut app, b, "answer-parents\n");
    app.wait_for(&id, "answer-parents\r\n[parents=0]\r\n");

    assert!(app.errors.is_empty(), "{:?}", app.errors);
    drop(app);
    server.clean_up(&prefix);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn a_program_that_ssh_starts_is_from_no_session_and_what_the_app_says_comes_out() {
    let server = server();
    let mut app = app_over_ssh(&server);

    let program = asker(&server);
    let a = app.take_ticket(0, 4096);
    assert_eq!(app.tickets[0].0, None, "from no session");
    app.until("what the program asked", |b| {
        b.flows[&a].eof && b.flows[&a].data == ASKED
    });
    reply(&mut app, a, "answer\n");

    let ran = ended(program);
    assert_eq!(ran.status.code(), Some(0), "{}", said(&ran.stderr));
    assert_eq!(written_by_the_program(&ran.stdout), b"answer\n");
    assert_eq!(said(&ran.stderr), "");
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn an_app_that_goes_away_ends_the_open_conversation_without_a_close_and_the_next_one_finds_no_app()
{
    let server = server();
    let mut app = app_over_ssh(&server);

    let program = asker(&server);
    let a = app.take_ticket(0, 4096);
    app.until("what the program asked", |b| b.flows[&a].data == ASKED);
    drop(app);

    // `ls` has the server's own list for an app that is not there.
    let ran = ended(program);
    assert_eq!(ran.status.code(), Some(0), "{}", said(&ran.stderr));
    assert!(
        said(&ran.stderr).contains("the bateri app is not connected"),
        "{}",
        said(&ran.stderr)
    );

    // The bridge goes with its connection, and its mailbox with it.
    let start = Instant::now();
    while server.count("kablo attach") > 0 {
        assert!(start.elapsed() < PATIENCE, "the bridge outlived its ssh");
        std::thread::sleep(Duration::from_millis(100));
    }
    let ran = ended(asker(&server));
    assert_eq!(ran.status.code(), Some(0), "{}", said(&ran.stderr));
    let words = said(&ran.stderr);
    assert!(words.contains("the bateri app is not connected"), "{words}");
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn an_app_that_connects_again_is_given_the_conversations_not_the_half_dead_connection_it_left() {
    let server = server();
    let prefix = prefix("mail-half");
    let id = format!("{prefix}-0");

    // The app, at a terminal of the server.
    let mut old = app_over_ssh(&server);
    old.try_new(Some(&id), &["/bin/sh"]).expect("a session");
    old.attach(&id, 0);
    old.type_in(&id, "echo up\n");
    old.wait_for(&id, "up\r\n");

    // The network goes: the app's ssh hangs, nothing closes the connection,
    // and the bridge on the server goes on as if the app were there.
    signal(old.pid(), libc::SIGSTOP);

    // The app comes back over a new connection and takes the terminal again.
    let mut new = app_over_ssh(&server);
    new.attach(&id, 0);
    new.type_in(&id, "kablo bateri ls; echo \"[terminal=$?]\"\n");
    let a = new.take_ticket(0, 4096);
    assert_eq!(new.tickets[0].0.as_deref(), Some(id.as_str()));
    new.until("what the program asked", |b| b.flows[&a].data == ASKED);
    reply(&mut new, a, "answer\n");
    new.wait_for(&id, "answer\r\n[terminal=0]\r\n");

    // A program outside every session goes to the one used last as well.
    let program = asker(&server);
    let b = new.take_ticket(1, 4096);
    assert_eq!(new.tickets[1].0, None);
    reply(&mut new, b, "ok");
    let ran = ended(program);
    assert_eq!(ran.status.code(), Some(0), "{}", said(&ran.stderr));
    assert_eq!(written_by_the_program(&ran.stdout), b"ok");

    // The old connection comes back. Anything given to it would be ahead of the
    // answer to a ping on the same stream.
    signal(old.pid(), libc::SIGCONT);
    let before = old.pongs.len();
    old.send(None, Message::Ping(vec![0x5a]));
    old.until("the pong", |b| b.pongs.len() > before);
    assert!(old.tickets.is_empty(), "{:?}", old.tickets);

    drop((old, new));
    server.clean_up(&prefix);
}
