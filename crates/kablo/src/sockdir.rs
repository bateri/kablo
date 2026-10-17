//! Where sessions live: the socket directory and the sockets in it.
//!
//! Every session is a Unix socket, `<dir>/<id>.sock`, in one directory that
//! belongs to the user: `/tmp/kablo-<uid>/` unless `KABLO_DIR` names another.
//! The directory is created with mode 0700 and **checked** every time it is
//! used: it must be a directory (not a link to one), owned by the user this
//! process acts as, with exactly mode 0700. A directory that is not is
//! refused, not repaired: someone else may have made it so on purpose, and
//! putting a session socket in it would let them connect to the shell.
//! `$XDG_RUNTIME_DIR` is deliberately not used; it is removed when the user's
//! last login session ends, and sessions are meant to outlive that.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use kablo_proto::SessionId;
use kablo_proto::v1::MAX_LABEL;

use crate::sys;

/// The environment variable that names the socket directory.
pub const ENV_DIR: &str = "KABLO_DIR";

/// The longest socket path, in bytes, that `sockaddr_un` holds (with its NUL):
/// 104 on macOS, 108 on Linux.
#[cfg(target_os = "linux")]
pub const SUN_PATH: usize = 108;
/// The longest socket path, in bytes, that `sockaddr_un` holds (with its NUL):
/// 104 on macOS, 108 on Linux.
#[cfg(not(target_os = "linux"))]
pub const SUN_PATH: usize = 104;

/// The socket directory in force: `KABLO_DIR` when set and non-empty (it must
/// be absolute), else `/tmp/kablo-<uid>`.
pub fn socket_dir() -> io::Result<PathBuf> {
    match std::env::var_os(ENV_DIR) {
        Some(dir) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                Ok(dir)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{ENV_DIR} must be an absolute path"),
                ))
            }
        }
        _ => Ok(PathBuf::from(format!(
            "/tmp/kablo-{}",
            sys::effective_uid()
        ))),
    }
}

/// Creates `dir` with mode 0700 if it does not exist, then checks it.
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check_dir(dir)
}

/// Refuses a directory that is not a real directory, not ours, or not 0700.
pub fn check_dir(dir: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    let refuse = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the socket directory {} {why}", dir.display()),
        ))
    };
    if !meta.file_type().is_dir() {
        return refuse("is not a directory");
    }
    if meta.uid() != sys::effective_uid() {
        return refuse("belongs to another user");
    }
    if meta.mode() & 0o777 != 0o700 {
        return refuse("is not mode 0700");
    }
    Ok(())
}

/// The path of the socket of session `id` in `dir`.
pub fn socket_path(dir: &Path, id: &SessionId) -> PathBuf {
    dir.join(format!("{id}.sock"))
}

/// The sessions in `dir`: the ids of the `*.sock` files, sorted.
pub fn session_ids(dir: &Path) -> io::Result<Vec<SessionId>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = name.strip_suffix(".sock") {
            if let Ok(id) = SessionId::new(id) {
                ids.push(id);
            }
        }
    }
    ids.sort();
    Ok(ids)
}

/// The path of the file that holds the label of session `id` in `dir`.
///
/// The file is the whole truth about a label: the relay keeps none of its own
/// and reads the file whenever it says how the session stands, so a label that
/// is changed, or that is there when the relay is replaced, is seen at once and
/// stays. It is made when a session is (see [`crate::launch::launch`]) and removed
/// when the session goes (the relay's last act), and its name does not end in
/// `.sock`, so [`session_ids`] does not take it for a session.
pub fn label_path(dir: &Path, id: &SessionId) -> PathBuf {
    dir.join(format!("{id}.label"))
}

/// The label of session `id` in `dir`: what the file says, cut at
/// [`kablo_proto::v1::MAX_LABEL`] bytes where a character ends and with what is
/// not UTF-8 replaced; `None` when there is no file, it cannot be read or it is
/// empty. A label that is missing is not an error, and neither is one that cannot
/// be read: the session is there all the same and `list` must show it.
pub fn read_label(dir: &Path, id: &SessionId) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(label_path(dir, id)).ok()?;
    // Longer than a label can be, whatever the file holds: it may have been
    // made by anyone who can write to the directory, which is the user.
    let mut bytes = Vec::new();
    file.take(4 * MAX_LABEL as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut end = text.len().min(MAX_LABEL);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let label = text.get(..end).unwrap_or_default();
    (!label.is_empty()).then(|| label.to_owned())
}

/// Gives session `id` in `dir` this label, replacing the one it has; an empty
/// label takes it away. The file is written under another name and renamed, so
/// that a relay that reads it at any moment finds a whole label, the old or the
/// new.
pub fn write_label(dir: &Path, id: &SessionId, label: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if label.is_empty() {
        return remove_label(dir, id);
    }
    if label.len() > MAX_LABEL {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a label is at most {MAX_LABEL} bytes"),
        ));
    }
    let path = label_path(dir, id);
    let scratch = dir.join(format!(".{id}.label.{}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&scratch)
        .and_then(|mut file| file.write_all(label.as_bytes()))
        .and_then(|()| std::fs::rename(&scratch, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&scratch);
    }
    written
}

/// Takes away the label of session `id` in `dir`; one that is not there is
/// taken away already.
pub fn remove_label(dir: &Path, id: &SessionId) -> io::Result<()> {
    match std::fs::remove_file(label_path(dir, id)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The name every mailbox starts with. The rest is the pid of the bridge that
/// keeps it, and there is no `.sock` at the end: [`session_ids`] reads that
/// ending as a session, and a mailbox is not one.
const MAILBOX_PREFIX: &str = "mail-";

/// The path of the mailbox of the bridge `pid` in `dir`: the socket a program
/// on the server connects to in order to talk to the app that bridge carries.
pub fn mailbox_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{MAILBOX_PREFIX}{pid}"))
}

/// A mailbox in the socket directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxFile {
    /// The bridge that keeps it.
    pub pid: u32,
    /// Where it is.
    pub path: PathBuf,
}

/// The mailboxes in `dir` whose bridge is running, by pid. One whose bridge is
/// gone (no process has its pid) was left by a bridge that was killed, and is
/// removed.
///
/// That is the whole test of whether a mailbox is stale. Whether the bridge
/// answers is not: a bridge that is new, or busy, is slow to accept, and a
/// refused connection would send the one who tried to delete the name of a
/// bridge that is there.
pub fn mailboxes(dir: &Path) -> io::Result<Vec<MailboxFile>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(mailbox_pid) else {
            continue;
        };
        if sys::process_exists(pid) {
            found.push(MailboxFile {
                pid,
                path: entry.path(),
            });
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    found.sort_by_key(|mailbox| mailbox.pid);
    Ok(found)
}

/// The pid in the name of a mailbox, if `name` is one: `mail-` and the number
/// written as [`mailbox_path`] writes it, and nothing else.
fn mailbox_pid(name: &str) -> Option<u32> {
    let number = name.strip_prefix(MAILBOX_PREFIX)?;
    let pid: u32 = number.parse().ok()?;
    (pid > 0 && number == pid.to_string()).then_some(pid)
}

/// Binds the socket of a new session. A socket file left behind by a session
/// that no longer exists (nothing answers on it) is replaced; one that
/// answers is [`io::ErrorKind::AddrInUse`].
pub fn listen(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::ffi::OsStrExt;
    let len = path.as_os_str().as_bytes().len();
    if len >= SUN_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the socket path {} is {len} bytes; a Unix socket path holds at most {}",
                path.display(),
                SUN_PATH - 1
            ),
        ));
    }
    match UnixListener::bind(path) {
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path) {
                Ok(_) => return Err(e),
                Err(c) if c.kind() == io::ErrorKind::ConnectionRefused => {}
                Err(c) if c.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err(e),
            }
            std::fs::remove_file(path)?;
            UnixListener::bind(path)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/kt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_new_directory_is_private_and_a_private_one_passes() {
        let dir = scratch("sd-ok");
        ensure_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        ensure_dir(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_that_is_not_private_is_refused_and_left_alone() {
        let dir = scratch("sd-open");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = ensure_dir(&dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::metadata(&dir).unwrap().mode() & 0o777,
            0o755,
            "not repaired"
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(check_dir(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_to_a_private_directory_is_refused() {
        let real = scratch("sd-real");
        let link = scratch("sd-link");
        ensure_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(check_dir(&link).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir_all(&real).unwrap();
    }

    #[test]
    fn a_directory_that_is_not_ours_is_refused() {
        // `/` is owned by root and not 0700 on every system the tests run on.
        assert!(check_dir(Path::new("/")).is_err());
    }

    /// The file a session that was killed leaves behind: a stream socket that
    /// was bound and nothing listens on. (Made by hand, because dropping a
    /// listener in a test that runs beside others can race with a child that
    /// another test forked and that still holds the descriptor.)
    fn leave_a_stale_socket(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: plain socket calls on values owned by this frame.
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0);
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as _;
            for (slot, byte) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
                *slot = *byte as _;
            }
            let status = libc::bind(
                fd,
                (&raw const addr).cast(),
                size_of::<libc::sockaddr_un>() as libc::socklen_t,
            );
            assert_eq!(status, 0);
            libc::close(fd);
        }
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_not() {
        let dir = scratch("sd-sock");
        ensure_dir(&dir).unwrap();
        let stale = socket_path(&dir, &SessionId::new("stale").unwrap());
        leave_a_stale_socket(&stale);
        assert!(stale.exists());
        let replaced = listen(&stale).unwrap();
        let err = listen(&stale).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "now it answers");
        drop(replaced);
        assert_eq!(session_ids(&dir).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The pid of a process that has ended and been waited for.
    fn a_pid_that_is_gone() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn a_mailbox_is_stale_when_its_bridge_is_gone_and_not_when_it_is_only_silent() {
        let dir = scratch("sd-mail");
        ensure_dir(&dir).unwrap();
        let gone = a_pid_that_is_gone();
        let stale = mailbox_path(&dir, gone);
        let silent = mailbox_path(&dir, std::process::id());
        // Both are sockets nobody accepts on; only the pid tells them apart.
        leave_a_stale_socket(&stale);
        leave_a_stale_socket(&silent);
        // Names that are not mailboxes are left where they are.
        let others = ["mail-0", "mail-007", "mail-1x", "mail-", "mailbox"];
        for name in others {
            std::fs::write(dir.join(name), "").unwrap();
        }

        let found = mailboxes(&dir).unwrap();
        assert_eq!(
            found,
            vec![MailboxFile {
                pid: std::process::id(),
                path: silent.clone()
            }]
        );
        assert!(!stale.exists(), "the stale one is removed");
        assert!(silent.exists(), "one that does not answer is not stale");
        for name in others {
            assert!(dir.join(name).exists(), "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_mailbox_is_not_a_session() {
        let dir = scratch("sd-mail-ids");
        ensure_dir(&dir).unwrap();
        leave_a_stale_socket(&mailbox_path(&dir, std::process::id()));
        assert!(session_ids(&dir).unwrap().is_empty());
        let session = socket_path(&dir, &SessionId::new("mail-5").unwrap());
        leave_a_stale_socket(&session);
        assert_eq!(session_ids(&dir).unwrap().len(), 1);
        assert_eq!(
            mailboxes(&dir).unwrap().len(),
            1,
            "a session is not a mailbox"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_too_long_for_a_socket_is_named_as_such() {
        let long = PathBuf::from(format!("/tmp/{}/x.sock", "d".repeat(200)));
        let err = listen(&long).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("at most"));
    }

    #[test]
    fn a_label_is_written_whole_read_back_and_taken_away() {
        let dir = scratch("sd-label");
        ensure_dir(&dir).unwrap();
        let id = SessionId::new("build").unwrap();
        assert_eq!(read_label(&dir, &id), None, "no file is no label");
        write_label(&dir, &id, "the build").unwrap();
        assert_eq!(read_label(&dir, &id).as_deref(), Some("the build"));
        write_label(&dir, &id, "the tests").unwrap();
        assert_eq!(read_label(&dir, &id).as_deref(), Some("the tests"));
        assert_eq!(
            std::fs::metadata(label_path(&dir, &id)).unwrap().mode() & 0o777,
            0o600
        );
        // Only the file is left: the scratch name it was written under is gone.
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["build.label"]);
        assert!(session_ids(&dir).unwrap().is_empty(), "not a session");
        write_label(&dir, &id, "").unwrap();
        assert_eq!(read_label(&dir, &id), None);
        assert!(!label_path(&dir, &id).exists());
        remove_label(&dir, &id).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_label_file_that_is_too_long_or_not_text_is_cut_to_what_a_label_can_be() {
        let dir = scratch("sd-label-cut");
        ensure_dir(&dir).unwrap();
        let id = SessionId::new("odd").unwrap();
        // Two bytes a character: the 64th byte is the middle of one.
        std::fs::write(label_path(&dir, &id), "é".repeat(40)).unwrap();
        assert_eq!(read_label(&dir, &id), Some("é".repeat(32)));
        std::fs::write(label_path(&dir, &id), vec![b'x'; 10_000]).unwrap();
        assert_eq!(read_label(&dir, &id), Some("x".repeat(MAX_LABEL)));
        std::fs::write(label_path(&dir, &id), b"a\xffb").unwrap();
        assert_eq!(read_label(&dir, &id).as_deref(), Some("a\u{fffd}b"));
        std::fs::write(label_path(&dir, &id), b"").unwrap();
        assert_eq!(read_label(&dir, &id), None, "an empty file is no label");
        let too_long = "x".repeat(MAX_LABEL + 1);
        assert!(write_label(&dir, &id, &too_long).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
