//! kablo: a transparent terminal session keeper.
//!
//! One binary with sub-commands. A session is two processes: a *warden* that
//! is the shell's parent and never changes while the shell lives, and a
//! *relay* that serves clients and can be replaced without touching the shell.
//! The wire clients speak is the `kablo-proto` crate; the pieces here are the
//! ones that are not for clients.

pub mod bridge;
pub mod cli;
pub mod connect;
pub mod dial;
pub mod forward;
pub mod handshake;
pub mod inner;
pub mod journal;
pub mod launch;
pub mod link;
pub mod listing;
pub mod outbound;
pub mod peer;
pub mod procinfo;
pub mod pty;
pub mod relay;
pub mod scan;
pub mod sockdir;
pub mod streams;
pub mod sys;
pub mod upgrade;
pub mod warden;
