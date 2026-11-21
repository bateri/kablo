//! `bateri browser VERB PANE`: what an agent asks of a browser a pane's page holds. The browser is
//! the bateri app's (its helper draws it, on the computer the app runs on), so the words go to the
//! app of this terminal as one `browser` message ([`kablo_bateri::browser`]) and are never relayed
//! to another machine; what the app says is printed.
//!
//! An answer that is a file is a file *here*: a program on a server cannot open a path of the
//! computer the app runs on. The command names the file (`part`, new at every ask), makes the
//! folder the app writes it in ([`paths::BROWSER`]) and sets it to mode 0700 each time, whatever it
//! was (a page may hold what a person signed in to), sweeps the files in it that are more than a
//! day old (at the next `text`, `snapshot` or `shot`, the asks that may leave one), and when the app
//! says it wrote one, checks that it is there whole and prints its path, which is the one thing a
//! program is to be told of it: a path that cannot be told as it is, is an error.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use kablo_bateri::browser::{Action, Browser, FileKind, Filed, TYPED_MAX};
use kablo_bateri::message::{Reply, Request};
use kablo_bateri::{Anchor, package, paths};

use super::package::fresh_part;
use super::verbs::not_a_terminal;
use super::view::input;
use super::words::{address, clean};
use super::{Asked, Ctx, Done, Exit, Io, failure, unread};
use crate::sys;

/// How old a file of an answer is when the next `text`, `snapshot` or `shot` sweeps it away.
const KEPT: Duration = Duration::from_secs(24 * 60 * 60);

/// What each sub-verb takes after the pane, for the mistake of one that gets another count.
const TAKES: [(&str, &[&str]); 9] = [
    ("ls", &[]),
    ("text", &[]),
    ("snapshot", &[]),
    ("shot", &[]),
    ("click", &["TARGET"]),
    ("type", &["TEXT"]),
    ("open", &["URL"]),
    ("back", &[]),
    ("reload", &[]),
];

/// `bateri browser VERB PANE ...`.
pub fn browser(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let asked = read(io, words, fresh_part, || input(TYPED_MAX as u64))?;
    let folder = ctx.state.join(paths::BROWSER);
    if asked.action.file().is_some() {
        make_ready(&folder)?;
    }
    let reply = match ctx.ask(&Request::Browser(asked.clone()))? {
        Asked::Reply(Reply::Failed(failed)) if package::is_old(&failed) => {
            return Err(Exit::failed(
                "this bateri does not drive browsers yet; update bateri",
            ));
        }
        Asked::Reply(Reply::Failed(failed)) => return Err(failure(&failed)),
        Asked::Reply(reply) => reply,
        Asked::Unreadable => return Err(unread()),
        Asked::NoApp | Asked::Silent => {
            return Err(Exit::new(
                69,
                "the bateri app is not connected: it keeps the browsers",
            ));
        }
    };
    answer(io, &folder, &asked.action, reply)
}

/// What the app said, if it is what the sub-verb takes for an answer.
fn answer(io: &mut Io<'_>, folder: &Path, action: &Action, reply: Reply) -> Done {
    match (action, reply) {
        (Action::Ls | Action::Text { .. } | Action::Snapshot { .. }, Reply::Text(body)) => {
            // A page's words: nothing in them moves the cursor of the terminal they are printed in.
            let _ = io.out.write_all(clean(&body).as_bytes());
            Ok(())
        }
        (
            Action::Text { part } | Action::Snapshot { part } | Action::Shot { part },
            Reply::Filed(filed),
        ) if action.file() == Some(filed.kind) => say_file(io, folder, part, &filed),
        (
            Action::Click { .. }
            | Action::Type { .. }
            | Action::Open { .. }
            | Action::Back
            | Action::Reload,
            Reply::Done,
        ) => Ok(()),
        _ => Err(unread()),
    }
}

/// Prints the path of the file the app said it wrote, once it is seen to be there and whole.
fn say_file(io: &mut Io<'_>, folder: &Path, part: &str, filed: &Filed) -> Done {
    let path = folder.join(filed.name(part));
    let shown = nameable(&path)?;
    let there = fs::metadata(&path).is_ok_and(|seen| seen.is_file() && seen.len() == filed.bytes);
    if !there {
        let sudo = std::env::var_os("SUDO_UID").is_some();
        return Err(Exit::failed(not_there(shown, folder, login_folder(), sudo)));
    }
    if filed.kind == FileKind::Text {
        io.say("the text is too long for an answer: it is in the file");
    }
    let _ = writeln!(io.out, "{shown}");
    Ok(())
}

/// `path` as the one thing a program is told of it: text, with no character in it that moves a
/// terminal or a line. A path that would have to be changed to be shown (it is not UTF-8, it has a
/// control character) is an error: a wrong path told is a program sent to another file.
fn nameable(path: &Path) -> Result<&str, Exit> {
    match path.to_str() {
        Some(text) if !text.chars().any(char::is_control) => Ok(text),
        _ => Err(Exit::failed(format!(
            "{} is not a path that can be told as it is (it is not UTF-8, or has a control character): set HOME to a plain path",
            clean(&path.to_string_lossy())
        ))),
    }
}

/// What is said of a file that the app told of and that is not there: where this looked, which is
/// below `HOME`; and, when that may not be the home the app writes below (the user's own, the one
/// whose terminal asked), what there is to tell the two apart: the login home of the user this runs
/// as (`login`) has the folder elsewhere, or it runs through `sudo` (`sudo`), which leaves `HOME`
/// alone or resets it. What the app wrote, and where, is not known here, so it is not said.
fn not_there(shown: &str, folder: &Path, login: Option<PathBuf>, sudo: bool) -> String {
    let mut said = format!(
        "the bateri app says it wrote {shown}, which is not there, or not as long as it said"
    );
    let apart = login.filter(|login| login != folder);
    if apart.is_some() || sudo {
        said.push_str(&format!(
            "; this looked below its HOME, in {}",
            clean(&folder.to_string_lossy())
        ));
        if let Some(login) = apart {
            said.push_str(&format!(
                ", and the login home of the user it runs as has the folder in {}",
                clean(&login.to_string_lossy())
            ));
        }
        said.push_str(
            ": the app writes below the home of the user whose terminal asked, so run this as that user, with that user's HOME",
        );
    }
    said
}

/// The folder the app writes the files in, below the login home of the user this runs as, which is
/// not the folder below `$HOME` where a program run with another `HOME` (`sudo -E`) looks.
fn login_folder() -> Option<PathBuf> {
    let home = PathBuf::from(OsString::from_vec(sys::passwd()?.home));
    home.is_absolute()
        .then(|| home.join(paths::DIR).join(paths::BROWSER))
}

/// The folder of the files, made, and what is old in it swept. It is mode 0700 whatever it was
/// before: the pictures of pages that a person signed in to go into it. What is above it is made as
/// anything is, not closed, since it is not ours.
fn make_ready(folder: &Path) -> Done {
    nameable(folder)?;
    let made = || -> io::Result<()> {
        if let Some(above) = folder.parent() {
            fs::create_dir_all(above)?;
        }
        match fs::DirBuilder::new().mode(0o700).create(folder) {
            Err(why) if why.kind() == io::ErrorKind::AlreadyExists && folder.is_dir() => {}
            made => made?,
        }
        fs::set_permissions(folder, fs::Permissions::from_mode(0o700))
    };
    made().map_err(|why| {
        Exit::failed(format!(
            "cannot make {} for the answers: {why}",
            clean(&folder.to_string_lossy())
        ))
    })?;
    sweep(folder, SystemTime::now());
    Ok(())
}

/// Removes the files of answers (`<part>.png`, `<part>.txt`) that are more than [`KEPT`] old at
/// `now`. What is not named so is not ours, and nothing but a plain file is touched.
fn sweep(folder: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_answer) {
            continue;
        }
        // `DirEntry::metadata` does not follow a link: a link is not an answer.
        let Ok(seen) = entry.metadata() else {
            continue;
        };
        let old = seen
            .modified()
            .ok()
            .and_then(|written| now.duration_since(written).ok())
            .is_some_and(|age| age > KEPT);
        if seen.is_file() && old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Whether `name` is what the app writes an answer as: a part, a dot and `png` or `txt`.
fn is_answer(name: &str) -> bool {
    let Some((part, extension)) = name.rsplit_once('.') else {
        return false;
    };
    matches!(extension, "png" | "txt")
        && !part.is_empty()
        && part.len() <= 64
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// What the words say: the sub-verb, the pane and what the sub-verb takes. `part` names the file an
/// answer may be, for the sub-verbs whose answers can be one, and `input` reads the text that `type`
/// takes from standard input where the word is a `-` that no `--` came before, as `view type` does.
fn read(
    io: &mut Io<'_>,
    words: &[String],
    part: impl FnOnce() -> String,
    input: impl FnOnce() -> Result<String, Exit>,
) -> Result<Browser, Exit> {
    let Some((verb, rest)) = words.split_first() else {
        return Err(Exit::mistake(format!("browser takes {}", list_of_verbs())));
    };
    let Some((_, takes)) = TAKES.iter().find(|(known, _)| *known == verb.as_str()) else {
        return Err(Exit::mistake(format!(
            "browser takes {}, not '{}'",
            list_of_verbs(),
            clean(verb)
        )));
    };
    // Each word that is no option, and whether it is the bare `-` of standard input.
    let mut positional: Vec<(&str, bool)> = Vec::new();
    let mut element: Option<String> = None;
    let mut into: Option<String> = None;
    let mut options_end = false;
    let mut at = rest.iter();
    while let Some(word) = at.next() {
        let word = word.as_str();
        if options_end || !word.starts_with('-') || word == "-" {
            positional.push((word, !options_end && word == "-"));
        } else if word == "--" {
            options_end = true;
        } else if word == "--element" || (word == "--into" && verb == "type") {
            let Some(value) = at.next() else {
                return Err(Exit::mistake(format!(
                    "{word} takes {}",
                    if word == "--into" {
                        "a target"
                    } else {
                        "an id"
                    }
                )));
            };
            let place = if word == "--into" {
                &mut into
            } else {
                &mut element
            };
            if place.replace(value.clone()).is_some() {
                return Err(Exit::mistake(format!("{word} is given twice")));
            }
        } else {
            return Err(Exit::mistake(format!(
                "browser {verb} takes no '{}'",
                clean(word)
            )));
        }
    }
    let Some(((pane, _), args)) = positional.split_first() else {
        return Err(Exit::mistake(format!("browser {verb} takes a pane")));
    };
    if address(pane).is_none() {
        return Err(not_a_terminal(io, pane, ""));
    }
    if args.len() != takes.len() {
        let mut usage = format!("browser {verb} takes PANE");
        for word in *takes {
            usage.push(' ');
            usage.push_str(word);
        }
        return Err(Exit::mistake(usage));
    }
    let pane = Anchor::address(pane).ok_or_else(|| not_a_terminal(io, pane, ""))?;
    let word = |at: usize| {
        args.get(at)
            .map(|(word, _)| (*word).to_owned())
            .unwrap_or_default()
    };
    let action = match verb.as_str() {
        "ls" => Action::Ls,
        "text" => Action::Text { part: part() },
        "snapshot" => Action::Snapshot { part: part() },
        "shot" => Action::Shot { part: part() },
        "click" => Action::Click { target: word(0) },
        "type" => {
            let dash = args.first().is_some_and(|(_, dash)| *dash);
            Action::Type {
                text: if dash { input()? } else { word(0) },
                into,
            }
        }
        "open" => Action::Open { url: word(0) },
        "back" => Action::Back,
        _ => Action::Reload,
    };
    let asked = Browser {
        pane,
        element,
        action,
    };
    // Written once here, so that what the message cannot carry is a mistake before the app is asked.
    Request::Browser(asked.clone())
        .encode()
        .map_err(|failure| Exit::mistake(clean(&failure.text)))?;
    Ok(asked)
}

fn list_of_verbs() -> String {
    Action::WORDS.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    fn io_of<'a>(out: &'a mut Vec<u8>, err: &'a mut Vec<u8>) -> Io<'a> {
        Io { out, err }
    }

    fn read_words(text: &str) -> Result<Browser, Exit> {
        read_with_input(text, Err(Exit::failed("standard input is not to be read")))
    }

    fn read_with_input(text: &str, input: Result<String, Exit>) -> Result<Browser, Exit> {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        read(
            &mut io_of(&mut out, &mut err),
            &words(text),
            || "part-1".to_owned(),
            || input,
        )
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn folder(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kbrowser-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_words_say_a_verb_a_pane_and_what_the_verb_takes() {
        let asked = read_words("text p9@prod --element main").unwrap();
        assert_eq!(asked.pane, Anchor::address("p9@prod").unwrap());
        assert_eq!(asked.element.as_deref(), Some("main"));
        assert_eq!(
            asked.action,
            Action::Text {
                part: "part-1".to_owned()
            }
        );
        // The options may stand anywhere; what follows `--` is the text.
        let asked = read_words("type --element a p9 --into e1 -- --into").unwrap();
        assert_eq!(asked.element.as_deref(), Some("a"));
        assert_eq!(
            asked.action,
            Action::Type {
                text: "--into".to_owned(),
                into: Some("e1".to_owned())
            }
        );
        // After `--` a dash is a word, as it is to `send`.
        let asked = read_words("type p9 -- -").unwrap();
        assert_eq!(
            asked.action,
            Action::Type {
                text: "-".to_owned(),
                into: None
            }
        );
    }

    #[test]
    fn a_lone_dash_takes_the_text_of_type_from_standard_input_as_view_does() {
        let asked = read_with_input("type p9 -", Ok("hi\nthere\n".to_owned())).unwrap();
        assert_eq!(
            asked.action,
            Action::Type {
                text: "hi\nthere\n".to_owned(),
                into: None
            }
        );
        let asked = read_with_input("type --into e1 p9 -", Ok("x".to_owned())).unwrap();
        assert_eq!(
            asked.action,
            Action::Type {
                text: "x".to_owned(),
                into: Some("e1".to_owned())
            }
        );
        // What standard input says wrong, or too much, or nothing, is the command's mistake.
        let wrong = read_with_input("type p9 -", Err(Exit::mistake("too long"))).unwrap_err();
        assert_eq!((wrong.code, wrong.text.as_deref()), (64, Some("too long")));
        assert_eq!(
            read_with_input("type p9 -", Ok(String::new()))
                .unwrap_err()
                .code,
            64
        );
        // Only `type` has a text; the dash of another verb is a word as any other.
        assert_eq!(
            read_with_input("click p9 -", Err(Exit::failed("not read")))
                .unwrap()
                .action,
            Action::Click {
                target: "-".to_owned()
            }
        );
        // Nor is the dash of `open` read: it is an address, and a wrong one (64, not 1).
        assert_eq!(
            read_with_input("open p9 -", Err(Exit::failed("not read")))
                .unwrap_err()
                .code,
            64
        );
    }

    #[test]
    fn an_address_that_open_does_not_take_is_a_mistake_before_the_app_is_asked() {
        let said = read_words("open p9 example.com").unwrap_err();
        assert_eq!(said.code, 64);
        assert!(said.text.unwrap().contains("http:// or https://"));
        assert_eq!(
            read_words("open p9 file:///etc/passwd").unwrap_err().code,
            64
        );
        assert!(read_words("open p9 https://example.com").is_ok());
        assert!(read_words("open p9 about:blank").is_ok());
    }

    #[test]
    fn a_count_of_words_that_is_not_the_verbs_is_a_mistake_that_says_what_it_takes() {
        let mistake = |text: &str| read_words(text).unwrap_err();
        let said = mistake("click p9");
        assert_eq!(said.code, 64);
        assert_eq!(
            said.text.as_deref(),
            Some("browser click takes PANE TARGET")
        );
        assert_eq!(
            mistake("back p9 now").text.as_deref(),
            Some("browser back takes PANE")
        );
        assert_eq!(
            mistake("dance p9").text.as_deref(),
            Some(
                "browser takes ls, text, snapshot, shot, click, type, open, back, reload, not 'dance'"
            )
        );
        assert_eq!(mistake("ls p9 --element a").code, 64);
        assert_eq!(mistake("text p9 --into e1").code, 64);
        assert_eq!(mistake("text p9 --element a --element b").code, 64);
    }

    #[test]
    fn only_files_of_answers_that_are_old_are_swept() {
        let dir = folder("sweep");
        let now = SystemTime::now();
        let old = now - KEPT - Duration::from_secs(60);
        let make = |name: &str, written: SystemTime| {
            let path = dir.join(name);
            fs::write(&path, b"x").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(written)
                .unwrap();
        };
        make("a-1.png", old);
        make("b_2.txt", old);
        make("fresh.png", now - Duration::from_secs(60));
        make("notes.md", old);
        make(".hidden.png", old);
        make("a.b.png", old);
        fs::create_dir(dir.join("dir.png")).unwrap();
        sweep(&dir, now);
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [".hidden.png", "a.b.png", "dir.png", "fresh.png", "notes.md"]
        );
        // A folder that is not there is nothing to sweep.
        sweep(&dir.join("none"), now);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_folder_is_0700_whatever_it_was_and_what_is_above_it_is_not_closed() {
        let base = folder("ready");
        let plain = base.join("plain");
        fs::create_dir(&plain).unwrap();
        let usual = mode_of(&plain);
        // Made below folders that are not there: only the last is closed.
        let made = base.join(".local/share/bateri/browser");
        make_ready(&made).unwrap();
        assert_eq!(mode_of(&made), 0o700);
        for above in [".local", ".local/share", ".local/share/bateri"] {
            assert_eq!(mode_of(&base.join(above)), usual, "{above}");
        }
        // A folder that was there, and open: closed, with what it holds left as it was.
        fs::set_permissions(&made, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(made.join("keep.png"), b"x").unwrap();
        make_ready(&made).unwrap();
        assert_eq!(mode_of(&made), 0o700);
        assert!(made.join("keep.png").is_file());
        // A file in its place is a failure that names the folder.
        let file = base.join("file");
        fs::write(&file, b"x").unwrap();
        let failed = make_ready(&file).unwrap_err();
        assert_eq!(failed.code, 1);
        assert!(failed.text.unwrap().contains("cannot make"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_path_that_cannot_be_told_as_it_is_is_an_error_not_a_wrong_path() {
        let plain = Path::new("/home/a b/.local/share/bateri/browser/p-1.png");
        assert_eq!(
            nameable(plain).unwrap(),
            "/home/a b/.local/share/bateri/browser/p-1.png"
        );
        for bad in [
            PathBuf::from(OsString::from_vec(b"/home/\xff/browser".to_vec())),
            PathBuf::from("/home/a\nb/browser"),
            PathBuf::from("/home/a\tb/browser"),
            PathBuf::from("/home/a\x1b[2Jb/browser"),
        ] {
            let failed = nameable(&bad).unwrap_err();
            assert_eq!(failed.code, 1);
            assert!(failed.text.unwrap().contains("not a path that can be told"));
        }
        // Neither the folder is made, nor a file said, below such a home.
        let base = folder("named");
        let odd = base.join("a\nb");
        assert!(make_ready(&odd.join("browser")).is_err());
        assert!(!odd.exists());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let filed = Filed {
            kind: FileKind::Png,
            bytes: 0,
        };
        assert!(say_file(&mut io_of(&mut out, &mut err), &odd, "p-1", &filed).is_err());
        assert!(out.is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_file_that_is_not_there_says_where_this_looked_and_where_the_app_writes() {
        let looked = Path::new("/home/a/.local/share/bateri/browser");
        let shown = "/home/a/.local/share/bateri/browser/p-1.png";
        // Where the login home is the HOME, and no `sudo`, there is nothing to tell apart.
        let same = not_there(shown, looked, Some(looked.to_owned()), false);
        assert!(same.contains("is not there"));
        assert!(!same.contains("HOME"), "{same}");
        assert!(!not_there(shown, looked, None, false).contains("HOME"));
        // A login home that has the folder elsewhere: both places are named, and neither is said
        // to hold the file.
        let root = PathBuf::from("/root/.local/share/bateri/browser");
        let said = not_there(shown, looked, Some(root), false);
        assert!(said.contains("below its HOME, in /home/a/.local/share/bateri/browser"));
        assert!(said.contains("has the folder in /root/.local/share/bateri/browser"));
        assert!(said.contains("run this as that user"));
        // `sudo -E` leaves HOME as the user's and the login home is root's: told with or without it.
        let said = not_there(shown, looked, Some(looked.to_owned()), true);
        assert!(said.contains("below its HOME, in /home/a/.local/share/bateri/browser"));
        assert!(!said.contains("has the folder in"));
        // The login folder is below the passwd home, where the app writes.
        if let Some(login) = login_folder() {
            assert!(login.ends_with(".local/share/bateri/browser"));
        }
    }

    #[test]
    fn the_path_of_a_file_is_said_once_it_is_there_whole() {
        let dir = folder("file");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let filed = Filed {
            kind: FileKind::Png,
            bytes: 4,
        };
        // Not yet, and not whole: no path is said.
        for body in [None, Some(&b"123"[..]), Some(&b"12345"[..])] {
            if let Some(body) = body {
                fs::write(dir.join("p-1.png"), body).unwrap();
            }
            let done = say_file(&mut io_of(&mut out, &mut err), &dir, "p-1", &filed);
            let said = done.unwrap_err();
            assert_eq!(said.code, 1);
            assert!(said.text.unwrap().contains("is not there"));
        }
        assert!(out.is_empty());
        fs::write(dir.join("p-1.png"), b"1234").unwrap();
        say_file(&mut io_of(&mut out, &mut err), &dir, "p-1", &filed).unwrap();
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            format!("{}\n", dir.join("p-1.png").display())
        );
        assert!(err.is_empty(), "a picture needs no note");
        // A text that went to a file says why it did.
        out.clear();
        fs::write(dir.join("p-2.txt"), b"123").unwrap();
        let text = Filed {
            kind: FileKind::Text,
            bytes: 3,
        };
        say_file(&mut io_of(&mut out, &mut err), &dir, "p-2", &text).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{}\n", dir.join("p-2.txt").display())
        );
        assert!(String::from_utf8(err).unwrap().contains("too long"));
        // A folder in the place of the file is no file.
        fs::create_dir(dir.join("p-3.png")).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let filed = Filed {
            kind: FileKind::Png,
            bytes: 0,
        };
        assert!(say_file(&mut io_of(&mut out, &mut err), &dir, "p-3", &filed).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_is_taken_as_the_answer_only_of_the_verb_that_takes_one() {
        let dir = folder("kind");
        fs::write(dir.join("p.png"), b"1234").unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let picture = Reply::Filed(Filed {
            kind: FileKind::Png,
            bytes: 4,
        });
        let shot = Action::Shot {
            part: "p".to_owned(),
        };
        let text = Action::Text {
            part: "p".to_owned(),
        };
        assert!(answer(&mut io_of(&mut out, &mut err), &dir, &shot, picture.clone()).is_ok());
        for action in [&text, &Action::Ls, &Action::Back] {
            assert!(
                answer(
                    &mut io_of(&mut out, &mut err),
                    &dir,
                    action,
                    picture.clone()
                )
                .is_err()
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
