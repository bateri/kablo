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

use std::process::Command;
use std::time::{Duration, Instant};

use common::{Bridge, PATIENCE};
use kablo_proto::v1::{ErrorCode, Message, State};

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
            .args(["-o", "ControlMaster=no", "-o", "ControlPath=none"])
            .arg("tester@127.0.0.1")
            .arg(remote);
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
fn a_session_the_server_does_not_have_is_an_answer_not_a_hang() {
    let server = server();
    let (mut bridge, _) = server.bridge();
    bridge.send(Some("nobody-home"), Message::Attach { offset: 0 });
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
        assert!(
            stdout.contains(&format!("{id}: relay replaced")),
            "{stdout}"
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
