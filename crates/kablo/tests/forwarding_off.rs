//! An administrator's switch, end to end: `/etc/kablo/no-forwarding` turns
//! streams off for a bridge that is already running.
//!
//! The file is in a place only root can write, so this runs as root (the
//! container of `make linux`) and says it was skipped when it is not. It has a
//! test binary to itself, which cargo runs apart from the others, so the file
//! does not turn streams off under a test that is using them; the one test
//! here removes the file whatever happens.

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

use common::{Bridge, Env};
use kablo::bridge::{FORWARDING_OFF, NO_FORWARDING};
use kablo_proto::v1::Message;
use kablo_proto::v1::stream::{FailCode, MAX_WINDOW};

/// The file, there while this is alive and gone after, with the directory it
/// is in if this made it.
struct Switch {
    file: PathBuf,
    dir: Option<PathBuf>,
}

impl Switch {
    fn new() -> Switch {
        let file = PathBuf::from(NO_FORWARDING);
        let dir = file.parent().map(Path::to_path_buf);
        let made = dir.filter(|dir| !dir.exists());
        if let Some(dir) = &made {
            std::fs::create_dir_all(dir).unwrap();
        }
        // A file left by an earlier run that was killed would turn streams off
        // before the test asks for it.
        let _ = std::fs::remove_file(&file);
        Switch { file, dir: made }
    }

    fn on(&self) {
        std::fs::write(&self.file, "").unwrap();
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
    let switch = Switch::new();
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
}
