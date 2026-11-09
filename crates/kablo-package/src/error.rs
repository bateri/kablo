use std::fmt;
use std::io;
use std::path::PathBuf;

use pencere_host::apps::{self, Version};

/// Why something was not done to the programs a machine has. Everything but [`Error::Io`] is a
/// refusal: the folder is as it was.
#[derive(Debug)]
pub enum Error {
    /// Not a program name.
    Name(String),
    /// A name only kablo's own files can have.
    Reserved(String),
    /// A program that is not here.
    NotInstalled(String),
    /// A word that cannot be a part of a file name.
    Part(String),
    /// A note of the folder that is not one line of text.
    Record(&'static str),
    /// The `app.json` is not one, or its signature does not hold.
    Manifest(apps::Error),
    /// The `app.json` is a release of another program than the one asked for.
    WrongName { asked: String, signed: String },
    /// The `app.json` lists no file for this kind of machine.
    NoFile(String),
    /// The program's file is not where it was placed, or is not a plain file.
    Missing(PathBuf),
    /// The program's file is larger than a program may be.
    TooBig(u64),
    /// The program's file is not the one the `app.json` signs.
    Damaged,
    /// The release is not newer than the one the machine has.
    Older { current: Version, offered: Version },
    /// The key signing the release is nobody's that the machine knows, and no one approved it.
    Unapproved { key: String },
    /// The key signing the release is not the one approved for the program.
    KeyChanged { recorded: String, offered: String },
    /// `bin/<name>` is something kablo did not put there, and is left as it is.
    Foreign(PathBuf),
    /// The machine's folder could not be read or written.
    Io { path: PathBuf, source: io::Error },
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Error {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Name(name) => write!(
                f,
                "'{}' is not a program name (lowercase letters, digits and - from 1 to 64 bytes)",
                name.escape_debug()
            ),
            Error::Reserved(name) => write!(
                f,
                "'{}' is not a name a program may have",
                name.escape_debug()
            ),
            Error::NotInstalled(name) => write!(f, "{} is not here", name.escape_debug()),
            Error::Part(part) => write!(f, "'{}' is not the name of a part", part.escape_debug()),
            Error::Record(what) => write!(f, "{what} is not one line of text"),
            Error::Manifest(why) => write!(f, "app.json: {why}"),
            Error::WrongName { asked, signed } => write!(
                f,
                "app.json is the release of {}, not of {}",
                signed.escape_debug(),
                asked.escape_debug()
            ),
            Error::NoFile(target) => {
                write!(f, "app.json has no file for {}", target.escape_debug())
            }
            Error::Missing(path) => write!(f, "{} is not a file", path.display()),
            Error::TooBig(size) => {
                write!(f, "the program is {size} bytes, more than a program may be")
            }
            Error::Damaged => f.write_str("the file does not match the sum its app.json signs"),
            Error::Older { current, offered } => {
                write!(f, "{offered} is not newer than {current}, which is here")
            }
            Error::Unapproved { key } => write!(
                f,
                "the key {} is not one that was approved",
                key.escape_debug()
            ),
            Error::KeyChanged { recorded, offered } => write!(
                f,
                "the release is signed with {} and not with {}, which was approved",
                offered.escape_debug(),
                recorded.escape_debug()
            ),
            Error::Foreign(path) => {
                write!(f, "{} is not kablo's and is left as it is", path.display())
            }
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
