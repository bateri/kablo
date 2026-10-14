//! Which kablo binary a server needs and where on the server it goes.
//!
//! A program that puts kablo on a machine (a terminal app, the first time it
//! connects to a server) has to answer three questions, and every such program
//! has to answer them the same way: two apps that disagree leave two copies of
//! kablo on one server and neither finds the other's sessions' relays. The
//! answers are here, as functions, so nobody writes them again:
//!
//! 1. *What is the server?* Run [`UNAME_COMMAND`] there and give its output to
//!    [`Target::from_uname_output`]. `None` is a platform kablo is not built
//!    for.
//! 2. *Which file is it?* The release has one binary per target, named by
//!    [`artifact_name`], and a [`CHECKSUMS`] file that lists the SHA-256 of
//!    each. A client checks the sum of what it fetched before it puts the
//!    file on a server.
//! 3. *Where does it go?* Into a directory of its own for its version,
//!    [`install_path`], below the user's home. A new version is a new
//!    directory, so a binary that sessions run from is never written over, and
//!    `kablo upgrade` is handed the new version's path ([`install_path_in`]
//!    when the home directory is known) while the old one stays for the
//!    sessions that still use it. A client writes the file beside its place
//!    under another name, gives it mode 0755 and renames it into place, so
//!    that a file at the path is always a whole one, and leaves a version that
//!    is there already alone.
//!
//! The targets are the ones `make dist` builds; a test compares the two lists,
//! so a target added to one and not the other is a failing build.

use std::fmt;

use crate::{Error, Result};

/// What to run on a server to learn which [`Target`] it is.
pub const UNAME_COMMAND: &str = "uname -s -m";

/// The file name of the checksums of a release: one `<sha256>  <name>` line per
/// binary, as `sha256sum` writes and checks it.
pub const CHECKSUMS: &str = "SHA256SUMS";

/// The directory, relative to the user's home, that holds one directory per
/// installed version.
pub const ROOT: &str = ".local/share/kablo";

/// The name of the binary inside a version's directory.
pub const BINARY: &str = "kablo";

/// The longest version [`install_dir`] and [`artifact_name`] accept, in bytes.
pub const MAX_VERSION_LEN: usize = 64;

/// A platform kablo is built for.
///
/// The Linux builds are static (musl): they open on a server with any C
/// library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Target {
    /// Linux on x86-64.
    LinuxX86_64,
    /// Linux on 64-bit ARM.
    LinuxAarch64,
    /// macOS on Apple silicon.
    MacosAarch64,
    /// macOS on Intel.
    MacosX86_64,
}

impl Target {
    /// Every target, in the order `make dist` builds them.
    pub const ALL: [Target; 4] = [
        Target::LinuxX86_64,
        Target::LinuxAarch64,
        Target::MacosAarch64,
        Target::MacosX86_64,
    ];

    /// The Rust target triple the binary is built for; it ends the file name
    /// of the binary ([`artifact_name`]).
    pub const fn triple(self) -> &'static str {
        match self {
            Target::LinuxX86_64 => "x86_64-unknown-linux-musl",
            Target::LinuxAarch64 => "aarch64-unknown-linux-musl",
            Target::MacosAarch64 => "aarch64-apple-darwin",
            Target::MacosX86_64 => "x86_64-apple-darwin",
        }
    }

    /// The target of a machine whose `uname -s` and `uname -m` print `system`
    /// and `machine`, or `None` for a platform kablo is not built for.
    ///
    /// Linux says `x86_64` or `aarch64` (some images and emulators say `amd64`
    /// and `arm64`); macOS says `arm64` or `x86_64`. A 32-bit userland
    /// (`armv7l`, `i686`) is not a target, even when the kernel is 64-bit. The
    /// words are the ones `uname` prints, in its letter case.
    pub fn from_uname(system: &str, machine: &str) -> Option<Target> {
        match (system, machine) {
            ("Linux", "x86_64" | "amd64") => Some(Target::LinuxX86_64),
            ("Linux", "aarch64" | "arm64") => Some(Target::LinuxAarch64),
            ("Darwin", "arm64" | "aarch64") => Some(Target::MacosAarch64),
            ("Darwin", "x86_64") => Some(Target::MacosX86_64),
            _ => None,
        }
    }

    /// The target for the output of [`UNAME_COMMAND`].
    ///
    /// Only the last non-empty line counts and it must be exactly two words:
    /// a shell startup file on the server may print before the command runs,
    /// and what it prints is not an answer.
    pub fn from_uname_output(output: &str) -> Option<Target> {
        let line = output.lines().rev().find(|line| !line.trim().is_empty())?;
        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some(system), Some(machine), None) => Target::from_uname(system, machine),
            _ => None,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.triple())
    }
}

/// The directory of a version, relative to the user's home:
/// `.local/share/kablo/<version>`.
///
/// A version is 1 to [`MAX_VERSION_LEN`] bytes of `[A-Za-z0-9._+-]` that starts
/// with a letter or a digit (`0.3.1`, `0.4.0-rc.1`): a name that can only be a
/// directory directly under [`ROOT`].
pub fn install_dir(version: &str) -> Result<String> {
    check_version(version)?;
    Ok(format!("{ROOT}/{version}"))
}

/// The installed binary of a version, relative to the user's home. A command
/// run over ssh starts in the home directory, so this is also the command:
/// `ssh host .local/share/kablo/0.3.1/kablo attach`.
pub fn install_path(version: &str) -> Result<String> {
    Ok(format!("{}/{BINARY}", install_dir(version)?))
}

/// The installed binary of a version under a known home directory, which must
/// be an absolute path without a NUL. `kablo upgrade` takes this form: the
/// path is read by the server's own processes, which do not start in the home
/// directory.
pub fn install_path_in(home: &str, version: &str) -> Result<String> {
    if !home.starts_with('/') || home.contains('\0') {
        return Err(Error::Malformed("home directory"));
    }
    Ok(format!(
        "{}/{}",
        home.trim_end_matches('/'),
        install_path(version)?
    ))
}

/// The name of a version's binary for a target in a release (and in `dist/`):
/// `kablo-<version>-<triple>`, with the checksum of it listed in
/// [`CHECKSUMS`].
pub fn artifact_name(version: &str, target: Target) -> Result<String> {
    check_version(version)?;
    Ok(format!("kablo-{version}-{}", target.triple()))
}

fn check_version(version: &str) -> Result<()> {
    let first = version.bytes().next();
    let valid = first.is_some_and(|b| b.is_ascii_alphanumeric())
        && version.len() <= MAX_VERSION_LEN
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(Error::Malformed("version"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_is_known_by_what_uname_prints() {
        let known = [
            ("Linux", "x86_64", Target::LinuxX86_64),
            ("Linux", "amd64", Target::LinuxX86_64),
            ("Linux", "aarch64", Target::LinuxAarch64),
            ("Linux", "arm64", Target::LinuxAarch64),
            ("Darwin", "arm64", Target::MacosAarch64),
            ("Darwin", "aarch64", Target::MacosAarch64),
            ("Darwin", "x86_64", Target::MacosX86_64),
        ];
        for (system, machine, target) in known {
            assert_eq!(Target::from_uname(system, machine), Some(target));
            assert_eq!(
                Target::from_uname_output(&format!("{system} {machine}\n")),
                Some(target)
            );
        }
    }

    #[test]
    fn a_platform_kablo_is_not_built_for_is_none() {
        for (system, machine) in [
            ("Linux", "armv7l"),
            ("Linux", "armv8l"),
            ("Linux", "i686"),
            ("Linux", "riscv64"),
            ("Linux", "ppc64le"),
            ("Linux", ""),
            ("Darwin", "i386"),
            ("FreeBSD", "amd64"),
            ("CYGWIN_NT-10.0", "x86_64"),
            ("linux", "x86_64"),
            ("Linux", "X86_64"),
            ("", ""),
        ] {
            assert_eq!(
                Target::from_uname(system, machine),
                None,
                "{system} {machine}"
            );
        }
    }

    #[test]
    fn the_output_of_the_command_is_two_words_on_the_last_line() {
        assert_eq!(
            Target::from_uname_output("Linux x86_64"),
            Some(Target::LinuxX86_64)
        );
        assert_eq!(
            Target::from_uname_output("  Linux \t aarch64 \r\n\n"),
            Some(Target::LinuxAarch64)
        );
        // A startup file that talks before the command runs.
        assert_eq!(
            Target::from_uname_output("Welcome to the server\nLast login: Tue\n\nDarwin arm64\n"),
            Some(Target::MacosAarch64)
        );
        for bad in [
            "",
            "\n\n",
            "Linux",
            "Linux x86_64 GNU/Linux",
            "Linux x86_64\nthe shell says goodbye\n",
            "uname: command not found",
            "Linux\nx86_64",
        ] {
            assert_eq!(Target::from_uname_output(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_targets_are_four_distinct_triples() {
        let triples: Vec<&str> = Target::ALL.iter().map(|t| t.triple()).collect();
        assert_eq!(
            triples,
            [
                "x86_64-unknown-linux-musl",
                "aarch64-unknown-linux-musl",
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
            ]
        );
        for target in Target::ALL {
            assert_eq!(target.to_string(), target.triple());
        }
    }

    #[test]
    fn a_version_has_a_directory_of_its_own_under_the_root() {
        assert_eq!(install_dir("0.3.1").unwrap(), ".local/share/kablo/0.3.1");
        assert_eq!(
            install_path("0.3.1").unwrap(),
            ".local/share/kablo/0.3.1/kablo"
        );
        assert_eq!(
            install_path("0.4.0-rc.1+build_5").unwrap(),
            ".local/share/kablo/0.4.0-rc.1+build_5/kablo"
        );
        assert_eq!(
            install_path_in("/home/ada", "0.3.1").unwrap(),
            "/home/ada/.local/share/kablo/0.3.1/kablo"
        );
        assert_eq!(
            install_path_in("/home/ada/", "0.3.1").unwrap(),
            "/home/ada/.local/share/kablo/0.3.1/kablo"
        );
        assert_eq!(
            install_path_in("/", "0.3.1").unwrap(),
            "/.local/share/kablo/0.3.1/kablo"
        );
    }

    #[test]
    fn a_version_cannot_name_another_place() {
        let long = "1".repeat(MAX_VERSION_LEN + 1);
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "-1",
            "+1",
            "1/2",
            "../1",
            "1/../2",
            "1 2",
            "1\0",
            "1\n",
            "ü",
            long.as_str(),
        ] {
            assert!(install_dir(bad).is_err(), "{bad:?}");
            assert!(install_path(bad).is_err(), "{bad:?}");
            assert!(install_path_in("/home/ada", bad).is_err(), "{bad:?}");
            assert!(artifact_name(bad, Target::LinuxX86_64).is_err(), "{bad:?}");
        }
        assert!(install_dir(&"1".repeat(MAX_VERSION_LEN)).is_ok());
    }

    #[test]
    fn a_home_that_is_not_an_absolute_path_is_refused() {
        for bad in ["", "home/ada", "~", "~/ada", "./ada", "/home/\0ada"] {
            assert!(install_path_in(bad, "0.3.1").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_release_names_its_binary_by_version_and_triple() {
        assert_eq!(
            artifact_name("0.3.1", Target::LinuxAarch64).unwrap(),
            "kablo-0.3.1-aarch64-unknown-linux-musl"
        );
        assert_eq!(
            artifact_name("0.3.1", Target::MacosX86_64).unwrap(),
            "kablo-0.3.1-x86_64-apple-darwin"
        );
    }
}
