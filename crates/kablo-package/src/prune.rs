//! What a machine keeps of the versions it has had: the one linked, the one before it, and every one
//! younger than thirty days. The rest go, and so does a release that was being placed and never
//! came to an end.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use pencere_host::apps::Version;

/// A version older than this, that is neither the linked one nor the one before it, goes.
pub(crate) const KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A part of a release older than this was left behind by one that did not come to an end.
pub(crate) const PART_KEEP: Duration = Duration::from_secs(24 * 60 * 60);

/// Cleans the folder of one program, `current` being the version linked. Each directory is judged
/// by when it was last changed, which for a version is when it was put in.
pub(crate) fn prune(dir: &Path, current: Version, now: SystemTime) -> io::Result<()> {
    let mut versions = Vec::new();
    let mut parts = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let age = now
            .duration_since(entry.metadata()?.modified()?)
            .unwrap_or_default();
        if let Some(version) = Version::parse(name) {
            versions.push((version, age, entry.path()));
        } else if name.starts_with('.') && name.contains(".part-") {
            parts.push((age, entry.path()));
        }
    }
    let before = versions
        .iter()
        .map(|(version, ..)| *version)
        .filter(|version| *version < current)
        .max();
    for (version, age, path) in versions {
        if version != current && Some(version) != before && age >= KEEP {
            fs::remove_dir_all(path)?;
        }
    }
    for (age, path) in parts {
        if age >= PART_KEEP {
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}
