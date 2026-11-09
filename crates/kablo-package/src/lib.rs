//! The programs a machine has: `~/.local/share/bateri/apps`, and the one place that writes it.
//!
//! **The folder is the record.** What is installed, at which version, where it came from, which key
//! was approved for it and whether programs are updated on their own are all in it, and nothing else
//! keeps them: `list` reads the folder, `put` changes it. The layout (names in
//! [`kablo_bateri::paths`]):
//!
//! ```text
//! apps/bin/<name>                  a link to ../<name>/<version>/<name>
//! apps/<name>/<version>/           <name>, app.json, app.json.sig
//! apps/<name>/{source,key,stopped} where it came from, the approved key, the key that stopped it
//! apps/<name>/.<version>.part-<p>  a release being placed: what was sent, not yet checked
//! apps/auto-off                    present when programs are not updated on their own
//! apps/not-here                    names the person said not to bring here
//! ```
//!
//! **What is checked before a release goes in** ([`Store::put`]): the signature of `app.json`, that
//! it is the release of the program asked for, the key (the one approved for the program, one of
//! those kablo ships with, or one the caller says a person approved just now), that it is newer than
//! what is here, and that the file is the one `app.json` signs by its sum. A file is put in with
//! one `rename`, the link is changed with another, and a version that was linked never goes back;
//! a release that is refused leaves the folder as it was.
//!
//! No bateri, no network and no terminal in here: the bytes of `app.json` and its signature come as
//! arguments and the file as a directory that was placed, so that whoever brings them (bateri over
//! ssh, a person, a test) is not this crate's business. Nothing of a program is ever run.

mod error;
mod list;
mod prune;
mod put;
mod records;

use std::path::{Path, PathBuf};

use kablo_bateri::paths;
use pencere_host::apps::is_name;

pub use error::Error;
pub use list::{Listing, Program};
pub use pencere_host::apps::Version;
pub use put::{Outcome, Put, Release, Standing, Trust, inspect};

/// Names that would be a file or a folder of the machine's own, or the command of a person's:
/// no program can have them.
const RESERVED: [&str; 6] = [
    "bateri",
    "kablo",
    "pencere",
    paths::APPS_BIN,
    paths::AUTO_OFF,
    paths::NOT_HERE,
];

/// The programs one machine has.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    embedded: Vec<String>,
}

impl Store {
    /// The programs of the user whose home is `home`, with the keys kablo ships with.
    pub fn in_home(home: &Path) -> Store {
        Store::new(
            home.join(paths::DIR).join(paths::APPS),
            kablo_bateri::keys::EMBEDDED,
        )
    }

    /// The programs in `root`; the keys in `embedded` are trusted without being asked.
    pub fn new(root: PathBuf, embedded: &[&str]) -> Store {
        Store {
            root,
            embedded: embedded.iter().map(|key| (*key).to_string()).collect(),
        }
    }

    /// The folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder of a program; the name is checked first, so that no path leaves the folder.
    fn app_dir(&self, name: &str) -> Result<PathBuf, Error> {
        check_name(name)?;
        Ok(self.root.join(name))
    }

    fn bin_dir(&self) -> PathBuf {
        self.root.join(paths::APPS_BIN)
    }
}

/// A name a program may have: the rule `app.json` states, and none of the reserved.
pub fn check_name(name: &str) -> Result<(), Error> {
    if !is_name(name) {
        return Err(Error::Name(name.to_string()));
    }
    if RESERVED.contains(&name) {
        return Err(Error::Reserved(name.to_string()));
    }
    Ok(())
}

/// The kind of machine this is, as `app.json` names it in `files`: `aarch64-apple-darwin`,
/// `x86_64-unknown-linux-musl`, and so on. None for a machine nothing is released for.
pub fn this_target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-musl")
    } else {
        None
    }
}
