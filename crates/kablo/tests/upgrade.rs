//! Replacing a session's relay while the session is in use: the shell is
//! never touched, a client that is attached is not told, and a replacement
//! that does not take leaves the old relay serving. The warden renews itself
//! into the same binary behind it, as the same process.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Bridge, Env, PATIENCE, binary_of};
use kablo_proto::v1::{ErrorCode, Exit, Message, State};

const SH: [&str; 2] = ["--", "/bin/sh"];

fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: a signal to a process of this test's own session.
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

fn numbers(text: &str) -> Vec<u32> {
    // A number sits at the end of a line, after the prompt on the first one.
    text.split("\r\n")
        .filter_map(|l| l.rsplit_once("line"))
        .filter_map(|(_, n)| n.parse().ok())
        .collect()
}

fn bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

/// The pid of the relay of `id` that is run from `binary`, once there is one.
fn relay_from(env: &Env, id: &str, binary: &Path) -> u32 {
    let start = Instant::now();
    loop {
        let wanted = format!("{} relay --id {id}", binary.display());
        if let Some((pid, _, _)) = env
            .processes()
            .into_iter()
            .find(|(_, _, command)| command.starts_with(&wanted))
        {
            return pid;
        }
        assert!(
            start.elapsed() < PATIENCE,
            "no relay of {id} runs from {}",
            binary.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_relay_is_replaced_under_a_bridge_and_nobody_notices() {
    let mut env = Env::new();
    let id = env.start("swap", &SH);
    let newer = env.newer_kablo("a");
    let mut bridge = Bridge::start(&env);
    bridge.attach(&id, 0);
    let before = bridge.status(&id);
    let relay = env.find("relay", &id).expect("a relay");
    let warden = env.find("warden", &id).expect("a warden");

    // Numbered lines, so that a missing or doubled byte shows as a missing or
    // doubled number; they go on all through the change.
    bridge.type_in(
        &id,
        "i=0; while [ $i -lt 300 ]; do echo line$i; i=$((i+1)); sleep 0.02; done &\n",
    );
    bridge.wait_for(&id, "line20\r\n");
    let said = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&newer)
        .output()
        .unwrap();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
    assert!(
        String::from_utf8_lossy(&said.stdout).contains(&format!("{id}: replaced (now")),
        "{}",
        String::from_utf8_lossy(&said.stdout)
    );
    // The warden renews while the lines are still coming, not after them: the
    // numbers that follow have gone through its change.
    assert_eq!(
        env.warden_running(&id, &newer),
        warden,
        "the parent is the same process, now running the new binary"
    );
    bridge.pump(Duration::from_millis(50));
    assert!(
        !String::from_utf8_lossy(&bridge.seen(&id).data).contains("line299"),
        "the lines were over before the warden renewed"
    );
    assert_eq!(env.parent_of(before.pid), Some(warden));
    bridge.wait_for(&id, "line299\r\n");

    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);
    assert_eq!(
        bridge.seen(&id).attached.len(),
        1,
        "the client was attached once"
    );
    let text = String::from_utf8_lossy(&bridge.seen(&id).data).into_owned();
    assert_eq!(numbers(&text), (0..300).collect::<Vec<u32>>());
    assert_eq!(
        bridge.status(&id).pid,
        before.pid,
        "the shell was never touched"
    );
    assert_eq!(env.find("warden", &id), Some(warden), "nor was its parent");
    let now = relay_from(&env, &id, &newer);
    assert_ne!(now, relay, "the relay is another process");
    assert_eq!(
        env.find("relay", &id),
        Some(now),
        "the old relay is gone, and only the new one is"
    );

    // A relay that dies from here on is replaced from the new binary.
    signal(now, libc::SIGKILL);
    let start = Instant::now();
    let again = loop {
        match env.find("relay", &id) {
            Some(pid) if pid != now => break pid,
            _ => {
                assert!(start.elapsed() < PATIENCE, "no relay took its place");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    assert_eq!(relay_from(&env, &id, &newer), again);
    // Typed after the bridge is served again: what is typed into a connection
    // that is being torn down is lost with it, upgrade or not.
    bridge.status(&id);
    bridge.type_in(&id, "echo after-$((20+2))\n");
    bridge.wait_for(&id, "after-22\r\n");
    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);
}

#[test]
fn an_upgrade_that_does_not_take_leaves_the_relay_serving() {
    let mut env = Env::new();
    let id = env.start("fails", &SH);
    let relay = env.find("relay", &id).expect("a relay");
    let mut a = env.connect(&id);
    let shell = a.status().pid;
    a.attach(0);

    // A file that is fine to run and a relay that is not: it ends at once.
    let dies = env.script("dies", "#!/bin/sh\nexit 1\n");
    a.send(Message::Upgrade { path: bytes(&dies) });
    a.until("a refusal", |c| !c.errors.is_empty());
    assert_eq!(a.errors[0].0, ErrorCode::UPGRADE_FAILED);
    assert!(a.errors[0].1.contains("stopped"), "{:?}", a.errors[0]);
    // The connection that asked is still served by the relay it asked.
    a.type_in("echo still-$((6*7))\n");
    a.wait_for("still-42\r\n", 0);
    assert_eq!(env.find("relay", &id), Some(relay));

    // A path that is not one to run is refused for what is wrong with it.
    let plain = env.script("plain", "#!/bin/sh\n");
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    let shared = env.script("shared", "#!/bin/sh\n");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o775)).unwrap();
    let wrong: [(Vec<u8>, &str); 5] = [
        (b"kablo".to_vec(), "not an absolute path"),
        (bytes(&env.dir.join("nothing")), "cannot read"),
        (bytes(&env.dir), "not a regular file"),
        (bytes(&plain), "not executable"),
        (bytes(&shared), "written by others"),
    ];
    for (path, why) in wrong {
        let seen = a.errors.len();
        a.send(Message::Upgrade { path });
        a.until(why, |c| c.errors.len() > seen);
        let (code, text) = &a.errors[seen];
        assert_eq!(*code, ErrorCode::UPGRADE_FAILED);
        assert!(text.contains(why), "{text:?} should say {why:?}");
    }
    assert_eq!(env.find("relay", &id), Some(relay));

    // One that starts and never speaks does not make the next upgrade wait.
    let mute = env.script("mute", "#!/bin/sh\nexec sleep 1000\n");
    a.send(Message::Upgrade { path: bytes(&mute) });
    a.pump(Duration::from_millis(300));
    let newer = env.newer_kablo("b");
    a.send(Message::Upgrade {
        path: bytes(&newer),
    });
    a.until_closed();
    let mut b = env.connect(&id);
    assert_eq!(b.status().pid, shell, "the shell was never touched");
    let now = relay_from(&env, &id, &newer);
    assert_ne!(now, relay);
    b.attach(0);
    b.type_in("echo carried-on-$((7*6))\n");
    b.wait_for("carried-on-42\r\n", 0);
}

#[test]
fn a_session_whose_program_has_ended_is_not_moved() {
    let mut env = Env::new();
    let id = env.start("ended", &["--", "/bin/sh", "-c", "exit 3"]);
    let mut a = env.connect(&id);
    let start = Instant::now();
    while a.status().state != State::ENDED {
        assert!(start.elapsed() < PATIENCE, "the program never ended");
        std::thread::sleep(Duration::from_millis(50));
    }
    let newer = env.newer_kablo("c");
    a.send(Message::Upgrade {
        path: bytes(&newer),
    });
    a.until("a refusal", |c| !c.errors.is_empty());
    assert_eq!(a.errors[0].0, ErrorCode::UPGRADE_FAILED);
    assert!(a.errors[0].1.contains("has ended"), "{:?}", a.errors[0]);
}

#[test]
fn a_relay_that_will_not_let_go_is_not_waited_for_forever() {
    let mut env = Env::new();
    let id = env.start("stuck", &SH);
    let relay = env.find("relay", &id).expect("a relay");
    let mut bridge = Bridge::start(&env);
    bridge.attach(&id, 0);
    bridge.type_in(
        &id,
        "i=0; while [ $i -lt 350 ]; do echo line$i; i=$((i+1)); sleep 0.02; done &\n",
    );
    bridge.wait_for(&id, "line10\r\n");

    // The new relay takes two seconds to get ready; the old one is stopped in the
    // meantime, so it cannot hear `Quit` when it comes.
    let real = env!("CARGO_BIN_EXE_kablo");
    let slow = env.script(
        "slow",
        &format!("#!/bin/sh\nsleep 2\nexec '{real}' \"$@\"\n"),
    );
    let mut asker = env.connect(&id);
    asker.send(Message::Upgrade { path: bytes(&slow) });
    asker.pump(Duration::from_millis(300));
    signal(relay, libc::SIGSTOP);

    // Not for ever: the old relay is killed and the new one takes over, but
    // only once the warden has waited for it to answer `Quit`.
    let stopped = Instant::now();
    while env.find("relay", &id) == Some(relay) {
        assert!(
            stopped.elapsed() < PATIENCE,
            "the old relay was never let go"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        stopped.elapsed() >= Duration::from_secs(3),
        "the stopped relay was given up on after {:?}",
        stopped.elapsed()
    );
    bridge.wait_for(&id, "line349\r\n");
    let text = String::from_utf8_lossy(&bridge.seen(&id).data).into_owned();
    assert_eq!(numbers(&text), (0..350).collect::<Vec<u32>>());
    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);
    assert_ne!(env.find("relay", &id), Some(relay));
    assert!(env.find("relay", &id).is_some(), "a relay serves");
}

#[test]
fn the_command_says_what_it_did_and_to_whom() {
    let mut env = Env::new();
    let one = env.start("one", &SH);
    let two = env.start("two", &SH);
    let newer = env.newer_kablo("d");

    // Everything on the machine.
    let said = env
        .kablo()
        .args(["upgrade", "--all"])
        .arg(&newer)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&said.stdout).into_owned();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
    for id in [&one, &two] {
        assert!(stdout.contains(&format!("{id}: replaced (now")), "{stdout}");
        relay_from(&env, id, &newer);
    }

    // One that is not there, a path that is not one, and a line that is not
    // a command.
    let said = env
        .kablo()
        .args(["upgrade", "nobody"])
        .arg(&newer)
        .output()
        .unwrap();
    assert_eq!(said.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&said.stderr).contains("nobody: there is no such session"));
    let said = env
        .kablo()
        .args(["upgrade", &one, "/nonexistent/kablo"])
        .output()
        .unwrap();
    assert_eq!(said.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&said.stderr).into_owned();
    assert!(
        stderr.contains(&format!(
            "{one}: not replaced: cannot read /nonexistent/kablo"
        )),
        "{stderr}"
    );
    for args in [
        &["upgrade"][..],
        &["upgrade", "--all"],
        &["upgrade", "a", "b", "c"],
    ] {
        let said = env.kablo().args(args).output().unwrap();
        assert_eq!(said.status.code(), Some(64), "{args:?}");
    }
    // Relative to where the command is run, as a person writes it.
    let said = env
        .kablo()
        .current_dir(newer.parent().unwrap())
        .args(["upgrade", &two, "kablo"])
        .output()
        .unwrap();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
}

/// Ends a process that no session of the test owns any more when the test
/// is over, whichever way it ends.
struct Orphan(u32);

impl Drop for Orphan {
    fn drop(&mut self) {
        signal(self.0, libc::SIGKILL);
    }
}

/// A session upgraded to a copy of this build, once its warden has renewed
/// itself into it: the id, the copy and the warden's pid.
fn renewed(env: &mut Env, label: &str) -> (String, PathBuf, u32) {
    let id = env.start(label, &SH);
    let newer = env.newer_kablo(label);
    let warden = env.find("warden", &id).expect("a warden");
    let said = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&newer)
        .output()
        .unwrap();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
    assert_eq!(
        env.warden_running(&id, &newer),
        warden,
        "the same process, now another program"
    );
    (id, newer, warden)
}

#[test]
fn a_renewed_warden_is_still_the_parent_of_the_shell_and_collects_its_exit_code() {
    let mut env = Env::new();
    let (id, _, warden) = renewed(&mut env, "parent");
    let mut a = env.connect(&id);
    let shell = a.status().pid;
    assert_eq!(env.parent_of(shell), Some(warden));
    assert!(
        env.command_of("warden", &id)
            .is_some_and(|command| command.contains("--resume")),
        "{:?}",
        env.command_of("warden", &id)
    );
    a.attach(0);
    a.type_in("exit 7\n");
    a.until("the end", |c| c.ended.is_some());
    assert_eq!(a.ended, Some(Exit::Code(7)));
    env.wait_gone(&id);
}

#[test]
fn a_renewed_warden_restarts_a_relay_hangs_up_on_terminate_and_cleans_up() {
    let mut env = Env::new();
    let (id, newer, warden) = renewed(&mut env, "works");
    let relay = relay_from(&env, &id, &newer);

    signal(relay, libc::SIGKILL);
    let start = Instant::now();
    let again = loop {
        match env.find("relay", &id) {
            Some(pid) if pid != relay => break pid,
            _ => {
                assert!(start.elapsed() < PATIENCE, "no relay took its place");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    assert_eq!(relay_from(&env, &id, &newer), again);
    assert_eq!(env.find("warden", &id), Some(warden));

    let mut a = env.connect(&id);
    a.attach(0);
    a.type_in("echo served-by-$((6*7))\n");
    a.wait_for("served-by-42\r\n", 0);
    a.send(Message::Terminate { force: false });
    a.until("the end", |c| c.ended.is_some());
    // The renewed warden heard that the end was delivered and cleaned up as
    // the first one does: no process and no socket.
    env.wait_gone(&id);
}

#[test]
fn the_renewed_warden_tells_the_relay_which_build_it_is() {
    let mut env = Env::new();
    let id = env.start("builds", &SH);
    let mut a = env.connect(&id);
    let before = a.status();
    assert_eq!(before.warden_version, env!("CARGO_PKG_VERSION"));
    let warden = env.find("warden", &id).expect("a warden");

    // A "newer" build: the same code that says it is another release.
    let real = env!("CARGO_BIN_EXE_kablo");
    let newer = env.script(
        "newer",
        &format!("#!/bin/sh\nKABLO_TEST_VERSION=9.9.9 exec '{real}' \"$@\"\n"),
    );
    let said = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&newer)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&said.stdout).into_owned();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
    assert_eq!(stdout, format!("{id}: replaced (now 9.9.9)\n"));
    let mut b = env.connect(&id);
    let after = b.status();
    assert_eq!(
        (after.version.as_str(), after.warden_version.as_str()),
        ("9.9.9", "9.9.9")
    );
    assert_eq!(after.pid, before.pid, "the shell was never touched");
    assert_eq!(env.find("warden", &id), Some(warden), "nor its parent");
}

/// An upgrade to `script` (a relay that says it is release 9.9.9 and then does
/// what `script` makes of the file it started from) after which the warden
/// stays as it was: the same process, running the program it ran, with the
/// session whole and served, and `kablo upgrade` saying that it stayed.
fn assert_the_warden_stays(label: &str, spoil: &str) {
    let mut env = Env::new();
    let id = env.start(label, &SH);
    let warden = env.find("warden", &id).expect("a warden");
    let mut a = env.connect(&id);
    let shell = a.status().pid;
    a.attach(0);

    let real = env!("CARGO_BIN_EXE_kablo");
    let once = env.script(
        "once",
        &format!("#!/bin/sh\n{spoil}\nKABLO_TEST_VERSION=9.9.9 exec '{real}' \"$@\"\n"),
    );
    let said = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&once)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&said.stdout).into_owned();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
    assert_eq!(
        stdout,
        format!(
            "{id}: relay replaced (now 9.9.9); its warden stays at {} until the session ends\n",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert_eq!(env.find("warden", &id), Some(warden));
    assert_eq!(
        binary_of(warden).and_then(|path| std::fs::canonicalize(path).ok()),
        std::fs::canonicalize(real).ok(),
        "it still runs the program it ran"
    );
    assert!(
        !env.command_of("warden", &id)
            .is_some_and(|command| command.contains("--resume")),
        "{:?}",
        env.command_of("warden", &id)
    );

    // Nothing it held was lost to the attempt: the shell is its child, the
    // session is served, and the end of it is still delivered through it.
    let mut b = env.connect(&id);
    assert_eq!(b.status().pid, shell);
    assert_eq!(env.parent_of(shell), Some(warden));
    b.attach(0);
    b.type_in("echo still-$((6*7))\n");
    b.wait_for("still-42\r\n", 0);
    b.send(Message::Terminate { force: false });
    b.until("the end", |c| c.ended.is_some());
    env.wait_gone(&id);
}

#[test]
fn a_warden_stays_when_the_new_binary_is_no_longer_fit_to_run() {
    // The relay starts from the file, and the file stops being one to run
    // once it has: the warden looks again before it replaces itself.
    assert_the_warden_stays("unfit", "chmod 600 \"$0\"");
}

#[test]
fn a_warden_stays_when_the_exec_itself_fails() {
    // The relay starts from the file, and then the file is another one that
    // looks fine and names a program that is not there: the exec fails.
    assert_the_warden_stays(
        "execfails",
        "printf '#!/nonexistent/interpreter\\n' > \"$0.new\" && chmod 755 \"$0.new\" && mv \"$0.new\" \"$0\"",
    );
}

#[test]
fn a_new_image_that_ends_before_it_has_the_record_leaves_the_session_to_its_relay() {
    let mut env = Env::new();
    let id = env.start("orphan", &SH);
    let mut a = env.connect(&id);
    let shell = a.status().pid;
    // The shell is nobody's child once the warden is gone, so the end of the
    // test (which finds processes by their parents) would not find it.
    let _shell = Orphan(shell);
    a.attach(0);

    // The relay it starts is a real one; the warden it renews into ends at
    // once, as a build that cannot take the record up would.
    let real = env!("CARGO_BIN_EXE_kablo");
    let hollow = env.script(
        "hollow",
        &format!("#!/bin/sh\ncase \"$1\" in warden) exit 0;; esac\nexec '{real}' \"$@\"\n"),
    );
    let said = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&hollow)
        .output()
        .unwrap();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
    // Of the same release, a warden that is gone cannot be told from one that
    // renewed: what the command says is all it can see of the status.
    assert!(
        String::from_utf8_lossy(&said.stdout).starts_with(&format!("{id}: ")),
        "{}",
        String::from_utf8_lossy(&said.stdout)
    );
    let start = Instant::now();
    while env.find("warden", &id).is_some() {
        assert!(start.elapsed() < PATIENCE, "the warden is still there");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The shell lives and the relay serves, with the socket where it was.
    assert!(env.socket(&id).exists());
    let mut b = env.connect(&id);
    assert_eq!(b.status().pid, shell, "the shell was never touched");
    b.attach(0);
    b.type_in("echo carried-on-$((7*6))\n");
    b.wait_for("carried-on-42\r\n", 0);
    // Nobody is left to hang up on it, so it is ended here. With no warden to
    // say how it ended, the relay only closes the connection.
    b.type_in("exit\n");
    b.until_closed();
}
