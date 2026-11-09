//! The small notes the folder keeps beside the programs: where one came from, the key approved
//! for it, the key that stopped it, whether programs are updated on their own, and the
//! names the person said not to bring here. Each is a file of plain text, and each is replaced
//! whole (a name that is made and renamed), never written in place.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use kablo_bateri::paths;
use pencere_host::apps::is_name;

use crate::{Error, Store};

/// More than this is not a note.
const NOTE_BYTES: u64 = 4096;

impl Store {
    /// The address or `user/repo` the program came from.
    pub fn source(&self, name: &str) -> Result<Option<String>, Error> {
        read_line(&self.app_dir(name)?.join(paths::APP_SOURCE))
    }

    /// The key approved for the program.
    pub fn key(&self, name: &str) -> Result<Option<String>, Error> {
        read_line(&self.app_dir(name)?.join(paths::APP_KEY))
    }

    /// The key a newer release came signed with, if the program was stopped for it.
    pub fn stopped(&self, name: &str) -> Result<Option<String>, Error> {
        read_line(&self.app_dir(name)?.join(paths::APP_STOPPED))
    }

    /// Stops the program at the version it is: a release came signed with `key`, which is not the
    /// approved one. True when that is news (nothing was written, or another key), so that the
    /// person is told once.
    pub fn stop(&self, name: &str, key: &str) -> Result<bool, Error> {
        let dir = self.app_dir(name)?;
        if !dir.is_dir() {
            return Err(Error::NotInstalled(name.to_string()));
        }
        let path = dir.join(paths::APP_STOPPED);
        if read_line(&path)?.as_deref() == Some(key) {
            return Ok(false);
        }
        write_line(&path, key, "the key")?;
        Ok(true)
    }

    /// Whether programs are updated on their own (on unless the person said off).
    pub fn auto(&self) -> bool {
        !self.root.join(paths::AUTO_OFF).exists()
    }

    /// Turns the updating of programs on their own on or off for this machine.
    pub fn set_auto(&self, on: bool) -> Result<(), Error> {
        let path = self.root.join(paths::AUTO_OFF);
        if on {
            return match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(Error::io(path, error)),
            };
        }
        fs::create_dir_all(&self.root).map_err(|error| Error::io(&self.root, error))?;
        write_note(&path, "")
    }

    /// The names the person said not to bring here, in order.
    pub fn not_here(&self) -> Result<Vec<String>, Error> {
        Ok(self.not_here_set()?.into_iter().collect())
    }

    /// Adds names to those not to bring here.
    pub fn mark_not_here(&self, names: &[&str]) -> Result<(), Error> {
        let mut all = self.not_here_set()?;
        for name in names {
            crate::check_name(name)?;
            all.insert((*name).to_string());
        }
        self.write_not_here(&all)
    }

    /// A program installed is brought here, whatever was said before.
    pub(crate) fn unmark_not_here(&self, name: &str) -> Result<(), Error> {
        let mut all = self.not_here_set()?;
        if all.remove(name) {
            self.write_not_here(&all)?;
        }
        Ok(())
    }

    fn not_here_set(&self) -> Result<BTreeSet<String>, Error> {
        let path = self.root.join(paths::NOT_HERE);
        let text = match read_note(&path)? {
            Some(text) => text,
            None => return Ok(BTreeSet::new()),
        };
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| is_name(line))
            .map(str::to_string)
            .collect())
    }

    fn write_not_here(&self, names: &BTreeSet<String>) -> Result<(), Error> {
        let mut text = String::new();
        for name in names {
            text.push_str(name);
            text.push('\n');
        }
        fs::create_dir_all(&self.root).map_err(|error| Error::io(&self.root, error))?;
        write_note(&self.root.join(paths::NOT_HERE), &text)
    }

    /// Writes the notes a placed release leaves: the key when it is new or approved again, where it
    /// came from, and that nothing stops it now.
    pub(crate) fn record(
        &self,
        name: &str,
        key: Option<&str>,
        source: Option<&str>,
    ) -> Result<(), Error> {
        let dir = self.app_dir(name)?;
        if let Some(key) = key {
            write_line(&dir.join(paths::APP_KEY), key, "the key")?;
        }
        if let Some(source) = source {
            write_line(&dir.join(paths::APP_SOURCE), source, "the source")?;
        }
        let stopped = dir.join(paths::APP_STOPPED);
        match fs::remove_file(&stopped) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io(stopped, error)),
        }
    }
}

fn write_line(path: &Path, text: &str, what: &'static str) -> Result<(), Error> {
    if text.is_empty() || text.len() as u64 > NOTE_BYTES || text.chars().any(char::is_control) {
        return Err(Error::Record(what));
    }
    write_note(path, &format!("{text}\n"))
}

fn read_line(path: &Path) -> Result<Option<String>, Error> {
    Ok(read_note(path)?.and_then(|text| {
        let line = text.lines().next().unwrap_or("").trim();
        (!line.is_empty()).then(|| line.to_string())
    }))
}

fn read_note(path: &Path) -> Result<Option<String>, Error> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io(path, error)),
    };
    let mut text = String::new();
    file.take(NOTE_BYTES)
        .read_to_string(&mut text)
        .map_err(|error| Error::io(path, error))?;
    Ok(Some(text))
}

/// `text` becomes the file's whole content or the file is as it was.
pub(crate) fn write_note(path: &Path, text: &str) -> Result<(), Error> {
    let Some(parent) = path.parent() else {
        return Err(Error::io(path, io::ErrorKind::InvalidInput.into()));
    };
    let file = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = parent.join(format!(".{file}.new-{}", std::process::id()));
    let written = fs::write(&temp, text).and_then(|()| fs::rename(&temp, path));
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(Error::io(path, error));
    }
    Ok(())
}
