//! What the command keeps on this machine, under `~/.local/share/bateri`: the counter that gives
//! the terminals their numbers, who opened which terminal, and the message that waits for a Mac.
//!
//! The state is under `$HOME`, not `XDG_DATA_HOME`: the Mac and a cron job do not have a login
//! shell's environment, and a counter that two environments place in two directories would give
//! the same number twice. Everything is written for its owner alone; the terminal the command
//! starts is another matter and keeps the umask of whoever asked.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::sys;

/// A nap while another holds the counter's lock.
const NAP: Duration = Duration::from_millis(100);

/// Naps that are about five seconds: a lock that names no holder is taken over after them.
const NAPS_FOR_AN_UNNAMED_HOLDER: u32 = 50;

/// Naps in a minute: a lock held that long is taken over all the same, since something that took
/// the number over would hold it for ever.
const NAPS_FOR_ANY_HOLDER: u32 = NAPS_FOR_AN_UNNAMED_HOLDER * 12;

/// `~/.local/share/bateri`.
#[derive(Clone, Debug)]
pub struct State {
    dir: PathBuf,
}

impl State {
    pub fn new(dir: PathBuf) -> State {
        State { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the bootstrap writes the command's files: `shell/`, with `bin/bateri` and, on a
    /// server, `boot.sh`.
    pub fn shell_dir(&self) -> PathBuf {
        self.dir.join("shell")
    }

    /// The directory the command is found in, which a terminal it starts gets on its `PATH`.
    pub fn bin_dir(&self) -> PathBuf {
        self.shell_dir().join("bin")
    }

    /// The bootstrap of a server's terminal; a machine that has none is not a server (and the Mac
    /// says which it is, when it is there to say).
    pub fn boot(&self) -> PathBuf {
        self.shell_dir().join("boot.sh")
    }

    /// `~/.local/share/bateri/terminals/pN`, where who opened a terminal is written.
    pub fn record_of(&self, id: &str) -> PathBuf {
        self.dir.join("terminals").join(id)
    }

    /// `~/.local/share/bateri/placing/pN`, where a message waits for a Mac.
    pub fn note_of(&self, id: &str) -> PathBuf {
        self.dir.join("placing").join(id)
    }

    /// A directory that only its owner reads, and its parents.
    fn make_dir(path: &Path) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }

    /// Writes `text` to `path` whole or not at all: to a file of its own beside it, then a rename.
    fn write_whole(path: &Path, text: &str) -> io::Result<()> {
        let Some(parent) = path.parent() else {
            return Err(io::Error::other("a file with no directory"));
        };
        State::make_dir(parent)?;
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".tmp.{}", std::process::id()));
        let tmp = parent.join(name);
        let written = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .and_then(|()| fs::rename(&tmp, path));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written
    }

    /// Who opened terminal `id` (`-` for none) and whether it stays when its program ends: plain
    /// `key value` lines. `ls` shows it and nothing decides by it. Not being able to write it is
    /// not worth a failure.
    pub fn record(&self, id: &str, opener: Option<&str>, keep_open: bool) {
        let text = format!(
            "opener {}\nkeep-open {}\n",
            opener.unwrap_or("-"),
            if keep_open { "yes" } else { "no" }
        );
        let _ = State::write_whole(&self.record_of(id), &text);
    }

    /// Puts the message of `id` in the line for a Mac; whether it could be.
    pub fn note(&self, id: &str, message: &str) -> bool {
        State::write_whole(&self.note_of(id), message).is_ok()
    }

    /// Forgets what is kept of `id`.
    pub fn forget(&self, id: &str) {
        let _ = fs::remove_file(self.record_of(id));
        let _ = fs::remove_file(self.note_of(id));
    }

    /// The number of the next terminal, never given twice, whatever became of its terminal: the
    /// panes an agent opens are `p1`, `p2`, … per machine. `None` when it cannot count: a directory
    /// it cannot write, or a counter that is not a number (never guessed at: a guess is how a number
    /// is given twice).
    ///
    /// The lock is a directory (`mkdir` is the one atomic step there is) that names its holder. A
    /// holder that is gone loses it at once, one that wrote no name loses it after about five
    /// seconds (it died before it could), and one that is alive keeps it (a lock held a full minute
    /// is taken over all the same). The taking over is a rename, which one contender wins. A holder
    /// only lets go of a lock that is still its own.
    ///
    /// The number is written back BEFORE it is given, so a caller that dies in between loses a
    /// number, which is allowed, and never repeats one.
    pub fn next_number(&self) -> Option<u64> {
        State::make_dir(&self.dir).ok()?;
        let lock = self.dir.join("next.lock");
        let me = std::process::id();
        State::take(&lock)?;
        let held = Held {
            lock: &lock,
            me: format!("{me}\n"),
        };
        fs::write(lock.join("pid"), &held.me).ok();
        let counter = self.dir.join("next");
        let number = match fs::read_to_string(&counter) {
            Ok(text) => {
                let line = text.lines().next().unwrap_or("");
                let plain = !line.is_empty()
                    && line.bytes().all(|byte| byte.is_ascii_digit())
                    && !line.starts_with('0');
                if !plain {
                    return None;
                }
                line.parse::<u64>().ok()?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => 1,
            Err(_) => return None,
        };
        State::write_whole(&counter, &format!("{}\n", number.checked_add(1)?)).ok()?;
        drop(held);
        Some(number)
    }

    /// Takes the lock at `lock`, waiting for whoever has it, or taking it from one that is gone.
    fn take(lock: &Path) -> Option<()> {
        let mut tries = 0u32;
        let mut waited = 0u32;
        loop {
            match fs::DirBuilder::new().mode(0o700).create(lock) {
                Ok(()) => return Some(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => return None,
            }
            std::thread::sleep(NAP);
            tries += 1;
            waited += 1;
            let holder = fs::read_to_string(lock.join("pid")).unwrap_or_default();
            let holder = holder.trim_end_matches('\n');
            if !holder.is_empty() && holder.bytes().all(|byte| byte.is_ascii_digit()) {
                match holder.parse::<u32>() {
                    Ok(pid) if sys::process_exists(pid) => tries = 0,
                    _ => tries = NAPS_FOR_AN_UNNAMED_HOLDER,
                }
            }
            if tries >= NAPS_FOR_AN_UNNAMED_HOLDER || waited >= NAPS_FOR_ANY_HOLDER {
                let mut dead = lock.as_os_str().to_os_string();
                dead.push(format!(".dead.{}", std::process::id()));
                let dead = PathBuf::from(dead);
                if fs::rename(lock, &dead).is_ok() {
                    let _ = fs::remove_dir_all(&dead);
                }
                tries = 0;
                waited = 0;
            }
        }
    }
}

/// The counter's lock while it is held: it is let go of, if it is still ours, when this ends.
struct Held<'a> {
    lock: &'a Path,
    /// What the lock's `pid` holds when it is ours.
    me: String,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if fs::read_to_string(self.lock.join("pid")).is_ok_and(|text| text == self.me) {
            let _ = fs::remove_file(self.lock.join("pid"));
            let _ = fs::remove_dir(self.lock);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// How long `f` takes.
    fn took(f: impl FnOnce()) -> Duration {
        let start = Instant::now();
        f();
        start.elapsed()
    }

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kablo-state-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_counter_counts_from_one_and_lets_the_lock_go() {
        let state = State::new(dir());
        assert_eq!(state.next_number(), Some(1));
        assert_eq!(state.next_number(), Some(2));
        assert_eq!(fs::read_to_string(state.dir().join("next")).unwrap(), "3\n");
        assert!(!state.dir().join("next.lock").exists());
        let _ = fs::remove_dir_all(state.dir());
    }

    #[test]
    fn a_counter_that_is_no_number_is_not_guessed_at() {
        let state = State::new(dir());
        State::make_dir(state.dir()).unwrap();
        for bad in ["abc\n", "0\n", "007\n", "\n", "-3\n", "1x\n"] {
            fs::write(state.dir().join("next"), bad).unwrap();
            assert_eq!(state.next_number(), None, "{bad:?}");
            assert!(!state.dir().join("next.lock").exists(), "{bad:?}");
        }
        let _ = fs::remove_dir_all(state.dir());
    }

    #[test]
    fn a_lock_held_by_a_process_that_is_gone_is_taken_at_once() {
        let state = State::new(dir());
        State::make_dir(state.dir()).unwrap();
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let dead = gone.id();
        gone.wait().unwrap();
        let lock = state.dir().join("next.lock");
        fs::create_dir(&lock).unwrap();
        fs::write(lock.join("pid"), format!("{dead}\n")).unwrap();
        let mut got = None;
        let waited = took(|| got = state.next_number());
        assert_eq!(got, Some(1));
        assert!(waited < Duration::from_secs(4), "{waited:?}");
        assert!(!lock.exists());
        let _ = fs::remove_dir_all(state.dir());
    }

    #[test]
    fn a_lock_that_names_nobody_is_taken_after_a_wait() {
        let state = State::new(dir());
        State::make_dir(state.dir()).unwrap();
        let lock = state.dir().join("next.lock");
        fs::create_dir(&lock).unwrap();
        let mut got = None;
        let waited = took(|| got = state.next_number());
        assert_eq!(got, Some(1));
        assert!(waited >= Duration::from_secs(4), "{waited:?}");
        assert!(waited < Duration::from_secs(30), "{waited:?}");
        let _ = fs::remove_dir_all(state.dir());
    }
}
