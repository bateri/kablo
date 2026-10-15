//! An administrator's switch, end to end: `/etc/kablo/no-forwarding` turns
//! streams off for a bridge that is already running.
//!
//! The file is in a place only root can write, so this runs as root (the
//! container of `make linux`) and says it was skipped when it is not. It has a
//! test binary to itself, which cargo runs apart from the others, so the file
//! does not turn streams off under a test that is using them; the one test
//! here removes the file it made whatever happens, and does not run (it says
//! so) where an administrator's file is already there.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::path::{Path, PathBuf};

use common::{Bridge, Env, Forward, client, read_exactly};
use kablo::bridge::{FORWARDING_OFF, NO_FORWARDING};
use kablo_proto::v1::Message;
use kablo_proto::v1::stream::{Command, FailCode, MAX_WINDOW};

/// The file, there while this is alive and gone after, with the directory it
/// is in if this made it.
struct Switch {
    file: PathBuf,
    dir: Option<PathBuf>,
}

/// What the tests write in the file, so that it is known to be theirs.
const MARK: &str = "written by the tests of kablo; safe to remove\n";

impl Switch {
    /// `None` when the file is there and is not the tests' own: it is an
    /// administrator's, and this runs as root, which is no reason to take it
    /// away. (One left by an earlier run that was killed is theirs, and goes:
    /// it would turn streams off before a test asks for it.)
    fn new() -> Option<Switch> {
        let file = PathBuf::from(NO_FORWARDING);
        if file.symlink_metadata().is_ok() {
            match std::fs::read_to_string(&file) {
                Ok(text) if text == MARK => std::fs::remove_file(&file).unwrap(),
                _ => return None,
            }
        }
        let dir = file.parent().map(Path::to_path_buf);
        let made = dir.filter(|dir| !dir.exists());
        if let Some(dir) = &made {
            std::fs::create_dir_all(dir).unwrap();
        }
        Some(Switch { file, dir: made })
    }

    fn on(&self) {
        std::fs::write(&self.file, MARK).unwrap();
    }

    fn off(&self) {
        std::fs::remove_file(&self.file).unwrap();
    }
}

impl Drop for Switch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

#[test]
fn the_administrator_turns_streams_off_and_on_for_a_bridge_that_is_running() {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("SKIPPED: {NO_FORWARDING} can only be made by root");
        return;
    }
    let Some(switch) = Switch::new() else {
        eprintln!("SKIPPED: {NO_FORWARDING} is there and is not this test's to take away");
        return;
    };
    let env = Env::new();
    let mut bridge = Bridge::start(&env);

    // A target that takes a connection and says its end back.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            let Ok(mut connection) = connection else {
                continue;
            };
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = connection.read(&mut buf) {
                    if n == 0 || connection.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = connection.shutdown(Shutdown::Write);
            });
        }
    });

    // On to begin with: asked, it says so, and a stream opens.
    bridge.send(None, Message::Features);
    bridge.until("features", |b| b.features.len() == 1);
    assert!(bridge.features[0].streams);
    let (open, _) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();

    // The file appears while the bridge runs: the next ask says no and why,
    // and a stream that is opened is failed. The one that is open goes on.
    switch.on();
    bridge.send(None, Message::Features);
    bridge.until("features", |b| b.features.len() == 2);
    assert!(!bridge.features[1].streams);
    assert_eq!(bridge.features[1].reason.as_deref(), Some(FORWARDING_OFF));
    let (_, code, text) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap_err();
    assert_eq!((code, text.as_str()), (FailCode::FORBIDDEN, FORWARDING_OFF));
    // A program is not a connection: the switch is not about it, and the
    // bridge says so.
    assert!(bridge.features[1].commands);
    let runs = Command {
        argv: vec![b"/bin/echo".to_vec(), b"runs".to_vec()],
        ..Command::default()
    };
    let (ran, _) = bridge.run_command(runs, MAX_WINDOW).unwrap();
    bridge.until("the command's end", |b| b.flows[&ran].closed);
    assert_eq!(bridge.flows[&ran].data, b"runs\n");
    bridge.stream_send(open, b"still open", 100);
    bridge.until("the echo", |b| b.flows[&open].data == b"still open");

    // The file goes: the next stream opens.
    switch.off();
    bridge.send(None, Message::Features);
    bridge.until("features", |b| b.features.len() == 3);
    assert!(bridge.features[2].streams);
    let (again, _) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();
    bridge.stream_send(again, b"again", 100);
    bridge.until("the echo", |b| b.flows[&again].data == b"again");

    // `kablo forward` on a server where the file is there says why it does
    // nothing, and starts nothing; when the file is gone it carries.
    let kablo = env!("CARGO_BIN_EXE_kablo");
    let spec = format!("0:127.0.0.1:{port}");
    let args = ["--bind", "127.0.0.1", &spec, "--", kablo, "attach"];
    switch.on();
    let mut refused = Forward::start(&env, &args);
    assert_eq!(refused.wait_exit().code(), Some(1));
    refused.said(FORWARDING_OFF);
    refused.announced_nothing();
    switch.off();
    let mut carried = Forward::start(&env, &args);
    let address = carried.announced(1)[0];
    let mut socket = client(address);
    socket.write_all(b"through the forwarding").unwrap();
    assert_eq!(read_exactly(&mut socket, 22), b"through the forwarding");
}
