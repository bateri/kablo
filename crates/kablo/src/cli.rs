//! The command line.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use kablo_proto::SessionId;
use kablo_proto::v1::{MAX_COLS, MAX_HISTORY, MAX_LABEL, MAX_ROWS, NewSession, Size};

use crate::{
    app, bridge, connect, drive, exec, forward, inner, journal, launch, listing, relay, scan,
    sockdir, sys, upgrade, warden,
};

/// The exit status of a command line that does not parse (`EX_USAGE`).
pub const EXIT_USAGE: u8 = 64;

const USAGE: &str = "usage: kablo <command>

commands:
  new       start a session that outlives its clients; prints its id
  list      show the sessions on this machine
  connect   use a session from this terminal (Ctrl-\\ leaves it running)
  send      type text to a session, without attaching to it
  send-key  press keys in a session, without attaching to it
  capture   print what a session's screen shows
  wait      wait for the program in a session to end; ends with its status
  label     show or set a session's label
  attach    carry every session over this program's input and output; for
            programs (`ssh host kablo attach`), not for people
  forward   carry connections to local ports to ports the server reaches, as
            `ssh -L` does, over a command that runs `kablo attach` there
  exec      run a program on the server over a command that runs `kablo attach`
            there, with its input, output and errors here; ends with its status
  app       hand this program's input to the app connected to this machine and
            print what the app says back; ends with 69 when there is none
  upgrade   run a session's relay and warden from another kablo binary,
            without ending the session or dropping the clients
  version   print the version and the protocol versions this build speaks
  help      print this text

kablo new [--id ID] [--label TEXT] [--wait] [--cols N] [--rows N] [--cwd DIR]
          [--env NAME=VALUE]... [--journal-size BYTES[K|M|G]] [--history ROWS]
          [-- PROGRAM [ARGUMENT]...]
kablo list [--json]
kablo connect [--fresh] ID
kablo send [ID] [TEXT | -]
kablo send-key [ID] KEY...
kablo capture [ID] [--history ROWS] [--unwrap] [--trim] [--vt]
kablo wait ID [--for TIME]
kablo label [ID] [TEXT]
kablo attach
kablo forward [--bind ADDRESS] (PORT | LOCAL:HOST:PORT)... -- COMMAND [ARGUMENT]...
kablo exec [--cwd DIR] [--env NAME=VALUE]... -- COMMAND [ARGUMENT]... -- PROGRAM [ARGUMENT]...
kablo app
kablo upgrade (--all | ID) PATH
";

/// The line `kablo version` prints: `key=value` tokens, to which keys may be
/// added but from which none is removed.
pub fn version_line() -> String {
    format!(
        "version={} wire_min={} wire_max={} inner_min={} inner_max={}",
        crate::release(),
        kablo_proto::MIN_VERSION,
        kablo_proto::MAX_VERSION,
        inner::MIN_VERSION,
        inner::MAX_VERSION,
    )
}

/// Runs a command line (without the program name) and returns the exit status.
pub fn run<I>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    if let Some((command, rest)) = args.split_first() {
        match command.to_str() {
            Some("new") => return command_new(rest, out, err),
            Some("list") => return command_list(rest, out, err),
            Some("connect") => return command_connect(rest, err),
            Some("send") => return drive::send(rest, err),
            Some("send-key") => return drive::send_key(rest, err),
            Some("capture") => return drive::capture(rest, out, err),
            Some("wait") => return drive::wait(rest, err),
            Some("label") => return drive::label(rest, out, err),
            Some("attach") => return command_attach(rest, err),
            Some("forward") => return command_forward(rest, out, err),
            Some("exec") => return command_exec(rest, err),
            Some("app") => return command_app(rest, err),
            Some("upgrade") => return command_upgrade(rest, out, err),
            // The two halves of a session; `kablo new` starts the first and the
            // first starts the second. Not for people.
            Some("warden") => {
                return match internal(rest) {
                    // A warden that renewed itself: the session is already up.
                    Ok(Internal {
                        id,
                        resume: Some(record),
                        ..
                    }) => warden::resume(id, record),
                    Ok(Internal {
                        id, dir: Some(dir), ..
                    }) => warden::run(id, &dir),
                    Ok(_) => usage_error(err, "warden needs --dir"),
                    Err(why) => usage_error(err, why),
                };
            }
            Some("relay") => {
                return match internal(rest) {
                    Ok(Internal { id, .. }) => relay::run(id),
                    Err(why) => usage_error(err, why),
                };
            }
            _ => {}
        }
    }
    let words: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
    let all_text = words.len() == args.len();
    match (all_text, words.as_slice()) {
        (true, ["version" | "-V" | "--version"]) => {
            let _ = writeln!(out, "{}", version_line());
            0
        }
        (true, ["help" | "-h" | "--help"]) => {
            let _ = write!(out, "{USAGE}");
            0
        }
        _ => {
            let _ = write!(err, "{USAGE}");
            EXIT_USAGE
        }
    }
}

pub(crate) fn usage_error(err: &mut dyn Write, why: &str) -> u8 {
    let _ = writeln!(err, "kablo: {why}");
    let _ = write!(err, "{USAGE}");
    EXIT_USAGE
}

/// The arguments of `kablo warden` and `kablo relay`.
#[derive(Debug, PartialEq, Eq)]
struct Internal {
    id: SessionId,
    /// The socket directory, for the warden.
    dir: Option<PathBuf>,
    /// The descriptor a renewed warden finds its record on.
    resume: Option<RawFd>,
}

/// Reads `--id ID` and, for the warden, `--dir DIR` and `--resume FD`.
fn internal(args: &[OsString]) -> Result<Internal, &'static str> {
    let mut id = None;
    let mut dir = None;
    let mut resume = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match (flag.to_str(), it.next()) {
            (Some("--id"), Some(value)) => {
                id = value.to_str().and_then(|v| SessionId::new(v).ok());
            }
            (Some("--dir"), Some(value)) => dir = Some(PathBuf::from(value)),
            (Some("--resume"), Some(value)) => {
                resume = Some(
                    value
                        .to_str()
                        .and_then(|v| v.parse().ok())
                        .ok_or("--resume needs a descriptor number")?,
                );
            }
            _ => return Err("unknown argument"),
        }
    }
    id.map(|id| Internal { id, dir, resume })
        .ok_or("--id is missing or not a session id")
}

/// What `kablo new` was asked for.
#[derive(Debug, Default, PartialEq, Eq)]
struct NewArgs {
    id: Option<SessionId>,
    request: NewSession,
    /// Wait for the program to end and end with its status.
    wait: bool,
}

fn parse_new(args: &[OsString]) -> Result<NewArgs, String> {
    let mut parsed = NewArgs::default();
    let (mut cols, mut rows) = (None, None);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        if flag == "--" {
            parsed.request.argv = it.by_ref().map(|a| a.as_bytes().to_vec()).collect();
            break;
        }
        let name = flag.to_str().unwrap_or("?");
        let mut value = || it.next().ok_or_else(|| format!("{name} needs a value"));
        match name {
            "--id" => {
                let value = value()?;
                let text = value.to_str().ok_or("the id is not text")?;
                parsed.id = Some(SessionId::new(text).map_err(|_| {
                    "an id is 1 to 64 characters of letters, digits, '.', '_' and '-'".to_owned()
                })?);
            }
            "--label" => {
                let text = value()?.to_str().ok_or("a label is text")?;
                if text.len() > MAX_LABEL {
                    return Err(format!("a label is at most {MAX_LABEL} bytes"));
                }
                parsed.request.label = Some(text.to_owned());
            }
            "--wait" => parsed.wait = true,
            "--cols" => cols = Some(number(value()?, "--cols", MAX_COLS)?),
            "--rows" => rows = Some(number(value()?, "--rows", MAX_ROWS)?),
            "--cwd" => {
                let value = value()?;
                let path = PathBuf::from(value);
                // The keeper starts elsewhere than this process; a relative
                // path would mean something else there.
                let path = std::path::absolute(path).map_err(|e| e.to_string())?;
                parsed.request.cwd = Some(path.as_os_str().as_bytes().to_vec());
            }
            "--env" => {
                let value = value()?.as_bytes();
                let at = value
                    .iter()
                    .position(|&b| b == b'=')
                    .filter(|&at| at > 0)
                    .ok_or("--env takes NAME=VALUE")?;
                let (name, rest) = value.split_at(at);
                parsed
                    .request
                    .env
                    .push((name.to_vec(), rest.get(1..).unwrap_or_default().to_vec()));
            }
            "--journal-size" => {
                parsed.request.journal_size = Some(size_with_unit(value()?)?);
            }
            "--history" => {
                parsed.request.history = Some(history(value()?)?);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    if cols.is_some() || rows.is_some() {
        let default = crate::pty::DEFAULT_SIZE;
        parsed.request.size = Some(Size {
            cols: cols.unwrap_or(default.cols),
            rows: rows.unwrap_or(default.rows),
            xpixel: 0,
            ypixel: 0,
        });
    }
    Ok(parsed)
}

fn number(value: &OsStr, name: &str, most: u16) -> Result<u16, String> {
    value
        .to_str()
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|&n| (1..=most).contains(&n))
        .ok_or_else(|| format!("{name} takes a number from 1 to {most}"))
}

/// The rows of scrollback the screen keeps: 0 to the most a screen may.
fn history(value: &OsStr) -> Result<u32, String> {
    value
        .to_str()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n <= MAX_HISTORY)
        .ok_or_else(|| format!("--history takes a number of rows from 0 to {MAX_HISTORY}"))
}

/// `1048576`, `64K`, `8M` or `1G`.
fn size_with_unit(value: &OsStr) -> Result<u64, String> {
    let wrong = || {
        format!(
            "--journal-size takes a size from {} to {} bytes, with K, M or G if you like",
            journal::MIN_CAPACITY,
            journal::MAX_CAPACITY
        )
    };
    let text = value.to_str().ok_or_else(wrong)?;
    let (digits, unit) = match text.char_indices().last() {
        Some((at, c)) if c.is_ascii_alphabetic() => (text.get(..at).unwrap_or(""), Some(c)),
        _ => (text, None),
    };
    let factor: u64 = match unit {
        None => 1,
        Some('k' | 'K') => 1 << 10,
        Some('m' | 'M') => 1 << 20,
        Some('g' | 'G') => 1 << 30,
        Some(_) => return Err(wrong()),
    };
    let bytes = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .filter(|n| (journal::MIN_CAPACITY..=journal::MAX_CAPACITY).contains(n))
        .ok_or_else(wrong)?;
    Ok(bytes)
}

/// The socket directory in force, or the reason there is none.
fn directory(err: &mut dyn Write) -> Option<std::path::PathBuf> {
    match sockdir::socket_dir() {
        Ok(dir) => Some(dir),
        Err(e) => {
            let _ = writeln!(err, "kablo: {e}");
            None
        }
    }
}

fn command_list(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => return usage_error(err, "list takes only --json"),
    };
    let Some(dir) = directory(err) else {
        return 1;
    };
    let statuses = match scan::Scan::start(&dir, scan::WAIT) {
        Ok(scan) => scan.run(),
        Err(e) => {
            let _ = writeln!(err, "kablo: {e}");
            return 1;
        }
    };
    let text = if json {
        listing::json(&statuses)
    } else {
        listing::table(&statuses, sys::unix_now().unwrap_or(0))
    };
    let _ = out.write_all(text.as_bytes());
    0
}

fn command_connect(args: &[OsString], err: &mut dyn Write) -> u8 {
    let mut fresh = false;
    let mut id = None;
    for arg in args {
        match arg.to_str() {
            Some("--fresh") => fresh = true,
            Some(text) if id.is_none() && !text.starts_with('-') => match SessionId::new(text) {
                Ok(parsed) => id = Some(parsed),
                Err(_) => return usage_error(err, "that is not a session id"),
            },
            _ => return usage_error(err, "connect takes a session id and --fresh"),
        }
    }
    let Some(id) = id else {
        return usage_error(err, "connect needs a session id");
    };
    let Some(dir) = directory(err) else {
        return 1;
    };
    connect::run(&dir, id, fresh, err)
}

fn command_attach(args: &[OsString], err: &mut dyn Write) -> u8 {
    if !args.is_empty() {
        return usage_error(err, "attach takes no arguments");
    }
    // SAFETY: descriptor 0 is open for the life of the process.
    let input = unsafe { std::os::fd::BorrowedFd::borrow_raw(0) };
    if sys::is_tty(input) {
        let _ = writeln!(
            err,
            "kablo: attach speaks the protocol on standard input and output, for programs; \
             to use a session yourself, `kablo connect`"
        );
        return EXIT_USAGE;
    }
    let Some(dir) = directory(err) else {
        return 1;
    };
    if let Err(e) = bridge::check_dir(&dir) {
        let _ = writeln!(err, "kablo: {e}");
        return 1;
    }
    bridge::run(dir)
}

fn command_forward(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    match forward::parse(args) {
        Ok(options) => forward::run(options, out, err),
        Err(why) => usage_error(err, &why),
    }
}

fn command_exec(args: &[OsString], err: &mut dyn Write) -> u8 {
    match exec::parse(args) {
        Ok(options) => exec::run(options, err),
        Err(why) => usage_error(err, &why),
    }
}

fn command_app(args: &[OsString], err: &mut dyn Write) -> u8 {
    match app::parse(args) {
        Ok(()) => app::run(err),
        Err(why) => usage_error(err, &why),
    }
}

fn command_upgrade(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let all = args.iter().any(|arg| arg == "--all");
    let words: Vec<&OsString> = args.iter().filter(|arg| *arg != "--all").collect();
    let (target, binary) = match (all, words.as_slice()) {
        (true, [binary]) => (upgrade::Target::All, *binary),
        (false, [id, binary]) => {
            let id = id
                .to_str()
                .filter(|text| !text.starts_with('-'))
                .and_then(|text| SessionId::new(text).ok());
            match id {
                Some(id) => (upgrade::Target::One(id), *binary),
                None => return usage_error(err, "that is not a session id"),
            }
        }
        _ => {
            return usage_error(
                err,
                "upgrade takes --all or a session id, and the path of the new kablo",
            );
        }
    };
    // The session's keeper is somewhere else: a relative path would mean
    // something else there.
    let binary = match std::path::absolute(binary) {
        Ok(binary) => binary,
        Err(e) => {
            let _ = writeln!(err, "kablo: {e}");
            return 1;
        }
    };
    let Some(dir) = directory(err) else {
        return 1;
    };
    upgrade::run(&dir, target, &binary, out, err)
}

fn command_new(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let parsed = match parse_new(args) {
        Ok(parsed) => parsed,
        Err(why) => return usage_error(err, &why),
    };
    // What the command ends with when kablo itself cannot: 255 when it is to end
    // with a program's status, which has 1 of its own.
    let trouble = if parsed.wait {
        drive::EXIT_WAIT_TROUBLE
    } else {
        1
    };
    let dir = match sockdir::socket_dir() {
        Ok(dir) => dir,
        Err(e) => {
            let _ = writeln!(err, "kablo: {e}");
            return trouble;
        }
    };
    let id = match parsed.id {
        Some(id) => id,
        None => match launch::fresh_id() {
            Ok(id) => id,
            Err(e) => {
                let _ = writeln!(err, "kablo: cannot make a session id: {e}");
                return trouble;
            }
        },
    };
    match launch::launch(&dir, &id, &parsed.request) {
        Ok(()) => {
            let _ = writeln!(out, "{id}");
            let _ = out.flush();
            if let Some(label) = &parsed.request.label
                && let Some(note) = drive::clash_note(&dir, &id, label)
            {
                let _ = writeln!(err, "kablo: {note}");
            }
            if parsed.wait {
                // The program's status, and the session is taken over once it
                // has ended: waiting is how an agent runs a program, and one
                // that ran hundreds would leave hundreds of ended sessions.
                return drive::await_end(&dir, &id, err);
            }
            0
        }
        Err(why) => {
            let _ = writeln!(err, "kablo: {why}");
            trouble
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_words(words: &[&str]) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(words.iter().map(OsString::from), &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn a_renewed_wardens_command_line_names_the_descriptor_of_its_record() {
        let words = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let renewed = internal(&words(&[
            "--id", "work", "--dir", "/tmp/d", "--resume", "9",
        ]))
        .unwrap();
        assert_eq!(renewed.id.as_str(), "work");
        assert_eq!(renewed.dir, Some(PathBuf::from("/tmp/d")));
        assert_eq!(renewed.resume, Some(9));
        let first = internal(&words(&["--id", "work", "--dir", "/tmp/d"])).unwrap();
        assert_eq!(first.resume, None);
        for wrong in [
            &["--id", "work", "--resume", "nine"][..],
            &["--id", "work", "--resume"],
            &["--resume", "9"],
        ] {
            assert!(internal(&words(wrong)).is_err(), "{wrong:?}");
        }
    }

    #[test]
    fn version_is_one_line_of_tokens() {
        let (code, out, err) = run_words(&["version"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out.lines().count(), 1);
        assert_eq!(out, format!("{}\n", version_line()));
        for token in out.split_whitespace() {
            assert!(
                token
                    .split_once('=')
                    .is_some_and(|(k, v)| !k.is_empty() && !v.is_empty()),
                "{token}"
            );
        }
        // The spellings people type out of habit say the same, as `--help` and
        // `-h` say what `help` does.
        for spelling in ["--version", "-V"] {
            assert_eq!(run_words(&[spelling]), (0, out.clone(), String::new()));
        }
    }

    #[test]
    fn anything_else_is_a_usage_error() {
        for words in [
            &[][..],
            &["bogus"],
            &["version", "extra"],
            &["--version", "extra"],
            &["-v"],
            &[""],
        ] {
            let (code, out, err) = run_words(words);
            assert_eq!(code, EXIT_USAGE, "{words:?}");
            assert_eq!(out, "");
            assert!(err.starts_with("usage: kablo"), "{words:?}");
        }
        let mut err = Vec::new();
        assert_eq!(
            run([OsString::from("ver\u{0}sion")], &mut Vec::new(), &mut err),
            EXIT_USAGE
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert_eq!(
                run([OsString::from_vec(vec![0xff])], &mut Vec::new(), &mut err),
                EXIT_USAGE
            );
        }
    }

    #[test]
    fn help_goes_to_stdout_and_succeeds() {
        for word in ["help", "-h", "--help"] {
            let (code, out, err) = run_words(&[word]);
            assert_eq!((code, err.as_str()), (0, ""));
            assert!(out.starts_with("usage: kablo"));
        }
    }

    fn os(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn new_reads_its_options_and_the_program_after_the_dashes() {
        let parsed = parse_new(&os(&[
            "--id",
            "work",
            "--cols",
            "100",
            "--env",
            "A=b=c",
            "--env",
            "E=",
            "--journal-size",
            "64K",
            "--history",
            "500",
            "--",
            "/bin/sh",
            "-c",
            "--id",
        ]))
        .unwrap();
        assert_eq!(parsed.id.unwrap().as_str(), "work");
        let size = parsed.request.size.unwrap();
        assert_eq!((size.cols, size.rows), (100, 24));
        assert_eq!(
            parsed.request.env,
            vec![
                (b"A".to_vec(), b"b=c".to_vec()),
                (b"E".to_vec(), Vec::new())
            ]
        );
        assert_eq!(parsed.request.journal_size, Some(64 * 1024));
        assert_eq!(parsed.request.history, Some(500));
        assert_eq!(
            parsed.request.argv,
            vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"--id".to_vec()]
        );
        assert_eq!(parse_new(&[]).unwrap(), NewArgs::default());
    }

    #[test]
    fn a_relative_directory_is_made_absolute_here() {
        let parsed = parse_new(&os(&["--cwd", "sub"])).unwrap();
        let cwd = parsed.request.cwd.unwrap();
        assert!(cwd.starts_with(b"/") && cwd.ends_with(b"/sub"));
    }

    #[test]
    fn what_new_cannot_use_is_named() {
        for bad in [
            &["--id"][..],
            &["--id", "a/b"],
            &["--cols", "0"],
            &["--cols", "1001"],
            &["--rows", "1001"],
            &["--rows", "x"],
            &["--env", "NOEQUALS"],
            &["--env", "=v"],
            &["--journal-size", "5"],
            &["--journal-size", "2T"],
            &["--journal-size", "1G1"],
            &["--history", "-1"],
            &["--history", "100001"],
            &["--history", "lots"],
            &["--bogus"],
        ] {
            assert!(parse_new(&os(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(history(OsStr::new("0")), Ok(0));
        assert_eq!(history(OsStr::new("100000")), Ok(MAX_HISTORY));
        assert_eq!(size_with_unit(OsStr::new("1G")), Ok(1 << 30));
        assert!(size_with_unit(OsStr::new("2G")).is_err());
    }
}
