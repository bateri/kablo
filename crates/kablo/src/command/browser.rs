//! `bateri browser VERB PANE`: what an agent asks of a browser a pane's page holds. The browser is
//! the bateri app's (its helper draws it, on the computer the app runs on), so the words go to the
//! app of this terminal as one `browser` message ([`kablo_bateri::browser`]) and are never relayed
//! to another machine; what the app says is printed.
//!
//! An answer that is a file is a file *here*: a program on a server cannot open a path of the
//! computer the app runs on. The command names the file (`part`, new at every ask), makes the
//! folder the app writes it in ([`paths::BROWSER`], mode 0700: a page may hold what a person signed
//! in to), sweeps the files in it that are more than a day old, and when the app says it wrote one,
//! checks that it is there whole and prints its path, which is the one thing a program is to be
//! told of it.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use kablo_bateri::browser::{Action, Browser, FileKind, Filed};
use kablo_bateri::message::{Reply, Request};
use kablo_bateri::{Anchor, package, paths};

use super::package::fresh_part;
use super::verbs::not_a_terminal;
use super::words::{address, clean};
use super::{Asked, Ctx, Done, Exit, Io, failure, unread};

/// How old a file of an answer is when the next `bateri browser` sweeps it away.
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
    let asked = read(io, words, fresh_part)?;
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
    let there = fs::metadata(&path).is_ok_and(|seen| seen.is_file() && seen.len() == filed.bytes);
    let shown = clean(&path.to_string_lossy());
    if !there {
        return Err(Exit::failed(format!(
            "the bateri app says it wrote {shown}, which is not there, or not as long as it said"
        )));
    }
    if filed.kind == FileKind::Text {
        io.say("the text is too long for an answer: it is in the file");
    }
    let _ = writeln!(io.out, "{shown}");
    Ok(())
}

/// The folder of the files, made, and what is old in it swept.
fn make_ready(folder: &Path) -> Done {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)
        .map_err(|why| {
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
/// answer may be, for the sub-verbs whose answers can be one.
fn read(io: &mut Io<'_>, words: &[String], part: impl FnOnce() -> String) -> Result<Browser, Exit> {
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
    let mut positional: Vec<&str> = Vec::new();
    let mut element: Option<String> = None;
    let mut into: Option<String> = None;
    let mut options_end = false;
    let mut at = rest.iter();
    while let Some(word) = at.next() {
        let word = word.as_str();
        if options_end || !word.starts_with('-') || word == "-" {
            positional.push(word);
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
    let Some((pane, args)) = positional.split_first() else {
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
            .map(|word| (*word).to_owned())
            .unwrap_or_default()
    };
    let action = match verb.as_str() {
        "ls" => Action::Ls,
        "text" => Action::Text { part: part() },
        "snapshot" => Action::Snapshot { part: part() },
        "shot" => Action::Shot { part: part() },
        "click" => Action::Click { target: word(0) },
        "type" => Action::Type {
            text: word(0),
            into,
        },
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
        let (mut out, mut err) = (Vec::new(), Vec::new());
        read(&mut io_of(&mut out, &mut err), &words(text), || {
            "part-1".to_owned()
        })
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
        // A lone dash is a word, as it is to `send`.
        let asked = read_words("type p9 -").unwrap();
        assert_eq!(
            asked.action,
            Action::Type {
                text: "-".to_owned(),
                into: None
            }
        );
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
