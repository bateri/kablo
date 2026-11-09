//! Reading the folder: what is here, and taking a program away.

use std::fs;
use std::io;

use pencere_host::apps::Version;

use crate::{Error, Store, check_name};

/// A program the machine has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub name: String,
    /// The version linked.
    pub version: Version,
    /// Where it came from, if that was written down.
    pub source: Option<String>,
    /// The key approved for it.
    pub key: Option<String>,
    /// The key a newer release came signed with, when that stopped it.
    pub stopped: Option<String>,
}

/// What the machine has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub programs: Vec<Program>,
    /// Whether programs are updated on their own.
    pub auto: bool,
    /// The names the person said not to bring here.
    pub not_here: Vec<String>,
}

impl Store {
    /// The names of the programs that are here, in order: those with a version linked.
    pub fn names(&self) -> Result<Vec<String>, Error> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::io(&self.root, error)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| Error::io(&self.root, error))?;
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if is_dir && check_name(&name).is_ok() && self.current(&name)?.is_some() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Everything the folder says.
    pub fn list(&self) -> Result<Listing, Error> {
        let mut programs = Vec::new();
        for name in self.names()? {
            let Some(version) = self.current(&name)? else {
                continue;
            };
            programs.push(Program {
                source: self.source(&name)?,
                key: self.key(&name)?,
                stopped: self.stopped(&name)?,
                name,
                version,
            });
        }
        Ok(Listing {
            programs,
            auto: self.auto(),
            not_here: self.not_here()?,
        })
    }

    /// Takes the program away: its link and everything it has, every version and every note. True
    /// when there was something. A link that is not kablo's is left, and so is what the person
    /// said about not bringing the program here.
    pub fn remove(&self, name: &str) -> Result<bool, Error> {
        let dir = self.app_dir(name)?;
        let mut was = false;
        let link = self.bin_dir().join(name);
        let ours = fs::read_link(&link).is_ok_and(|text| text.starts_with(format!("../{name}/")));
        if ours {
            fs::remove_file(&link).map_err(|error| Error::io(&link, error))?;
            was = true;
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => was = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(dir, error)),
        }
        Ok(was)
    }
}
