//! The shells bateri has an integration script for.

use std::path::Path;

/// A shell bateri has an integration script for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    Zsh,
    Fish,
    Bash,
}

impl ShellKind {
    /// The kind of shell at `path`: the last component of the path is exactly `zsh`, `fish` or
    /// `bash`, so `/bin/zsh` and `/opt/homebrew/bin/zsh` are the same shell. A name like `zsh-5.9`
    /// is not recognized: zsh is not installed under that name, and an integration for a shell we
    /// do not recognize would mean a session that opens with files it cannot load. `sh` is not bash
    /// even where it is a link to it: it starts in POSIX mode by its own name, which is not the
    /// start the script is written for. `None` is a shell without a script (the terminal then
    /// works as it always did, only the marks do not arrive).
    pub fn of(shell: &Path) -> Option<Self> {
        Self::of_name(shell.file_name()?.to_str()?)
    }

    /// The kind of shell a program's name is: `zsh`, `fish` or `bash`, and nothing else.
    pub fn of_name(name: &str) -> Option<Self> {
        match name {
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            "bash" => Some(Self::Bash),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_is_known_by_the_last_component_of_its_path_alone() {
        for (path, kind) in [
            ("/bin/zsh", Some(ShellKind::Zsh)),
            ("/opt/homebrew/bin/fish", Some(ShellKind::Fish)),
            ("/usr/bin/bash", Some(ShellKind::Bash)),
            ("zsh", Some(ShellKind::Zsh)),
            ("/bin/sh", None),
            ("/bin/zsh-5.9", None),
            ("/bin/zsh/", Some(ShellKind::Zsh)),
            ("/bin/dash", None),
            ("", None),
        ] {
            assert_eq!(ShellKind::of(Path::new(path)), kind, "{path:?}");
        }
    }
}
