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

/// The programs the machine has (`apps/<name>/<version>/<name>`), in [`DIR`]: kablo is the only
/// one that writes it, on a computer and on a server alike.
pub const APPS: &str = "apps";

/// The links to the newest version of each program, in [`APPS`]: `bin/<name>` is
/// `../<name>/<version>/<name>`.
pub const APPS_BIN: &str = "bin";

/// What a version of a program is signed in, in its directory.
pub const APP_MANIFEST: &str = "app.json";

/// The signature of [`APP_MANIFEST`], in the same directory.
pub const APP_SIGNATURE: &str = "app.json.sig";

/// Where the program came from (an address or `user/repo`), in the program's directory.
pub const APP_SOURCE: &str = "source";

/// The key the machine's person approved for the program, in the program's directory.
pub const APP_KEY: &str = "key";

/// Present when a newer release came signed with another key than [`APP_KEY`]: the key it came
/// with. The program stays at the version it was, in the program's directory.
pub const APP_STOPPED: &str = "stopped";

/// Present when programs are not updated on their own, in [`APPS`].
pub const AUTO_OFF: &str = "auto-off";

/// The programs the person said not to bring here, one name a line, in [`APPS`].
pub const NOT_HERE: &str = "not-here";

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

    // The folder of the programs is written by kablo and read by every kablo of every version
    // that has been on the machine, and bateri's older build wrote the first two levels.
    #[test]
    fn the_folder_of_the_programs_is_the_one_bateri_started() {
        let bin = format!("{DIR}/{APPS}/{APPS_BIN}");
        let manifest = format!("{DIR}/{APPS}/textpen/1.0.0/{APP_MANIFEST}");
        assert_eq!(bin, ".local/share/bateri/apps/bin");
        assert_eq!(manifest, ".local/share/bateri/apps/textpen/1.0.0/app.json");
        assert_eq!(APP_SIGNATURE, "app.json.sig");
        assert_eq!(
            [APP_SOURCE, APP_KEY, APP_STOPPED, AUTO_OFF, NOT_HERE],
            ["source", "key", "stopped", "auto-off", "not-here"]
        );
    }
}
