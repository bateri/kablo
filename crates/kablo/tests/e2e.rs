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
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{
    Bridge, Env, Exec, Forward, PATIENCE, client, is_over, read_exactly, rebuilt, same, signal,
    whole,
};
use kablo_proto::v1::{ErrorCode, Message, NewSession, State};

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

/// The arguments of `kablo forward` for the echo server, with `ssh host kablo
/// attach` as the carrier.
fn forward_args(server: &Server) -> Vec<String> {
    let carrier = server.ssh("kablo attach");
    let mut args = vec![
        "--bind".to_owned(),
        "127.0.0.1".to_owned(),
        format!("0:127.0.0.1:{ECHO_PORT}"),
        "--".to_owned(),
        carrier.get_program().to_str().unwrap().to_owned(),
    ];
    args.extend(carrier.get_args().map(|a| a.to_str().unwrap().to_owned()));
    args
}

fn start_forward(env: &Env, server: &Server) -> Forward {
    let args = forward_args(server);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Forward::start(env, &args)
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
fn kablo_forward_carries_what_sshs_own_forwarding_is_refused() {
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

    // The same server, the same echo, by kablo.
    let env = Env::new();
    let mut forward = start_forward(&env, &server);
    let address = forward.announced(1)[0];
    let mut first = client(address);
    let mut second = client(address);
    first.write_all(b"request from the first").unwrap();
    second.write_all(b"request from the second").unwrap();
    assert_eq!(
        read_exactly(&mut second, "request from the second".len()),
        b"request from the second"
    );
    assert_eq!(
        read_exactly(&mut first, "request from the first".len()),
        b"request from the first"
    );
    // One of them says it is done and still hears the end of the other half.
    first.shutdown(Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    first.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    second.write_all(b" and more").unwrap();
    assert_eq!(read_exactly(&mut second, 9), b" and more");
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn kablo_forward_ends_when_its_ssh_is_killed() {
    let server = server();
    let env = Env::new();
    let mut forward = start_forward(&env, &server);
    let address = forward.announced(1)[0];
    let mut socket = client(address);
    socket.write_all(b"alive").unwrap();
    assert_eq!(read_exactly(&mut socket, 5), b"alive");

    // The ssh that carries it is the one child `kablo forward` has.
    let children = Command::new("pgrep")
        .args(["-P", &forward.pid().to_string()])
        .output()
        .unwrap();
    let ssh: libc::pid_t = String::from_utf8_lossy(&children.stdout)
        .lines()
        .next()
        .expect("kablo forward has a child")
        .trim()
        .parse()
        .unwrap();
    // SAFETY: the pid of the ssh this test's `kablo forward` started.
    unsafe { libc::kill(ssh, libc::SIGKILL) };

    assert_eq!(forward.wait_exit().code(), Some(1));
    forward.said("ssh was killed by signal 9");
    assert!(is_over(&mut socket), "the connection outlived its carrier");
}

// --- kablo exec ------------------------------------------------------------------

/// A `kablo exec` of this test's directory with the server's ssh as the
/// carrier, and the program the arguments name.
fn start_exec(env: &Env, server: &Server, options: &[&str], program: &[&str]) -> Exec {
    let carrier = server.ssh("kablo attach");
    let mut words = vec![carrier.get_program().to_str().unwrap().to_owned()];
    words.extend(carrier.get_args().map(|a| a.to_str().unwrap().to_owned()));
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    Exec::start_over(env, options, &words, program)
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn kablo_exec_runs_a_program_where_sshs_own_forwarding_is_off_and_its_output_pipes_on() {
    let server = server();
    let env = Env::new();

    // The server's directory, as a stream of an archive made there and listed
    // here: the work `ssh host tar -cf - …` does, over the one connection.
    let mut archive = env.kablo();
    let carrier = server.ssh("kablo attach");
    archive
        .arg("exec")
        .arg("--")
        .arg(carrier.get_program())
        .args(carrier.get_args())
        .args(["--", "tar", "-cf", "-", "-C", "/etc/ssh", "sshd_config.d"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut archive = archive.spawn().unwrap();
    let listing = Command::new("tar")
        .args(["-tf", "-"])
        .stdin(Stdio::from(archive.stdout.take().unwrap()))
        .output()
        .unwrap();
    let ended = archive.wait_with_output().unwrap();
    assert_eq!(ended.status.code(), Some(0), "{ended:?}");
    let names = String::from_utf8_lossy(&listing.stdout);
    assert!(listing.status.success(), "{listing:?}");
    assert!(
        names.lines().any(|l| l == "sshd_config.d/kablo-e2e.conf"),
        "{names}"
    );
    assert!(
        ended.stderr.is_empty(),
        "tar's own words are the only ones that could be here: {:?}",
        String::from_utf8_lossy(&ended.stderr)
    );
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn kablo_exec_over_ssh_keeps_the_streams_apart_and_ends_with_the_programs_status() {
    let server = server();
    let env = Env::new();

    let mut running = start_exec(
        &env,
        &server,
        &["--env", "E2E_WORD=over ssh"],
        &[
            "/bin/sh",
            "-c",
            "echo \"out: $E2E_WORD\"; echo err >&2; cat; exit 7",
        ],
    );
    running.type_in(b"typed\n");
    running.end_input();
    let ran = running.finish();
    assert_eq!(ran.status.code(), Some(7), "{ran:?}");
    // What the startup file prints is the ssh login's and not the program's: it
    // is noise before the signature.
    assert_eq!(ran.out(), "out: over ssh\ntyped\n");
    assert_eq!(ran.err(), "err\n");

    // Not a program the server has.
    let ran = start_exec(&env, &server, &[], &["/no/such/program"]).finish();
    assert_eq!(ran.status.code(), Some(255));
    assert_eq!(
        ran.err(),
        "kablo: cannot run /no/such/program: No such file or directory\n"
    );
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn kablo_exec_ends_with_its_ssh_and_the_servers_program_goes_with_the_connection() {
    let server = server();
    let env = Env::new();
    // A sleep for a number of seconds of its own, so that it is found by its
    // command line and no other program of the server is taken for it.
    let tag = (1_000_000 + std::process::id()).to_string();
    let running = start_exec(&env, &server, &[], &["/bin/sleep", &tag]);
    let start = Instant::now();
    while server.count(&tag) == 0 {
        assert!(start.elapsed() < PATIENCE, "the program never started");
        std::thread::sleep(Duration::from_millis(100));
    }

    // The ssh that carries it is the one child `kablo exec` has.
    let children = Command::new("pgrep")
        .args(["-P", &running.child.id().to_string()])
        .output()
        .unwrap();
    let ssh: libc::pid_t = String::from_utf8_lossy(&children.stdout)
        .lines()
        .next()
        .expect("kablo exec has a child")
        .trim()
        .parse()
        .unwrap();
    // SAFETY: the pid of the ssh this test's `kablo exec` started.
    unsafe { libc::kill(ssh, libc::SIGKILL) };
    let ran = running.finish();
    assert_eq!(ran.status.code(), Some(255), "{ran:?}");
    assert!(
        ran.err().contains("ssh was killed by signal 9"),
        "{}",
        ran.err()
    );
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

// --- kablo app ---------------------------------------------------------------------

/// An app as one connects over ssh, saying it takes tickets.
fn app_over_ssh(server: &Server) -> Bridge {
    let (mut app, _) = server.bridge();
    app.take_tickets();
    app
}

/// `ssh host kablo app`, with its input, output and errors piped. sshd gives it
/// no terminal (`-T`), so it is in none of the server's sessions.
fn sender(server: &Server, input: Stdio) -> Child {
    server
        .ssh("kablo app")
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Waits for `child` to end, and returns how it did and what it wrote.
fn ended(mut child: Child) -> Output {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > PATIENCE {
            let _ = child.kill();
            panic!("kablo app never ended");
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

/// What `kablo app` itself wrote, of the standard output of a `ssh host kablo
/// app`: what is left after the startup file's words.
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
    app.type_in(
        &id,
        "echo from-the-terminal | kablo app; echo \"[terminal=$?]\"\n",
    );
    let a = app.take_ticket(0, 4096);
    assert_eq!(app.tickets[0].0.as_deref(), Some(id.as_str()));
    app.until("what the program wrote", |b| {
        b.flows[&a].data == b"from-the-terminal\n"
    });
    app.stream_send(a, b"answer-terminal\n", 16);
    app.stream_close(a);
    app.wait_for(&id, "answer-terminal\r\n[terminal=0]\r\n");

    // Without its terminal (`setsid` drops it): the parents say which.
    app.type_in(
        &id,
        "echo from-the-parents | setsid -w kablo app; echo \"[parents=$?]\"\n",
    );
    let b = app.take_ticket(1, 4096);
    assert_eq!(app.tickets[1].0.as_deref(), Some(id.as_str()));
    app.until("what the program wrote", |b2| {
        b2.flows[&b].data == b"from-the-parents\n"
    });
    app.stream_send(b, b"answer-parents\n", 15);
    app.stream_close(b);
    app.wait_for(&id, "answer-parents\r\n[parents=0]\r\n");

    assert!(app.errors.is_empty(), "{:?}", app.errors);
    drop(app);
    server.clean_up(&prefix);
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn a_program_that_ssh_starts_is_from_no_session_and_what_the_app_says_comes_out_as_it_is() {
    let server = server();
    let mut app = app_over_ssh(&server);

    let mut program = sender(&server, Stdio::piped());
    let mut input = program.stdin.take().unwrap();
    input
        .write_all(b"from outside \x1b[1mevery\x00session")
        .unwrap();
    drop(input);
    let a = app.take_ticket(0, 4096);
    assert_eq!(app.tickets[0].0, None, "from no session");
    app.until("what the program wrote", |b| {
        b.flows[&a].eof && b.flows[&a].data == b"from outside \x1b[1mevery\x00session"
    });
    app.stream_send(a, b"answer \x1b[0m\r\n\x00end", 8);
    app.stream_close(a);

    let ran = ended(program);
    assert_eq!(ran.status.code(), Some(0), "{}", said(&ran.stderr));
    assert_eq!(
        written_by_the_program(&ran.stdout),
        b"answer \x1b[0m\r\n\x00end"
    );
    assert_eq!(said(&ran.stderr), "");
}

#[test]
#[ignore = "needs the sshd container of `make e2e`"]
fn an_app_that_goes_away_ends_the_open_conversation_without_a_close_and_the_next_one_finds_no_app()
{
    let server = server();
    let mut app = app_over_ssh(&server);

    let mut program = sender(&server, Stdio::piped());
    let mut input = program.stdin.take().unwrap();
    input.write_all(b"half").unwrap();
    let a = app.take_ticket(0, 4096);
    app.until("what the program wrote", |b| b.flows[&a].data == b"half");
    drop(app);

    let ran = ended(program);
    drop(input);
    assert_eq!(ran.status.code(), Some(1), "{}", said(&ran.stderr));
    assert!(
        said(&ran.stderr).contains("the app went away"),
        "{}",
        said(&ran.stderr)
    );

    // The bridge goes with its connection, and its mailbox with it.
    let start = Instant::now();
    while server.count("kablo attach") > 0 {
        assert!(start.elapsed() < PATIENCE, "the bridge outlived its ssh");
        std::thread::sleep(Duration::from_millis(100));
    }
    let ran = ended(sender(&server, Stdio::null()));
    assert_eq!(ran.status.code(), Some(69), "{}", said(&ran.stderr));
    let words = said(&ran.stderr);
    assert!(words.contains("no app is connected"), "{words}");
    assert_eq!(written_by_the_program(&ran.stdout), b"");
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
    new.type_in(
        &id,
        "echo from-the-terminal | kablo app; echo \"[terminal=$?]\"\n",
    );
    let a = new.take_ticket(0, 4096);
    assert_eq!(new.tickets[0].0.as_deref(), Some(id.as_str()));
    new.until("what the program wrote", |b| {
        b.flows[&a].data == b"from-the-terminal\n"
    });
    new.stream_send(a, b"answer\n", 7);
    new.stream_close(a);
    new.wait_for(&id, "answer\r\n[terminal=0]\r\n");

    // A program outside every session goes to the one used last as well.
    let mut program = sender(&server, Stdio::piped());
    let mut input = program.stdin.take().unwrap();
    input.write_all(b"from outside").unwrap();
    drop(input);
    let b = new.take_ticket(1, 4096);
    assert_eq!(new.tickets[1].0, None);
    new.stream_send(b, b"ok", 2);
    new.stream_close(b);
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
