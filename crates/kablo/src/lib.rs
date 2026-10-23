//! kablo: a transparent terminal session keeper.
//!
//! One binary with sub-commands. A session is two processes: a *warden* that
//! is the shell's parent and stays the same process while the shell lives
//! (it replaces its own program in place when the session is upgraded), and a
//! *relay* that serves clients and can be replaced without touching the shell.
//! The wire clients speak is the `kablo-proto` crate; the pieces here are the
//! ones that are not for clients.

pub mod app;
pub mod bridge;
pub mod carrier;
pub mod cli;
pub mod connect;
pub mod conversation;
pub mod dial;
pub mod drive;
pub mod exec;
pub mod forward;
pub mod handshake;
pub mod inner;
pub mod journal;
pub mod launch;
pub mod link;
pub mod listing;
pub mod mailbox;
pub mod origin;
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
pub mod tune;
pub mod typing;
pub mod upgrade;
pub mod warden;

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
