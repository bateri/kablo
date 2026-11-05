//! kablo: a transparent terminal session keeper.
//!
//! One binary with sub-commands. A session is two processes: a *warden* that
//! is the shell's parent and stays the same process while the shell lives
//! (it replaces its own program in place when the session is upgraded), and a
//! *relay* that serves clients and can be replaced without touching the shell.
//! The bridge (`kablo attach`) carries every session on the machine to a
//! client over one connection. The wire a client speaks is the `kablo-proto`
//! crate; the client is bateri, which speaks it itself, and the commands that
//! drive a session from the machine it runs on (`send`, `capture`, ...), which
//! are in the pieces here.

pub mod app;
pub mod bridge;
pub mod cli;
mod command;
mod conversation;
mod dial;
mod drive;
mod handshake;
pub mod inner;
mod journal;
mod launch;
pub mod link;
mod listing;
mod mailbox;
mod mirror;
pub mod origin;
mod outbound;
mod peer;
mod process;
mod procinfo;
pub mod pty;
mod relay;
mod scan;
pub mod sockdir;
pub mod streams;
pub mod sys;
mod typing;
mod upgrade;
mod warden;

/// The variable that stands in for this build's release, so that a test can
/// run a "newer" build of the same code.
pub const ENV_TEST_VERSION: &str = "KABLO_TEST_VERSION";

/// The variable that stands in for the ten minutes an ended session is kept for
/// a client that asked (in milliseconds), so that a test does not wait that long.
pub const ENV_TEST_END_KEEP: &str = "KABLO_TEST_END_KEEP_MS";

/// The release this build says it is, to the clients that ask (`status`) and
/// to the relay a warden serves.
pub fn release() -> String {
    std::env::var(ENV_TEST_VERSION)
        .ok()
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| build_version().to_owned())
}

/// The name this build was made under: the crate's version, or the one the build
/// was told to carry (`KABLO_BUILD_VERSION`, set when it is compiled). A release is
/// named by its version alone; a build for development is told a name with the commit
/// it was made from (`0.2.0-dev+3f2a9c1d0b44`, see `make dist`), so that two builds of
/// one version are not the same name, and neither is a build and the release.
pub fn build_version() -> &'static str {
    named(
        option_env!("KABLO_BUILD_VERSION"),
        env!("CARGO_PKG_VERSION"),
    )
}

/// `told`, unless it is nothing, else `version`.
fn named<'a>(told: Option<&'a str>, version: &'a str) -> &'a str {
    told.filter(|name| !name.is_empty()).unwrap_or(version)
}

#[cfg(test)]
mod tests {
    use super::named;

    #[test]
    fn a_build_is_named_by_what_it_was_told_and_else_by_its_version() {
        assert_eq!(named(None, "0.2.0"), "0.2.0");
        assert_eq!(named(Some(""), "0.2.0"), "0.2.0");
        assert_eq!(
            named(Some("0.2.0-dev+3f2a9c1d0b44"), "0.2.0"),
            "0.2.0-dev+3f2a9c1d0b44"
        );
    }
}
