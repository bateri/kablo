//! The text bateri and the `bateri` command say to each other on kablo's app channel.
//!
//! **This is not kablo's wire.** `kablo-proto` is the frozen protocol that carries any app's
//! bytes; what rides inside one conversation with the app is the business of bateri and of the
//! command that talks to it, and it changes with bateri. This crate is that text, in one place
//! for both ends: [`message`] writes and reads it, strictly and without a panic, and the other
//! modules are the names it carries (a terminal, a tab's id, a side, a shell). It has no I/O and
//! depends on nothing but `kablo-proto`'s session name, so a build of bateri and a build of
//! kablo from the same revision agree on every byte; the frozen texts of the version read are in
//! the tests of [`message`].

#![forbid(unsafe_code)]

pub mod ids;
pub mod message;
pub mod paths;
pub mod place;
pub mod shell;
pub mod terminal;

pub use ids::{Id, Kind as IdKind};
pub use place::{Landed, Side};
pub use shell::ShellKind;
pub use terminal::{Anchor, LOCAL_MACHINE, LOCAL_SERVER, Terminal, is_machine};
