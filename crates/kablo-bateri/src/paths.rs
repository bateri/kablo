//! Where bateri keeps its files on a machine, under the home directory: one place, the same on a
//! computer and on a server, for the one that writes them (bateri, when it sets a server up) and for the
//! ones that read them (`kablo bateri`).
//!
//! No environment variable moves them. The bateri app and a cron job do not have a login shell's
//! environment, and a counter that two environments place in two directories would give the same
//! number twice.

/// bateri's directory, relative to the home: `~/.local/share/bateri`.
pub const DIR: &str = ".local/share/bateri";

/// The counter that gives the terminals their numbers, in [`DIR`].
pub const COUNTER: &str = "next";

/// Who opened which terminal, one file per terminal, in [`DIR`].
pub const TERMINALS: &str = "terminals";

/// The messages that wait for the bateri app to place a pane, one file per terminal, in [`DIR`].
pub const PLACING: &str = "placing";

/// What the bootstrap of a terminal writes, in [`DIR`].
pub const SHELL: &str = "shell";

/// The directory of the `bateri` command, in [`SHELL`]: the terminals the command starts get it on
/// their `PATH`.
pub const BIN: &str = "bin";

/// The `bateri` command, in [`BIN`].
pub const COMMAND: &str = "bateri";

/// The bootstrap of a server's terminal, in [`SHELL`].
pub const BOOT: &str = "boot.sh";

#[cfg(test)]
mod tests {
    use super::*;

    // The writer of these files is bateri, a program of its own: the layout is a contract.
    #[test]
    fn the_layout_is_the_one_the_writer_uses() {
        let boot = format!("{DIR}/{SHELL}/{BOOT}");
        let command = format!("{DIR}/{SHELL}/{BIN}/{COMMAND}");
        assert_eq!(boot, ".local/share/bateri/shell/boot.sh");
        assert_eq!(command, ".local/share/bateri/shell/bin/bateri");
        assert_eq!(
            [COUNTER, TERMINALS, PLACING],
            ["next", "terminals", "placing"]
        );
    }
}
