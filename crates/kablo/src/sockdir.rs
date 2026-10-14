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

    #[test]
    fn a_path_too_long_for_a_socket_is_named_as_such() {
        let long = PathBuf::from(format!("/tmp/{}/x.sock", "d".repeat(200)));
        let err = listen(&long).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("at most"));
    }
}
