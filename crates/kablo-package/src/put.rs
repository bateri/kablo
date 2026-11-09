//! Putting a release in.

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::SystemTime;

use kablo_bateri::paths;
use pencere_host::apps::{self, Manifest, Version};
use pencere_host::bundle::Digest;

use crate::{Error, Store, prune};

/// The largest program a machine takes.
const PROGRAM_BYTES: u64 = 256 * 1024 * 1024;

/// A release placed on the machine, waiting to be put in.
#[derive(Debug, Clone, Copy)]
pub struct Put<'a> {
    /// The program asked for.
    pub name: &'a str,
    /// The `<p>` of the directory `apps/<name>/.<version>.part-<p>` the file was placed in, as
    /// `<name>` in it.
    pub part: &'a str,
    /// The `app.json`, as it was signed.
    pub manifest: &'a [u8],
    /// Its signature.
    pub signature: &'a [u8],
    /// The kind of machine this is, as `app.json` names it in `files`.
    pub target: &'a str,
    /// Where the release came from, to be kept with the program.
    pub source: Option<&'a str>,
    /// Whose key it may be signed with.
    pub trust: Trust<'a>,
}

/// Whose word makes a key good enough.
#[derive(Debug, Clone, Copy)]
pub enum Trust<'a> {
    /// The key approved for the program, or one of those kablo ships with when none was yet.
    Recorded,
    /// That, and this key too: a person approved it just now. It is kept for the program, and it
    /// takes the place of the key approved before.
    Approved(&'a str),
}

/// Where a key stands for a program on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// It is the key approved for the program.
    Approved,
    /// The program has no approved key yet, and this is one kablo ships with.
    Shipped,
    /// The program has no approved key yet, and this is nobody's the machine knows.
    Unknown,
    /// The program has another approved key.
    Changed { recorded: String },
}

/// Who a release says it is, once its signature holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub name: String,
    pub version: Version,
    /// The key it is signed with.
    pub key: String,
}

/// Reads an `app.json` and checks the signature it came with against the key it names: whose
/// release it is, at which version, signed by whom. Nothing about the machine is looked at, and
/// nothing is written.
pub fn inspect(manifest: &[u8], signature: &[u8]) -> Result<Release, Error> {
    let manifest = apps::verify(manifest, signature).map_err(Error::Manifest)?;
    Ok(Release {
        name: manifest.name,
        version: manifest.version,
        key: manifest.key,
    })
}

/// What [`Store::put`] came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The release is the one linked now; `before` is what was linked.
    Installed {
        version: Version,
        before: Option<Version>,
    },
    /// The machine has this version already; nothing was changed.
    Unchanged { current: Version },
}

impl Store {
    /// The version linked for the program, if it has one.
    pub fn current(&self, name: &str) -> Result<Option<Version>, Error> {
        self.app_dir(name)?;
        let link = self.bin_dir().join(name);
        match fs::read_link(&link) {
            Ok(text) => Ok(version_of(name, &text)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(Error::io(link, error)),
        }
    }

    /// Puts a release in, if it is a good one and a newer one than the machine has.
    pub fn put(&self, put: &Put<'_>) -> Result<Outcome, Error> {
        let dir = self.app_dir(put.name)?;
        check_part(put.part)?;
        let manifest = apps::verify(put.manifest, put.signature).map_err(Error::Manifest)?;
        if manifest.name != put.name {
            return Err(Error::WrongName {
                asked: put.name.to_string(),
                signed: manifest.name,
            });
        }
        let part = dir.join(format!(".{}.part-{}", manifest.version, put.part));
        let done = self.place(put, &manifest, &dir, &part);
        if !matches!(done, Ok(Outcome::Installed { .. })) {
            // What was placed is not wanted now: it is not left to be found by the next one.
            let _ = fs::remove_dir_all(&part);
        }
        done
    }

    fn place(
        &self,
        put: &Put<'_>,
        manifest: &Manifest,
        dir: &Path,
        part: &Path,
    ) -> Result<Outcome, Error> {
        let name = put.name;
        let version = manifest.version;
        let file = manifest
            .files
            .get(put.target)
            .ok_or_else(|| Error::NoFile(put.target.to_string()))?;
        let record_key = self.trust(name, &manifest.key, put.trust)?;
        let before = self.current(name)?;
        match before {
            Some(current) if current == version => return Ok(Outcome::Unchanged { current }),
            Some(current) if current > version => {
                return Err(Error::Older {
                    current,
                    offered: version,
                });
            }
            _ => {}
        }
        self.check_link(name)?;

        let program = part.join(name);
        let sum = sum_of(&program)?;
        if Some(sum) != Digest::parse(&file.sha256) {
            return Err(Error::Damaged);
        }
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
            .map_err(|error| Error::io(&program, error))?;
        let manifest_path = part.join(paths::APP_MANIFEST);
        fs::write(&manifest_path, put.manifest)
            .map_err(|error| Error::io(&manifest_path, error))?;
        let signature_path = part.join(paths::APP_SIGNATURE);
        fs::write(&signature_path, put.signature)
            .map_err(|error| Error::io(&signature_path, error))?;

        // A version that is there was put in whole by a rename, so it is not put in again.
        let there = dir.join(version.to_string());
        if there.is_dir() {
            let _ = fs::remove_dir_all(part);
        } else if let Err(error) = fs::rename(part, &there) {
            if !there.is_dir() {
                return Err(Error::io(there, error));
            }
            let _ = fs::remove_dir_all(part);
        }
        self.record(
            name,
            record_key.then_some(manifest.key.as_str()),
            put.source,
        )?;
        self.switch(name, version)?;
        let _ = self.unmark_not_here(name);
        // The install is done; the clearing is for the next one to find undone if it fails.
        let _ = prune::prune(dir, version, SystemTime::now());
        Ok(Outcome::Installed { version, before })
    }

    /// Where a key stands for a program on this machine.
    pub fn standing(&self, name: &str, key: &str) -> Result<Standing, Error> {
        Ok(match self.key(name)? {
            Some(recorded) if recorded == key => Standing::Approved,
            Some(recorded) => Standing::Changed { recorded },
            None if self.embedded.iter().any(|known| known == key) => Standing::Shipped,
            None => Standing::Unknown,
        })
    }

    /// Whether the key signing a release is good enough, and whether it is to be written down.
    fn trust(&self, name: &str, key: &str, trust: Trust<'_>) -> Result<bool, Error> {
        let approved = matches!(trust, Trust::Approved(approved) if approved == key);
        match self.standing(name, key)? {
            Standing::Approved => Ok(false),
            Standing::Shipped => Ok(true),
            Standing::Unknown | Standing::Changed { .. } if approved => Ok(true),
            Standing::Unknown => Err(Error::Unapproved {
                key: key.to_string(),
            }),
            Standing::Changed { recorded } => Err(Error::KeyChanged {
                recorded,
                offered: key.to_string(),
            }),
        }
    }

    /// The link for the program is kablo's or is not there; anything else is left as it is.
    fn check_link(&self, name: &str) -> Result<(), Error> {
        let link = self.bin_dir().join(name);
        match fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(&link) {
                Ok(text) if text.starts_with(format!("../{name}/")) => Ok(()),
                _ => Err(Error::Foreign(link)),
            },
            Ok(_) => Err(Error::Foreign(link)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io(link, error)),
        }
    }

    /// Links the program to the version: a new link is made and renamed over the old, so that a
    /// run of the program finds one or the other.
    fn switch(&self, name: &str, version: Version) -> Result<(), Error> {
        let bin = self.bin_dir();
        fs::create_dir_all(&bin).map_err(|error| Error::io(&bin, error))?;
        let link = bin.join(name);
        let temp = bin.join(format!(".{name}.new-{}", std::process::id()));
        let _ = fs::remove_file(&temp);
        let made = symlink(format!("../{name}/{version}/{name}"), &temp)
            .and_then(|()| fs::rename(&temp, &link));
        if let Err(error) = made {
            let _ = fs::remove_file(&temp);
            return Err(Error::io(link, error));
        }
        Ok(())
    }
}

/// The version a link's text names, if the text is the one kablo writes for `name`.
fn version_of(name: &str, text: &Path) -> Option<Version> {
    let text = text.to_str()?;
    let rest = text
        .strip_prefix("../")?
        .strip_prefix(name)?
        .strip_prefix('/')?;
    let (version, last) = rest.split_once('/')?;
    (last == name).then(|| Version::parse(version))?
}

/// The sum of the program's file, which has to be a plain file of a size a program may be.
fn sum_of(program: &Path) -> Result<Digest, Error> {
    let meta = fs::symlink_metadata(program).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => Error::Missing(program.to_path_buf()),
        _ => Error::io(program, error),
    })?;
    if !meta.file_type().is_file() {
        return Err(Error::Missing(program.to_path_buf()));
    }
    if meta.len() > PROGRAM_BYTES {
        return Err(Error::TooBig(meta.len()));
    }
    let bytes = fs::read(program).map_err(|error| Error::io(program, error))?;
    Ok(Digest::of(&bytes))
}

fn check_part(part: &str) -> Result<(), Error> {
    let fine = !part.is_empty()
        && part.len() <= 64
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if fine {
        Ok(())
    } else {
        Err(Error::Part(part.to_string()))
    }
}
