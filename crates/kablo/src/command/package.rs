//! `bateri package`: the programs this machine has. The folder is `kablo-package`'s; this is the
//! words a person types and the lines they read. What needs no bateri app to be connected is here:
//! the list, taking a program away, and the two notes that decide what is brought.

use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use kablo_bateri::message::Request;
use kablo_package::{Error, Store, this_target};

use super::bring::{Bateri, CHECK_EVERY, Machine, Person};
use super::words::clean;
use super::{Asked, Ctx, Done, Exit, Io};
use crate::{app, bridge, sys};

const USAGE: &str = "usage: bateri package list
       bateri package install SOURCE [--trust KEY]
       bateri package update [NAME]
       bateri package watch
       bateri package remove NAME
       bateri package auto [on|off]
       bateri package not-here [NAME]...
";

/// `bateri package VERB ARGS...`.
pub fn package(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let store = Store::in_home(&ctx.home());
    if let Some((verb, rest)) = words.split_first() {
        if matches!(verb.as_str(), "install" | "update" | "watch") {
            return bring(ctx, &store, io, verb, rest);
        }
    }
    run(&store, io, words)
}

/// The app of the bridge that started this program, when one did; else whichever one answers.
struct Asking<'a> {
    ctx: &'a Ctx,
    bridge: Option<u32>,
}

impl Bateri for Asking<'_> {
    fn ask(&self, request: &Request) -> Result<Asked, Exit> {
        self.ctx.ask_at(request, self.bridge)
    }
}

/// The person at the terminal this runs in: asked on standard error, answering on standard input.
struct AtTerminal;

impl Person for AtTerminal {
    fn at_terminal(&self) -> bool {
        sys::is_tty(std::io::stdin().as_fd()) && sys::is_tty(std::io::stderr().as_fd())
    }

    fn agrees(&mut self, question: &str) -> bool {
        let mut err = std::io::stderr();
        let _ = write!(err, "{question}");
        let _ = err.flush();
        let mut line = String::new();
        let read = std::io::stdin().lock().read_line(&mut line);
        read.is_ok() && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// A name for the directory a file is brought to, new at every call.
fn fresh_part() -> String {
    static SERIAL: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or_default();
    format!(
        "{:x}-{nanos:x}-{:x}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    )
}

fn bring(ctx: &Ctx, store: &Store, io: &mut Io<'_>, verb: &str, args: &[String]) -> Done {
    let Some(target) = this_target() else {
        return Err(Exit::failed(
            "no program is released for this kind of machine",
        ));
    };
    let asking = Asking {
        ctx,
        bridge: app::started_by_bridge(),
    };
    let parts = fresh_part;
    let mut person = AtTerminal;
    let mut machine = Machine {
        store,
        bateri: &asking,
        person: &mut person,
        target,
        switched_off: bridge::forwarding_off(std::path::Path::new(bridge::NO_FORWARDING)),
        parts: &parts,
    };
    match verb {
        "install" => machine.install(io, args),
        "update" => machine.update(io, args),
        _ => {
            if let Some(arg) = args.first() {
                return Err(Exit::mistake(format!(
                    "package watch takes no '{}'",
                    clean(arg)
                )));
            }
            machine.watch(&mut || {
                std::thread::sleep(CHECK_EVERY);
                true
            })
        }
    }
}

fn run(store: &Store, io: &mut Io<'_>, words: &[String]) -> Done {
    let Some((verb, rest)) = words.split_first() else {
        let _ = write!(io.err, "{USAGE}");
        return Err(Exit::status(64));
    };
    match verb.as_str() {
        "list" => list(store, io, rest),
        "remove" => remove(store, io, rest),
        "auto" => auto(store, io, rest),
        "not-here" => not_here(store, io, rest),
        other => {
            io.say(&format!("package has no '{}'", clean(other)));
            let _ = write!(io.err, "{USAGE}");
            Err(Exit::status(64))
        }
    }
}

/// What a refusal of the folder comes to for a person: a name that is wrong is a mistake in the
/// command, the rest is trouble.
fn failed(error: &Error) -> Exit {
    let text = clean(&error.to_string());
    match error {
        Error::Name(_) | Error::Reserved(_) => Exit::mistake(text),
        _ => Exit::failed(text),
    }
}

fn list(store: &Store, io: &mut Io<'_>, args: &[String]) -> Done {
    if let Some(arg) = args.first() {
        return Err(Exit::mistake(format!(
            "package list takes no '{}'",
            clean(arg)
        )));
    }
    let listing = store.list().map_err(|error| failed(&error))?;
    if listing.programs.is_empty() {
        let _ = writeln!(io.out, "no programs here");
    }
    for program in &listing.programs {
        let from = program
            .source
            .as_deref()
            .map(|source| format!(" from {}", clean(source)))
            .unwrap_or_default();
        let _ = writeln!(io.out, "{} {}{from}", program.name, program.version);
        if let Some(key) = &program.stopped {
            let _ = writeln!(
                io.out,
                "  stopped: a newer release is signed with {}, not with the approved key",
                clean(key)
            );
        }
    }
    let _ = writeln!(
        io.out,
        "updates on their own: {}",
        if listing.auto { "on" } else { "off" }
    );
    if !listing.not_here.is_empty() {
        let _ = writeln!(io.out, "not brought here: {}", listing.not_here.join(", "));
    }
    Ok(())
}

fn remove(store: &Store, io: &mut Io<'_>, args: &[String]) -> Done {
    let [name] = args else {
        return Err(Exit::mistake(
            "package remove takes the name of one program",
        ));
    };
    if store.remove(name).map_err(|error| failed(&error))? {
        let _ = writeln!(io.out, "removed {name}");
        Ok(())
    } else {
        Err(Exit::failed(format!("{} is not here", clean(name))))
    }
}

fn auto(store: &Store, io: &mut Io<'_>, args: &[String]) -> Done {
    match args {
        [] => {
            let _ = writeln!(io.out, "{}", if store.auto() { "on" } else { "off" });
            Ok(())
        }
        [state] if state == "on" || state == "off" => store
            .set_auto(state == "on")
            .map_err(|error| failed(&error)),
        _ => Err(Exit::mistake("package auto takes on or off")),
    }
}

fn not_here(store: &Store, io: &mut Io<'_>, args: &[String]) -> Done {
    if args.is_empty() {
        for name in store.not_here().map_err(|error| failed(&error))? {
            let _ = writeln!(io.out, "{name}");
        }
        return Ok(());
    }
    let names: Vec<&str> = args.iter().map(String::as_str).collect();
    store.mark_not_here(&names).map_err(|error| failed(&error))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use super::*;

    /// A folder of programs that is made for one test and taken away with it.
    struct Folder(PathBuf);

    impl Folder {
        fn new(label: &str) -> Folder {
            let dir = std::env::temp_dir().join(format!("kpkg-{}-{label}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Folder(dir)
        }

        /// A program as the folder keeps it, put there by hand.
        fn program(&self, name: &str, version: &str) {
            let dir = self.0.join(name).join(version);
            fs::create_dir_all(&dir).unwrap();
            fs::create_dir_all(self.0.join("bin")).unwrap();
            symlink(
                format!("../{name}/{version}/{name}"),
                self.0.join("bin").join(name),
            )
            .unwrap();
        }

        fn store(&self) -> Store {
            Store::new(self.0.clone(), &[])
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Said {
        status: u8,
        out: String,
        err: String,
    }

    fn say(folder: &Folder, words: &[&str]) -> Said {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let words: Vec<String> = words.iter().map(|word| (*word).to_string()).collect();
        let result = run(
            &folder.store(),
            &mut Io {
                out: &mut out,
                err: &mut err,
            },
            &words,
        );
        let status = match result {
            Ok(()) => 0,
            Err(exit) => {
                if let Some(text) = exit.text {
                    err.extend_from_slice(format!("bateri: {text}\n").as_bytes());
                }
                exit.code
            }
        };
        Said {
            status,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    #[test]
    fn an_empty_machine_says_so_and_that_updating_is_on() {
        let folder = Folder::new("empty");
        let said = say(&folder, &["list"]);
        assert_eq!(said.status, 0);
        assert_eq!(said.out, "no programs here\nupdates on their own: on\n");
    }

    #[test]
    fn the_list_has_a_line_for_a_program_and_one_for_what_stops_it() {
        let folder = Folder::new("list");
        folder.program("textpen", "1.0.0");
        folder.program("other", "0.2.1");
        fs::write(folder.0.join("textpen/source"), "bateri/textpen\n").unwrap();
        fs::write(folder.0.join("textpen/stopped"), "ed25519:abc\n").unwrap();
        let said = say(&folder, &["not-here", "zed", "alpha"]);
        assert_eq!(said.status, 0, "{}", said.err);
        assert_eq!(say(&folder, &["auto", "off"]).status, 0);
        let said = say(&folder, &["list"]);
        assert_eq!(said.status, 0, "{}", said.err);
        assert_eq!(
            said.out,
            "other 0.2.1\n\
             textpen 1.0.0 from bateri/textpen\n  \
             stopped: a newer release is signed with ed25519:abc, not with the approved key\n\
             updates on their own: off\n\
             not brought here: alpha, zed\n"
        );
    }

    #[test]
    fn auto_says_its_state_and_takes_on_and_off() {
        let folder = Folder::new("auto");
        assert_eq!(say(&folder, &["auto"]).out, "on\n");
        assert_eq!(say(&folder, &["auto", "off"]).status, 0);
        assert_eq!(say(&folder, &["auto"]).out, "off\n");
        assert_eq!(say(&folder, &["auto", "on"]).status, 0);
        assert_eq!(say(&folder, &["auto"]).out, "on\n");
        assert_eq!(say(&folder, &["auto", "maybe"]).status, 64);
        assert_eq!(say(&folder, &["auto", "on", "off"]).status, 64);
    }

    #[test]
    fn remove_takes_one_program_and_says_when_it_is_not_there() {
        let folder = Folder::new("remove");
        folder.program("textpen", "1.0.0");
        let said = say(&folder, &["remove", "textpen"]);
        assert_eq!((said.status, said.out.as_str()), (0, "removed textpen\n"));
        assert!(!folder.0.join("textpen").exists());
        let said = say(&folder, &["remove", "textpen"]);
        assert_eq!(said.status, 1);
        assert!(said.err.contains("textpen is not here"), "{}", said.err);
        assert_eq!(say(&folder, &["remove"]).status, 64);
        assert_eq!(say(&folder, &["remove", "a", "b"]).status, 64);
    }

    #[test]
    fn a_name_that_is_not_a_program_is_a_mistake_in_the_command() {
        let folder = Folder::new("names");
        for name in ["../x", "bin", "Textpen", ""] {
            assert_eq!(say(&folder, &["remove", name]).status, 64, "{name:?}");
            assert_eq!(say(&folder, &["not-here", name]).status, 64, "{name:?}");
        }
        // Nothing of the folder was touched.
        assert_eq!(fs::read_dir(&folder.0).unwrap().count(), 0);
    }

    #[test]
    fn not_here_without_names_lists_them() {
        let folder = Folder::new("nothere");
        assert_eq!(say(&folder, &["not-here"]).out, "");
        say(&folder, &["not-here", "b", "a"]);
        assert_eq!(say(&folder, &["not-here"]).out, "a\nb\n");
    }

    #[test]
    fn a_word_that_is_no_verb_gets_the_usage() {
        let folder = Folder::new("verbs");
        let said = say(&folder, &["install"]);
        assert_eq!(said.status, 64);
        assert!(
            said.err.contains("package has no 'install'"),
            "{}",
            said.err
        );
        assert!(said.err.contains("usage: bateri package list"));
        assert_eq!(say(&folder, &[]).status, 64);
    }
}
