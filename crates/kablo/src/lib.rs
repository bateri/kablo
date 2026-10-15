//! kablo: a transparent terminal session keeper.
//!
//! One binary with sub-commands. A session is two processes: a *warden* that
//! is the shell's parent and stays the same process while the shell lives
//! (it replaces its own program in place when the session is upgraded), and a
//! *relay* that serves clients and can be replaced without touching the shell.
//! The wire clients speak is the `kablo-proto` crate; the pieces here are the
//! ones that are not for clients.

pub mod bridge;
pub mod carrier;
pub mod cli;
pub mod connect;
pub mod dial;
pub mod exec;
pub mod forward;
pub mod handshake;
pub mod inner;
pub mod journal;
pub mod launch;
pub mod link;
pub mod listing;
pub mod outbound;
pub mod peer;
pub mod process;
pub mod procinfo;
pub mod pty;
pub mod relay;
pub mod scan;
pub mod sockdir;
pub mod streams;
pub mod sys;
pub mod upgrade;
pub mod warden;

/// The variable that stands in for this build's release, so that a test can
/// run a "newer" build of the same code.
pub const ENV_TEST_VERSION: &str = "KABLO_TEST_VERSION";

/// The release this build says it is, to the clients that ask (`status`) and
/// to the relay a warden serves.
pub fn release() -> String {
    std::env::var(ENV_TEST_VERSION)
        .ok()
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}
