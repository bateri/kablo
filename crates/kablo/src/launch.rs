//! Starting a session: what `kablo new` does.
//!
//! The command makes the socket directory if it is not there, starts a warden
//! (this same binary, `kablo warden`) with a socket as its standard input,
//! sends it the request over that socket and waits for one line back: `ok`
//! when the first relay is set up and the session answers on its socket, or
//! `error: …` with the reason. The warden is not waited for and not kept as a
//! child: it detaches itself into a session of its own and outlives this
//! process.

use std::io::{self, BufRead, BufReader, Read};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};

use kablo_proto::SessionId;
use kablo_proto::v1::{Message, NewSession, Packet};

use crate::sockdir;

/// The longest reply line read from the warden.
const REPLY_LIMIT: u64 = 4096;

/// A name for a session whose maker did not choose one: eight hex digits
/// from the system's randomness.
pub fn fresh_id() -> io::Result<SessionId> {
    let mut bytes = [0u8; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    SessionId::new(&name).map_err(io::Error::other)
}

/// Starts session `id` in socket directory `dir`. `Ok` means the session is
/// up and its socket answers.
pub fn launch(dir: &Path, id: &SessionId, request: &NewSession) -> Result<(), String> {
    sockdir::ensure_dir(dir).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find myself: {e}"))?;
    let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .arg("warden")
        .arg("--id")
        .arg(id.as_str())
        .arg("--dir")
        .arg(dir)
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the session keeper: {e}"))?;
    let packet = Packet::about(id.clone(), Message::New(request.clone()));
    let sent = kablo_proto::v1::write_packet(&mut &ours, &packet);
    if let Err(e) = sent {
        // The warden is waiting for a request that will not come.
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("the request cannot be sent: {e}"));
    }
    let _ = ours.shutdown(Shutdown::Write);
    let mut line = String::new();
    let _ = BufReader::new((&ours).take(REPLY_LIMIT)).read_line(&mut line);
    let line = line.trim_end();
    if line == "ok" {
        // The warden is on its own now.
        drop(child);
        return Ok(());
    }
    let status = child.wait();
    match line.strip_prefix("error: ") {
        Some(why) => Err(why.to_owned()),
        None => Err(match status {
            Ok(status) => format!("the session keeper ended before it was ready ({status})"),
            Err(e) => format!("the session keeper ended before it was ready ({e})"),
        }),
    }
}
